//! Wayland native desktop capture: xdg-desktop-portal ScreenCast → PipeWire.
//!
//! Wayland 合成器协议不允许应用随意抓屏（正是它相对 X11 的安全改进），
//! 唯一的标准路径是 xdg-desktop-portal 的 ScreenCast 接口：
//!
//! 1. `ScreenCast.CreateSession` → 会话句柄
//! 2. `ScreenCast.SelectSources`（types=monitor）→ 用户在系统弹窗里授权
//!    要共享的屏幕
//! 3. `ScreenCast.Start` → 返回 PipeWire 流 node path
//! 4. `ScreenCast.OpenPipeWireRemote` → PipeWire daemon 的 fd
//! 5. gstreamer `pipewiresrc fd=<fd> path=<node>` → `videoconvert` →
//!    `appsink`（BGRx）逐帧拉取
//!
//! 该流程与 RustDesk 的 `libs/scrap/src/wayland/pipewire.rs` 同构（去掉
//! 多显示器/窗口选择与 KDE 偏移修正——单屏共享场景不需要）。
//!
//! 输入注入在 Wayland 原生会话下同样受协议限制：必须走 portal 的
//! RemoteDesktop 接口注入（当前构建只做画面共享，见 input.rs 说明）。
//!
//! 构建：`--features wayland`（需要 zbus + gstreamer 与系统
//! xdg-desktop-portal / pipewire / gst-plugin-pipewire）。未启用时
//! `--desktop-capture wayland` 报可操作错误。

#![cfg(all(target_os = "linux", feature = "wayland"))]

use super::capture::{Frame, FrameSource};
use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer::prelude::*;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, Value};

/// Portal 请求的 Response 信号最长等待（用户在授权弹窗上的操作时间）。
const PORTAL_WAIT: Duration = Duration::from_secs(180);

/// One PipeWire-backed capture session.
pub struct WaylandSource {
    /// PipeWire 管道。appsink 每次拉一帧 BGRx。
    pipeline: gstreamer::Pipeline,
    appsink: gstreamer_app::AppSink,
    width: usize,
    height: usize,
    /// 上一帧复用：Wayland 桌面静止时 pipewire 不推帧，next_frame 仍需
    /// 返回帧（编码时钟按 fps 推进；编码器对重复帧输出空帧，被心跳
    /// IDR 路径吸收）。
    last_frame: Arc<Mutex<Vec<u8>>>,
    /// D-Bus 连接与 fd 必须存活整个会话生命周期（fd 关闭即流终止）。
    _conn: Arc<Connection>,
    _fd: Arc<OwnedFdKeepalive>,
}

/// zvariant OwnedFd 已在 drop 时关闭，这里再包一层（gstreamer 持有的是
/// raw fd，必须确保 rust 侧的关闭时机在整个管道停止之后）。
struct OwnedFdKeepalive(zbus::zvariant::OwnedFd);

unsafe impl Send for WaylandSource {}

impl WaylandSource {
    pub fn open() -> Result<Self, String> {
        gstreamer::init().map_err(|e| format!("gstreamer init: {e}"))?;
        let (conn, fd, node) = request_screencast()?;
        Self::start_stream(conn, fd, node)
    }

    fn start_stream(
        conn: Arc<Connection>,
        fd: Arc<OwnedFdKeepalive>,
        node: u64,
    ) -> Result<Self, String> {
        let pipeline = gstreamer::Pipeline::new();
        let src = gstreamer::ElementFactory::make("pipewiresrc")
            .build()
            .map_err(|_| "pipewiresrc unavailable — install gst-plugin-pipewire".to_string())?;
        src.set_property("fd", fd.0.as_raw_fd() as i32);
        src.set_property("path", node.to_string());
        // pipewire 在 appsink 析构时可能卡死, always-copy 规避（rustdesk
        // 同款 workaround, pipewire#982）。
        src.set_property("always-copy", true);

        // videoconvert 放宽协商（COSMIC 等 portal 后端协商窄格式集时
        // not-negotiated）。
        let convert = gstreamer::ElementFactory::make("videoconvert")
            .build()
            .map_err(|e| format!("videoconvert: {e}"))?;
        let sink = gstreamer::ElementFactory::make("appsink")
            .build()
            .map_err(|e| format!("appsink: {e}"))?;
        sink.set_property("drop", true);
        sink.set_property("max-buffers", 1u32);

        pipeline
            .add_many([&src, &convert, &sink])
            .map_err(|e| format!("pipeline add: {e}"))?;
        src.link(&convert).map_err(|e| format!("link src: {e}"))?;
        convert.link(&sink).map_err(|e| format!("link convert: {e}"))?;

        let appsink = sink
            .dynamic_cast::<gstreamer_app::AppSink>()
            .map_err(|_| "sink is not an appsink".to_string())?;
        let mut caps = gstreamer::Caps::new_empty();
        caps.merge_structure(gstreamer::Structure::builder("video/x-raw")
            .field("format", "BGRx")
            .build());
        caps.merge_structure(gstreamer::Structure::builder("video/x-raw")
            .field("format", "RGBx")
            .build());
        appsink.set_caps(Some(&caps));

        pipeline
            .set_state(gstreamer::State::Playing)
            .map_err(|e| format!("pipeline playing: {e}"))?;
        // 等待真正 PLAYING（并发的多管道启动会触发 pipewire 竞态崩溃）。
        let _ = pipeline.state(gstreamer::ClockTime::from_mseconds(2000));
        std::thread::sleep(Duration::from_millis(150));

        // 用首帧的 caps 确定分辨率。
        let sample = appsink
            .try_pull_sample(gstreamer::ClockTime::from_mseconds(3000))
            .ok_or("no first frame from PipeWire within 3s (portal denied?)")?;
        let (w, h) = sample_size(&sample)?;

        Ok(Self {
            pipeline,
            appsink,
            width: w,
            height: h,
            last_frame: Arc::new(Mutex::new(Vec::new())),
            _conn: conn,
            _fd: fd,
        })
    }
}

