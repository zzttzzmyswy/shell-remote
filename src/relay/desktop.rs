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
}

struct Inner {
    /// Latest fMP4 init segment (ftyp+moov). New viewers are replayed this
    /// before any fragments.
    init: tokio::sync::RwLock<Option<Vec<u8>>>,
    /// connected viewers, keyed by a per-connection id.
    viewers: tokio::sync::RwLock<HashMap<String, ViewerCtx>>,
}

impl Default for DesktopStream {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopStream {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                init: tokio::sync::RwLock::new(None),
                viewers: tokio::sync::RwLock::new(HashMap::new()),
            }),
        }
    }

    /// Cache a fresh init segment and forward it to all connected viewers
    /// (a re-init after a codec/parameter-set change).
    pub async fn set_init(&self, bytes: Vec<u8>) {
        *self.inner.init.write().await = Some(bytes.clone());
        self.broadcast_to_viewers(bytes).await;
    }

    /// Forward one media fragment. `is_key` marks a random-access frame.
    pub async fn push_frag(&self, is_key: bool, bytes: Vec<u8>) {
        let mut viewers = self.inner.viewers.write().await;
        let mut dead = Vec::new();
        for (id, ctx) in viewers.iter_mut() {
            if !ctx.key_ok && !is_key {
                continue;
            }
            ctx.key_ok = true;
            if ctx.tx.try_send(bytes.clone()).is_err() {
                dead.push(id.clone());
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

    async fn broadcast_to_viewers(&self, bytes: Vec<u8>) {
        let viewers = self.inner.viewers.read().await;
        let mut dead = Vec::new();
        for (id, ctx) in viewers.iter() {
            if ctx.tx.try_send(bytes.clone()).is_err() {
                dead.push(id.clone());
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

    /// Register a new viewer. Returns `(viewer_id, receiver, cached_init)`.
    /// The receiver yields no fragments until the first key frame arrives.
    pub async fn add_viewer(&self) -> (String, mpsc::Receiver<Vec<u8>>, Option<Vec<u8>>) {
        let id = format!("dv_{}", uuid::Uuid::new_v4().simple());
        // 256 帧缓冲（30fps ≈ 8.5s; 单帧 ~2-10KB, 内存上限 ~2.5MB）。
// 之前 64 帧在浏览器解码短暂卡顿(标签页切后台)时 try_send 失败即把
// viewer 踢掉——表现成局域网"丢帧"/断流（MYS-886 问题6）。
let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
        self.inner
            .viewers
            .write()
            .await
            .insert(id.clone(), ViewerCtx { tx, key_ok: false });
        let init = self.inner.init.read().await.clone();
        (id, rx, init)
    }

    pub async fn remove_viewer(&self, id: &str) {
        self.inner.viewers.write().await.remove(id);
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
        // 空闲超时：健康流每秒都有帧（最差每 2s 一个 IDR），
        // 30s 无字节说明 agent 侧已停/崩溃、或上行中断，主动收尾
        // 让浏览器 fetch 结束、viewer 条目被清理，避免永久悬挂。
        loop {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Some(chunk)) => {
                    yield Ok::<_, Infallible>(Bytes::from(chunk));
                }
                _ => break,
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
        let st = DesktopStream::new();
        let (vid, rx, _) = st.add_viewer().await;
        drop(rx);
        st.push_frag(true, vec![1]).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        st.remove_viewer(&vid).await;
        assert_eq!(st.inner.viewers.read().await.len(), 0);
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
}