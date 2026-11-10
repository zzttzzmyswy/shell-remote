//! Desktop frame capture backends.
//!
//! `FrameSource::next_frame()` yields a packed BGRA frame (B,G,R,A per byte,
//! 4 bytes/pixel) that the pipeline feeds to `bgra_to_i420` and then the
//! encoder. Backends:
//! - Windows: DXGI Desktop Duplication (`dxgi`, default — GPU composed
//!   surface, supports 60fps) with GDI `BitBlt` fallback (`gdi`).
//! - X11: pure-Rust `x11rb` (any Unix with an X server; works under Xvfb and
//!   through XWayland for Wayland sessions).
//! - Wayland native: xdg-desktop-portal ScreenCast → PipeWire (`wayland`,
//!   `cfg(target_os = "linux")` only).
//!
//! `open_source` resolves `auto` per-platform: Windows tries dxgi→gdi, Linux
//! tries wayland (when a portal is reachable) then X11.

use std::sync::Arc;
use crate::agent::desktop::FramePool;

/// One captured frame: packed BGRA (little-endian byte order B,G,R,A).
pub struct Frame {
    pub bgra: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// 一台远端显示器（X11 RANDR 输出）：名称 + 分辨率 + 偏移（桌面坐标系）。
#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}

/// 把所选显示器原点打包进一个原子量（`x << 32 | y`，两半都按 u32 位模式
/// 存，负数原点如 Windows 主屏左侧的副屏 `-1920` 也能原样取回）。
///
/// 采集源（`FrameSource::origin`）在打开/重建后把值写进共享原子，
/// `DesktopManager::handle_mouse` 读它给鼠标移动加偏移（MYS-2765 #10）。
pub fn pack_origin(x: i32, y: i32) -> i64 {
    (((x as u32) as u64) << 32 | (y as u32) as u64) as i64
}

/// [`pack_origin`] 的逆运算。
pub fn unpack_origin(packed: i64) -> (i32, i32) {
    let v = packed as u64;
    ((v >> 32) as u32 as i32, (v & 0xffff_ffff) as u32 as i32)
}

// ── DXGI 错误分类（MYS-2765 #4）───────────────────────────────
//
// Windows 侧 `windows-result` 的 `Error` Display 是本地化的 `消息 (0x887A0026)`，
// 不含常量名——旧实现用 `err.contains("ACCESS_LOST")` 判定重建，永不匹配，
// 锁屏/UAC/分辨率切换后捕获必死。这里按 **HRESULT 值**分类，纯函数放平台
// 无关模块，非 Windows 构建也能编译并单测。

/// `DXGI_ERROR_ACCESS_LOST`（0x887A0026）：duplication 失效（锁屏、UAC、
/// 会话切换、显示器拓扑变化）——重建即可恢复。
pub const DXGI_ERROR_ACCESS_LOST_CODE: i32 = 0x887A_0026u32 as i32;
/// `DXGI_ERROR_WAIT_TIMEOUT`（0x887A0027）：静止桌面在超时窗口内无新帧，
/// **不是错误**。
pub const DXGI_ERROR_WAIT_TIMEOUT_CODE: i32 = 0x887A_0027u32 as i32;
/// `DXGI_ERROR_DEVICE_REMOVED`（0x887A0005）：显卡驱动重置/设备拔除。
pub const DXGI_ERROR_DEVICE_REMOVED_CODE: i32 = 0x887A_0005u32 as i32;
/// `DXGI_ERROR_DEVICE_RESET`（0x887A0007）：设备被驱动重置。
pub const DXGI_ERROR_DEVICE_RESET_CODE: i32 = 0x887A_0007u32 as i32;

/// 一次捕获失败该怎么处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureRecovery {
    /// 静止桌面：没有新帧，复用上一帧（不是错误）。
    WaitTimeout,
    /// duplication 已失效：重建捕获对象后重试。
    Rebuild,
    /// 其它错误：不重建，直接上报给上层（计入连续失败）。
    Fatal,
}

/// 按 HRESULT 判定 DXGI 捕获失败是否可通过重建自愈。
pub fn classify_dxgi_error(hresult: i32) -> CaptureRecovery {
    match hresult {
        DXGI_ERROR_WAIT_TIMEOUT_CODE => CaptureRecovery::WaitTimeout,
        DXGI_ERROR_ACCESS_LOST_CODE
        | DXGI_ERROR_DEVICE_REMOVED_CODE
        | DXGI_ERROR_DEVICE_RESET_CODE => CaptureRecovery::Rebuild,
        _ => CaptureRecovery::Fatal,
    }
}

/// Frame source abstraction over a captured desktop.
pub trait FrameSource: Send {
    /// Capture the next frame (`bgra` length must be `width*height*4`).
    fn next_frame(&mut self) -> Result<Frame, String>;
    /// Fixed capture resolution in pixels.
    fn resolution(&self) -> (usize, usize);
    /// 所选显示器在虚拟桌面坐标系中的原点（MYS-2765 #10）：鼠标注入要加上
    /// 它才能落到正在采集的那块屏。默认 `(0,0)`（单屏/全屏/无坐标后端）。
    fn origin(&self) -> (i32, i32) {
        (0, 0)
    }

    /// 光标位置回调（R5#64 光标独立通道）：默认 no-op，X11 实现注入生效。
    /// 捕获画面通常不含光标层——独立查询位置经 `desktop:cursor` 轻量消息
    /// 发给浏览器 overlay（光标移动不触发整帧重编码）。
    fn set_cursor_cb(&mut self, _cb: Box<dyn FnMut(i32, i32, bool) + Send>) {}
    /// 远端显示器拓扑（R5#136-146 多显示器最小子集）：默认空（Wayland/GDI/
    /// 测试桩无 X11 RANDR），X11 实现枚举输出（名称/分辨率/偏移）。随
    /// `desktop:started` 上报，浏览器/运维可见远端多屏。
    fn list_monitors(&self) -> Vec<MonitorInfo> {
        Vec::new()
    }
    /// R5 #127 捕获侧内存池第三块：注入预分配 buffer 池（next_frame 从池
    /// 借出 bgra，主循环处理完归还——帧 buffer 环形复用）。默认 no-op。
    fn attach_pool(&mut self, _pool: std::sync::Arc<FramePool>) {}
}

/// Open a capture source. `kind` is one of `auto`, `x11`, `wayland`,
/// `dxgi`, `gdi`/`windows` or `none`; `display` optionally overrides the X11
/// display (falls back to `DISPLAY` then the X default).
///
/// `x_display`（MYS-2765 #9）是**启动配置里非数字**的 `--desktop-display`
/// （如 `:0`）。`display` 为序号（`"1"`）时 X 连接串取它——否则浏览器切屏
/// 会把 `--desktop-display :0` 一起丢掉，在没有 `$DISPLAY` 的 systemd 服务里
/// 直接连不上。为 `None` 时回落 `$DISPLAY`。
///
/// Returns the source plus the **实际生效**的 backend 名（`dxgi` / `gdi` /
/// `x11` / `wayland`）——`auto` 解析后回退到的后端与请求值可能不同
/// （例如虚拟显示器上 dxgi 验证失败回退 gdi），浏览器指标面板展示
/// 的是这个真值。
pub fn open_source(
    kind: &str,
    display: Option<&str>,
    x_display: Option<&str>,
) -> Result<(Box<dyn FrameSource>, String), String> {
    match kind {
        "none" => Err("desktop capture disabled".to_string()),
        #[cfg(not(windows))]
        "x11" => ok_backend(X11Source::open_with_x_display(display, x_display), "x11"),
        #[cfg(windows)]
        "x11" => Err("X11 capture is Linux-only".to_string()),
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        "wayland" => ok_backend(crate::agent::desktop::wayland::WaylandSource::open(), "wayland"),
        #[cfg(not(all(target_os = "linux", feature = "wayland")))]
        "wayland" => Err(
            "Wayland native capture requires a Linux build with the `wayland` feature \
             (xdg-desktop-portal + PipeWire). Use an X11 session or Xvfb otherwise"
                .to_string(),
        ),
        #[cfg(windows)]
        // 显式指定时尊重用户选择（不验证首帧）。display 支持数字选屏
        // （"0"=主屏、"1"=第 2 块，MYS-954 多屏）——非数字串在 Windows
        // 上无 X11 DISPLAY 语义，忽略并走默认屏。
        "dxgi" => {
            let idx = display.and_then(parse_monitor_index).unwrap_or(0);
            ok_backend(
                crate::agent::desktop::dxgi::DxgiSource::open_at(idx),
                "dxgi",
            )
        }
        #[cfg(not(windows))]
        "dxgi" => Err("DXGI capture is Windows-only".to_string()),
        "gdi" | "windows" => {
            #[cfg(windows)]
            {
                let idx = display.and_then(parse_monitor_index).unwrap_or(0);
                ok_backend(gdi::GdiSource::open_at(idx), "gdi")
            }
            #[cfg(not(windows))]
            open_gdi().map(|s| (s, "gdi".to_string()))
        }
        "auto" => open_auto(display, x_display),
        other => Err(format!("unknown capture kind: {}", other)),
    }
}

fn ok_backend(
    r: Result<impl FrameSource + 'static, String>,
    name: &str,
) -> Result<(Box<dyn FrameSource>, String), String> {
    r.map(|s| (Box::new(s) as Box<dyn FrameSource>, name.to_string()))
}

/// 解析 Windows 多屏选择值（MYS-954）："0"/"1"/… = 显示器枚举序号。
/// 其它值（X11 ":0" 之类）在 Windows 上无意义 → None（走默认主屏）。
#[cfg(windows)]
fn parse_monitor_index(display: &str) -> Option<usize> {
    display.trim().parse::<usize>().ok()
}

