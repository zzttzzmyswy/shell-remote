#![allow(unused_imports)]

pub mod auth;
pub mod mcp;
pub mod session;
pub mod ws;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot, RwLock};

#[allow(dead_code)]
use crate::relay::session::SessionRegistry;

#[allow(dead_code)]
pub struct ChannelMap {
    pub agent: Option<mpsc::UnboundedSender<String>>,
    pub browsers: HashMap<String, mpsc::UnboundedSender<String>>,
}

#[allow(dead_code)]
impl ChannelMap {
    pub fn new() -> Self {
        Self { agent: None, browsers: HashMap::new() }
    }
}

#[allow(dead_code)]
pub struct SharedState {
    pub sessions: SessionRegistry,
    pub agent_broadcast: RwLock<HashMap<String, ChannelMap>>,
    pub pending_mcp: RwLock<HashMap<String, (String, oneshot::Sender<String>)>>,
    pub last_activity: RwLock<HashMap<String, Instant>>,
    pub server_auth: String,
}

impl SharedState {
    pub fn new(server_auth: String) -> Self {
        Self {
            sessions: SessionRegistry::new(),
            agent_broadcast: RwLock::new(HashMap::new()),
            pending_mcp: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
            server_auth,
        }
    }
}

use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::Response;
use futures_util::StreamExt;

