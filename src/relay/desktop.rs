//! Relay-side desktop video fan-out.
//!
//! The agent POSTs `desktop:video` (kind `init` | `frag`, base64 fMP4 bytes;
//! `frag` carries a `key` flag when the sample is a random-access point).
//! This module keeps one [`DesktopStream`] per session: an `init` byte cache
//! (replayed to late joiners) plus the set of connected browsers waiting on
//! `GET /agent/desktop/stream`. Each viewer receives the init bytes first,
//! then fragments — and non-key fragments are dropped until the viewer has
//! seen its first key frame, so every stream starts at an IDR (browser MSE
//! and ffmpeg both discard media before a random-access point).

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::relay::SharedState;

/// 带宽记账快照（#152）：会话级转发统计，relay admin/遥测画 KPI 曲线用。
#[derive(Clone, Debug, Default)]
pub struct DesktopStreamStats {
    /// 当前连接的 viewer 数。
    pub viewer_count: usize,
    /// 累计成功投递的字节数。
    pub forwarded_bytes: u64,
    /// 累计成功投递的帧数。
    pub forwarded_frames: u64,
}

/// Shared, cloneable fan-out state for one session's desktop stream.
#[derive(Clone)]
pub struct DesktopStream {
    inner: Arc<Inner>,
}

/// One connected viewer: its send channel plus whether a key frame has been
/// delivered yet (fragments before the first key frame are dropped).
struct ViewerCtx {
    tx: mpsc::Sender<Vec<u8>>,
    key_ok: bool,
    /// 连续 try_send 丢帧计数：为"丢旧保新"而非"满即踢"。
    ///
    /// 缓冲满时 drop 是旧帧，viewer 仍留在 fan-out 里——瞬时积压（浏览器
    /// 解码停顿/标签页切后台）会自己恢复，而不是被踢出去重连（重连走缓存
    /// init 秒级恢复，但反复踢→重连会造成可感知的闪烁）。只有当 viewer
    /// 长期跟不上（连续丢帧超过阈值，说明它已彻底失联/死亡），才在下一拍
    /// 从 fan-out 移除 -- 对齐 rustdesk 丢帧重连语义（烂帧直接丢旧，订阅
    /// 者涝死再踢）。
    drop_count: u32,
    /// 带宽记账（R3 丙113 / R5#152）：成功投递给该 viewer 的字节与帧数。
    /// relay admin/遥测据此画每 viewer 转发曲线（带宽用量可观测）。
    bytes: u64,
    frames: u64,
}

/// 连续丢帧超过此值判定 viewer 死亡（30fps 下 ≈ 2s 完全无消费能力）。
/// 主要触发场景是浏览器页面休眠/切走（rAF 停、fetch 停读），这类 viewer
/// 保留无意义还白占内存 -- 到阈值即移除，浏览器回来时靠重连恢复。
const MAX_CONSECUTIVE_DROPS: u32 = 60;

struct Inner {
    /// Latest fMP4 init segment (ftyp+moov). New viewers are replayed this
    /// before any fragments.
    init: tokio::sync::RwLock<Option<Vec<u8>>>,
    /// connected viewers, keyed by a per-connection id.
    viewers: tokio::sync::RwLock<HashMap<String, ViewerCtx>>,
    /// relay 的整体带宽限制器（`None` = 不限速 / agent 侧 LAN fan-out）。
    /// 视频走 `try_acquire`：令牌不足直接丢帧（等下一个关键帧恢复），
    /// 绝不在这里 await——一帧的等待会把这个会话所有 viewer 一起拖住。
    limiter: Option<Arc<crate::relay::bandwidth::BandwidthLimiter>>,
}

impl Default for DesktopStream {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopStream {
    pub fn new() -> Self {
        Self::with_limiter(None)
    }

