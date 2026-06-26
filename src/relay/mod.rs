#![allow(unused_imports)]

pub mod auth;
pub mod mcp;
pub mod session;
pub mod ws;
pub mod admin;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, RwLock};

#[allow(dead_code)]
use crate::relay::session::SessionRegistry;

#[allow(dead_code)]
pub struct ChannelMap {
    pub agent: Option<mpsc::UnboundedSender<String>>,
    pub browser_sessions: HashMap<String, String>,
}

#[allow(dead_code)]
impl ChannelMap {
    pub fn new() -> Self {
        Self {
            agent: None,
            browser_sessions: HashMap::new(),
        }
    }
}

#[allow(dead_code)]
pub struct SharedState {
    pub sessions: SessionRegistry,
    pub agent_broadcast: RwLock<HashMap<String, ChannelMap>>,
    pub pending_mcp: RwLock<HashMap<String, (String, oneshot::Sender<String>)>>,
    pub last_activity: RwLock<HashMap<String, Instant>>,
    /// Server access password (`--auth`). Wrapped in a RwLock so the admin
    /// panel can rotate it live; reads on the hot auth path take a read lock.
    pub server_auth: RwLock<String>,
    pub agent_event_buffers: RwLock<HashMap<String, EventBuffer>>,
    pub rate_limiter: RwLock<RateLimiter>,
    pub max_upload_size: u64,
    pub sse_sessions: RwLock<HashMap<String, mpsc::UnboundedSender<String>>>,
    /// Admin panel config. `admin_path` is `None` when `--admin-path` is
    /// unset, in which case no admin routes are registered.
    pub admin_path: Option<String>,
    pub admin_user: String,
    pub admin_pass: String,
    /// Admin login session tokens -> expiry Instant.
    pub admin_sessions: RwLock<HashMap<String, Instant>>,
    /// Relay process start time, for the admin uptime display.
    pub started_at: Instant,
}

pub struct RateLimiter {
    attempts: HashMap<String, Vec<Instant>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            attempts: HashMap::new(),
        }
    }

    /// Returns true if the request should be allowed (not rate limited)
    pub fn check(&mut self, key: &str, max_per_window: usize, window: Duration) -> bool {
        let now = Instant::now();
        let cutoff = now - window;
        let entry = self.attempts.entry(key.to_string()).or_default();
        entry.retain(|t| *t > cutoff);
        if entry.len() >= max_per_window {
            return false;
        }
        entry.push(now);
        true
    }
}

const MAX_EVENT_BUFFER: usize = 1000;

/// Hard cap on the total number of concurrent sessions a relay will accept.
/// Guards against unauthenticated `agent:register` flooding the registry and
/// event buffers with unlimited sessions.
pub const MAX_SESSIONS: usize = 1000;

#[derive(Clone)]
pub struct EventBuffer {
    next_id: u64,
    events: VecDeque<(u64, String)>,
}

impl EventBuffer {
    pub fn new() -> Self {
        Self {
            next_id: 0,
            events: VecDeque::new(),
        }
    }

    pub fn push(&mut self, msg: String) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.events.push_back((id, msg));
        if self.events.len() > MAX_EVENT_BUFFER {
            self.events.pop_front();
        }
        id
    }

    pub fn replay_from(&self, last_id: u64) -> Vec<(u64, String)> {
        self.events
            .iter()
            .filter(|(id, _)| *id > last_id)
            .cloned()
            .collect()
    }
}

impl SharedState {
    pub fn new(
        server_auth: String,
        max_upload_size: u64,
        admin_path: Option<String>,
        admin_user: String,
        admin_pass: String,
    ) -> Self {
        Self {
            sessions: SessionRegistry::new(),
            agent_broadcast: RwLock::new(HashMap::new()),
            pending_mcp: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
            server_auth: RwLock::new(server_auth),
            agent_event_buffers: RwLock::new(HashMap::new()),
            rate_limiter: RwLock::new(RateLimiter::new()),
            max_upload_size,
            sse_sessions: RwLock::new(HashMap::new()),
            admin_path,
            admin_user,
            admin_pass,
            admin_sessions: RwLock::new(HashMap::new()),
            started_at: Instant::now(),
        }
    }

    pub async fn buffer_agent_event(&self, session_id: &str, msg: &str) -> u64 {
        self.agent_event_buffers
            .write()
            .await
            .entry(session_id.to_string())
            .or_insert_with(EventBuffer::new)
            .push(msg.to_string())
    }
}