impl FrameSource for WaylandSource {
    fn resolution(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn next_frame(&mut self) -> Result<Frame, String> {
        match self
            .appsink
            .try_pull_sample(gstreamer::ClockTime::from_mseconds(1000))
        {
            Some(sample) => {
                let (w, h) = sample_size(&sample)?;
                let buffer = sample
                    .buffer_owned()
                    .ok_or("sample without buffer")?
                    .into_mapped_buffer_readable()
                    .map_err(|_| "map buffer".to_string())?;
                let data = buffer.as_slice();
                if data.len() != w * h * 4 {
                    // caps 尺寸与 buffer 不符（pipewire#985）：丢帧不硬崩。
                    return Err(format!(
                        "pipewire buffer size {} != {}x{}x4",
                        data.len(),
                        w,
                        h
                    ));
                }
                // BGRx 即 BGRA 布局（X 通道任意）；RGBx 需要 R/B 交换。
                let bgra = if sample_caps_format(&sample)? == "BGRx" {
                    data.to_vec()
                } else {
                    let mut v = data.to_vec();
                    for px in v.chunks_exact_mut(4) {
                        px.swap(0, 2); // RGBx → BGRx
                    }
                    v
                };
                *self.last_frame.lock().unwrap() = bgra.clone();
                Ok(Frame {
                    bgra,
                    width: w,
                    height: h,
                })
            }
            None => {
                // 静止桌面无新帧：复用上一帧。从未取到帧则报错让上层重试。
                let guard = self.last_frame.lock().unwrap();
                if guard.is_empty() {
                    return Err("no frame available yet".to_string());
                }
                Ok(Frame {
                    bgra: guard.clone(),
                    width: self.width,
                    height: self.height,
                })
            }
        }
    }
}

impl Drop for WaylandSource {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gstreamer::State::Null);
        let _ = self.pipeline.state(gstreamer::ClockTime::from_mseconds(2000));
    }
}

fn sample_size(sample: &gstreamer::Sample) -> Result<(usize, usize), String> {
    let caps = sample.caps().ok_or("sample without caps")?;
    let s = caps.structure(0).ok_or("caps without structure")?;
    let w = s
        .get::<i32>("width")
        .map_err(|e| format!("caps width: {e}"))?;
    let h = s
        .get::<i32>("height")
        .map_err(|e| format!("caps height: {e}"))?;
    Ok((w as usize, h as usize))
}

fn sample_caps_format(sample: &gstreamer::Sample) -> Result<String, String> {
    let caps = sample.caps().ok_or("sample without caps")?;
    let s = caps.structure(0).ok_or("caps without structure")?;
    s.get::<String>("format")
        .map_err(|e| format!("caps format: {e}"))
}