/// Platform auto-detect: Windows prefers dxgi (60fps capable) then GDI;
/// Linux prefers wayland-portal when running under a Wayland session, else X11.
///
/// `x_display` 见 [`open_source`]：序号选屏时 X 连接串取它（启动配置里非数字
/// 的 `--desktop-display`），不因浏览器切屏而丢失。
fn open_auto(
    display: Option<&str>,
    x_display: Option<&str>,
) -> Result<(Box<dyn FrameSource>, String), String> {
    #[cfg(windows)]
    {
        let _ = x_display; // Windows 无 X display 语义
        let idx = display.and_then(parse_monitor_index).unwrap_or(0);
        match crate::agent::desktop::dxgi::DxgiSource::open_verified_at(idx) {
            Ok(s) => Ok((Box::new(s) as Box<dyn FrameSource>, "dxgi".to_string())),
            Err(e) => {
                tracing::warn!("dxgi capture unavailable ({e}) — falling back to GDI");
                #[cfg(windows)]
                {
                    ok_backend(gdi::GdiSource::open_at(idx), "gdi")
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        let wayland_session = std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
            || std::env::var("WAYLAND_DISPLAY").is_ok();
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        if wayland_session {
            match crate::agent::desktop::wayland::WaylandSource::open() {
                Ok(s) => {
                    return Ok((Box::new(s) as Box<dyn FrameSource>, "wayland".to_string()))
                }
                Err(e) => {
                    tracing::warn!("wayland portal capture unavailable ({e}) — falling back to X11/XWayland")
                }
            }
        }
        // MYS-2765 #9：`display` 是纯数字时表示"选第 N 块 X11 输出"，
        // 此时 X 连接串取 `x_display`（启动配置里的 X display），没有再回落
        // `$DISPLAY`——把它当 display 字符串连必然失败。
        // 非数字（":1"）保持旧语义：显式 X display。
        if display.is_some() || x_display.is_some() || std::env::var("DISPLAY").is_ok() {
            ok_backend(X11Source::open_with_x_display(display, x_display), "x11")
        } else {
            Err(
                "no DISPLAY found; desktop capture requires an X11 session (Xvfb works \
                 too) or a Wayland session with xdg-desktop-portal (build with the \
                 `wayland` feature)"
                    .to_string(),
            )
        }
    }
}

// 非 Windows 上 `"gdi"` 走这条兜底（返回明确的"仅 Windows"错误）；Windows
// 的 `"gdi"` 分支直接调 `gdi::GdiSource::open_at`，不需要这里的 windows 版本
// ——那个版本从来没被调用过，会编译但在 Windows 构建里报 dead_code。
#[cfg(not(windows))]
fn open_gdi() -> Result<Box<dyn FrameSource>, String> {
    Err("GDI capture is Windows-only".to_string())
}

// ── 截图线程化（rustdesk capture 线程对齐） ────────────────

/// 独立抓帧线程包装：把 `FrameSource` 挪到专用线程持续抓帧，主循环用
/// [`ThreadedFrameSource::try_latest`] 非阻塞取最新帧——编码线程不再被慢
/// 抓帧（X11 GetImage / DXGI 等）阻塞，抓帧与编码并行。
///
/// - `latest` 只保留**最新**一帧（旧帧丢弃，等同"追最新帧"跳帧语义）；
/// - capture 线程持续出错时累计 `err_count` 并保存 `last_err`，供主循环
///   按 `MAX_CAPTURE_ERRORS` 终止决策；
/// - drop 时置 stop 并 join，保证线程干净退出（不再有后台孤儿线程）。
pub struct ThreadedFrameSource {
    latest: Arc<std::sync::Mutex<Option<Frame>>>,
    err_count: Arc<std::sync::atomic::AtomicU32>,
    last_err: Arc<std::sync::Mutex<Option<String>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    width: usize,
    height: usize,
}

impl ThreadedFrameSource {
    /// 启动抓帧线程，不限速（等价 `spawn_with_max_fps(inner, 0)`）。
    pub fn spawn(inner: Box<dyn FrameSource>) -> Result<Self, String> {
        Self::spawn_with_max_fps(inner, 0.0, None)
    }

    /// 启动抓帧线程。`inner` 被 move 进线程（不再可从外部访问）。
    /// `max_fps` > 0 时限制**动态**产帧率（静态仍走 would-block 退避；
    /// R5#135 抓帧独立上限，与编码 fps 解耦）。
    pub fn spawn_with_max_fps(
        inner: Box<dyn FrameSource>,
        max_fps: f64,
        pool: Option<std::sync::Arc<FramePool>>,
    ) -> Result<Self, String> {
        Self::spawn_full(inner, max_fps, pool, None)
    }

    /// 同 [`Self::spawn_with_max_fps`]，额外把捕获源的**原点**持续写进
    /// `origin_sink`（打包格式见 [`pack_origin`]）。
    ///
    /// 源被 move 进线程后外部就取不到 `origin()` 了，而原点会随显示器位置
    /// 变化（用户在系统设置里挪屏）——由抓帧线程每拍同步一次给鼠标注入用
    /// （MYS-2765 #10）。
    pub fn spawn_full(
        mut inner: Box<dyn FrameSource>,
        max_fps: f64,
        pool: Option<std::sync::Arc<FramePool>>,
        origin_sink: Option<Arc<std::sync::atomic::AtomicI64>>,
    ) -> Result<Self, String> {
        let (width, height) = inner.resolution();
        // 打开时就把原点写一次：首帧到达前浏览器就可能发点击。
        if let Some(sink) = &origin_sink {
            let (ox, oy) = inner.origin();
            sink.store(pack_origin(ox, oy), std::sync::atomic::Ordering::Relaxed);
        }
        // R5 #127 捕获侧池：注入线程内 capture（next_frame 从池借出）。
        if let Some(p) = &pool {
            inner.attach_pool(p.clone());
        }
        let latest = Arc::new(std::sync::Mutex::new(None));
        let err_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let last_err = Arc::new(std::sync::Mutex::new(None));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s1, l1, e1, le1) = (stop.clone(), latest.clone(), err_count.clone(), last_err.clone());
        let o1 = origin_sink.clone();
        let builder = std::thread::Builder::new().name("desktop-capture".to_string());
        let thread = builder
            .spawn(move || {
                use std::sync::atomic::Ordering as O;
                // unwind 捕获：capture 内部若有 Rust panic（如 unwrap/越界），
                // 转成错误上报而非让整个 agent 进程闪退（panic=unwind 构建）。
                // abort 构建下无法拦截，但有 crash 日志 hook 记录现场。
                let run = || {
                    // rustdesk would-block 语义：缓存上一帧原始像素，逐字节判重，
                    // 画面未变则不发布"新帧"。静止=零发布=编码循环真正闲置
                    // （不再复用 last_frame 喂时钟空转），动态变化首帧立即唤醒。
                    let mut last_raw: Option<(usize, usize, Vec<u8>)> = None;
                    loop {
                    if s1.load(O::Relaxed) {
                        break;
                    }
                    // 原点同步（MYS-2765 #10）：显示器被挪动 / 换分辨率 /
                    // DXGI 掉线重建都可能改变它，每拍读一次（后端内部只是
                    // 缓存字段，真机零成本）写进共享原子供鼠标注入使用。
                    if let Some(sink) = &o1 {
                        let (ox, oy) = inner.origin();
                        sink.store(pack_origin(ox, oy), O::Relaxed);
                    }
                    let mut same = false; // 本拍画面未变（静止退化）
                    match inner.next_frame() {
                        Ok(f) => {
                            e1.store(0, O::Relaxed);
                            same = match &last_raw {
                                Some((lw, lh, buf)) => {
                                    *lw == f.width && *lh == f.height && buf.as_slice() == f.bgra.as_slice()
                                }
                                None => false,
                            };
                            if same {
                                // 画面未变：不发布（等变化帧），静止零产。
                            } else {
                                last_raw = Some((f.width, f.height, f.bgra.clone()));
                                *l1.lock().unwrap() = Some(f);
                            }
                        }
                        Err(e) => {
                            e1.fetch_add(1, O::Relaxed);
                            *le1.lock().unwrap() = Some(e);
                        }
                    }
                    // 防空转（对齐 rustdesk 静止停抓）：
                    // - 动态（本拍画面变化）：让出 CPU 即可，快源（测试 mock）全速产帧；
                    // - 静止（本拍画面未变）：退避 sleep 缩短抓帧频率——X 往返/DXGI
                    //   acquire 在此刻只是白耗 CPU，静置桌面 agent 应空载（R3 乙85）。
                    //   上限 ~100ms ≈ 10fps 轮询：内容一旦变化下一拍立即恢复全速
                    //   （waiting-on-change 抓帧语义，R3 乙57/82）。
                    if same {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    } else if max_fps > 0.0 {
                        // 抓帧独立上限（R5#135）：动态也按 max_fps 节流，
                        // 省 X/DXGI 往返（低配 CPU 友好）。1/0 防除零。
                        let interval = (1000.0 / max_fps).max(1.0) as u64;
                        std::thread::sleep(std::time::Duration::from_millis(interval));
                    } else {
                        std::thread::yield_now();
                    }
                    }
                };
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
                    // 置 u32::MAX 触发 pipeline 的 MAX_CAPTURE_ERRORS 终止并回传。
                    e1.store(u32::MAX, O::Relaxed);
                    let msg = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "capture thread panicked (unknown payload)".to_string());
                    *le1.lock().unwrap() = Some(format!("capture thread panic: {msg}"));
                }
            })
            .map_err(|e| format!("spawn capture thread: {e}"))?;
        Ok(Self {
            latest,
            err_count,
            last_err,
            stop,
            thread: Some(thread),
            width,
            height,
        })
    }

    /// 取走当前最新帧；尚无新帧时返回 `None`（主循环跳帧，不阻塞）。
    pub fn try_latest(&self) -> Option<Frame> {
        self.latest.lock().unwrap().take()
    }

    /// capture 线程累计的连续失败次数（成功后清零）。
    pub fn err_count(&self) -> u32 {
        use std::sync::atomic::Ordering as O;
        self.err_count.load(O::Relaxed)
    }

    /// 最近一次抓帧错误（用于报错回传浏览器）。
    pub fn last_err(&self) -> Option<String> {
        self.last_err.lock().unwrap().clone()
    }

    /// 捕获源初始分辨率（抓帧线程内 self-inner 自身会随 display 变更更新
    /// 帧尺寸，主循环用帧尺寸检测变更）。
    pub fn resolution(&self) -> (usize, usize) {
        (self.width, self.height)
    }
}

impl Drop for ThreadedFrameSource {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering as O;
        self.stop.store(true, O::Relaxed);
        if let Some(t) = self.thread.take() {
            // join 等线程退出；真实后端最多阻塞一个抓帧周期（DXGI 静止
            // 200ms timeout / X11 一次往返），毫秒级可接受。
            let _ = t.join();
        }
    }
}

// ── X11 ────────────────────────────────────────────────────────

/// X11 frame source (uses `GetImage`; the server returns the root window's
/// pixel data, already composed including overlapping windows).
///
/// Some composited servers — including the XWayland root under a Wayland
/// compositor — refuse `GetImage` on the root window with a `BadMatch` error.
/// When that happens we fall back to the same technique as `ffmpeg x11grab`:
/// `XCompositeRedirectWindow(root, Automatic)` + `XCompositeNameWindowPixmap`,
/// then `GetImage` on the named pixmap (which reads the window backing store
/// and works on composited servers).
///
/// Fast path: when the X server exposes the MIT-SHM extension we attach a
/// System V shared-memory segment and use `ShmGetImage` (rustdesk
/// `x11/capturer.rs`): the server writes pixels directly into our memory,
/// avoiding a full-frame round-trip through the X protocol on every capture.
/// Falls back to plain `GetImage` if SHM init fails (no extension / server
/// refuses the attach).
#[cfg(not(windows))]
pub struct X11Source {
    conn: x11rb::rust_connection::RustConnection,
    root: x11rb::protocol::xproto::Drawable,
    width: u16,
    height: u16,
    depth: u8,
    /// Lazily-initialised XComposite backend used when root `GetImage` fails.
    composite: Option<x11rb::protocol::xproto::Pixmap>,
    /// MIT-SHM fast-path state (segment + x11rb shm seg id). `None` = SHM
    /// unavailable (not tried yet, or init failed) — capture falls back.
    shm: Option<ShmState>,
    /// SHM init already failed on this geometry — don't retry every frame.
    /// Reset when the display geometry changes (segment would be stale).
    shm_failed: bool,
    /// 每 N 帧重查一次 root geometry（display 分辨率运行时变更检测，
    /// rustdesk `Rect`/HotPlug 对齐）。X setup 缓存不反映 xrandr 变更。
    recheck: u32,
    /// R5 #127 捕获侧内存池：bgra 借出池（next_frame 复用归还 buffer）。
    pool: Option<std::sync::Arc<FramePool>>,
    /// 是否已注册 XRANDR ScreenChangeNotify（R5#131 分辨率事件驱动）。
    /// 注册后 `next_frame` 用 `poll_for_event` 读事件——只有真变更才触发
    /// 重建，替代固定 30 帧一次的 `get_geometry` 轮询往返。
    randr_registered: bool,
    /// 光标位置回调（R5#64 光标独立通道）：X11 `GetImage`/`ShmGetImage`
    /// **不含光标层**——远程用户看不到鼠标指针。`next_frame` 每 100ms 查询
    /// 一次 root 指针位置，经此回调以 `desktop:cursor {x,y,shown}` 轻量消息
    /// 发给浏览器 overlay（光标移动不再触发整帧重编码）。None = 非 X11
    /// 后端或未注入。
    cursor_cb: Option<Box<dyn FnMut(i32, i32, bool) + Send>>,
    /// 上次光标查询时刻（100ms 节流）。
    last_cursor_at: std::time::Instant,
    /// 选中的 RANDR 输出序号（MYS-2765 #9）：`Some(i)` = 只采集第 i 个输出
    /// 的 CRTC 矩形（浏览器显示器下拉发来的枚举序号）；`None` = 采集整个
    /// root（`--desktop-display` 传的是 X display 字符串，旧语义）。
    output_index: Option<usize>,
    /// 采集区域在 root 坐标系中的原点：序号选屏时 = 该输出 CRTC 的 x/y，
    /// 整屏采集时为 (0,0)。`GetImage`/`ShmGetImage` 用它做裁剪偏移，
    /// `FrameSource::origin()` 把它交给鼠标注入做坐标补偿。
    crop_x: i16,
    crop_y: i16,
}