    /// 带 relay 带宽限制器的 fan-out（`relay::start` 创建的流用这个）。
    pub fn with_limiter(
        limiter: Option<Arc<crate::relay::bandwidth::BandwidthLimiter>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                init: tokio::sync::RwLock::new(None),
                viewers: tokio::sync::RwLock::new(HashMap::new()),
                limiter,
            }),
        }
    }

    /// Cache a fresh init segment and forward it to all connected viewers
    /// (a re-init after a codec/parameter-set change).
    pub async fn set_init(&self, bytes: Vec<u8>) {
        *self.inner.init.write().await = Some(bytes.clone());
        self.broadcast_to_viewers(bytes).await;
    }

    /// 清空缓存的 init（MYS-954 重建竞态修复）：agent 在 stop→start（切
    /// codec / 切灰度 monochrome）时调用。否则新 viewer 的 `add_viewer`
    /// 会立即拿到**旧 codec 的缓存 init**，随后新 codec 的 frag 到达 →
    /// 解码器按旧 init 建 → AV1 解码异常（切灰度/切编码偶发复现的根因）。
    /// 清空后新 viewer 走 `wait_first_init` 等到新 init 才收流。
    pub async fn clear_init(&self) {
        *self.inner.init.write().await = None;
    }

    /// Forward one media fragment. `is_key` marks a random-access frame.
    ///
    /// 背压策略（对齐 rustdesk 丢旧保新）：channel 满时**不立即踢 viewer**，
    /// 而是让这一帧跳过该 viewer（它自然是旧帧）并累计 `drop_count`；
    /// 只有连续丢帧超过 [`MAX_CONSECUTIVE_DROPS`] 才判定 viewer 死亡移除。
    /// 瞬时积压（解码停顿/切后台几百 ms）能自愈，不至于"一满就踢→反复
    /// 重连→闪烁"。
    ///
    /// 返回本拍因缓冲满被跳过（丢旧）的 viewer 数（R5#16 回传拥塞信号：
    /// >0 说明 relay→浏览器传输段发生了拥塞/积压，调用方据此限频回传
    /// agent，作为浏览器段 e2e/dq 之外的传输段拥塞证据）。
    pub async fn push_frag(&self, is_key: bool, bytes: Vec<u8>) -> u32 {
        let limiter = self.inner.limiter.clone();
        let mut viewers = self.inner.viewers.write().await;
        let mut dead = Vec::new();
        let mut congested = 0u32;
        let mut bw_dropped = 0u64;
        for (id, ctx) in viewers.iter_mut() {
            if !ctx.key_ok && !is_key {
                // Waiting for a key frame after a drop: skipped deltas still
                // count toward dead-viewer detection while its queue is full.
                if ctx.tx.capacity() == 0 {
                    ctx.drop_count += 1;
                    congested += 1;
                    if ctx.drop_count >= MAX_CONSECUTIVE_DROPS {
                        dead.push(id.clone());
                    }
                }
                continue;
            }
            // 带宽限速（MYS-2287）：令牌不足就直接丢这一帧（关键帧同样受限）。
            // 与"缓冲满"不同，这里**不累加 `drop_count`**——带宽丢帧不是
            // viewer 消费能力的问题，绝不能因此把它踢出 fan-out。丢帧同样
            // 打断参考链，必须等下一个关键帧才能继续发。
            if let Some(lim) = &limiter {
                if !lim.down.try_acquire(bytes.len()) {
                    ctx.key_ok = false;
                    congested += 1;
                    bw_dropped += 1;
                    continue;
                }
            }
            match ctx.tx.try_send(bytes.clone()) {
                Ok(()) => {
                    ctx.key_ok = true;
                    ctx.drop_count = 0;
                    // 带宽记账（#152）：只在真正投递时计数，丢帧不计（丢帧
                    // 本身由 drop_count 追踪）。
                    ctx.bytes += bytes.len() as u64;
                    ctx.frames += 1;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    dead.push(id.clone());
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // A skipped frame breaks this viewer's reference chain:
                    // later deltas would decode as corrupted blocks (花屏)
                    // until the next IDR. Resume only from a key frame
                    // (heartbeat IDR every 1.5–4.5s, see agent desktop).
                    ctx.key_ok = false;
                    ctx.drop_count += 1;
                    congested += 1;
                    if ctx.drop_count >= MAX_CONSECUTIVE_DROPS {
                        dead.push(id.clone());
                    }
                }
            }
        }
        drop(viewers);
        if bw_dropped > 0 {
            // 带宽丢帧单独计数（admin 概览展示），与 viewer 侧 drop_count 分开。
            if let Some(lim) = &limiter {
                lim.count_dropped_frame(bw_dropped);
            }
        }
        if !dead.is_empty() {
            let mut w = self.inner.viewers.write().await;
            for id in dead {
                w.remove(&id);
            }
        }
        congested
    }

    async fn broadcast_to_viewers(&self, bytes: Vec<u8>) {
        // 与 push_frag 一样用 write 锁：需要改 drop_count，且 init 重发是
        // 低频事件（不会与并发读形成热路径竞争）。read 锁内嵌套 write 会
        // 死锁（tokio RwLock write 等待 read 释放）。
        let limiter = self.inner.limiter.clone();
        let mut viewers = self.inner.viewers.write().await;
        let mut dead = Vec::new();
        for (id, ctx) in viewers.iter_mut() {
            // init 段是解码前提，绝不能丢（丢了 viewer 就再也解不出画面）。
            // 因此这里只记账（`consume`，可透支成负额度），不等待、不丢帧。
            if let Some(lim) = &limiter {
                lim.down.consume(bytes.len());
            }
            match ctx.tx.try_send(bytes.clone()) {
                Ok(()) => {
                    ctx.drop_count = 0;
                    // 带宽记账（#152）：init 也是 relay 转发的字节，计入。
                    ctx.bytes += bytes.len() as u64;
                }
                Err(_) => {
                    ctx.drop_count += 1;
                    if ctx.drop_count >= MAX_CONSECUTIVE_DROPS {
                        dead.push(id.clone());
                    }
                }
            }
        }
        drop(viewers);
        if !dead.is_empty() {
            let mut w = self.inner.viewers.write().await;
            for id in dead {
                w.remove(&id);
            }
        }
    }

    /// 让所有 viewer 回到"等关键帧"状态（MYS-2287）。
    ///
    /// 上行带宽不足时 relay 会丢掉 agent 发来的视频帧；只要丢了一帧，所有
    /// viewer 的参考链就断了，后续 delta 帧会解成花屏。这里把它们统一置为
    /// `key_ok=false`，下一次 IDR 到达后自动恢复。返回受影响的 viewer 数。
    pub async fn invalidate_keys(&self) -> usize {
        let mut viewers = self.inner.viewers.write().await;
        for ctx in viewers.values_mut() {
            ctx.key_ok = false;
        }
        viewers.len()
    }

    /// Register a new viewer. Returns `(viewer_id, receiver, cached_init)`.
    /// The receiver yields no fragments until the first key frame arrives.
    pub async fn add_viewer(&self) -> (String, mpsc::Receiver<Vec<u8>>, Option<Vec<u8>>) {
        let id = format!("dv_{}", uuid::Uuid::new_v4().simple());
        // 16 帧缓冲（30fps ≈ 0.5s）。原 256 帧≈8.5s 是 13s 级端到端时延的
        // 头号结构性来源：浏览器解码稍慢，relay 侧越积越多，观感"越来越卡"。
        // 16 帧保住 <0.5s 延迟；解码停顿由浏览器 reqkey/重连兜底（不再靠
        // 大缓冲硬扛）。标签页切后台等瞬时停顿仍可能 try_send 失败被踢，
        // 但重连走缓存 init 秒级恢复（代价 << 8.5s 积压）。
        let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
        self.inner
            .viewers
            .write()
            .await
            .insert(id.clone(), ViewerCtx { tx, key_ok: false, drop_count: 0, bytes: 0, frames: 0 });
        let init = self.inner.init.read().await.clone();
        (id, rx, init)
    }

    pub async fn remove_viewer(&self, id: &str) {
        self.inner.viewers.write().await.remove(id);
    }

    /// 带宽记账快照（R3 丙113 / R5#152）：会话级累计转发字节与帧数（全部
    /// viewer 求和），relay admin/遥测据此画每 session 带宽曲线。
    pub async fn stats(&self) -> DesktopStreamStats {
        let viewers = self.inner.viewers.read().await;
        let mut bytes = 0u64;
        let mut frames = 0u64;
        for ctx in viewers.values() {
            bytes += ctx.bytes;
            frames += ctx.frames;
        }
        let viewer_count = viewers.len();
        drop(viewers);
        DesktopStreamStats {
            viewer_count,
            forwarded_bytes: bytes,
            forwarded_frames: frames,
        }
    }

    /// Wait for the first init segment to be cached (a viewer that joins right
    /// after `desktop:started` can otherwise receive a fragment as its first
    /// chunk, which breaks MSE parsing — the init must always come first).
    /// Returns `None` if no init arrives within `timeout`.
    pub async fn wait_first_init(&self, timeout: std::time::Duration) -> Option<Vec<u8>> {
        let start = std::time::Instant::now();
        loop {
            if let Some(v) = self.inner.init.read().await.as_ref() {
                return Some(v.clone());
            }
            if start.elapsed() >= timeout {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

// ── GET /agent/desktop/stream ─────────────────────────────────

use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::StreamExt as _;

/// Streaming endpoint for one session's fMP4 desktop video.
///
/// Auth: session token (Bearer header or `?token=`), same as the browser SSE
/// handler. Response body is a chunked `video/mp4` byte stream: the cached
/// init segment first, then media fragments as the agent posts them.
pub async fn stream_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !crate::relay::auth::server_password_ok(&state, &headers, params.get("auth")).await {
        return crate::relay::auth::invalid_password_response();
    }
    let token =
        match crate::relay::auth::extract_token_from_headers_or_query(&headers, params.get("token")) {
            Some(t) => t,
            None => {
                return (StatusCode::UNAUTHORIZED, "Missing token").into_response();
            }
        };
    let (session_id, _perm) = match state.sessions.authenticate(&token).await {
        Some(r) => r,
        None => return (StatusCode::UNAUTHORIZED, "Invalid token").into_response(),
    };

    // The stream only exists once the agent has posted at least one
    // desktop:video message for this session.
    let ds = {
        let streams = state.desktop_streams.read().await;
        streams.get(&session_id).cloned()
    };
    let ds = match ds {
        Some(ds) => ds,
        None => {
            return (
                StatusCode::NOT_FOUND,
                "desktop stream is not active; start it first (desktop:start)",
            )
                .into_response();
        }
    };

    let (vid, mut rx, init) = ds.add_viewer().await;

    let stream = async_stream::stream! {
        // Remove this viewer from the fan-out when the stream ends — whether
        // the client disconnects (cancels the polling future) or the agent
        // stops pushing. Drop guards run on cancellation too.
        let guard = ViewerGuard {
            stream: ds.clone(),
            id: vid.clone(),
        };
        // 浏览器在中途加入时, 首块必须是 init 段(ftyp+moov+avcC), MSE 才会开始
        // 解析。若 init 尚未被 agent 首次推送(desktop:started 预建流之后、
        // 首个 init 到达之前加入的 viewer), 等待它出现, 而不是把第一个 fragment
        // 直接发给浏览器(那会触发 CHUNK_DEMUXER_ERROR_APPEND_FAILED)。
        // 放…在流内执行: 首段字节被消费时才等待。若超时仍未等到 init, 结束本
        // viewer 的流(不发送任何片段)——浏览器会收到流结束并自动重连, 而不会
        // 收到一个 demux 失败的首块。给到 10s: Windows agent GDI+编码器首帧
        // 初始化可能超过 5s。
        let init = match init {
            Some(i) => Some(i),
            None => ds.wait_first_init(Duration::from_secs(10)).await,
        };
        let Some(init) = init else {
            drop(guard);
            return;
        };
        yield Ok::<_, Infallible>(Bytes::from(init));
        // 空闲超时：健康流每秒都有帧（静止桌面最差 4.5s 一个 IDR），
        // 12s 无字节说明 agent 侧已停/崩溃、或上行中断（对齐 rustdesk
        // SEND_TIMEOUT_VIDEO=12s，MYS-886），主动收尾让浏览器 fetch 结束、
        // viewer 条目被清理，避免永久悬挂。
        loop {
            match next_frame_or_idle(&mut rx, DOWNLINK_IDLE_TIMEOUT).await {
                Some(chunk) => {
                    yield Ok::<_, Infallible>(Bytes::from(chunk));
                }
                None => break,
            }
        }
        drop(guard);
    };

    let body = Body::from_stream(stream);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap()
}

/// Connected viewers stay in the fan-out until they miss this long. A healthy
/// stream produces at least one frame every 4.5s (static desktop IDR
/// heartbeat); 12s of silence means the agent stopped or the uplink broke, so
/// the viewer is closed instead of hanging forever. Aligned with rustdesk's
/// `SEND_TIMEOUT_VIDEO=12s` (MYS-886). Shared by both downlink transports so
/// HTTP and WS cannot drift apart.
const DOWNLINK_IDLE_TIMEOUT: Duration = Duration::from_secs(12);

/// Next frame for a viewer, or `None` when the stream is over: the fan-out
/// dropped the viewer, or nothing arrived within `budget`. Both downlink
/// transports go through this so the idle policy is one place, not two.
async fn next_frame_or_idle(
    rx: &mut mpsc::Receiver<Vec<u8>>,
    budget: Duration,
) -> Option<Vec<u8>> {
    match tokio::time::timeout(budget, rx.recv()).await {
        Ok(Some(chunk)) => Some(chunk),
        _ => None,
    }
}

/// Viewer cleanup on disconnect (caller owns the id; remove on stream end).
pub struct ViewerGuard {
    pub stream: DesktopStream,
    pub id: String,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        let st = self.stream.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            st.remove_viewer(&id).await;
        });
    }
}