/// 走完 portal ScreenCast 四步握手，拿到 (conn, pw fd, node path)。
fn request_screencast() -> Result<(Arc<Connection>, Arc<OwnedFdKeepalive>, u64), String> {
    let conn = Connection::session()
        .map_err(|e| format!("session bus: {e}"))?;
    let conn = Arc::new(conn);

    // 1. CreateSession（ScreenCast 接口代理）。
    let portal = portal_proxy(&conn, "org.freedesktop.portal.ScreenCast")?;
    let session: String = {
        let mut opts: HashMap<String, Value> = HashMap::new();
        opts.insert("session_handle_token".to_string(), Value::from("sr1"));
        let (resp, results) = call_and_wait(&conn, &portal, "CreateSession", &opts, "sr1")?;
        if resp != 0 {
            return Err(format!("CreateSession denied (code {resp})"));
        }
        results
            .get("session_handle")
            .and_then(|v| match v {
                Value::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .ok_or("CreateSession response missing session_handle")?
    };

    // 2. SelectSources（monitor、隐藏光标——远端注入光标会双影）。
    {
        let mut opts: HashMap<String, Value> = HashMap::new();
        opts.insert("types".to_string(), Value::from(1u32)); // MONITOR
        opts.insert("multiple".to_string(), Value::from(false));
        opts.insert("cursor_mode".to_string(), Value::from(1u32)); // HIDDEN
        let session_path = ObjectPath::try_from(session.as_str())
            .map_err(|e| format!("session path: {e}"))?;
        let (resp, _) = call_and_wait(&conn, &portal, "SelectSources", &(session_path, opts), "sr2")?;
        if resp != 0 {
            return Err(format!("SelectSources denied (code {resp})"));
        }
    }

    // 3. Start → streams。
    let (node, session_path) = {
        let mut opts: HashMap<String, Value> = HashMap::new();
        let session_path = ObjectPath::try_from(session.as_str())
            .map_err(|e| format!("session path: {e}"))?;
        let (resp, results) =
            call_and_wait(&conn, &portal, "Start", &(session_path.clone(), "", opts), "sr3")?;
        if resp != 0 {
            return Err(format!(
                "Start denied (code {resp}) — user cancelled the share dialog?"
            ));
        }
        let node = parse_streams_node(&results)
            .ok_or("Start response has no usable streams")?;
        (node, session_path)
    };

    // 4. OpenPipeWireRemote → fd（zvariant3 的 OwnedFd 在反序列化时 dup，
    // 自己持有并负责关闭——直接用它做 keepalive）。
    let empty: HashMap<String, Value> = HashMap::new();
    let fd: zbus::zvariant::OwnedFd = portal
        .call("OpenPipeWireRemote", &(session_path, empty))
        .map_err(|e| format!("OpenPipeWireRemote: {e}"))?;

    drop(portal);
    Ok((conn, Arc::new(OwnedFdKeepalive(fd)), node))
}

fn portal_proxy<'a>(
    conn: &'a Connection,
    interface: &'a str,
) -> Result<Proxy<'a>, String> {
    Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        interface,
    )
    .map_err(|e| format!("portal proxy: {e}"))
}

/// 调一个 portal 方法并等待其 Request Response 信号（先订阅再调用，避免
/// 竞态——portal 文档明确要求的约定）。返回 (response_code, results)。
fn call_and_wait(
    conn: &Connection,
    portal: &Proxy<'_>,
    method: &str,
    args: &(impl serde::ser::Serialize + zbus::zvariant::DynamicType),
    token: &str,
) -> Result<(u32, HashMap<String, Value<'static>>), String> {
    let unique = conn
        .unique_name()
        .map(|n| n.trim_start_matches(':').replace('.', "_"))
        .unwrap_or_default();
    let path = format!(
        "/org/freedesktop/portal/desktop/request/{unique}/{token}"
    );
    let request_path = ObjectPath::try_from(path.as_str())
        .map_err(|e| format!("request path: {e}"))?;

    // Request 接口代理（destination 同 portal 服务）。
    let req_proxy = Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        request_path,
        "org.freedesktop.portal.Request",
    )
    .map_err(|e| format!("request proxy: {e}"))?;
    let mut signals = req_proxy
        .receive_signal("Response")
        .map_err(|e| format!("subscribe Response: {e}"))?;

    portal
        .call_method::<_, _>(method, args)
        .map_err(|e| format!("call {method}: {e}"))?;

    // 阻塞等信号。SignalIterator 的 next() 无超时参数——portal 在用户
    // 取消/超时时会发出非 0 response，信号总会到来；万一服务消失，
    // next() 返回 None 由上面 ok_or 兜底。
    let msg = signals.next().ok_or("signal stream closed")?;
    let raw_body = msg
        .body::<zbus::zvariant::Structure>()
        .map_err(|e| format!("Response body: {e}"))?;
    let fields = raw_body.fields();
    let resp_code = fields
        .first()
        .and_then(|v| match v {
            Value::U32(n) => Some(*n),
            _ => None,
        })
        .unwrap_or(1);
    let results: HashMap<String, Value> = fields
        .get(1)
        .and_then(|v| match v {
            Value::Dict(d) => {
                // zvariant3: Dict → HashMap 需要克隆出 owned 键值。
                let dict_clone = d.clone();
                let owned: HashMap<String, zbus::zvariant::OwnedValue> =
                    dict_clone.try_into().ok()?;
                let mut m = HashMap::new();
                for (k, v2) in owned {
                    m.insert(k, Value::from(v2));
                }
                Some(m)
            }
            _ => None,
        })
        .unwrap_or_default();
    Ok((resp_code, results))
}

/// 从 Start 响应的 results["streams"] 提取第一个流的 node path。
fn parse_streams_node(results: &HashMap<String, Value>) -> Option<u64> {
    let streams = results.get("streams")?;
    // D-Bus 类型 a(ua{sv}): 第 0 字段是 PipeWire node path (u32)。
    let Value::Array(arr) = streams else {
        return None;
    };
    let first = arr.iter().next()?;
    let Value::Structure(s) = first else {
        return None;
    };
    let fields: &[Value] = s.fields();
    match fields.first() {
        Some(Value::U32(n)) => Some(*n as u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_module_compiles() {
        // 存在性检查（真实 portal 会话需要用户在弹窗授权，CI 无法自动化；
        // 真机验证步骤见 MYS-886 交付说明）。
    }
}
