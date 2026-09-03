//! Relay-side desktop video fan-out.
//!
//! The agent POSTs `desktop:video` (kind `init` | `frag`, base64 fMP4 bytes).
//! This module keeps one [`DesktopStream`] per session: an `init` byte cache
//! (replayed to late joiners) plus the set of currently connected browsers
//! waiting on `GET /agent/desktop/stream`. Each viewer gets the init bytes
//! first, then every fragment appended afterwards — exactly the byte layout
//! browser MSE needs.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::relay::SharedState;
use axum::body::Bytes;

/// Shared, cloneable fan-out state for one session's desktop stream.
#[derive(Clone)]
pub struct DesktopStream {
    inner: Arc<Inner>,
}

struct Inner {
    /// Latest fMP4 init segment (ftyp+moov). New viewers are replayed this
    /// before any fragments.
    init: tokio::sync::RwLock<Option<Vec<u8>>>,
    /// connected viewers, keyed by a per-connection id.
    viewers: tokio::sync::RwLock<HashMap<String, mpsc::Sender<Vec<u8>>>>,
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
        self.broadcast(bytes).await;
    }

    /// Forward one media fragment to all viewers.
    pub async fn push_frag(&self, bytes: Vec<u8>) {
        self.broadcast(bytes).await;
    }

    async fn broadcast(&self, bytes: Vec<u8>) {
        let viewers = self.inner.viewers.read().await;
        let mut dead = Vec::new();
        for (id, tx) in viewers.iter() {
            if tx.try_send(bytes.clone()).is_err() {
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
    pub async fn add_viewer(&self) -> (String, mpsc::Receiver<Vec<u8>>, Option<Vec<u8>>) {
        let id = format!("dv_{}", uuid::Uuid::new_v4().simple());
        let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
        self.inner.viewers.write().await.insert(id.clone(), tx);
        let init = self.inner.init.read().await.clone();
        (id, rx, init)
    }

    pub async fn remove_viewer(&self, id: &str) {
        self.inner.viewers.write().await.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_init_cached_and_replayed_to_existing_viewer() {
        let st = DesktopStream::new();
        let (_vid, mut rx, init) = st.add_viewer().await;
        assert!(init.is_none());
        st.set_init(vec![1, 2, 3]).await;
        st.push_frag(vec![4, 5]).await;
        assert_eq!(rx.recv().await.unwrap(), vec![1, 2, 3]);
        assert_eq!(rx.recv().await.unwrap(), vec![4, 5]);
    }

    #[tokio::test]
    async fn test_new_viewer_receives_cached_init_as_return_value() {
        // init 通过 add_viewer 的返回值下发（stream handler 先 yield init 再
        // 转发 channel 中的 fragments）；channel 只承载 init 之后的碎片。
        let st = DesktopStream::new();
        st.set_init(vec![9, 9]).await;
        let (_vid, mut rx, init) = st.add_viewer().await;
        assert_eq!(init, Some(vec![9, 9]));
        st.push_frag(vec![7]).await;
        assert_eq!(rx.recv().await.unwrap(), vec![7]);
    }

    #[tokio::test]
    async fn test_dead_viewer_cleaned_on_broadcast() {
        let st = DesktopStream::new();
        let (vid, rx, _) = st.add_viewer().await;
        drop(rx); // 消费端消失 → try_send 失败
        st.push_frag(vec![1]).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        st.remove_viewer(&vid).await;
        let viewers = st.inner.viewers.read().await.len();
        assert_eq!(viewers, 0);
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
        // 首个 chunk = 缓存的 init
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

        // 随后 agent push frag → 该 viewer 收到
        ds.push_frag(vec![0xaa, 0xbb]).await;
        let second = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            body.next(),
        )
        .await
        .expect("frag chunk")
        .unwrap()
        .unwrap()
        .to_vec();
        assert_eq!(second, vec![0xaa, 0xbb]);
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
// ── GET /agent/desktop/stream ─────────────────────────────────

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::body::Body;
use std::convert::Infallible;
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
    let token = match crate::relay::auth::extract_token_from_headers_or_query(&headers, params.get("token")) {
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
        if let Some(init) = init {
            yield Ok::<_, Infallible>(Bytes::from(init));
        }
        while let Some(chunk) = rx.recv().await {
            yield Ok::<_, Infallible>(Bytes::from(chunk));
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