/// WS 下行推流端点：`GET /agent/desktop/ws?token=<session token>`。
///
/// 鉴权与 `/agent/desktop/stream` 相同（Authorization: Bearer 或 ?token=）。
/// 与 HTTP stream 共用同一 `DesktopStream` fan-out。srtc 由 agent 在
/// 捕获后打点并校准到 relay 时基（agent 连上后对 /api/clock 采样偏移），
/// 因此无论 HTTP 还是 WS 下行，浏览器拿到的 srtc 都在 relay 时间轴上。
///
/// 帧格式：binary frame = 原始 fMP4 字节（init/frag），与 HTTP 流逐块前进
/// 完全等价；浏览器用同一 demux 解析。`clk` 文本帧（浏览器发起）回复中
/// 不消费视频字节。
pub async fn ws_downlink_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    if !crate::relay::auth::server_password_ok(&state, &headers, params.get("auth")).await {
        return crate::relay::auth::invalid_password_response();
    }
    let token =
        match crate::relay::auth::extract_token_from_headers_or_query(&headers, params.get("token")) {
            Some(t) => t,
            None => {
                return (StatusCode::UNAUTHORIZED, "Missing token").into_response();
            }
        };
    let (session_id, _perm) = match state.sessions.authenticate(&token).await {
        Some(r) => r,
        None => return (StatusCode::UNAUTHORIZED, "Invalid token").into_response(),
    };
    let ds = {
        let streams = state.desktop_streams.read().await;
        streams.get(&session_id).cloned()
    };
    let ds = match ds {
        Some(ds) => ds,
        None => {
            return (
                StatusCode::NOT_FOUND,
                "desktop stream is not active; start it first (desktop:start)",
            )
                .into_response();
        }
    };
    ws.on_upgrade(move |socket| ws_downlink_loop(ds, session_id, socket, DOWNLINK_IDLE_TIMEOUT))
}