/// MIT-SHM segment: SysV shared memory mapped into our address space and
/// attached to the X server under an x11rb `shm::Seg` id.
///
/// `ptr` aliases a SysV mapping that outlives this struct only while the
/// struct lives (dropped via `shmdt`); X writes into it on `ShmGetImage`.
/// Access is guarded: capture reads it only after the `ShmGetImage` reply
/// has been waited on, so no explicit sync fence is needed between X and us.
#[cfg(not(windows))]
struct ShmState {
    shmid: i32,
    ptr: *mut u8,
    size: usize,
    seg: x11rb::protocol::shm::Seg,
    width: u16,
    height: u16,
}

// SAFETY: the raw pointer is only ever dereferenced from the owning thread
// (capture loop) after a `ShmGetImage` reply, so it is safe to consider the
// struct Send within this single-threaded `ThreadedFrameSource` usage.
#[cfg(not(windows))]
unsafe impl Send for ShmState {}

#[cfg(not(windows))]
impl X11Source {
    /// 打开 X11 捕获源（不带显式 X display 串；序号选屏时回落 `$DISPLAY`）。
    ///
    /// `display` 有两种语义（MYS-2765 #9）：
    /// - **可解析为 `usize`** → RANDR 输出枚举序号（与 `list_monitors` /
    ///   `desktop:started.displays` 的数组下标一致）。此时 X 连接走 `$DISPLAY`
    ///   （需要显式 X display 时用 [`Self::open_with_x_display`]，由
    ///   `open_source` 传入启动配置的 `--desktop-display`），采集区域裁剪到
    ///   该输出的 CRTC 矩形。序号越界、或该输出没有活动 CRTC（0x0，未连接的
    ///   输出）→ 明确报错。
    /// - 其它字符串 → 旧语义：直接当 X display 连（`":1"`、`"host:0"`）。
    pub fn open(display: Option<&str>) -> Result<Self, String> {
        Self::open_with_x_display(display, None)
    }

    /// 同上，但把"捕获源选择值"与"X display 连接串"分开：`selector` 是
    /// 浏览器下拉发来的枚举序号（可解析为 `usize` 时按序号裁剪），
    /// `x_display` 是 `--desktop-display` 里真正的 X display（非数字时）。
    fn open_with_x_display(selector: Option<&str>, x_display: Option<&str>) -> Result<Self, String> {
        use x11rb::connection::Connection;
        use x11rb::rust_connection::RustConnection;
        // 序号 vs X display 字符串：只有纯数字才是"选第 N 块屏"。
        let output_index = selector.and_then(|s| s.trim().parse::<usize>().ok());
        let display = match (output_index, x_display) {
            // 序号选屏：X 连接用显式的 x_display，否则 $DISPLAY（再否则 X 默认）。
            (Some(_), xd) => xd.map(|d| d.to_string()).or_else(|| std::env::var("DISPLAY").ok()),
            // X display 字符串语义：优先显式传入，其次 $DISPLAY。
            (None, xd) => xd
                .map(|d| d.to_string())
                .or_else(|| selector.map(|d| d.to_string()))
                .or_else(|| std::env::var("DISPLAY").ok()),
        };
        let (conn, screen_num) =
            RustConnection::connect(display.as_deref()).map_err(|e| format!("x11rb connect: {e}"))?;
        let (full_w, full_h, depth, root) = {
            let screen = &conn.setup().roots[screen_num];
            (
                screen.width_in_pixels,
                screen.height_in_pixels,
                screen.root_depth,
                screen.root,
            )
        };
        let mut src = Self {
            conn,
            root,
            width: full_w,
            height: full_h,
            depth,
            composite: None,
            shm: None,
            shm_failed: false,
            pool: None,
            recheck: 0,
            randr_registered: false,
            cursor_cb: None,
            last_cursor_at: std::time::Instant::now(),
            output_index,
            crop_x: 0,
            crop_y: 0,
        };
        // 序号选屏：把采集区域与尺寸裁到该输出的 CRTC 矩形。序号越界 /
        // 未连接输出在这里就报错，而不是悄悄退化成整屏（旧实现把输出名
        // 当 DISPLAY 连接，必然失败）。
        if let Some(idx) = output_index {
            let rect = src
                .output_rect(idx)
                .map_err(|e| format!("x11 monitor #{idx}: {e}"))?;
            src.apply_crop(rect);
            tracing::info!(
                monitor = idx,
                geom = %format!("{}x{}+{},{}", rect.2, rect.3, rect.0, rect.1),
                "x11 capture output selected"
            );
        }
        // 注册 XRANDR ScreenChangeNotify（R5#131）：分辨率事件驱动替代轮询。
        // 失败静默——事件驱动是优化，无扩展时回落现有 30 帧轮询。
        use x11rb::protocol::randr::{ConnectionExt as _, NotifyMask};
        match src
            .conn
            .randr_select_input(src.root, NotifyMask::SCREEN_CHANGE)
        {
            Ok(cookie) => match cookie.check() {
                Ok(()) => {
                    src.randr_registered = true;
                }
                Err(e) => {
                    tracing::debug!(%e, "randr select_input failed — falling back to geometry polling");
                }
            },
            Err(e) => {
                tracing::debug!(%e, "randr select_input cookie failed — falling back to geometry polling");
            }
        }
        Ok(src)
    }

    /// R5#136-146 多显示器最小子集：枚举 X11 RANDR 输出（名称/分辨率/偏移）。
    /// 失败返回空（观测缺失不致命，desktop:started 的 displays 为空数组）。
    ///
    /// **数组下标即选屏序号**（MYS-2765 #9）：浏览器显示器下拉的 value 是这个
    /// 下标，`open(Some("1"))` 采的就是 `list_monitors()[1]`。未连接/无 CRTC
    /// 的输出仍留在数组里（尺寸 0，浏览器据此过滤掉不下拉），但选中它会报错
    /// （见 [`Self::output_rect`]）——序号不会因过滤而错位。
    pub fn list_monitors(&self) -> Vec<MonitorInfo> {
        use x11rb::protocol::randr::ConnectionExt as _;
        let mut out = Vec::new();
        let Some(res) = self
            .conn
            .randr_get_screen_resources_current(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return out;
        };
        for oid in res.outputs {
            let Some(info) = self
                .conn
                .randr_get_output_info(oid, 0)
                .ok()
                .and_then(|c| c.reply().ok())
            else {
                continue;
            };
            let name = String::from_utf8_lossy(&info.name).to_string();
            let (w, h, x, y) = Self::crtc_rect_of(&self.conn, info.crtc);
            out.push(MonitorInfo {
                name,
                width: w,
                height: h,
                x,
                y,
            });
        }
        out
    }

    /// 某个 RANDR 输出的 CRTC 矩形 `(x, y, w, h)`；`crtc == 0` 或查询失败时
    /// 返回全 0（未连接输出）。
    fn crtc_rect_of(
        conn: &x11rb::rust_connection::RustConnection,
        crtc: x11rb::protocol::randr::Crtc,
    ) -> (u32, u32, i32, i32) {
        use x11rb::protocol::randr::ConnectionExt as _;
        if crtc == 0 {
            return (0, 0, 0, 0);
        }
        match conn.randr_get_crtc_info(crtc, 0) {
            Ok(cookie) => match cookie.reply() {
                Ok(c) => (c.width as u32, c.height as u32, c.x as i32, c.y as i32),
                Err(_) => (0, 0, 0, 0),
            },
            Err(_) => (0, 0, 0, 0),
        }
    }

    /// 第 `index` 个 RANDR 输出（`list_monitors` 下标顺序）的 CRTC 矩形，
    /// 作为可采集区域 `(x, y, w, h)`。
    ///
    /// 序号越界 / 该输出没有活动 CRTC / 尺寸为 0（未连接）→ 明确报错：选中的
    /// 屏不可采集时必须让浏览器看到失败，而不是悄悄回落到整屏把用户点的位置
    /// 映射到另一个屏上。
    fn output_rect(&self, index: usize) -> Result<(i32, i32, u16, u16), String> {
        use x11rb::protocol::randr::ConnectionExt as _;
        let res = self
            .conn
            .randr_get_screen_resources_current(self.root)
            .map_err(|e| format!("randr get_screen_resources: {e}"))?
            .reply()
            .map_err(|e| format!("randr get_screen_resources reply: {e}"))?;
        let oid = *res.outputs.get(index).ok_or_else(|| {
            format!(
                "output index {index} out of range (only {} outputs)",
                res.outputs.len()
            )
        })?;
        let info = self
            .conn
            .randr_get_output_info(oid, 0)
            .map_err(|e| format!("randr get_output_info: {e}"))?
            .reply()
            .map_err(|e| format!("randr get_output_info reply: {e}"))?;
        let name = String::from_utf8_lossy(&info.name).to_string();
        if info.crtc == 0 {
            return Err(format!(
                "output #{index} ({name}) is not connected (no CRTC) — pick a connected display"
            ));
        }
        let (w, h, x, y) = Self::crtc_rect_of(&self.conn, info.crtc);
        if w < 2 || h < 2 {
            return Err(format!(
                "output #{index} ({name}) has no active mode ({w}x{h}) — pick a connected display"
            ));
        }
        // GetImage 的宽高是 u16：超出协议域的屏（>65535px）无法按矩形裁剪，
        // 直接报错而不是截断成错误的区域。
        let (w16, h16) = (
            u16::try_from(w).map_err(|_| format!("output #{index} ({name}) width {w} exceeds X11 limits"))?,
            u16::try_from(h).map_err(|_| format!("output #{index} ({name}) height {h} exceeds X11 limits"))?,
        );
        let (x16, y16) = (
            i16::try_from(x).map_err(|_| format!("output #{index} ({name}) x={x} exceeds X11 limits"))?,
            i16::try_from(y).map_err(|_| format!("output #{index} ({name}) y={y} exceeds X11 limits"))?,
        );
        Ok((x16 as i32, y16 as i32, w16, h16))
    }

    /// 把采集区域设为 `rect`（root 坐标系）并清掉依赖旧几何的缓存。
    fn apply_crop(&mut self, rect: (i32, i32, u16, u16)) {
        let (x, y, w, h) = rect;
        self.crop_x = x as i16;
        self.crop_y = y as i16;
        self.width = w;
        self.height = h;
        // SHM 段尺寸绑定旧几何，换区域必须重建。
        self.teardown_shm();
        self.shm_failed = false;
    }

    /// Try to initialise the MIT-SHM fast path. Quietly falls back to
    /// `GetImage` on any failure (no extension, or the server refuses to
    /// attach our SysV segment) — SHM is an optimisation, not a requirement.
    fn try_init_shm(&mut self) -> Result<(), String> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::shm::ConnectionExt as _;

