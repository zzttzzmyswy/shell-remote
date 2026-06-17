use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response, Sse};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;

use crate::proto::{requires_write, Message as ProtoMessage, Permission, TokenType};

use crate::relay::ChannelMap;
use crate::relay::SharedState;

// ── Shared agent message routing ─────────────────────────────────────

pub async fn route_agent_message(state: &Arc<SharedState>, session_id: &str, text_str: &str) {
    if let Ok(proto_msg) = serde_json::from_str::<ProtoMessage>(text_str) {
        let broadcast_types = [
            "session:users",
            "session:tab_list",
            "session:tab_switched",
            "terminal:output",
            "fs:result",
            "fs:mkdir",
            "mcp:result",
        ];
        if broadcast_types.contains(&proto_msg.msg_type.as_str()) {
            let is_mcp_rpc = proto_msg.payload.get("_mcp_request_id").is_some();
            if !is_mcp_rpc {
                let sse_sessions = state.sse_sessions.read().await;
                let broadcast = state.agent_broadcast.read().await;
                if let Some(channel_map) = broadcast.get(session_id) {
                    let target_user = proto_msg
                        .payload
                        .get("_target_user_id")
                        .and_then(|v| v.as_str());
                    for (uid, sse_sid) in &channel_map.browser_sessions {
                        if target_user.is_none_or(|t| t == uid.as_str()) {
                            if let Some(tx) = sse_sessions.get(sse_sid) {
                                let _ = tx.send(text_str.to_string());
                            }
                        }
                    }
                }
            }
        }

        // MCP oneshot
        if proto_msg.msg_type == "mcp:result" || proto_msg.msg_type == "mcp:exec_result" {
            if let Some(request_id) = proto_msg
                .payload
                .get("_mcp_request_id")
                .and_then(|v| v.as_str())
            {
                let mut pending = state.pending_mcp.write().await;
                if let Some((_sid, tx)) = pending.remove(request_id) {
                    let result_text = if proto_msg.msg_type == "mcp:exec_result" {
                        serde_json::to_string(&proto_msg.payload).unwrap_or_default()
                    } else {
                        serde_json::to_string(&json!({
                            "stdout": proto_msg.payload.get("stdout").and_then(|v| v.as_str()).unwrap_or(""),
                            "stderr": proto_msg.payload.get("stderr").and_then(|v| v.as_str()).unwrap_or(""),
                            "exit_code": proto_msg.payload.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0)
                        })).unwrap_or_default()
                    };
                    let _ = tx.send(result_text);
                }
            }
        }

        // FS oneshot
        if proto_msg.msg_type == "fs:result" {
            if let Some(request_id) = proto_msg
                .payload
                .get("_mcp_request_id")
                .and_then(|v| v.as_str())
            {
                let mut pending = state.pending_mcp.write().await;
                if let Some((_sid, tx)) = pending.remove(request_id) {
                    let result_text = serde_json::to_string(&json!({
                        "success": proto_msg.payload.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
                        "error": proto_msg.payload.get("error").and_then(|v| v.as_str()).unwrap_or(""),
                        "content": proto_msg.payload.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                        "entries": proto_msg.payload.get("entries"),
                        "path": proto_msg.payload.get("path").and_then(|v| v.as_str()).unwrap_or(""),
                        "new_path": proto_msg.payload.get("new_path").and_then(|v| v.as_str()).unwrap_or("")
                    })).unwrap_or_default();
                    let _ = tx.send(result_text);
                }
            }
        }
    }
}

// ── Agent send handler (POST, for HTTP-mode agents) ──────────────────