use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio_stream::StreamExt;

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
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, Request};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_static_handler_root_serves_index() {
        let uri = "/".parse::<Uri>().unwrap();
        let resp = static_handler(uri).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert!(body.len() > 0);
        assert!(std::str::from_utf8(&body).unwrap().contains("shell-remote"));
    }

    #[tokio::test]
    async fn test_static_handler_session_without_extension() {
        let uri = "/session".parse::<Uri>().unwrap();
        let resp = static_handler(uri).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            content_type.starts_with("text/html"),
            "Expected text/html, got {}",
            content_type
        );
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
            "term.js",
            "files.js",
            "session.js",
            "install.sh",
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

        let state = Arc::new(SharedState::new("test".into(), 100 * 1024 * 1024, None, String::new(), String::new()));
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        let app: Router = Router::new()
            .route("/agent/session/sse", get(super::ws::browser_sse_handler))
            .route(
                "/agent/session/send",
                axum::routing::post(super::ws::browser_send_handler),
            )
            .route("/mcp/sse", get(super::mcp::sse_handler))
            .route(
                "/mcp/messages",
                axum::routing::post(super::mcp::messages_handler),
            )
            .route("/", get(static_handler))
            .route("/session", get(static_handler))
            .route("/style.css", get(static_handler))
            .route("/sse.js", get(static_handler))
            .route("/term.js", get(static_handler))
            .route("/files.js", get(static_handler))
            .route("/session.js", get(static_handler))
            .route("/agent/install", get(install_script_handler))
            .route("/agent/install.ps1", get(install_script_ps1_handler))
            .fallback(get(static_handler))
            .layer(cors)
            .with_state(state);

        let _ = app;
    }

    #[tokio::test]
    async fn test_upload_handler_unauthorized_no_token() {
        let state = Arc::new(SharedState::new("".into(), 100 * 1024 * 1024, None, String::new(), String::new()));
        let headers = HeaderMap::new();
        let params = HashMap::new();
        let body = Body::from("test content");
        let result = upload_handler(State(state), headers, Query(params), body).await;
        assert_eq!(result, Err(StatusCode::UNAUTHORIZED));
    }

    #[tokio::test]
    async fn test_upload_handler_readonly_token_forbidden() {
        let state = Arc::new(SharedState::new("".into(), 100 * 1024 * 1024, None, String::new(), String::new()));
        let (_sid, tokens) = state.sessions.register(None, "ro").await;
        let token = &tokens[0].0;
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", token).parse().unwrap(),
        );
        let mut params = HashMap::new();
        params.insert("path".to_string(), "/tmp/test".to_string());
        let body = Body::from("test content");
        let result = upload_handler(State(state), headers, Query(params), body).await;
        assert_eq!(result, Err(StatusCode::FORBIDDEN));
    }

    #[tokio::test]
    async fn test_install_script_handler_returns_script() {
        let state = Arc::new(SharedState::new("".into(), 100 * 1024 * 1024, None, String::new(), String::new()));
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("host", "example.com:3000".parse().unwrap());
        let resp = install_script_handler(State(state), headers)
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("RELAY_URL=\"http://example.com:3000\""));
        assert!(text.contains("agent --relay-url"));
        assert!(text.contains("#!/bin/sh"));
    }

    #[tokio::test]
    async fn test_install_script_handler_https_forwarded() {
        let state = Arc::new(SharedState::new("".into(), 100 * 1024 * 1024, None, String::new(), String::new()));
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("host", "example.com".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        let resp = install_script_handler(State(state), headers)
            .await
            .into_response();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("RELAY_URL=\"https://example.com\""));
    }

    #[tokio::test]
    async fn test_install_script_handler_default_host() {
        let state = Arc::new(SharedState::new("".into(), 100 * 1024 * 1024, None, String::new(), String::new()));
        let headers = axum::http::HeaderMap::new();
        let resp = install_script_handler(State(state), headers)
            .await
            .into_response();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("RELAY_URL=\"http://localhost\""));
    }

    #[tokio::test]
    async fn test_install_script_ps1_handler_returns_script() {
        let state = Arc::new(SharedState::new("".into(), 100 * 1024 * 1024, None, String::new(), String::new()));
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("host", "example.com:3000".parse().unwrap());
        let resp = install_script_ps1_handler(State(state), headers)
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("$RELAY_URL = \"http://example.com:3000\""));
        assert!(text.contains("agent --relay-url"));
        assert!(text.contains("Invoke-WebRequest"));
        assert!(text.contains("--download-only"));
    }

    #[tokio::test]
    async fn test_upload_handler_missing_path() {
        let state = Arc::new(SharedState::new("".into(), 100 * 1024 * 1024, None, String::new(), String::new()));
        let (_sid, tokens) = state.sessions.register(None, "rw").await;
        let token = &tokens[0].0;
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", token).parse().unwrap(),
        );
        let params = HashMap::new();
        let body = Body::from("test content");
        let result = upload_handler(State(state), headers, Query(params), body).await;
        assert_eq!(result, Err(StatusCode::BAD_REQUEST));
    }

    // ── EventBuffer tests ───────────────────────────────────────────

    #[test]
    fn test_event_buffer_push_and_replay() {
        let mut buf = EventBuffer::new();
        let id1 = buf.push("msg1".into());
        let id2 = buf.push("msg2".into());
        let id3 = buf.push("msg3".into());
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);

        let replay = buf.replay_from(1);
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].0, 2);
        assert_eq!(replay[1].0, 3);
    }

    #[test]
    fn test_event_buffer_replay_from_zero() {
        let mut buf = EventBuffer::new();
        buf.push("a".into());
        buf.push("b".into());
        let replay = buf.replay_from(0);
        assert_eq!(replay.len(), 2);
    }

    #[test]
    fn test_event_buffer_replay_none_found() {
        let mut buf = EventBuffer::new();
        buf.push("x".into());
        let replay = buf.replay_from(5);
        assert!(replay.is_empty());
    }

    #[test]
    fn test_event_buffer_max_capacity() {
        let mut buf = EventBuffer::new();
        for i in 0..1200 {
            buf.push(format!("msg{}", i));
        }
        // Should still only hold 1000
        let replay = buf.replay_from(0);
        assert_eq!(replay.len(), 1000);
        // Oldest should have been evicted
        assert_eq!(replay[0].0, 201);
    }
}