        if self.shm.is_some() {
            return Ok(());
        }
        // Query the extension version — if the server has no MIT-SHM, shmget
        // attach would just hang the capture; fail fast instead.
        self.conn
            .shm_query_version()
            .map_err(|e| format!("shm_query_version cookie: {e}"))?
            .reply()
            .map_err(|e| format!("shm_query_version reply: {e}"))?;
        let w = self.width as usize;
        let h = self.height as usize;
        let size = w * h * 4; // BGRA 32bpp
        // shmget with IPC_PRIVATE: server writes into this segment.
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            return Err(format!("shmget failed: {}", std::io::Error::last_os_error()));
        }
        // Attach in our address space.
        let ptr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
        if ptr as isize == -1 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            return Err(format!("shmat failed: {err}"));
        }
        // Register the segment with the X server.
        let seg = self.conn.generate_id().map_err(|e| format!("generate shm id: {e}"))?;
        self.conn
            .shm_attach(seg, shmid as u32, false)
            .map_err(|e| {
                unsafe { libc::shmdt(ptr) };
                unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
                format!("shm_attach cookie: {e}")
            })?
            .check()
            .map_err(|e| {
                unsafe { libc::shmdt(ptr) };
                unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
                format!("shm_attach reply: {e}")
            })?;
        self.shm = Some(ShmState {
            shmid,
            ptr: ptr as *mut u8,
            size,
            seg,
            width: self.width,
            height: self.height,
        });
        tracing::debug!("X11 MIT-SHM fast path enabled ({w}x{h} @ {size} bytes)");
        Ok(())
    }

    /// Drop the SHM segment (server detach + sysv detach/remove). Called on
    /// Drop and before re-init after a display geometry change.
    fn teardown_shm(&mut self) {
        use x11rb::protocol::shm::ConnectionExt as _;
        if let Some(s) = self.shm.take() {
            let _ = self.conn.shm_detach(s.seg);
            unsafe { libc::shmdt(s.ptr as *const _ as *mut _) };
            unsafe { libc::shmctl(s.shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        }
    }

    fn bpp(depth: u8) -> x11rb::image::BitsPerPixel {
        use x11rb::image::BitsPerPixel;
        match depth {
            0..=8 => BitsPerPixel::B8,
            9..=16 => BitsPerPixel::B16,
            _ => BitsPerPixel::B32, // depth 24/30/32 all come back as 32-bit pixels
        }
    }

    /// Set up the XComposite fallback: redirect the root's children into
    /// off-screen storage and name the root's backing pixmap (the standard
    /// screen-capture sequence used by `maim`/`scrot` and `ffmpeg x11grab`).
    /// The pixmap is reused across frames.
    fn ensure_composite(&mut self) -> Result<(), String> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::composite::{self, ConnectionExt as _};
        if self.composite.is_some() {
            return Ok(());
        }
        self.conn
            .composite_query_version(0, 7)
            .map_err(|e| format!("composite query: {e}"))?
            .reply()
            .map_err(|e| format!("composite query reply: {e}"))?;
        // 不能直接 RedirectWindow(root)（规范禁止, 返回 BadMatch）——
        // 重定向所有子窗口即可让 root 拥有可命名的后备 pixmap。
        self.conn
            .composite_redirect_subwindows(self.root, composite::Redirect::AUTOMATIC)
            .map_err(|e| format!("composite redirect: {e}"))?
            .check()
            .map_err(|e| format!("composite redirect reply: {e}"))?;
        let pixmap = self
            .conn
            .generate_id()
            .map_err(|e| format!("generate pixmap id: {e}"))?;
        self.conn
            .composite_name_window_pixmap(self.root, pixmap)
            .map_err(|e| format!("composite name pixmap: {e}"))?
            .check()
            .map_err(|e| format!("composite name pixmap reply: {e}"))?;
        self.composite = Some(pixmap);
        Ok(())
    }

    /// `GetImage` on a drawable and return packed BGRA rows for the capture
    /// region (see `next_frame` for the depth/stride contract).
    ///
    /// MYS-2765 #9：采集区域是 `(crop_x, crop_y, width, height)`——整屏采集
    /// 时原点是 (0,0)，序号选屏时是该输出 CRTC 的左上角。`GetImage` 直接按
    /// 矩形读，不需要额外裁切。
    fn capture_impl(&self, drawable: impl Into<x11rb::protocol::xproto::Drawable>) -> Result<Vec<u8>, String> {
        use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};

        let drawable: x11rb::protocol::xproto::Drawable = drawable.into();
        let w = self.width as u16;
        let h = self.height as u16;
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                drawable,
                self.crop_x,
                self.crop_y,
                w,
                h,
                !0,
            )
            .map_err(|e| format!("get_image cookie: {e}"))?
            .reply()
            .map_err(|e| format!("get_image reply: {e}"))?;
        let depth = if reply.depth > 0 { reply.depth } else { self.depth };
        Self::bgra_from_pixels(&reply.data, depth, self.width as usize, self.height as usize, &self.conn, self.pool.as_ref())
    }

    /// `ShmGetImage` on the *current root* — the server writes the pixels
    /// straight into our attached SysV segment (no X protocol frame payload),
    /// then we copy the segment into a fresh BGRA buffer for the encoder.
    /// This is the rustdesk `x11/capturer.rs` fast path.
    ///
    /// 序号选屏时同样按裁剪矩形 `(crop_x, crop_y)` 取图（MYS-2765 #9）。
    fn capture_shm(&self) -> Result<Vec<u8>, String> {
        use x11rb::protocol::shm::ConnectionExt as _;
        use x11rb::protocol::xproto::ImageFormat;

        let s = self
            .shm
            .as_ref()
            .ok_or_else(|| "capture_shm called without SHM state".to_string())?;
        let w = self.width as u16;
        let h = self.height as u16;
        // Wait for the reply: after it lands the segment holds the frame.
        self.conn
            .shm_get_image(
                self.root,
                self.crop_x,
                self.crop_y,
                w,
                h,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                s.seg,
                0,
            )
            .map_err(|e| format!("shm_get_image cookie: {e}"))?
            .reply()
            .map_err(|e| format!("shm_get_image reply: {e}"))?;
        // SAFETY: `s.ptr` points at our own SysV segment (`shmat`), which stays
        // mapped until `teardown_shm`/`Drop`; the `ShmGetImage` reply above
        // guarantees the server has finished writing `size` bytes.
        let data = unsafe { std::slice::from_raw_parts(s.ptr, s.size) };
        Self::bgra_from_pixels(data, self.depth, self.width as usize, self.height as usize, &self.conn, self.pool.as_ref())
    }

    /// Normalize a raw X11 pixel buffer (depth 24/32, server byte order) into
    /// packed BGRA rows (B,G,R,A bytes), shared by the `GetImage` and
    /// MIT-SHM paths so both produce byte-identical output.
    ///
    /// `data` is the pixel payload in the server's native byte order
    /// (x11rb `Image::new` handles byte-order/scanline details), read from a
    /// `GetImage` reply or a MIT-SHM segment.
    fn bgra_from_pixels(
        data: &[u8],
        depth: u8,
        w: usize,
        h: usize,
        conn: &x11rb::rust_connection::RustConnection,
        pool: Option<&Arc<FramePool>>,
    ) -> Result<Vec<u8>, String> {
        use x11rb::connection::Connection as _;
        use x11rb::image::{BitsPerPixel, Image, ImageOrder, ScanlinePad};
        use std::borrow::Cow;

        // depth 30 (10-bit per channel) is NOT byte-addressable BGRA: treating
        // it as 32bpp would silently produce wrong colors. Fail loudly instead.
        // 8/16-bit depths are likewise unsupported for the packed-BGRA contract.
        if depth != 24 && depth != 32 {
            return Err(format!(
                "unsupported X11 root depth {depth}: only 24/32-bit (packed BGRA) captures are supported"
            ));
        }
        let bpp = Self::bpp(depth);
        let byte_order = ImageOrder::try_from(conn.setup().image_byte_order)
            .unwrap_or(ImageOrder::LsbFirst);
        let image = Image::new(
            w as u16,
            h as u16,
            ScanlinePad::Pad32,
            depth,
            bpp,
            byte_order,
            Cow::Borrowed(data),
        )
        .map_err(|e| format!("Image::new: {e}"))?;

        // Normalize to the server's native layout so bytes are BGRA (X pixmap
        // byte order is preserved; on little-endian hosts B is the lowest byte).
        let native = image
            .native(&conn.setup())
            .map_err(|e| format!("Image::native: {e}"))?;

        let stride = match bpp {
            BitsPerPixel::B32 => ((w as usize) * 4).max(1),
            BitsPerPixel::B16 => ((w as usize) * 2).max(1),
            BitsPerPixel::B8 => (w as usize).max(1),
            _ => (w as usize) * 4,
        };
        let data = native.data();
        // R5 #127 捕获侧池：有池则借出复用 buffer（主循环归还），否则自分配。
        let mut bgra = match pool {
            Some(p) => p.acquire((w as usize) * (h as usize) * 4),
            None => Vec::with_capacity((w as usize) * (h as usize) * 4),
        };
        for row in 0..h as usize {
            let start = row * stride;
            let end = (start + w as usize * 4).min(data.len());
            if end > start {
                bgra.extend_from_slice(&data[start..end]);
            }
        }
        // B16/B8 sources are rare; pad to the uniform BGRA contract by copying
        // the raw run (packing channels would be project-specific).
        if bgra.len() != w as usize * h as usize * 4 {
            return Err(format!(
                "unexpected X11 pixel layout (depth={}, bgra={} expected {})",
                depth,
                bgra.len(),
                w as usize * h as usize * 4
            ));
        }
        Ok(bgra)
    }
}