pub async fn agent_send_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let msg_type = body["type"].as_str().unwrap_or("");

    // agent:register is allowed without server auth (agents use keys for identity)
    if msg_type == "agent:register" {
        let fixed_key = body["key"].as_str().map(|s| s.to_string());
        let token_type_str = body["token_type"].as_str().unwrap_or("rw");
        let token_type = crate::proto::TokenType::from_str_val(token_type_str)
            .unwrap_or(crate::proto::TokenType::Rw);
        let (session_id, tokens) = state
            .sessions
            .register(fixed_key.clone(), token_type.as_str())
            .await;

        let tokens_json: Vec<Value> = tokens
            .iter()
            .map(|(token, perm)| {
                let perm_str = match perm {
                    Permission::ReadWrite => "rw",
                    Permission::ReadOnly => "ro",
                };
                json!({"token": token, "permission": perm_str})
            })
            .collect();

        let key_info = fixed_key
            .as_ref()
            .map(|k| format!("key:{}", k))
            .unwrap_or_else(|| "temp".to_string());
        tracing::info!("Session {} created ({}) HTTP-mode", session_id, key_info);
        println!("Session: {}", session_id);
        println!("  {}", key_info);
        for (token, perm) in &tokens {
            let perm_str = match perm {
                Permission::ReadWrite => "rw",
                Permission::ReadOnly => "ro",
            };
            println!("  {} -> {}", perm_str, token);
        }

        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast
                .entry(session_id.clone())
                .or_insert_with(ChannelMap::new);
        }

        return Json(json!({
            "type": "agent:registered",
            "session_id": session_id,
            "payload": { "tokens": tokens_json }
        }))
        .into_response();
    }

    // All other message types require server auth, unless they carry a valid session_id
    if !state.server_auth.is_empty() {
        let session_for_auth = body["session_id"].as_str().unwrap_or("");
        let has_valid_session = if !session_for_auth.is_empty() {
            let broadcasts = state.agent_broadcast.read().await;
            broadcasts.contains_key(session_for_auth)
        } else {
            false
        };
        if !has_valid_session {
            let auth_header = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or("");
            let body_auth = body["auth"].as_str().unwrap_or("");
            if !crate::relay::auth::constant_time_eq(auth_header, &state.server_auth)
                && !crate::relay::auth::constant_time_eq(body_auth, &state.server_auth)
            {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    "Invalid server password",
                )
                    .into_response();
            }
        }
    }

    let session_id = body["session_id"].as_str().unwrap_or("").to_string();
    if session_id.is_empty() {
        return (axum::http::StatusCode::BAD_REQUEST, "Missing session_id").into_response();
    }

    let text_str = serde_json::to_string(&body).unwrap_or_default();

    {
        let mut activity = state.last_activity.write().await;
        activity.insert(session_id.clone(), Instant::now());
    }

    route_agent_message(&state, &session_id, &text_str).await;

    axum::http::StatusCode::OK.into_response()
}

// ── Agent SSE handler (GET, for HTTP-mode agent receive) ─────────────

pub async fn agent_events_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    {
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&client_ip, 30, std::time::Duration::from_secs(60)) {
            return axum::http::StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }

    let session_id = match params.get("session") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return axum::http::StatusCode::BAD_REQUEST.into_response(),
    };

    {
        let broadcast = state.agent_broadcast.read().await;
        if !broadcast.contains_key(&session_id) {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
    }

    let last_event_id: Option<u64> = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    let (tx, rx) = mpsc::unbounded_channel::<String>();

    {
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(cm) = broadcast.get_mut(&session_id) {
            cm.agent = Some(tx);
        }
    }

    let state_clone = state.clone();
    let sid_clone = session_id.clone();

    let stream = async_stream::stream! {
        if let Some(last_id) = last_event_id {
            let buffers = state_clone.agent_event_buffers.read().await;
            if let Some(buf) = buffers.get(&sid_clone) {
                for (id, msg) in buf.replay_from(last_id) {
                    yield Ok::<_, Infallible>(
                        axum::response::sse::Event::default()
                            .id(id.to_string())
                            .data(msg)
                    );
                }
            }
        }

        let mut rx_stream = UnboundedReceiverStream::new(rx);
        while let Some(msg) = tokio_stream::StreamExt::next(&mut rx_stream).await {
            let id = state_clone.buffer_agent_event(&sid_clone, &msg).await;
            yield Ok::<_, Infallible>(
                axum::response::sse::Event::default()
                    .id(id.to_string())
                    .data(msg)
            );
        }
    };

    let mut response = axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response();
    response.headers_mut().insert(
        axum::http::header::HeaderName::from_static("x-accel-buffering"),
        axum::http::header::HeaderValue::from_static("no"),
    );
    response
}

// ── Browser SSE handler ────────────────────────────────────────────