async fn static_handler(uri: Uri) -> Response<Body> {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    let content = crate::web::WebAssets::get(path);
    let (resolved, data) = if content.is_some() {
        (path.to_string(), content)
    } else {
        let html = format!("{}.html", path);
        let c = crate::web::WebAssets::get(&html);
        (if c.is_some() { html } else { path.to_string() }, c)
    };

    match data {
        Some(content) => {
            let mime = mime_guess::from_path(&resolved).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data.into_owned()))
                .unwrap()
        }
        None => {
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Not Found"))
                .unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_static_handler_root_serves_index() {
        let uri = "/".parse::<Uri>().unwrap();
        let resp = static_handler(uri).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        assert!(body.len() > 0);
        assert!(std::str::from_utf8(&body).unwrap().contains("ssh-remote"));
    }

    #[tokio::test]
    async fn test_static_handler_session_without_extension() {
        let uri = "/session".parse::<Uri>().unwrap();
        let resp = static_handler(uri).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(content_type.starts_with("text/html"), "Expected text/html, got {}", content_type);
    }

    #[tokio::test]
    async fn test_static_handler_session_js() {
        let uri = "/session.js".parse::<Uri>().unwrap();
        let resp = static_handler(uri).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_static_handler_css() {
        let uri = "/style.css".parse::<Uri>().unwrap();
        let resp = static_handler(uri).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_static_handler_not_found() {
        let uri = "/nonexistent".parse::<Uri>().unwrap();
        let resp = static_handler(uri).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_all_web_assets_accessible() {
        let assets = [
            "index.html",
            "session.html",
            "style.css",
            "ws.js",
            "term.js",
            "files.js",
            "session.js",
        ];
        for name in assets {
            let content = crate::web::WebAssets::get(name);
            assert!(content.is_some(), "Asset not found: {}", name);
        }
    }

    #[tokio::test]
    async fn test_static_handler_session_html_direct() {
        let uri = "/session.html".parse::<Uri>().unwrap();
        let resp = static_handler(uri).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_relay_router_builds_without_error() {
        use axum::routing::get;
        use axum::Router;
        use tower_http::cors::{Any, CorsLayer};

        let state = Arc::new(SharedState::new("test".into()));
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        let app: Router = Router::new()
            .route("/ws", get(super::ws::ws_handler))
            .route("/mcp/sse", get(super::mcp::sse_handler))
            .route("/mcp/messages", axum::routing::post(super::mcp::messages_handler))
            .route("/", get(static_handler))
            .route("/session", get(static_handler))
            .route("/style.css", get(static_handler))
            .route("/ws.js", get(static_handler))
            .route("/term.js", get(static_handler))
            .route("/files.js", get(static_handler))
            .route("/session.js", get(static_handler))
            .fallback(get(static_handler))
            .layer(cors)
            .with_state(state);

        let _ = app;
    }
}

pub async fn upload_handler(
    State(state): State<Arc<SharedState>>,
    Query(params): Query<HashMap<String, String>>,
    body: axum::body::Body,
) -> Result<axum::http::StatusCode, axum::http::StatusCode> {
    let token = params.get("token").ok_or(StatusCode::UNAUTHORIZED)?;
    let path = params.get("path").ok_or(StatusCode::BAD_REQUEST)?;

    let (session_id, permission) = state.sessions.authenticate(token)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;

    use crate::proto::Permission;
    if permission == Permission::ReadOnly {
        return Err(StatusCode::FORBIDDEN);
    }

    let tmp_dir = std::path::PathBuf::from("/tmp/opencode/uploads");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let tmp_name = format!("{}_{}", uuid::Uuid::new_v4(), path.replace('/', "_"));
    let tmp_path = tmp_dir.join(&tmp_name);

    let mut file = std::fs::File::create(&tmp_path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut total: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(result) = stream.next().await {
        let chunk = result.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        total += chunk.len() as u64;
        std::io::Write::write_all(&mut file, &chunk).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    drop(file);

    let msg = serde_json::json!({
        "type": "fs:upload",
        "session_id": session_id,
        "payload": {
            "temp_path": tmp_path.to_string_lossy(),
            "final_path": path
        }
    }).to_string();

    {
        let broadcast = state.agent_broadcast.read().await;
        if let Some(channel_map) = broadcast.get(&session_id) {
            if let Some(agent_tx) = &channel_map.agent {
                let _ = agent_tx.send(msg);
            }
        }
    }

    tracing::info!("Upload received: {} ({} bytes) -> {}", path, total, tmp_path.display());
    Ok(StatusCode::OK)
}

pub async fn start(
    bind: String,
    _tls_cert: Option<String>,
    _tls_key: Option<String>,
    _dev: bool,
    server_auth: String,
) -> anyhow::Result<()> {
    use axum::routing::get;
    use axum::Router;
    use tower_http::cors::{Any, CorsLayer};

    let state = Arc::new(SharedState::new(server_auth));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/ws", get(ws::ws_handler))
        .route("/upload", axum::routing::post(upload_handler).layer(axum::extract::DefaultBodyLimit::disable()))
        .route("/mcp/sse", get(mcp::sse_handler))
        .route("/mcp/messages", axum::routing::post(mcp::messages_handler))
        .route("/", get(static_handler))
        .route("/session", get(static_handler))
        .route("/style.css", get(static_handler))
        .route("/ws.js", get(static_handler))
        .route("/term.js", get(static_handler))
        .route("/files.js", get(static_handler))
        .route("/session.js", get(static_handler))
        .fallback(get(static_handler))
        .layer(cors)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("Relay server listening on {}", bind);

    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
                let now = Instant::now();
                let mut to_remove = Vec::new();
                {
                    let activity = state_clone.last_activity.read().await;
                    for (session_id, last) in activity.iter() {
                        if now.duration_since(*last) > tokio::time::Duration::from_secs(1800) {
                            to_remove.push(session_id.clone());
                        }
                    }
                }
                for session_id in to_remove {
                    if !state_clone.sessions.is_temporary(&session_id).await {
                        continue;
                    }
                    tracing::info!("Idle timeout: removing session {}", session_id);
                    {
                        let mut pending = state_clone.pending_mcp.write().await;
                        pending.retain(|_rid, (sid, _tx)| sid != &session_id);
                    }
                    state_clone.sessions.remove(&session_id).await;
                    state_clone.agent_broadcast.write().await.remove(&session_id);
                    state_clone.last_activity.write().await.remove(&session_id);
                }
            }
        });
    }

    axum::serve(listener, app).await?;

    Ok(())
}