#[cfg(not(windows))]
impl FrameSource for X11Source {
    fn attach_pool(&mut self, pool: std::sync::Arc<FramePool>) {
        self.pool = Some(pool);
    }
    fn resolution(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    /// 所选输出的桌面原点（MYS-2765 #10）：鼠标注入加它才能落在采集的那块
    /// 屏上。整屏采集（未选序号）时为 (0,0)。
    fn origin(&self) -> (i32, i32) {
        (self.crop_x as i32, self.crop_y as i32)
    }

    /// 远端显示器拓扑（R5#136-146）。**必须在这里转发**：`X11Source` 另有一个
    /// 同名的固有方法，如果不覆盖 trait 方法，`Box<dyn FrameSource>`（pipeline
    /// 实际持有的类型）会落到 trait 的默认实现——返回空数组，X11 永远上报不了
    /// 显示器，浏览器多屏下拉永远是空的（MYS-2765 #9）。
    fn list_monitors(&self) -> Vec<MonitorInfo> {
        X11Source::list_monitors(self)
    }

    fn next_frame(&mut self) -> Result<Frame, String> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as _;
        // 分辨率变更检测（R5#131 事件驱动）：已注册 XRANDR ScreenChangeNotify
        // 时用 poll_for_event 读事件——只有真变更才重建，替代固定 30 帧一次的
        // get_geometry 轮询往返。无事件/未注册时回落轮询（保持旧行为）。
        //
        // MYS-2765 #9：序号选屏时关心的不是 root 几何，而是**所选输出的
        // CRTC 矩形**——用户在系统设置里换分辨率/挪动屏幕位置都会改它，
        // 采集区域与鼠标原点必须同步重算。事件里描述的 root 尺寸此时无关。
        let mut resized = false;
        if self.randr_registered {
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(ev)) => {
                        if let Ok(x11rb::protocol::Event::RandrScreenChangeNotify(randr_ev)) =
                            x11rb::protocol::Event::try_from(ev)
                        {
                            let (width, height) = (randr_ev.width, randr_ev.height);
                            if let Some(idx) = self.output_index {
                                // 该输出可能被拔掉/关掉：解析失败保持旧区域
                                // （画面冻结不如报错？——保持采集比中断更友好，
                                // 下一帧会再试；真断线由 capture 线程错误计数兜底）。
                                if let Ok(rect) = self.output_rect(idx) {
                                    if (rect.2, rect.3) != (self.width, self.height)
                                        || (rect.0 as i16, rect.1 as i16)
                                            != (self.crop_x, self.crop_y)
                                    {
                                        resized = true;
                                        self.apply_crop(rect);
                                    }
                                }
                            } else if width > 0 && height > 0 {
                                if self.width != width || self.height != height {
                                    resized = true;
                                }
                                self.width = width;
                                self.height = height;
                            }
                        }
                    }
                    Ok(None) => break, // 无更多事件
                    Err(e) => {
                        tracing::debug!(%e, "poll_for_event failed — falling back to geometry polling");
                        self.randr_registered = false;
                        break;
                    }
                }
            }
        } else {
            self.recheck += 1;
            if self.recheck >= 30 {
                self.recheck = 0;
                if let Some(idx) = self.output_index {
                    if let Ok(rect) = self.output_rect(idx) {
                        if (rect.2, rect.3) != (self.width, self.height)
                            || (rect.0 as i16, rect.1 as i16) != (self.crop_x, self.crop_y)
                        {
                            resized = true;
                            self.apply_crop(rect);
                        }
                    }
                } else if let Ok(cookie) = self.conn.get_geometry(self.root) {
                    if let Ok(geo) = cookie.reply() {
                        if geo.width > 0 && geo.height > 0 {
                            if self.width != geo.width || self.height != geo.height {
                                resized = true;
                            }
                            self.width = geo.width;
                            self.height = geo.height;
                        }
                    }
                }
            }
        }
        if resized {
            self.teardown_shm();
            self.shm_failed = false;
        }
        // 首次（或分辨率变更后）尝试开启 MIT-SHM 快路径；失败则标记失败
        // 不再每帧重试（回落 GetImage）。
        if self.shm.is_none() && !self.shm_failed {
            if let Err(e) = self.try_init_shm() {
                tracing::debug!(%e, "X11 MIT-SHM unavailable, using GetImage");
                self.shm_failed = true;
            }
        }
        let has_shm = self.shm.as_ref().map(|s| {
            s.width == self.width && s.height == self.height && s.size >= (self.width as usize * self.height as usize * 4)
        });
        let bgra = match has_shm {
            Some(true) => match self.capture_shm() {
                Ok(b) => b,
                // SHM 读取失败（极端竞态/服务器限制）：回退 GetImage 保活
                Err(shm_err) => {
                    tracing::debug!(%shm_err, "X11 SHM capture failed, falling back to GetImage");
                    self.teardown_shm();
                    self.shm = None;
                    self.shm_failed = true;
                    match self.capture_impl(self.root) {
                        Ok(b) => b,
                        Err(root_err) => {
                            return self.err_composite(shm_err, root_err);
                        }
                    }
                }
            },
            _ => match self.capture_impl(self.root) {
                Ok(b) => b,
                Err(root_err) => match self.composite_capture() {
                    Ok(b) => b,
                    Err(comp_err) => {
                        return Err(format!(
                            "X11 capture failed: GetImage on root — {root_err}; \
                             XComposite fallback — {comp_err}. If this is a Wayland session (XWayland), \
                             the X11 root has no image data — the native Wayland desktop cannot be \
                             captured over X11; run the agent in a real X11 session (Xorg/Xvfb) instead"
                        ));
                    }
                },
            },
        };
        // 光标位置通道（R5#64）：X11 GetImage 不含光标层——独立查询位置，
        // 经 cursor_cb 发 desktop:cursor 轻量消息（100ms 节流，不触发整帧
        // 重编码）。static 帧退避期也会被周期性调用，光标仍实时更新。
        self.poll_cursor();
        Ok(Frame {
            bgra,
            width: self.width as usize,
            height: self.height as usize,
        })
    }

    /// 注入光标位置回调（R5#64）：捕获线程 `next_frame` 每 100ms 节流查询
    /// root 指针位置后调用。须在 source move 进抓帧线程前设置。
    fn set_cursor_cb(&mut self, cb: Box<dyn FnMut(i32, i32, bool) + Send>) {
        self.cursor_cb = Some(cb);
    }
}

#[cfg(not(windows))]
impl X11Source {
    /// 光标位置查询（R5#64 光标独立通道）：X11 捕获画面不含光标层，远程
    /// 用户看不到鼠标指针。每 100ms 节流查询 root 指针位置并经 `cursor_cb`
    /// 回调上报（agent 侧转成 `desktop:cursor` 轻量消息，浏览器 overlay 渲染）。
    /// 失败静默（光标只是叠加层，不阻塞画面流）。
    fn poll_cursor(&mut self) {
        if self.cursor_cb.is_none() {
            return;
        }
        if self.last_cursor_at.elapsed() < std::time::Duration::from_millis(100) {
            return;
        }
        self.last_cursor_at = std::time::Instant::now();
        use x11rb::protocol::xproto::ConnectionExt as _;
        match self.conn.query_pointer(self.root) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => {
                    // 坐标减去采集区域原点（MYS-2765 #9/#10）：浏览器 overlay 是
                    // 画在**采集画面**上的，序号选屏时 root 坐标必须换成画面内
                    // 坐标，否则光标渲染在另一块屏的位置上。光标不在采集区域内
                    // 时上报 `shown=false`（隐藏 overlay，而不是画到画面外）。
                    let x = reply.root_x as i32 - self.crop_x as i32;
                    let y = reply.root_y as i32 - self.crop_y as i32;
                    let inside =
                        x >= 0 && y >= 0 && x < self.width as i32 && y < self.height as i32;
                    if let Some(cb) = self.cursor_cb.as_mut() {
                        cb(x, y, inside);
                    }
                }
                Err(e) => tracing::debug!(%e, "query_pointer reply failed"),
            },
            Err(e) => tracing::debug!(%e, "query_pointer failed"),
        }
    }
}

#[cfg(not(windows))]
impl Drop for X11Source {
    fn drop(&mut self) {
        self.teardown_shm();
    }
}

#[cfg(not(windows))]
impl X11Source {
    /// Compose the composite-fallback error (shared by the shm-fallback path).
    fn err_composite(&mut self, shm_err: String, root_err: String) -> Result<Frame, String> {
        match self.composite_capture() {
            Ok(b) => Ok(Frame {
                bgra: b,
                width: self.width as usize,
                height: self.height as usize,
            }),
            Err(comp_err) => Err(format!(
                "X11 capture failed: ShmGetImage — {shm_err}; GetImage on root — {root_err}; \
                 XComposite fallback — {comp_err}. If this is a Wayland session (XWayland), \
                 the X11 root has no image data — the native Wayland desktop cannot be \
                 captured over X11; run the agent in a real X11 session (Xorg/Xvfb) instead"
            )),
        }
    }

    fn composite_capture(&mut self) -> Result<Vec<u8>, String> {
        self.ensure_composite()?;
        let pixmap = self
            .composite
            .ok_or_else(|| "composite pixmap missing after ensure".to_string())?;
        self.capture_impl(pixmap)
    }
}

// ── Windows GDI ────────────────────────────────────────────────