pub async fn browser_sse_handler(
    State(state): State<Arc<SharedState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let token = match params.get("token") {
        Some(t) => t.clone(),
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "Missing token"})),
            )
                .into_response()
        }
    };

    let (session_id, permission) = match state.sessions.authenticate(&token).await {
        Some(r) => r,
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "Invalid token"})),
            )
                .into_response()
        }
    };

    let user_id = Uuid::new_v4().to_string();
    let sse_sid = format!("bs_{}", Uuid::new_v4());
    let (tx, rx) = mpsc::unbounded_channel::<String>();

    {
        state.sse_sessions.write().await.insert(sse_sid.clone(), tx);
    }

    let perm_str = match permission {
        Permission::ReadWrite => "rw",
        Permission::ReadOnly => "ro",
    };

    {
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(cm) = broadcast.get_mut(&session_id) {
            cm.browser_sessions.insert(user_id.clone(), sse_sid.clone());
        }
    }

    // Send session:join to agent
    let join_msg = json!({
        "type": "session:join",
        "session_id": session_id,
        "payload": { "user_id": user_id, "permission": perm_str }
    })
    .to_string();
    {
        let broadcast = state.agent_broadcast.read().await;
        if let Some(cm) = broadcast.get(&session_id) {
            if let Some(ref agent_tx) = cm.agent {
                let _ = agent_tx.send(join_msg);
            }
        }
    }

    // Broadcast updated user count to all browsers
    {
        let sse_sessions = state.sse_sessions.read().await;
        let broadcast = state.agent_broadcast.read().await;
        if let Some(cm) = broadcast.get(&session_id) {
            let count = cm.browser_sessions.len();
            let users_msg = json!({
                "type": "session:users",
                "session_id": session_id,
                "payload": { "count": count }
            })
            .to_string();
            for sse_sid_val in cm.browser_sessions.values() {
                if let Some(stx) = sse_sessions.get(sse_sid_val) {
                    let _ = stx.send(users_msg.clone());
                }
            }
        }
    }

    let state_clone = state.clone();
    let sid_clone = session_id.clone();
    let uid_clone = user_id.clone();
    let _sse_sid_clone = sse_sid.clone();
    let perm_clone = perm_str.to_string();

    // connected event data
    let connected_data = json!({
        "type": "browser:connected",
        "session_id": session_id,
        "payload": { "user_id": user_id, "permission": perm_str }
    });

    let stream = crate::relay::mcp::SseCleanup {
        inner: UnboundedReceiverStream::new(rx),
        state: state.clone(),
        sid: sse_sid.clone(),
        on_drop: Some(Box::new(move || {
            let s = state_clone.clone();
            let sid = sid_clone.clone();
            let uid = uid_clone.clone();
            let perm = perm_clone.clone();
            tokio::spawn(async move {
                let count = {
                    let mut broadcast = s.agent_broadcast.write().await;
                    if let Some(cm) = broadcast.get_mut(&sid) {
                        cm.browser_sessions.remove(&uid);
                        cm.browser_sessions.len()
                    } else {
                        0
                    }
                };

                // Broadcast updated count to remaining browsers
                let users_msg = json!({
                    "type": "session:users",
                    "session_id": sid,
                    "payload": { "count": count }
                })
                .to_string();
                {
                    let sse_sessions = s.sse_sessions.read().await;
                    let broadcast = s.agent_broadcast.read().await;
                    if let Some(cm) = broadcast.get(&sid) {
                        for sse_sid_val in cm.browser_sessions.values() {
                            if let Some(stx) = sse_sessions.get(sse_sid_val) {
                                let _ = stx.send(users_msg.clone());
                            }
                        }
                    }
                }

                // Send session:leave to agent
                let leave_msg = json!({
                    "type": "session:leave",
                    "session_id": sid,
                    "payload": { "user_id": uid, "permission": perm }
                })
                .to_string();
                let broadcast = s.agent_broadcast.read().await;
                if let Some(cm) = broadcast.get(&sid) {
                    if let Some(ref agent_tx) = cm.agent {
                        let _ = agent_tx.send(leave_msg);
                    }
                }
            });
        })),
    };

    let sse_stream = async_stream::stream! {
        yield Ok::<_, Infallible>(axum::response::sse::Event::default()
            .event("connected")
            .data(serde_json::to_string(&connected_data).unwrap_or_default()));

        let mut inner_stream = stream;
        while let Some(msg) = tokio_stream::StreamExt::next(&mut inner_stream).await {
            yield Ok::<_, Infallible>(axum::response::sse::Event::default()
                .data(msg));
        }
    };

    use axum::response::sse::KeepAlive;
    let mut response = Sse::new(sse_stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(5)))
        .into_response();
    response.headers_mut().insert(
        axum::http::header::HeaderName::from_static("x-accel-buffering"),
        axum::http::header::HeaderValue::from_static("no"),
    );
    response
}