pub async fn upload_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: axum::body::Body,
) -> Result<axum::http::StatusCode, axum::http::StatusCode> {
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    {
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&client_ip, 20, std::time::Duration::from_secs(60)) {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
    }

    let token =
        crate::relay::auth::extract_token_from_headers_or_query(&headers, params.get("token"))
            .ok_or(StatusCode::UNAUTHORIZED)?;
    let path = params.get("path").ok_or(StatusCode::BAD_REQUEST)?;

    let (session_id, permission) = state
        .sessions
        .authenticate(&token)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;

    use crate::proto::Permission;
    if permission == Permission::ReadOnly {
        return Err(StatusCode::FORBIDDEN);
    }

    let tmp_dir = std::path::PathBuf::from("/tmp/opencode/uploads");
    let _ = tokio::fs::create_dir_all(&tmp_dir).await;
    let tmp_name = format!("{}_{}", uuid::Uuid::new_v4(), path.replace('/', "_"));
    let tmp_path = tmp_dir.join(&tmp_name);

    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut total: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(result) = stream.next().await {
        let chunk = result.map_err(|_| {
            // Will clean up below
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        total += chunk.len() as u64;
        if total > state.max_upload_size {
            drop(file);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        use tokio::io::AsyncWriteExt;
        file.write_all(&chunk).await.map_err(|_| {
            // Will clean up below
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    drop(file);

    let msg = serde_json::json!({
        "type": "fs:upload",
        "session_id": session_id,
        "payload": {
            "temp_path": tmp_path.to_string_lossy(),
            "final_path": path
        }
    })
    .to_string();

    {
        let broadcast = state.agent_broadcast.read().await;
        if let Some(channel_map) = broadcast.get(&session_id) {
            if let Some(agent_tx) = &channel_map.agent {
                let _ = agent_tx.send(msg);
            }
        }
    }

    tracing::info!(
        "Upload received: {} ({} bytes) -> {}",
        path,
        total,
        tmp_path.display()
    );
    Ok(StatusCode::OK)
}

pub async fn install_script_handler(
    State(_state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|v| *v == "https")
        .map(|_| "https")
        .unwrap_or("http");
    let relay_url = format!("{}://{}", proto, host);

    let script = include_str!("../../web/install.sh").replace("__RELAY_URL__", &relay_url);

    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        script,
    )
}

pub async fn install_script_ps1_handler(
    State(_state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|v| *v == "https")
        .map(|_| "https")
        .unwrap_or("http");
    let relay_url = format!("{}://{}", proto, host);

    let script = include_str!("../../web/install.ps1").replace("__RELAY_URL__", &relay_url);

    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        script,
    )
}

pub async fn start(
    bind: String,
    server_auth: Option<String>,
    admin_path: Option<String>,
    admin_user: Option<String>,
    admin_pass: Option<String>,
) -> anyhow::Result<()> {
    let auth = match server_auth {
        Some(a) if !a.is_empty() => a,
        _ => {
            tracing::error!("--auth is required.");
            tracing::error!("  Usage: shell-remote relay --auth YOUR_PASSWORD ...");
            anyhow::bail!("Missing required --auth password");
        }
    };

    // Admin panel is opt-in via --admin-path. When set, --admin-pass is
    // required; --admin-user defaults to "admin". When unset, no admin routes
    // are registered and the panel is completely inaccessible.
    let admin_path_norm = admin_path.filter(|p| !p.is_empty());
    let (admin_path_v, admin_user_v, admin_pass_v) = match admin_path_norm {
        Some(p) => {
            let pass = admin_pass
                .filter(|p| !p.is_empty())
                .ok_or_else(|| anyhow::anyhow!("--admin-path requires --admin-pass"))?;
            let user = admin_user
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "admin".to_string());
            (Some(p), Some(user), Some(pass))
        }
        None => (None, None, None),
    };

    use axum::routing::get;
    use axum::Router;
    use tower_http::cors::{Any, CorsLayer};

    let state = Arc::new(SharedState::new(
        auth,
        100 * 1024 * 1024,
        admin_path_v.clone(),
        admin_user_v.clone().unwrap_or_default(),
        admin_pass_v.clone().unwrap_or_default(),
    ));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/agent/session/sse", get(ws::browser_sse_handler))
        .route(
            "/agent/session/send",
            axum::routing::post(ws::browser_send_handler),
        )
        .route("/agent/send", axum::routing::post(ws::agent_send_handler))
        .route("/agent/events", get(ws::agent_events_handler))
        .route(
            "/agent/upload",
            axum::routing::post(upload_handler).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/agent/mcp/sse", get(mcp::sse_handler))
        .route(
            "/agent/mcp/messages",
            axum::routing::post(mcp::messages_handler),
        )
        .route("/agent/install", get(install_script_handler))
        .route("/agent/install.ps1", get(install_script_ps1_handler))
        .route("/", get(static_handler))
        .route("/session", get(static_handler))
        .route("/style.css", get(static_handler))
        .route("/sse.js", get(static_handler))
        .route("/term.js", get(static_handler))
        .route("/files.js", get(static_handler))
        .route("/session.js", get(static_handler));

    // Admin panel routes — registered only when --admin-path is set. Paths
    // are built at startup from the configured secret prefix; axum parses them
    // immediately so the temporary strings need not outlive this call.
    let app = if let Some(ref ap_raw) = admin_path_v {
        let ap = if ap_raw.starts_with('/') {
            ap_raw.clone()
        } else {
            format!("/{}", ap_raw)
        };
        app.route(&ap, get(admin::admin_page_handler))
            .route(&format!("{}/login", ap), axum::routing::post(admin::login_handler))
            .route(&format!("{}/logout", ap), axum::routing::post(admin::logout_handler))
            .route(&format!("{}/api/overview", ap), get(admin::overview_handler))
            .route(&format!("{}/api/session/kick", ap), axum::routing::post(admin::kick_handler))
            .route(&format!("{}/api/token/revoke", ap), axum::routing::post(admin::revoke_handler))
            .route(&format!("{}/api/token/regenerate", ap), axum::routing::post(admin::regenerate_handler))
            .route(&format!("{}/api/token/permission", ap), axum::routing::post(admin::permission_handler))
            .route(&format!("{}/api/server-auth", ap), get(admin::get_server_auth_handler))
            .route(&format!("{}/api/server-auth", ap), axum::routing::post(admin::set_server_auth_handler))
    } else {
        app
    };

    let app = app
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
                    state_clone
                        .agent_broadcast
                        .write()
                        .await
                        .remove(&session_id);
                    state_clone.last_activity.write().await.remove(&session_id);
                    // Drop the replay buffer so reaped sessions don't leak memory.
                    state_clone
                        .agent_event_buffers
                        .write()
                        .await
                        .remove(&session_id);
                }
            }
        });
    }

    axum::serve(listener, app).await?;

    Ok(())
}