#[cfg(windows)]
mod gdi {
    use super::{Frame, FrameSource, MonitorInfo};
    use windows_sys::Win32::Foundation::{GetLastError, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, ReleaseDC, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, MONITORINFO,
        SRCCOPY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    pub struct GdiSource {
        width: usize,
        height: usize,
        /// 目标显示器在虚拟桌面坐标系中的矩形（MYS-954 多屏）。主屏（或
        /// 未指定）时为 (0,0)；BitBlt 源坐标用 left/top 偏移取到对应屏。
        mon_x: i32,
        mon_y: i32,
        /// 枚举顺序中的显示器序号（access-lost 重建时保持同一块屏）。
        monitor_index: usize,
        /// Cached screen DC (see `blit`): per-frame GetDC is 3-4x slower and
        /// starves high-fps capture; the cache is dropped on BitBlt failure
        /// via `rebuild`.
        screen_dc: HDC,
        mem_dc: HDC,
        bmp: HBITMAP,
        /// `SelectObject(mem_dc, bmp)` 返回的旧对象（DC 默认的 1x1 单色位图）。
        /// 删除 `bmp` 前必须先把它选回 `mem_dc`——GDI 不允许删除仍被 DC 选中的
        /// 对象（DeleteObject 静默失败 → 每次 resize/重建漏一张全屏位图，
        /// MYS-2765 #5）。
        old_bmp: HGDIOBJ,
    }

    unsafe impl Send for GdiSource {}

    /// EnumDisplayMonitors 枚举全部显示器几何（系统枚举顺序，主屏通常在
    /// 前）。`GetMonitorInfoW` 取 `rcMonitor`（虚拟桌面坐标 + 分辨率）。
    /// 失败的显示器（GetMonitorInfoW 返回 0）跳过。
    pub(super) fn list_monitors_gdi() -> Vec<MonitorInfo> {
        struct Ctx {
            idx: u32,
            out: Vec<MonitorInfo>,
        }
        let mut ctx = Ctx { idx: 0, out: Vec::new() };
        unsafe {
            EnumDisplayMonitors(
                std::ptr::null_mut(),
                std::ptr::null(),
                Some(enum_proc),
                &mut ctx as *mut Ctx as isize,
            );
        }
        unsafe extern "system" fn enum_proc(
            hmon: windows_sys::Win32::Graphics::Gdi::HMONITOR,
            _hdc: HDC,
            _rect: *mut RECT,
            lparam: isize,
        ) -> windows_sys::Win32::Foundation::BOOL {
            let ctx = unsafe { &mut *(lparam as *mut Ctx) };
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if unsafe { GetMonitorInfoW(hmon, &mut mi) } != 0 {
                let r = &mi.rcMonitor;
                // 名称里的序号用**已收集数量**，与 `open_at`/`list_monitors`
                // 的数组下标（浏览器下拉 value）严格一致（MYS-2765 #9）。
                let ordinal = ctx.out.len() + 1;
                ctx.out.push(MonitorInfo {
                    name: format!("DISPLAY{ordinal}"),
                    width: (r.right - r.left).max(0) as u32,
                    height: (r.bottom - r.top).max(0) as u32,
                    x: r.left,
                    y: r.top,
                });
            }
            ctx.idx += 1;
            1 // continue enumeration
        }
        ctx.out
    }

    impl GdiSource {
        /// Open capture for the `index`-th display monitor（EnumDisplayMonitors
        /// 枚举顺序，0 = 主屏）。抓帧时按该屏在虚拟桌面中的矩形 BitBlt。
        pub fn open_at(index: usize) -> Result<Self, String> {
            let monitors = list_monitors_gdi();
            if index >= monitors.len() {
                return Err(format!(
                    "no GDI monitor #{index} (only {} detected)",
                    monitors.len()
                ));
            }
            let m = &monitors[index];
            tracing::info!(
                monitor = index,
                name = %m.name,
                geom = %format!("{}x{}+{},{}", m.width, m.height, m.x, m.y),
                "gdi capture monitor selected"
            );
            let mut s = Self {
                width: 0,
                height: 0,
                mon_x: m.x,
                mon_y: m.y,
                monitor_index: index,
                screen_dc: std::ptr::null_mut(),
                mem_dc: std::ptr::null_mut(),
                bmp: std::ptr::null_mut(),
                old_bmp: std::ptr::null_mut(),
            };
            unsafe { s.rebuild()? };
            Ok(s)
        }

        /// (Re)create the compatible DC + bitmap sized to the *current*
        /// screen, and refresh the cached resolution. Called at open and
        /// again to self-heal when a capture fails (e.g. display mode
        /// changed or a screen-DC handle went stale).
        ///
        /// MYS-2765 #5：删除位图前必须先把 `SelectObject` 返回的旧对象选回
        /// `mem_dc`——GDI 不允许删除仍被 DC 选中的位图，`DeleteObject` 会静默
        /// 失败，每次分辨率变化/自愈重建都漏掉一张全屏位图。`screen_dc` 也
        /// 必须在重建后**重新取**：旧实现在 `blit` 的失败重试路径里沿用了
        /// 刚被 `ReleaseDC` 的句柄，重试必然再失败（"即时重试"形同虚设）。
        unsafe fn rebuild(&mut self) -> Result<(), String> {
            if !self.mem_dc.is_null() && !self.old_bmp.is_null() {
                SelectObject(self.mem_dc, self.old_bmp);
                self.old_bmp = std::ptr::null_mut();
            }
            if !self.bmp.is_null() {
                DeleteObject(self.bmp);
                self.bmp = std::ptr::null_mut();
            }
            if !self.mem_dc.is_null() {
                DeleteDC(self.mem_dc);
                self.mem_dc = std::ptr::null_mut();
            }
            if !self.screen_dc.is_null() {
                ReleaseDC(std::ptr::null_mut(), self.screen_dc);
                self.screen_dc = std::ptr::null_mut();
            }
            // 多屏（MYS-954）：分辨率取目标显示器矩形（EnumDisplayMonitors
            // 枚举顺序与 open_at 选屏一致）；屏幕 DC 是整个虚拟桌面，BitBlt
            // 源坐标用 mon_x/mon_y 偏移取到对应屏。
            let monitors = list_monitors_gdi();
            let (width, height) = match monitors.get(self.monitor_index) {
                Some(m) if m.width >= 2 && m.height >= 2 => (m.width as usize, m.height as usize),
                _ => (
                    GetSystemMetrics(SM_CXSCREEN) as usize,
                    GetSystemMetrics(SM_CYSCREEN) as usize,
                ),
            };
            self.mon_x = monitors.get(self.monitor_index).map(|m| m.x).unwrap_or(0);
            self.mon_y = monitors.get(self.monitor_index).map(|m| m.y).unwrap_or(0);
            // 缓存屏幕 DC（blit 的热路径用它；这里建好就不用每帧 GetDC）。
            let screen_dc = GetDC(std::ptr::null_mut());
            if screen_dc.is_null() {
                return Err(format!("GetDC failed (err={})", GetLastError()));
            }
            let mem_dc = CreateCompatibleDC(screen_dc);
            let bmp = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
            let err = GetLastError();
            if mem_dc.is_null() || bmp.is_null() {
                if !mem_dc.is_null() {
                    DeleteDC(mem_dc);
                }
                if !bmp.is_null() {
                    DeleteObject(bmp);
                }
                ReleaseDC(std::ptr::null_mut(), screen_dc);
                return Err(format!("GDI init failed (err={err})"));
            }
            let old_bmp = SelectObject(mem_dc, bmp);
            self.width = width;
            self.height = height;
            self.mem_dc = mem_dc;
            self.bmp = bmp;
            self.old_bmp = old_bmp;
            // 保留屏幕 DC 供 blit 复用；失败重试时由 blit 自己判空重取。
            self.screen_dc = screen_dc;
            Ok(())
        }

        /// Copy the current desktop into the compatible bitmap.
        ///
        /// The screen DC is cached between frames for speed (GetDC/ReleaseDC
        /// per frame costs 3-4x on high fps and starved 25-30fps capture on
        /// real hardware). A cached DC does go stale when the session/desktop
        /// changes (lock screen, UAC secure desktop, mode switch, screen
        /// off) — BitBlt then fails with `ERROR_INVALID_HANDLE` (err=6) —
        /// so on failure we rebuild the whole GDI context (fresh screen DC
        /// + size) once and retry; transient switches self-heal instead of
        /// freezing the feed.
        unsafe fn blit(&mut self) -> Result<(), String> {
            if self.screen_dc.is_null() {
                self.screen_dc = GetDC(std::ptr::null_mut());
                if self.screen_dc.is_null() {
                    return Err(format!("GetDC failed (err={})", GetLastError()));
                }
            }
            let w = self.width as i32;
            let h = self.height as i32;
            // 多屏（MYS-954）：BitBlt 源坐标 = 目标显示器在虚拟桌面中的
            // 原点（主屏 0,0；副屏为正/负偏移），取到该屏内容。
            let sx = self.mon_x;
            let sy = self.mon_y;
            let ok = BitBlt(self.mem_dc, 0, 0, w, h, self.screen_dc, sx, sy, SRCCOPY);
            if ok != 0 {
                return Ok(());
            }
            let err1 = GetLastError();
            // stale DC — rebuild everything (drops the cached screen_dc too)
            if let Err(e) = self.rebuild() {
                return Err(format!("BitBlt failed (err={err1}); rebuild failed: {e}"));
            }
            let w = self.width as i32;
            let h = self.height as i32;
            let sx = self.mon_x;
            let sy = self.mon_y;
            let ok = BitBlt(self.mem_dc, 0, 0, w, h, self.screen_dc, sx, sy, SRCCOPY);
            if ok == 0 {
                return Err(format!("BitBlt failed (err={})", GetLastError()));
            }
            Ok(())
        }

        unsafe fn read_pixels(&self) -> Result<Vec<u8>, String> {
            let w = self.width as i32;
            let h = self.height as i32;
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            if GetDIBits(
                self.mem_dc,
                self.bmp,
                0,
                h as u32,
                pixels.as_mut_ptr() as *mut _,
                &mut bmi,
                DIB_RGB_COLORS,
            ) == 0
            {
                return Err(format!("GetDIBits failed (err={})", GetLastError()));
            }
            Ok(pixels)
        }
    }

    impl FrameSource for GdiSource {
        fn resolution(&self) -> (usize, usize) {
            (self.width, self.height)
        }

        /// 所选显示器在虚拟桌面坐标系中的原点（MYS-2765 #10）：鼠标注入要把
        /// 它加上，否则点副屏会落到主屏。
        fn origin(&self) -> (i32, i32) {
            (self.mon_x, self.mon_y)
        }

        fn list_monitors(&self) -> Vec<MonitorInfo> {
            list_monitors_gdi()
        }

        fn next_frame(&mut self) -> Result<Frame, String> {
            unsafe {
                // display 分辨率 / 屏幕排布运行时变更检测：每帧重读显示器矩形
                // （廉价），变化则重建整个 GDI context（rustdesk display 变更
                // 对齐）。rebuild 内部重新枚举 monitors + 建 screen DC /
                // compatible bitmap，BitBlt 下一帧即新尺寸/新偏移。
                // 位置（mon_x/mon_y）变化同样要重建：屏幕在系统设置里被挪动时
                // 分辨率不变，但 BitBlt 源偏移和鼠标原点都变了（MYS-2765 #9）。
                let m = list_monitors_gdi().get(self.monitor_index).cloned();
                if let Some(m) = m {
                    let (mw, mh) = (m.width as usize, m.height as usize);
                    let moved = (m.x, m.y) != (self.mon_x, self.mon_y);
                    if mw >= 2 && mh >= 2 && (mw != self.width || mh != self.height || moved) {
                        tracing::info!(
                            from = %format!("{}x{}+{},{}", self.width, self.height, self.mon_x, self.mon_y),
                            to = %format!("{}x{}+{},{}", mw, mh, m.x, m.y),
                            "gdi display geometry changed — rebuilding capture context"
                        );
                        self.rebuild()?;
                    }
                }
                self.blit()?;
                let h = self.height;
                let (w, h) = (self.width, h);
                let bgra = self.read_pixels()?;
                Ok(Frame {
                    bgra,
                    width: w,
                    height: h,
                })
            }
        }
    }

    impl Drop for GdiSource {
        fn drop(&mut self) {
            unsafe {
                // 先解除选中再删位图（MYS-2765 #5）：仍被 mem_dc 选中的位图
                // DeleteObject 会静默失败。
                if !self.mem_dc.is_null() && !self.old_bmp.is_null() {
                    SelectObject(self.mem_dc, self.old_bmp);
                    self.old_bmp = std::ptr::null_mut();
                }
                if !self.bmp.is_null() {
                    DeleteObject(self.bmp);
                }
                if !self.mem_dc.is_null() {
                    DeleteDC(self.mem_dc);
                }
                if !self.screen_dc.is_null() {
                    ReleaseDC(std::ptr::null_mut(), self.screen_dc);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── MYS-2765 #4/#9/#10 平台无关部分 ───────────────────────

    /// 原点打包必须能原样往返，**包括负坐标**（Windows 主屏左侧的副屏是
    /// 负原点，X11 也可能有负 CRTC 偏移）。
    #[test]
    fn test_origin_pack_roundtrip() {
        for (x, y) in [
            (0, 0),
            (1280, 0),
            (0, 720),
            (-1920, 0),
            (-1, -1),
            (i32::MAX, i32::MIN),
            (65535, -65535),
        ] {
            assert_eq!(unpack_origin(pack_origin(x, y)), (x, y), "origin ({x},{y})");
        }
        // 默认值 0 解出来就是 (0,0)（未选屏/单屏语义）。
        assert_eq!(unpack_origin(0), (0, 0));
    }

    /// DXGI 错误分类（MYS-2765 #4）：必须按 **HRESULT** 判定，而不是字符串。
    /// windows-result 的 Display 是本地化的 `消息 (0x887A0026)`，不含常量名。
    #[test]
    fn test_classify_dxgi_error() {
        // 需要重建的三类：锁屏/UAC/会话切换（ACCESS_LOST）、驱动重置。
        for hr in [
            DXGI_ERROR_ACCESS_LOST_CODE,
            DXGI_ERROR_DEVICE_REMOVED_CODE,
            DXGI_ERROR_DEVICE_RESET_CODE,
        ] {
            assert_eq!(
                classify_dxgi_error(hr),
                CaptureRecovery::Rebuild,
                "0x{:08X} 必须触发重建",
                hr as u32
            );
        }
        // 静止桌面超时不是错误。
        assert_eq!(
            classify_dxgi_error(DXGI_ERROR_WAIT_TIMEOUT_CODE),
            CaptureRecovery::WaitTimeout
        );
        // 其它（如 DXGI_ERROR_UNSUPPORTED 0x887A0004、任意 HRESULT）→ 不重建。
        assert_eq!(classify_dxgi_error(0x887A_0004u32 as i32), CaptureRecovery::Fatal);
        assert_eq!(classify_dxgi_error(0), CaptureRecovery::Fatal);
        assert_eq!(classify_dxgi_error(0x8007_0057u32 as i32), CaptureRecovery::Fatal);
        // 常量值本身必须与 DXGI 头文件一致（写错就永远匹配不上）。
        assert_eq!(DXGI_ERROR_ACCESS_LOST_CODE as u32, 0x887A_0026);
        assert_eq!(DXGI_ERROR_WAIT_TIMEOUT_CODE as u32, 0x887A_0027);
        assert_eq!(DXGI_ERROR_DEVICE_REMOVED_CODE as u32, 0x887A_0005);
        assert_eq!(DXGI_ERROR_DEVICE_RESET_CODE as u32, 0x887A_0007);
    }

    /// 默认 `FrameSource::origin()` 是 (0,0)（单屏/全屏后端的语义）。
    #[test]
    fn test_default_origin_is_zero() {
        struct Stub;
        impl FrameSource for Stub {
            fn next_frame(&mut self) -> Result<Frame, String> {
                Err("unused".into())
            }
            fn resolution(&self) -> (usize, usize) {
                (4, 4)
            }
        }
        assert_eq!(Stub.origin(), (0, 0));
    }

    #[test]
    fn test_open_source_none_errors() {
        assert!(open_source("none", None, None).is_err());
    }

    #[test]
    fn test_open_source_invalid_kind_errors() {
        assert!(open_source("bogus", None, None).is_err());
    }

    #[test]
    fn test_wayland_reports_actionable_error() {
        // 未启用 wayland feature 的构建必须给出可操作错误（而不是 panic）。
        match open_source("wayland", None, None) {
            Ok(_) => {
                // 启用了 feature 且环境恰好可用的构建：允许成功。
            }
            Err(e) => {
                assert!(
                    e.contains("wayland") || e.contains("portal") || e.contains("Wayland"),
                    "err: {e}"
                );
            }
        }
    }

    /// X11 SHM fast path smoke test against a real X server (Xvfb :99).
    /// Verifies: SHM init succeeds, the first frame is captured via the
    /// segment, and multiple frames agree on size — plus drop() releases the
    /// SysV segment without leaking it (would show up as shmseg reuse errors).
    /// Skipped when no display is reachable: reads SR_XTEST_DISPLAY or :99.
    #[test]
    fn test_x11_shm_capture_on_xvfb() {
        let display = std::env::var("SR_XTEST_DISPLAY").unwrap_or_else(|_| ":99".into());
        let mut src = match X11Source::open(Some(&display)) {
            Ok(s) => s,
            Err(e) => {
                if e.contains("connect") && std::env::var("SR_NO_XTEST").is_ok() {
                    return;
                }
                panic!("x11 open failed: {e}");
            }
        };
        let (w, h) = src.resolution();
        assert!(w > 0 && h > 0);
        // First frame drives try_init_shm.
        let fr = src.next_frame().expect("frame");
        assert_eq!(fr.bgra.len(), w * h * 4);
        // On servers with MIT-SHM the fast path must be active after this.
        if src.shm.is_some() {
            assert_eq!(src.shm.as_ref().unwrap().size, w * h * 4);
            // Second frame also via SHM, same size.
            let fr2 = src.next_frame().expect("frame 2");
            assert_eq!(fr2.bgra.len(), w * h * 4);
        } else {
            eprintln!(
                "display {display} has no MIT-SHM support (or attach refused); \
                 GetImage fallback tested instead"
            );
        }
        // Drop must not leak: dropping the segment, a fresh X11Source to the
        // same display still captures (exercises segment id reuse + cleanup).
        // Xvfb aggressively resets a connection that closes then re-opens
        // immediately (os error 104 noise); retry briefly rather than flake.
        drop(src);
        let mut src2 = None;
        for attempt in 0..5 {
            match X11Source::open(Some(&display)) {
                Ok(s) => {
                    src2 = Some(s);
                    break;
                }
                Err(e) => {
                    if attempt == 4 {
                        panic!("reopen after drop, final attempt failed: {e}");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
        let mut src2 = src2.unwrap();
        let _ = src2.next_frame().expect("frame after reopen");
    }

    /// X11 capture smoke test against a real X server (Xvfb :99).
    /// Skipped when no display is reachable: reads SR_XTEST_DISPLAY or :99.
    #[test]
    fn test_x11_capture_on_xvfb() {
        let display = std::env::var("SR_XTEST_DISPLAY").unwrap_or_else(|_| ":99".into());
        let mut src = match X11Source::open(Some(&display)) {
            Ok(s) => s,
            Err(e) => {
                if e.contains("connect") && std::env::var("SR_NO_XTEST").is_ok() {
                    return;
                }
                panic!("x11 open failed: {e}");
            }
        };
        let (w, h) = src.resolution();
        assert!(w > 0 && h > 0);
        let fr = src.next_frame().expect("frame");
        assert_eq!(fr.bgra.len(), w * h * 4);
        // 允许全黑 (Xvfb 未上色时 root 是黑的); 若上色 (xsetroot -solid) 则校验像素进入
        let non_zero = fr.bgra.iter().filter(|&&b| b != 0).count();
        if non_zero == 0 {
            eprintln!("X11 frame is all-black {}x{} (Xvfb root unpainted)", w, h);
        }
    }

    /// #131 分辨率事件驱动：支持 RANDR 的服务器上应成功注册
    /// ScreenChangeNotify（`randr_registered=true`）——事件驱动替代轮询。
    /// 无扩展时允许注册失败（回落轮询），但必须不破坏首帧捕获。
    #[test]
    fn test_x11_list_monitors_returns_at_least_one() {
        // R5#136-146 多显示器最小子集：Xvfb（或真实 X）至少有一个 RANDR
        // 输出；枚举应非空且名称/分辨率可读。
        let display = std::env::var("SR_XTEST_DISPLAY").unwrap_or_else(|_| ":99".into());
        let src = match X11Source::open(Some(&display)) {
            Ok(s) => s,
            Err(e) => {
                if e.contains("connect") && std::env::var("SR_NO_XTEST").is_ok() {
                    return;
                }
                panic!("x11 open failed: {e}");
            }
        };
        let monitors = src.list_monitors();
        assert!(!monitors.is_empty(), "X server must expose ≥1 RANDR output");
        let m = &monitors[0];
        // 接通的输出应有非零分辨率（Xvfb 默认 screen 输出）
        assert!(m.width > 0 && m.height > 0, "monitor must have size, got {m:?}");
    }

    #[test]
    fn test_x11_randr_event_driven_resolution() {
        let display = std::env::var("SR_XTEST_DISPLAY").unwrap_or_else(|_| ":99".into());
        let mut src = match X11Source::open(Some(&display)) {
            Ok(s) => s,
            Err(e) => {
                if e.contains("connect") && std::env::var("SR_NO_XTEST").is_ok() {
                    return;
                }
                panic!("x11 open failed: {e}");
            }
        };
        // Xvfb :96 带 RANDR → 应注册成功；无扩展时允许失败但帧仍可捕获。
        if src.randr_registered {
            eprintln!("XRANDR ScreenChangeNotify registered (event-driven resolution)");
        } else {
            eprintln!("XRANDR unavailable — falling back to geometry polling (allowed)");
        }
        // 事件驱动路径必须不破坏捕获：首帧尺寸与分辨率一致。
        let (w, h) = src.resolution();
        let fr = src.next_frame().expect("frame with randr registered");
        assert_eq!(fr.bgra.len(), w * h * 4);
    }

    /// MYS-2765 #9/#10：X11 按 RANDR 输出序号选屏。
    ///
    /// `open(Some("0"))` 必须连上 `$DISPLAY`（不是把 "0" 当 display 字符串），
    /// 采集区域/分辨率 = 该输出的 CRTC 矩形，`origin()` = 该 CRTC 原点。
    /// 越界序号必须返回明确错误（旧实现把它当 DISPLAY 连接，报的是连接失败）。
    #[cfg(not(windows))]
    #[test]
    fn test_x11_open_by_output_index_selects_crtc() {
        let display = std::env::var("SR_XTEST_DISPLAY").unwrap_or_else(|_| ":99".into());
        // 先枚举拓扑（同一 display），拿到第 0 个输出的 CRTC 矩形作为期望值。
        let probe = match X11Source::open(Some(&display)) {
            Ok(s) => s,
            Err(e) => {
                if e.contains("connect") && std::env::var("SR_NO_XTEST").is_ok() {
                    return;
                }
                panic!("x11 open failed: {e}");
            }
        };
        let monitors = probe.list_monitors();
        assert!(!monitors.is_empty(), "X server must expose ≥1 RANDR output");
        let m0 = monitors[0].clone();
        assert!(m0.width > 0 && m0.height > 0, "output #0 must be connected: {m0:?}");
        drop(probe);

        // 序号选屏：X 连接仍走 SR_XTEST_DISPLAY（测试环境未必有 $DISPLAY）。
        let src = X11Source::open_with_x_display(Some("0"), Some(&display))
            .expect("open by output index 0");
        assert_eq!(src.resolution(), (m0.width as usize, m0.height as usize));
        assert_eq!(src.origin(), (m0.x, m0.y), "origin 必须等于该输出的 CRTC 原点");
        // 采集区域 = 该 CRTC 矩形；首帧尺寸随之（GetImage/SHM 都按裁剪取）。
        let mut src = src;
        let fr = src.next_frame().expect("frame from selected output");
        assert_eq!(fr.bgra.len(), m0.width as usize * m0.height as usize * 4);

        // 越界序号：明确报错（而不是连一个叫 "7" 的 display）。
        let err = match X11Source::open_with_x_display(Some("7"), Some(&display)) {
            Ok(_) => panic!("out-of-range output index must fail"),
            Err(e) => e,
        };
        assert!(
            err.contains("out of range") && err.contains("x11 monitor"),
            "越界序号应给出明确错误，实际: {err}"
        );
    }

    /// **MYS-2765 #9 review 退回项**：序号选屏时不得丢掉启动配置的 X display。
    ///
    /// 场景就是 systemd 服务：`--desktop-display :0` 起了 agent，环境里**没有**
    /// `$DISPLAY`；浏览器显示器下拉发 `desktop:select-display {display:"1"}`
    /// （枚举序号）。旧实现 `open_source` 只把选屏值传下去、`x_display` 恒为
    /// `None`，于是序号分支回落 `$DISPLAY`（未设置）→ 连接失败、桌面直接报错。
    ///
    /// 这里在**删掉 `DISPLAY`** 的进程环境里调 `open_source("x11", Some("0"),
    /// Some(SR_XTEST_DISPLAY))`，必须成功。修复前该调用返回
    /// `x11rb connect: ...` 错误（见交付说明）。
    ///
    /// `DISPLAY` 是进程级环境变量，与其它 X 测试互斥：用共享串行锁。
    #[cfg(not(windows))]
    #[test]
    fn test_x11_index_selection_keeps_configured_x_display() {
        let _serial = super::tests::x11_serial_lock();
        let x_display = std::env::var("SR_XTEST_DISPLAY").unwrap_or_else(|_| ":99".into());
        // 确认目标 display 真的可用（否则跳过，避免环境问题伪装成失败）。
        if x11rb::rust_connection::RustConnection::connect(Some(x_display.as_str())).is_err() {
            eprintln!("skipped: no X server at {x_display}");
            return;
        }
        // 模拟 systemd 服务：环境里没有 DISPLAY。
        let saved = std::env::var_os("DISPLAY");
        std::env::remove_var("DISPLAY");
        let result = open_source("x11", Some("0"), Some(&x_display));
        // 无论断言结果如何都要恢复环境，否则会污染同进程的其它测试。
        if let Some(v) = saved {
            std::env::set_var("DISPLAY", v);
        }

        match result {
            Ok((src, backend)) => {
                assert_eq!(backend, "x11");
                let monitors = src.list_monitors();
                assert!(!monitors.is_empty(), "至少要有一个 RANDR 输出");
                let m0 = &monitors[0];
                assert_eq!(
                    src.resolution(),
                    (m0.width as usize, m0.height as usize),
                    "序号 0 应裁到第一个输出的 CRTC"
                );
                assert_eq!(src.origin(), (m0.x, m0.y));
            }
            Err(e) => panic!(
                "序号选屏必须复用启动配置的 X display（{x_display}），实际报错: {e}"
            ),
        }
    }

    /// 纯数字的 `--desktop-display` 仍按序号语义（派生出的 X display 为 None，
    /// 回落 `$DISPLAY`）；非数字才作为 X display 串。
    #[test]
    fn test_x_display_from_config_semantics() {
        use crate::agent::desktop::x_display_from_config;
        assert_eq!(x_display_from_config(&Some("0".into())), None);
        assert_eq!(x_display_from_config(&Some("12".into())), None);
        assert_eq!(x_display_from_config(&Some(":0".into())), Some(":0".into()));
        assert_eq!(
            x_display_from_config(&Some("host:1".into())),
            Some("host:1".into())
        );
        assert_eq!(x_display_from_config(&Some("  :2  ".into())), Some(":2".into()));
        assert_eq!(x_display_from_config(&Some("".into())), None);
        assert_eq!(x_display_from_config(&None), None);
    }

    /// X11 相关测试的共享串行锁（`DISPLAY` 是进程级环境变量）。
    #[cfg(not(windows))]
    fn x11_serial_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `xrandr --setmonitor` 造的第二块"屏"在 Xvfb 上不会变成第二个 RANDR
    /// output（只拆根窗口），所以序号选屏的第二屏无法用 Xvfb 直接复现。
    /// 这个测试改为直接验证**裁剪矩形语义**：在 2560x720 的 root 上左右两半
    /// 涂不同颜色，然后把采集区域裁到右半 `+1280+0`（就是第二块物理屏接在
    /// 主屏右侧时的几何），断言：
    /// - `origin() == (1280, 0)`、`resolution() == 1280x720`
    /// - 裁剪帧的每个像素等于整屏帧对应位置的像素（GetImage 与 SHM 两条路径）
    ///
    /// 需要 `SR_SECOND_DISPLAY`（默认 `:94`）上有一个 ≥2560 宽的 X server；
    /// 没有就跳过并说明。
    #[cfg(not(windows))]
    #[test]
    fn test_x11_crop_region_and_origin() {
        let display = std::env::var("SR_SECOND_DISPLAY").unwrap_or_else(|_| ":94".into());
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{ConnectionExt as _, Rectangle};
        let (conn, screen_num) = match x11rb::rust_connection::RustConnection::connect(
            Some(display.as_str()),
        ) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("crop test skipped: cannot connect to {display} ({e})");
                return;
            }
        };
        let screen = &conn.setup().roots[screen_num];
        let (root_w, root_h) = (screen.width_in_pixels, screen.height_in_pixels);
        if root_w < 2560 || root_h < 720 {
            eprintln!(
                "crop test skipped: {display} is {root_w}x{root_h}, need ≥2560x720 \
                 (start Xvfb :94 -screen 0 2560x720x24)"
            );
            return;
        }
        // 左右两半涂不同颜色（纯色即可判别来源区域）。
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, screen.root, &x11rb::protocol::xproto::CreateGCAux {
            foreground: Some(0x00_00_00_ff), // 红（BGRX 低位 = B，高位 = R）
            ..Default::default()
        })
        .unwrap();
        conn.poly_fill_rectangle(
            screen.root,
            gc,
            &[Rectangle { x: 0, y: 0, width: 1280, height: root_h }],
        )
        .unwrap();
        conn.change_gc(gc, &x11rb::protocol::xproto::ChangeGCAux {
            foreground: Some(0x00_ff_00_00), // 蓝
            ..Default::default()
        })
        .unwrap();
        conn.poly_fill_rectangle(
            screen.root,
            gc,
            &[Rectangle { x: 1280, y: 0, width: 1280, height: root_h }],
        )
        .unwrap();
        conn.free_gc(gc).unwrap();
        conn.flush().unwrap();

        // 整屏（未选屏）参考帧。
        let mut full = X11Source::open_with_x_display(None, Some(&display)).expect("open full");
        assert_eq!(full.resolution(), (root_w as usize, root_h as usize));
        let full_frame = full.next_frame().expect("full frame");

        // 裁剪到右半 +1280+0：apply_crop 正是序号选屏用的那段逻辑。
        let mut cropped =
            X11Source::open_with_x_display(None, Some(&display)).expect("open cropped");
        cropped.apply_crop((1280, 0, 1280, root_h.min(720)));
        assert_eq!(cropped.origin(), (1280, 0), "第二屏原点必须是 +1280+0");
        assert_eq!(cropped.resolution(), (1280, root_h.min(720) as usize));
        let crop_frame = cropped.next_frame().expect("cropped frame");
        assert_eq!(
            crop_frame.bgra.len(),
            1280 * root_h.min(720) as usize * 4,
            "裁剪帧尺寸必须等于采集区域"
        );
        // 逐像素比对：裁剪帧 = 整屏帧在 (x+1280, y) 处的内容。
        let (cw, ch) = (crop_frame.width, crop_frame.height);
        let fw_stride = full_frame.width * 4;
        let mut mismatched = 0usize;
        for y in 0..ch {
            for x in 0..cw {
                let ci = (y * cw + x) * 4;
                let fi = y * fw_stride + (x + 1280) * 4;
                if crop_frame.bgra[ci..ci + 3] != full_frame.bgra[fi..fi + 3] {
                    mismatched += 1;
                }
            }
        }
        assert_eq!(
            mismatched, 0,
            "裁剪区域必须与整屏同一位置逐像素一致（GetImage/SHM 裁剪偏移错位）"
        );
    }

    /// XComposite fallback path (used when the root `GetImage` fails, e.g.
    /// composited XWayland): redirect the root and read the named pixmap.
    /// Xvfb ships the Composite extension, so this runs against :99.
    #[test]
    fn test_x11_composite_capture_on_xvfb() {
        let display = std::env::var("SR_XTEST_DISPLAY").unwrap_or_else(|_| ":99".into());
        let mut src = match X11Source::open(Some(&display)) {
            Ok(s) => s,
            Err(e) => {
                if e.contains("connect") && std::env::var("SR_NO_XTEST").is_ok() {
                    return;
                }
                panic!("x11 open failed: {e}");
            }
        };
        if let Err(e) = src.ensure_composite() {
            // 无 Composite 扩展的服务器直接跳过（不是断言失败）
            eprintln!("composite unavailable, skipping: {e}");
            return;
        }
        let (w, h) = src.resolution();
        let bgra = src.composite_capture().expect("composite capture");
        assert_eq!(bgra.len(), w * h * 4, "composite capture must match size");
        // 与主路径尺寸一致即可；内容允许全黑（Xvfb root 未上色）
    }

    #[cfg(windows)]
    #[test]
    fn test_gdi_source_compiles_on_windows_target() {
        // Compile-only presence check (never runs in tests).
    }

    /// 快速产帧源：每帧内容含递增序号（验证"取到最新帧"）。
    struct CounterSource {
        w: usize,
        h: usize,
        t: usize,
    }

    impl FrameSource for CounterSource {
        fn next_frame(&mut self) -> Result<Frame, String> {
            self.t += 1;
            let v = self.t as u8;
            Ok(Frame {
                bgra: vec![v; self.w * self.h * 4],
                width: self.w,
                height: self.h,
            })
        }
        fn resolution(&self) -> (usize, usize) {
            (self.w, self.h)
        }
    }

    #[test]
    fn test_threaded_source_yields_latest_frame() {
        let src: Box<dyn FrameSource> = Box::new(CounterSource { w: 16, h: 16, t: 0 });
        let ts = ThreadedFrameSource::spawn(src).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut frame = None;
        while std::time::Instant::now() < deadline {
            if let Some(f) = ts.try_latest() {
                frame = Some(f);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let f = frame.expect("threaded source must produce a frame within 2s");
        assert_eq!((f.width, f.height), (16, 16));
        assert_eq!(f.bgra.len(), 16 * 16 * 4);
        // 持续产帧：再取几帧都应成功
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut got = 0;
        while std::time::Instant::now() < deadline && got < 3 {
            if ts.try_latest().is_some() {
                got += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(got >= 3, "must keep yielding frames, got {got}");
    }

    /// #135 抓帧独立上限：动态源 + max_fps 应把产帧率压到 ~max_fps，
    /// 而不是全速（对齐 R3 乙83）。验证限速真实生效（1s 窗口产帧数
    /// 显著小于全速，且大致 ≈ max_fps × 1s）。
    #[test]
    fn test_threaded_source_max_fps_throttles() {
        let src: Box<dyn FrameSource> = Box::new(CounterSource { w: 8, h: 8, t: 0 });
        // max_fps=20 → 50ms 间隔；1s 窗口应 ~20 帧（远小于全速数百）。
        let ts = ThreadedFrameSource::spawn_with_max_fps(src, 20.0, None).unwrap();
        // 等首帧（拿到后首帧即被消费，不计入限速统计）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut first = None;
        while std::time::Instant::now() < deadline {
            if let Some(f) = ts.try_latest() {
                first = Some(f);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(first.is_some(), "throttled source must still produce");
        // 统计 1.1s 内的可取帧数。
        let mut got = 0u32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1100);
        while std::time::Instant::now() < deadline {
            if ts.try_latest().is_some() {
                got += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // 20fps × 1.1s ≈ 22 帧；允许 ±调度抖动（15..40）。
        assert!(
            (15..=45).contains(&got),
            "max_fps=20 must throttle capture to ~20fps, got {got} frames in 1.1s"
        );
        drop(ts);
    }

    /// 静止退避（R3 乙57/82 对齐）：画面恒定的源 → 抓帧线程应退避 sleep 控制
    /// 产帧频率（≤10fps 轮询），而不是全速空转。验证产帧被限频而非死锁：
    /// 静止源在 1.2s 窗口内产出的可取帧数应明显少于全速源。
    #[test]
    fn test_threaded_static_source_backs_off() {
        // 恒定画面源：每帧字节完全相同，would-block 判 `same` → 退避 sleep。
        struct StaticSource {
            w: usize,
            h: usize,
        }
        impl FrameSource for StaticSource {
            fn next_frame(&mut self) -> Result<Frame, String> {
                Ok(Frame {
                    bgra: vec![7u8; self.w * self.h * 4],
                    width: self.w,
                    height: self.h,
                })
            }
            fn resolution(&self) -> (usize, usize) {
                (self.w, self.h)
            }
        }
        let src: Box<dyn FrameSource> = Box::new(StaticSource { w: 8, h: 8 });
        let ts = ThreadedFrameSource::spawn(src).unwrap();
        // 必须等到首帧：第一帧 same=false 发布，之后全部 same → 退避。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut first = None;
        while std::time::Instant::now() < deadline {
            if let Some(f) = ts.try_latest() {
                first = Some(f);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(first.is_some(), "static source must start producing");
        // 消费掉首帧后，统计后续 1.2s 内到达的帧数——退避 100ms 应 ≤ ~12 帧
        //（10fps 轮询），若仍然全速（无限产）则测试失败（回归不要退避）。
        let mut got = 0u32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1200);
        while std::time::Instant::now() < deadline {
            if ts.try_latest().is_some() {
                got += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // 上限：10fps × 1.2s ≈ 12 帧 + 调度余量；远小于全速（≥100 帧）。
        assert!(
            got <= 20,
            "static capture must back off (10fps poll) instead of spinning: {got} frames in 1.2s"
        );
        drop(ts);
    }

    #[test]
    fn test_threaded_source_drop_stops_thread() {
        let src: Box<dyn FrameSource> = Box::new(CounterSource { w: 8, h: 8, t: 0 });
        let ts = ThreadedFrameSource::spawn(src).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(ts.try_latest().is_some(), "capture thread must run before drop");
        // drop 触发 stop + join，不应 hang。
        drop(ts);
    }

    #[test]
    fn test_threaded_source_reports_errors() {
        struct Fail;
        impl FrameSource for Fail {
            fn next_frame(&mut self) -> Result<Frame, String> {
                Err("boom".into())
            }
            fn resolution(&self) -> (usize, usize) {
                (4, 4)
            }
        }
        let ts = ThreadedFrameSource::spawn(Box::new(Fail)).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && ts.err_count() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(ts.err_count() > 0, "errors must accumulate in the thread");
        assert!(ts.try_latest().is_none(), "no frame when the source always fails");
        assert_eq!(ts.last_err().as_deref(), Some("boom"));
    }
}