// ── Browser send handler (POST) ─────────────────────────────────────

pub async fn browser_send_handler(
    State(state): State<Arc<SharedState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let token = match body["token"].as_str() {
        Some(t) => t,
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "Missing token"})),
            )
                .into_response()
        }
    };

    let (session_id, permission) = match state.sessions.authenticate(token).await {
        Some(r) => r,
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "Invalid token"})),
            )
                .into_response()
        }
    };

    let msg_type = body["type"].as_str().unwrap_or("");
    if msg_type.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "Missing message type"})),
        )
            .into_response();
    }

    if requires_write(msg_type) && permission == Permission::ReadOnly {
        return (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(json!({"error": "Read-only users cannot send write-type messages"})),
        )
            .into_response();
    }

    {
        let mut activity = state.last_activity.write().await;
        activity.insert(session_id.clone(), Instant::now());
    }

    let text_str = serde_json::to_string(&body).unwrap_or_default();

    {
        let broadcast = state.agent_broadcast.read().await;
        if let Some(cm) = broadcast.get(&session_id) {
            if let Some(ref agent_tx) = cm.agent {
                let _ = agent_tx.send(text_str);
            }
        }
    }

    axum::http::StatusCode::ACCEPTED.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::session::SessionRegistry;
    use crate::relay::RateLimiter;
    use tokio::sync::{oneshot, RwLock};

    fn make_state(server_auth: &str) -> Arc<SharedState> {
        Arc::new(SharedState {
            sessions: SessionRegistry::new(),
            agent_broadcast: RwLock::new(HashMap::new()),
            pending_mcp: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
            server_auth: server_auth.to_string(),
            bin_dir: None,
            agent_event_buffers: RwLock::new(HashMap::new()),
            rate_limiter: RwLock::new(RateLimiter::new()),
            max_upload_size: 100 * 1024 * 1024,
            sse_sessions: RwLock::new(HashMap::new()),
        })
    }

    async fn insert_channel_map(
        state: &Arc<SharedState>,
        session_id: &str,
    ) -> (
        mpsc::UnboundedSender<String>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let mut cm = ChannelMap::new();
        cm.agent = Some(tx.clone());
        state
            .agent_broadcast
            .write()
            .await
            .insert(session_id.to_string(), cm);
        (tx, rx)
    }

    async fn add_browser(
        state: &Arc<SharedState>,
        session_id: &str,
        user_id: &str,
    ) -> mpsc::UnboundedReceiver<String> {
        let sse_sid = format!("bs_test_{}", Uuid::new_v4());
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        state.sse_sessions.write().await.insert(sse_sid.clone(), tx);
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(cm) = broadcast.get_mut(session_id) {
            cm.browser_sessions.insert(user_id.to_string(), sse_sid);
        }
        rx
    }

    // ── route_agent_message tests ────────────────────────────────────

    #[tokio::test]
    async fn test_route_agent_message_broadcasts_to_all_browsers() {
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;
        let mut rx1 = add_browser(&state, "sid1", "user1").await;
        let mut rx2 = add_browser(&state, "sid1", "user2").await;

        let msg = json!({"type":"terminal:output","session_id":"sid1","payload":{"data":"hello"}})
            .to_string();
        route_agent_message(&state, "sid1", &msg).await;

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }

    #[tokio::test]
    async fn test_route_agent_message_target_user_only() {
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;
        let mut rx1 = add_browser(&state, "sid1", "user1").await;
        let mut rx2 = add_browser(&state, "sid1", "user2").await;

        let msg = json!({"type":"terminal:output","session_id":"sid1","payload":{"data":"hello","_target_user_id":"user1"}}).to_string();
        route_agent_message(&state, "sid1", &msg).await;

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_route_agent_message_missing_session_no_panic() {
        let state = make_state("");
        let msg =
            json!({"type":"terminal:output","session_id":"nonexistent","payload":{}}).to_string();
        route_agent_message(&state, "nonexistent", &msg).await;
    }

    #[tokio::test]
    async fn test_route_agent_message_mcp_result_oneshot() {
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;

        let (tx, mut rx) = oneshot::channel::<String>();
        state
            .pending_mcp
            .write()
            .await
            .insert("req1".to_string(), ("sid1".to_string(), tx));

        let msg = json!({"type":"mcp:result","session_id":"sid1","payload":{"stdout":"hello","stderr":"","exit_code":0,"_mcp_request_id":"req1"}}).to_string();
        route_agent_message(&state, "sid1", &msg).await;

        let result = rx.try_recv().unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["stdout"].as_str().unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_route_agent_message_fs_result_oneshot() {
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;

        let (tx, mut rx) = oneshot::channel::<String>();
        state
            .pending_mcp
            .write()
            .await
            .insert("fs1".to_string(), ("sid1".to_string(), tx));

        let msg = json!({"type":"fs:result","session_id":"sid1","payload":{"success":true,"error":"","content":"ok","path":"/tmp/x","_mcp_request_id":"fs1"}}).to_string();
        route_agent_message(&state, "sid1", &msg).await;

        let result = rx.try_recv().unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["success"].as_bool().unwrap(), true);
    }

    #[tokio::test]
    async fn test_route_agent_message_invalid_json_no_panic() {
        let state = make_state("");
        route_agent_message(&state, "sid1", "not valid json {{{").await;
    }

    // ── agent_send_handler tests ─────────────────────────────────────

    #[tokio::test]
    async fn test_agent_send_register_creates_session() {
        let state = make_state("");
        let body = json!({"type":"agent:register","token_type":"rw"});
        let headers = axum::http::HeaderMap::new();
        let resp = agent_send_handler(State(state), headers, Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn test_agent_send_no_session_id_returns_400() {
        let state = make_state("");
        let body = json!({"type":"terminal:output","payload":{"data":"x"}});
        let headers = axum::http::HeaderMap::new();
        let resp = agent_send_handler(State(state), headers, Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 400);
    }

    // ── agent_events_handler tests ───────────────────────────────────

    #[tokio::test]
    async fn test_agent_events_missing_session_returns_400() {
        let state = make_state("");
        let params = HashMap::new();
        let headers = axum::http::HeaderMap::new();
        let resp = agent_events_handler(State(state), headers, Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn test_agent_events_nonexistent_session_returns_404() {
        let state = make_state("");
        let mut params = HashMap::new();
        params.insert("session".to_string(), "nonexistent".to_string());
        let headers = axum::http::HeaderMap::new();
        let resp = agent_events_handler(State(state), headers, Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn test_agent_events_valid_session_returns_200() {
        let state = make_state("");
        state
            .agent_broadcast
            .write()
            .await
            .insert("sid1".to_string(), ChannelMap::new());
        let mut params = HashMap::new();
        params.insert("session".to_string(), "sid1".to_string());
        let headers = axum::http::HeaderMap::new();
        let resp = agent_events_handler(State(state), headers, Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
    }

    // ── browser_send_handler tests ────────────────────────────────────

    #[tokio::test]
    async fn test_browser_send_missing_token() {
        let state = make_state("");
        let body = json!({"type": "terminal:input", "payload": {}});
        let resp = browser_send_handler(State(state), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn test_browser_send_readonly_write_forbidden() {
        let state = make_state("");
        let (sid, tokens) = state.sessions.register(None, "ro").await;
        let token = &tokens[0].0;
        state
            .agent_broadcast
            .write()
            .await
            .insert(sid.clone(), ChannelMap::new());
        let body =
            json!({"token": token, "type": "terminal:input", "payload": {}});
        let resp = browser_send_handler(State(state), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 403);
    }
}