async fn ws_downlink_loop(
    ds: DesktopStream,
    _session_id: String,
    mut socket: axum::extract::ws::WebSocket,
    idle_timeout: Duration,
) {
    use axum::extract::ws::Message;
    // add_viewer() 只等第一个 init；先拿到 init 缓存（已存在则立即返回,
    // 否则等至多 10s），再开始推送 —— 首帧必须是 init。
    let (vid, mut rx, init) = ds.add_viewer().await;
    let guard = ViewerGuard { stream: ds.clone(), id: vid };
    // 若 init 尚未到：先尝试等待，超时未到则关闭连接（浏览器会重连）。
    if init.is_none() {
        let got = ds.wait_first_init(std::time::Duration::from_secs(10)).await;
        if got.is_none() {
            drop(guard);
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: "init timeout".into(),
                })))
                .await;
            return;
        }
    }
    let init = if init.is_some() { init } else { ds.inner.init.read().await.clone() };
    let Some(init) = init else { drop(guard); return; };

    if socket.send(Message::Binary(init.into())).await.is_err() {
        drop(guard);
        return;
    }
    loop {
        tokio::select! {
            // 浏览器侧文本帧：ping/校准请求，发 ping/rst?——使用标准 Pong
        // 由浏览器协议层处理；任何帧在此都不中断视频流。
        // 关键：浏览器断开时 recv() 返回 Close/None/Err——必须 break，
        // 否则该分支每次 select 都立即就绪，`rx.recv()` 永无机会执行，
        // 形成单核自旋死锁（实测 WS viewer 收完帧断开后 relay http 全卡，
        // 线程全 futex_wait、一个线程 R 自旋）。
        msg = socket.recv() => {
            match msg {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Ping(p))) => {
                    if socket.send(Message::Pong(p)).await.is_err() { break; }
                }
                Some(Ok(_)) => { /* Text/Binary 控制帧：忽略 */ }
                Some(Err(_)) => break,
            }
        }
            chunk = next_frame_or_idle(&mut rx, idle_timeout) => {
                match chunk {
                    Some(c) => {
                        if socket.send(Message::Binary(c.into())).await.is_err() {
                            break;
                        }
                    }
                    // Fan-out dropped the viewer, or no frame within the idle
                    // budget — same verdict as the HTTP stream, so a dead agent
                    // cannot leave this socket pinned open.
                    None => break,
                }
            }
        }
    }
    drop(guard);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_init_cached_and_replayed() {
        let st = DesktopStream::new();
        let (_vid, mut rx, init) = st.add_viewer().await;
        assert!(init.is_none());
        st.set_init(vec![1, 2, 3]).await;
        st.push_frag(true, vec![4, 5]).await; // key
        st.push_frag(false, vec![6]).await; // p
        assert_eq!(rx.recv().await.unwrap(), vec![1, 2, 3]);
        assert_eq!(rx.recv().await.unwrap(), vec![4, 5]);
        assert_eq!(rx.recv().await.unwrap(), vec![6]);
    }

    #[tokio::test]
    async fn test_push_frag_reports_congested_drops() {
        // R5#16：viewer 缓冲满（丢旧保新）时 push_frag 返回被跳过 viewer 数，
        // relay 据此限频回传 agent 拥塞信号。
        let st = DesktopStream::new();
        let (_vid, mut rx, _init) = st.add_viewer().await;
        // 填满 16 帧缓冲（channel cap=16）：第 17 帧起 try_send 失败被丢旧。
        for i in 0..17u8 {
            st.push_frag(true, vec![i]).await;
        }
        let congested = st.push_frag(true, vec![99]).await;
        assert!(congested >= 1, "viewer 缓冲满后应报告 drop，got {congested}");
        // 消费一帧腾出空位后，下一帧恢复投递（drop 计数归零）
        assert_eq!(rx.recv().await.unwrap(), vec![0]);
        let congested2 = st.push_frag(true, vec![100]).await;
        assert_eq!(congested2, 0, "缓冲有空位后不再 drop");
    }

    #[tokio::test]
    async fn test_new_viewer_gets_init_then_gated_on_key() {
        let st = DesktopStream::new();
        st.set_init(vec![9, 9]).await;
        let (_vid, mut rx, init) = st.add_viewer().await;
        assert_eq!(init, Some(vec![9, 9]));

        // 关键帧到达前, 非关键帧被丢弃
        st.push_frag(false, vec![1]).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "non-key frag must be gated");

        // 关键帧到达后开始放行（init 通过返回值下发, channel 只有后续帧）
        st.push_frag(true, vec![2]).await;
        st.push_frag(false, vec![3]).await;
        assert_eq!(rx.recv().await.unwrap(), vec![2]);
        assert_eq!(rx.recv().await.unwrap(), vec![3]);
    }

    #[tokio::test]
    async fn test_dead_viewer_cleaned_on_broadcast() {
        // 接收端已 drop（channel Closed）= viewer 确定已离开，立即移除；
        // 仅"缓冲满"才走 drop_count 容忍（见 test_burst_backpressure_*）。
        let st = DesktopStream::new();
        let (_vid, rx, _) = st.add_viewer().await;
        drop(rx);
        st.push_frag(true, vec![1]).await;
        assert_eq!(
            st.inner.viewers.read().await.len(),
            0,
            "closed viewer must be evicted on the next push"
        );
    }

    #[tokio::test]
    async fn test_full_viewer_evicted_after_max_drops() {
        // 缓冲长期满（viewer 卡死但连接未断）：跳过的 delta 也计入 drop_count，
        // 超过 MAX_CONSECUTIVE_DROPS 后移除。
        let st = DesktopStream::new();
        let (_vid, _rx, _) = st.add_viewer().await;
        st.push_frag(true, vec![0]).await;
        for i in 0..(MAX_CONSECUTIVE_DROPS + 20) {
            st.push_frag(false, vec![i as u8]).await;
        }
        assert_eq!(
            st.inner.viewers.read().await.len(),
            0,
            "viewer stuck beyond MAX_CONSECUTIVE_DROPS must be evicted"
        );
    }

    #[tokio::test]
    async fn test_dropped_frame_regates_viewer_until_key() {
        // 丢过一帧后，后续 delta 引用链已断（继续投递会花屏）：该 viewer
        // 重新等待关键帧，关键帧之后恢复放行。
        let st = DesktopStream::new();
        let (_vid, mut rx, _) = st.add_viewer().await;
        st.push_frag(true, vec![0]).await;
        for i in 1..=16u8 {
            st.push_frag(false, vec![i]).await; // 1..15 入队，16 被丢
        }
        for _ in 0..16 {
            rx.recv().await.unwrap(); // 排空
        }
        st.push_frag(false, vec![17]).await;
        assert!(rx.try_recv().is_err(), "delta after a drop must be gated");
        st.push_frag(true, vec![18]).await;
        st.push_frag(false, vec![19]).await;
        assert_eq!(rx.recv().await.unwrap(), vec![18]);
        assert_eq!(rx.recv().await.unwrap(), vec![19]);
    }

    #[tokio::test]
    async fn test_burst_backpressure_does_not_evict_viewer() {
        // 瞬时积压（一次多帧 push_frag 塞满 channel）不应把 viewer 踢出——
        // 这正是"满即踢→反复重连→闪烁"的旧行为回归点。满后 viewer 仍在，
        // 稍后消费（recv）恢复投递。
        let st = DesktopStream::new();
        let (vid, mut rx, _) = st.add_viewer().await;
        // 突发塞满 16 帧 + 额外几帧（全部 try_send 失败也不踢）。
        for i in 0..30u8 {
            st.push_frag(true, vec![i]).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            st.inner.viewers.read().await.len(),
            1,
            "burst of 30 fragments must not evict a live viewer"
        );
        // 恢复消费后还能收到帧（丢旧保新：收到的是最后能塞进去的帧）。
        assert!(rx.recv().await.is_some(), "live viewer resumes receiving");
        st.remove_viewer(&vid).await;
    }

    #[tokio::test]
    async fn test_bandwidth_stats_track_forwarded_bytes() {
        // #152 带宽记账：成功投递的帧计入 stats，丢帧（满缓冲）不计。
        let st = DesktopStream::new();
        let (_vid, mut rx, _) = st.add_viewer().await;
        st.set_init(vec![1, 2, 3]).await; // init 广播投递（也计入 bytes——它是转发字节）
        assert_eq!(rx.recv().await.unwrap().len(), 3); // init 先到
        // 投递 3 帧（key+2 delta），各帧字节数可累加。
        st.push_frag(true, vec![0xaa; 10]).await;
        st.push_frag(false, vec![0xbb; 20]).await;
        st.push_frag(false, vec![0xcc; 30]).await;
        // 消费掉，确保全部真正投递（channel 未满）。
        assert_eq!(rx.recv().await.unwrap().len(), 10);
        assert_eq!(rx.recv().await.unwrap().len(), 20);
        assert_eq!(rx.recv().await.unwrap().len(), 30);
        let stats = st.stats().await;
        assert_eq!(stats.viewer_count, 1);
        // init(3) + 3 帧(10+20+30) = 63 字节——init 也是 relay 转发的字节。
        assert_eq!(stats.forwarded_bytes, 3 + 60, "init(3) + 3 帧(10+20+30)");
        assert_eq!(stats.forwarded_frames, 3);
    }

    // ── stream_handler integration ─────────────────────────────

    use axum::http::HeaderMap;

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            format!("Bearer {}", token).parse().unwrap(),
        );
        h
    }

    #[tokio::test]
    async fn test_ws_uplink_routes_desktop_video() {
        // WS 上行（v0.21）: 真实 axum server + tungstenite 客户端走完整的
        // upgrade → text frame → route_agent_message 路径, 验证 batch 数组
        // 也被正确路由到桌面 fan-out。
        let state = Arc::new(crate::relay::SharedState::new(
            "".into(), 100 * 1024 * 1024, None, String::new(), String::new(), None,
        ));
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();

        // 预注册 channel map（模拟 agent 已注册）。
        state
            .agent_broadcast
            .write()
            .await
            .insert(sid.clone(), crate::relay::ChannelMap::new());

        let app: axum::Router = axum::Router::new()
            .route(
                "/agent/ws/send",
                axum::routing::get(super::super::ws::agent_ws_send_handler),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move { axum::serve(listener, app).await });

        // 客户端：tungstenite 直接连。
        use futures_util::SinkExt;
        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/agent/ws/send?session={sid}"
        ))
        .await
        .expect("ws connect");
        let batch = serde_json::json!([{
            "type": "desktop:started",
            "session_id": sid,
            "payload": { "codec": "h264" }
        }]);
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            batch.to_string(),
        ))
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let streams = state.desktop_streams.read().await;
        assert!(
            streams.contains_key(&sid),
            "WS uplink text frame must reach route_agent_message (pre-created stream)"
        );
    }

    #[tokio::test]
    async fn test_stream_handler_requires_auth() {
        let state = Arc::new(crate::relay::SharedState::new(
            "".into(), 100 * 1024 * 1024, None, String::new(), String::new(), None,
        ));
        let resp = stream_handler(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_stream_handler_requires_server_password() {
        // A valid session token is not enough when --auth is set: the server
        // password (X-Auth header) is checked first and reported distinctly.
        let state = Arc::new(crate::relay::SharedState::new(
            "srv-pw".into(), 100 * 1024 * 1024, None, String::new(), String::new(), None,
        ));
        state.sessions.register(Some("dk".into()), "both", Some("dsess".into())).await.unwrap();
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer dk".parse().unwrap());
        let resp = stream_handler(State(state.clone()), h.clone(), Query(HashMap::new())).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("AUTH_INVALID_PASSWORD"));
        // Correct password passes the gate (then 404: no desktop stream yet).
        h.insert("x-auth", "srv-pw".parse().unwrap());
        let resp = stream_handler(State(state), h, Query(HashMap::new())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_stream_handler_valid_token_streams_init_then_frags() {
        let state = Arc::new(crate::relay::SharedState::new(
            "".into(), 100 * 1024 * 1024, None, String::new(), String::new(), None,
        ));
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let token = r.tokens[0].0.clone();

        let ds = DesktopStream::new();
        ds.set_init(vec![0x1, 0x2, 0x3]).await;
        state.desktop_streams.write().await.insert(sid.clone(), ds.clone());

        let resp = stream_handler(
            State(state),
            bearer_headers(&token),
            Query(HashMap::new()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        use tokio_stream::StreamExt as _;
        let mut body = resp.into_body().into_data_stream();
        let first = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            body.next(),
        )
        .await
        .expect("init chunk")
        .unwrap()
        .unwrap()
        .to_vec();
        assert_eq!(first, vec![0x1, 0x2, 0x3]);

        ds.push_frag(true, vec![0xaa, 0xbb]).await;
        let second = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            body.next(),
        )
        .await
        .expect("key frag chunk")
        .unwrap()
        .unwrap()
        .to_vec();
        assert_eq!(second, vec![0xaa, 0xbb]);
    }

    #[tokio::test]
    async fn test_stream_handler_waits_for_first_init() {
        // 竞态回归: viewer 在 desktop:started 预建流后、首个 init 到达前
        // 加入。首块必须是 init(否则浏览器 MSE 收到 fragment 开头 →
        // CHUNK_DEMUXER_ERROR_APPEND_FAILED)。等在 5s 内 init 到来应作为
        // 第一块被发出, 而非任何其它字节。
        let state = Arc::new(crate::relay::SharedState::new(
            "".into(), 100 * 1024 * 1024, None, String::new(), String::new(), None,
        ));
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let token = r.tokens[0].0.clone();

        let ds = DesktopStream::new(); // 尚未 set_init
        state.desktop_streams.write().await.insert(sid.clone(), ds.clone());

        let resp = stream_handler(
            State(state),
            bearer_headers(&token),
            Query(HashMap::new()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        use tokio_stream::StreamExt as _;
        let mut body = resp.into_body().into_data_stream();

        // 先推一个 key 分片: 若 handler 未等 init, 它会被当成首块。
        ds.push_frag(true, vec![0xbb, 0xbb]).await;

        // 然后 init 才到达 → 首块必须是 init
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        ds.set_init(vec![0x1, 0x2, 0x3]).await;

        let first = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            body.next(),
        )
        .await
        .expect("first block must be the init")
        .unwrap()
        .unwrap()
        .to_vec();
        assert_eq!(first, vec![0x1, 0x2, 0x3], "first block must be init, got frag");
    }

    #[tokio::test]
    async fn test_stream_handler_inactive_stream_404() {
        let state = Arc::new(crate::relay::SharedState::new(
            "".into(), 100 * 1024 * 1024, None, String::new(), String::new(), None,
        ));
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let token = r.tokens[0].0.clone();
        let resp = stream_handler(
            State(state),
            bearer_headers(&token),
            Query(HashMap::new()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── B4: downlink idle timeout ───────────────────────────────

    #[tokio::test]
    async fn test_next_frame_or_idle_times_out_on_silence() {
        // The idle policy both transports share: no frame within the budget
        // ends the viewer. Uses a short budget so the test stays fast.
        let (_tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        let started = std::time::Instant::now();
        let got = next_frame_or_idle(&mut rx, Duration::from_millis(120)).await;
        assert!(got.is_none(), "silent viewer must be reported as ended");
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "must actually wait out the budget, elapsed {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn test_next_frame_or_idle_passes_frames_through() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        tx.send(vec![1, 2, 3]).await.unwrap();
        let got = next_frame_or_idle(&mut rx, Duration::from_secs(5)).await;
        assert_eq!(got, Some(vec![1, 2, 3]));
        // A closed channel ends the viewer immediately, without waiting.
        drop(tx);
        let started = std::time::Instant::now();
        assert!(next_frame_or_idle(&mut rx, Duration::from_secs(5))
            .await
            .is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_ws_downlink_closes_on_idle_and_removes_viewer() {
        // Regression: the WS downlink used to `select!` on a bare `rx.recv()`,
        // so an agent that stopped pushing left the socket open forever (the
        // HTTP stream had a 12s idle cut-off; WS did not). Exercise the real
        // loop directly with a short budget so the test stays fast.
        let state = Arc::new(crate::relay::SharedState::new(
            "".into(), 100 * 1024 * 1024, None, String::new(), String::new(), None,
        ));
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let token = r.tokens[0].0.clone();

        let ds = DesktopStream::new();
        ds.set_init(vec![0xa1, 0xa2]).await; // init so the loop starts
        state.desktop_streams.write().await.insert(sid.clone(), ds.clone());

        // A real upgrade through the router, so the socket is a real one.
        let app: axum::Router = axum::Router::new()
            .route(
                "/agent/desktop/ws",
                axum::routing::get(
                    move |State(st): State<Arc<crate::relay::SharedState>>,
                          Query(p): Query<HashMap<String, String>>,
                          ws: axum::extract::ws::WebSocketUpgrade| async move {
                        let stream = st.desktop_streams.read().await.get(&sid).cloned().unwrap();
                        ws.on_upgrade(move |socket| {
                            ws_downlink_loop(stream, String::new(), socket, Duration::from_millis(150))
                        })
                    },
                ),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move { axum::serve(listener, app).await });

        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/agent/desktop/ws?token={token}"
        ))
        .await
        .expect("ws connect");

        // First frame is the cached init.
        let first = tokio::time::timeout(
            Duration::from_secs(3),
            futures_util::StreamExt::next(&mut ws),
        )
        .await
        .expect("init frame")
        .expect("frame")
        .expect("ok frame");
        assert_eq!(first.into_data().to_vec(), vec![0xa1, 0xa2]);

        // The agent pushes nothing more: the loop must close the socket once
        // its idle budget expires instead of holding it open forever.
        let closed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match futures_util::StreamExt::next(&mut ws).await {
                    None => return true,
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => return true,
                    Some(Err(_)) => return true,
                    Some(Ok(_)) => continue,
                }
            }
        })
        .await;
        assert!(
            closed.expect("WS downlink must close after the idle budget"),
            "idle WS downlink must close the socket"
        );
        // ViewerGuard removes the viewer on the way out.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            ds.stats().await.viewer_count,
            0,
            "closing an idle WS viewer must release its fan-out slot"
        );
    }

    #[tokio::test]
    async fn test_ws_downlink_forwards_frames_before_idle_timeout() {
        // A live stream must be unaffected by the idle budget: frames keep
        // arriving and the socket stays open.
        let _state = Arc::new(crate::relay::SharedState::new(
            "".into(), 100 * 1024 * 1024, None, String::new(), String::new(), None,
        ));
        let ds = DesktopStream::new();
        ds.set_init(vec![0x01]).await;
        let app: axum::Router = axum::Router::new()
            .route(
                "/agent/desktop/ws",
                axum::routing::get({
                    let ds = ds.clone();
                    move |ws: axum::extract::ws::WebSocketUpgrade| async move {
                        let ds = ds.clone();
                        ws.on_upgrade(move |socket| {
                            ws_downlink_loop(ds, String::new(), socket, Duration::from_millis(150))
                        })
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move { axum::serve(listener, app).await });

        let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/agent/desktop/ws"))
            .await
            .expect("ws connect");
        let first = tokio::time::timeout(
            Duration::from_secs(3),
            futures_util::StreamExt::next(&mut ws),
        )
        .await
        .expect("init frame")
        .expect("frame")
        .expect("ok frame");
        assert_eq!(first.into_data().to_vec(), vec![0x01]);

        // Push inside the budget: the frame must be delivered, not dropped.
        ds.push_frag(true, vec![0x77]).await;
        let second = tokio::time::timeout(
            Duration::from_secs(3),
            futures_util::StreamExt::next(&mut ws),
        )
        .await
        .expect("frag frame")
        .expect("frame")
        .expect("ok frame");
        assert_eq!(second.into_data().to_vec(), vec![0x77]);
    }

    // ── 带宽限速丢帧（MYS-2287 验收 3.1）────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn test_push_frag_drops_on_bandwidth_without_kicking_viewer() {
        // 下行限到 1KB/s（8kbps）：burst = 64KiB。首帧把额度吃光，后续帧
        // 只能丢——但必须满足：drop_count 不增长（带宽丢帧不是 viewer 慢）、
        // viewer 不被移除、congested 计数 >0、限速丢帧独立计数。
        let limiter = Arc::new(crate::relay::bandwidth::BandwidthLimiter::with_mbps(
            0.008, // 8 kbit/s = 1 KB/s
            0.0,
        ));
        let ds = DesktopStream::with_limiter(Some(limiter.clone()));
        let (_vid, mut rx, _init) = ds.add_viewer().await;

        // 首帧：64KiB 突发额度足够，正常送达。
        let first = vec![0xAAu8; 20 * 1024];
        assert_eq!(ds.push_frag(true, first.clone()).await, 0);
        assert_eq!(rx.recv().await.unwrap(), first);

        // 之后连推 10 帧，额度耗尽 → 全部因带宽被丢。
        let mut congested_total = 0u32;
        for _ in 0..10 {
            congested_total += ds.push_frag(true, vec![0xBB; 20 * 1024]).await;
        }
        assert!(
            congested_total > 0,
            "带宽不足时应报告 congested > 0，实际 {congested_total}"
        );
        // burst = 64KiB：首帧 20KiB 后剩 44KiB，正好够再放 2 帧，其余 8 帧
        // 全部因带宽被丢弃（虚拟时间不前进，所以这个数是确定的）。
        assert_eq!(
            limiter.dropped_frames(),
            8,
            "额度耗尽后的 8 帧应因带宽丢弃，且独立计数（与 viewer drop_count 分开）"
        );
        // 关键断言：viewer 仍在 fan-out 里，drop_count 未增长。
        assert_eq!(
            ds.stats().await.viewer_count,
            1,
            "带宽丢帧绝不能把 viewer 踢出 fan-out"
        );
        {
            let viewers = ds.inner.viewers.read().await;
            let ctx = viewers.values().next().expect("viewer must still exist");
            assert_eq!(ctx.drop_count, 0, "带宽丢帧不得累加 drop_count");
            assert!(!ctx.key_ok, "丢帧后必须回到等关键帧状态");
        }
        // burst = 64KiB、单帧 20KiB：首帧后还剩 44KiB，正好够再放 2 帧，
        // 之后 8 帧全部被限速丢弃 → 队列里只应多出这 2 帧。
        assert!(rx.try_recv().is_ok(), "burst 额度内的帧应正常送达");
        assert!(rx.try_recv().is_ok(), "burst 额度内的帧应正常送达");
        assert!(
            rx.try_recv().is_err(),
            "额度耗尽后的帧应被限速丢弃，不进 viewer 队列"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_push_frag_keyframe_also_bounded_by_bandwidth() {
        // 设计明确：关键帧同样受限（限速不能因为"这是 IDR"就放行，否则
        // 一个 IDR 就击穿速率上限）。
        let limiter = Arc::new(crate::relay::bandwidth::BandwidthLimiter::with_mbps(0.008, 0.0));
        let ds = DesktopStream::with_limiter(Some(limiter.clone()));
        let (_vid, _rx, _init) = ds.add_viewer().await;
        for _ in 0..20 {
            ds.push_frag(true, vec![0xCC; 16 * 1024]).await;
        }
        assert!(
            limiter.dropped_frames() > 0,
            "关键帧也必须受限，实际丢帧 {}",
            limiter.dropped_frames()
        );
        assert_eq!(ds.stats().await.viewer_count, 1, "viewer 不得被移除");
    }

    #[tokio::test(start_paused = true)]
    async fn test_set_init_only_accounts_never_drops() {
        // init 段是解码前提，绝不能丢：即使额度早已透支，也必须送达并只记账。
        let limiter = Arc::new(crate::relay::bandwidth::BandwidthLimiter::with_mbps(0.008, 0.0));
        let ds = DesktopStream::with_limiter(Some(limiter.clone()));
        let (_vid, mut rx, _init) = ds.add_viewer().await;
        limiter.down.consume(1024 * 1024); // 先把额度打穿
        let init = vec![0x11u8; 4096];
        ds.set_init(init.clone()).await;
        assert_eq!(
            rx.recv().await.unwrap(),
            init,
            "init 段必须送达（丢了解码器就建不起来）"
        );
        assert_eq!(limiter.dropped_frames(), 0, "init 不丢帧");
        assert_eq!(ds.stats().await.viewer_count, 1);
    }

    #[tokio::test]
    async fn test_invalidate_keys_forces_wait_for_next_idr() {
        // 上行丢帧后 relay 调用 invalidate_keys()：所有 viewer 回到等关键帧，
        // 后续 delta 被跳过，直到下一个 IDR 恢复。
        let ds = DesktopStream::new();
        let (_vid, mut rx, _init) = ds.add_viewer().await;
        ds.push_frag(true, vec![1]).await; // key：viewer 进入可解码状态
        assert_eq!(rx.recv().await.unwrap(), vec![1]);
        assert_eq!(ds.invalidate_keys().await, 1, "应返回受影响的 viewer 数");
        ds.push_frag(false, vec![2]).await; // delta：必须被跳过
        assert!(
            rx.try_recv().is_err(),
            "invalidate 后的 delta 帧不得下发（会花屏）"
        );
        ds.push_frag(true, vec![3]).await; // 下一个 IDR：恢复
        assert_eq!(rx.recv().await.unwrap(), vec![3]);
    }
}