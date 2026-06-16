use axum::extract::{Query, State, WebSocketUpgrade};
use axum::response::{IntoResponse, Response, Sse};
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;

use crate::proto::{Message as ProtoMessage, Permission, TokenType};

use crate::relay::ChannelMap;
use crate::relay::SharedState;

// ── WebSocket handler (agent + browser) ──────────────────────────────

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    {
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&client_ip, 10, std::time::Duration::from_secs(60)) {
            return axum::http::StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }

    ws.max_message_size(4 * 1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| handle_socket(socket, state, params))
}

async fn handle_socket(
    socket: axum::extract::ws::WebSocket,
    state: Arc<SharedState>,
    _params: HashMap<String, String>,
) {
    let (mut sender, mut receiver) = socket.split();

    let first_msg = match receiver.next().await {
        Some(Ok(axum::extract::ws::Message::Text(text))) => text.to_string(),
        _ => {
            tracing::warn!("No initial message from WebSocket client, closing");
            return;
        }
    };

    let parsed: Value = match serde_json::from_str(&first_msg) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Invalid JSON in first message: {}", e);
            return;
        }
    };

    let msg_type = parsed["type"].as_str().unwrap_or("");

    match msg_type {
        "agent:register" => {
            handle_agent_ws(sender, receiver, state, parsed).await;
        }
        "browser:join" => {
            handle_browser_ws(sender, receiver, state, parsed).await;
        }
        other => {
            tracing::warn!("Unknown first message type: {}", other);
            let _ = sender
                .send(axum::extract::ws::Message::Text(
                    json!({"type":"error","payload":{"code":"UNKNOWN_ROLE","message":format!("Unknown role: {}", other)}}).to_string(),
                ))
                .await;
        }
    }
}

// ── Shared agent message routing ─────────────────────────────────────

pub async fn route_agent_message(state: &Arc<SharedState>, session_id: &str, text_str: &str) {
    if let Ok(proto_msg) = serde_json::from_str::<ProtoMessage>(text_str) {
        let broadcast_types = [
            "session:users", "session:tab_list", "session:tab_switched",
            "terminal:output", "fs:result", "fs:mkdir", "mcp:result",
        ];
        if broadcast_types.contains(&proto_msg.msg_type.as_str()) {
            let is_mcp_rpc = proto_msg.payload.get("_mcp_request_id").is_some();
            if !is_mcp_rpc {
                let broadcast = state.agent_broadcast.read().await;
                if let Some(channel_map) = broadcast.get(session_id) {
                    let target_user = proto_msg.payload
                        .get("_target_user_id")
                        .and_then(|v| v.as_str());
                    for (uid, tx) in &channel_map.browsers {
                        if target_user.is_none_or(|t| t == uid.as_str()) {
                            let _ = tx.send(text_str.to_string());
                        }
                    }
                }
            }
        }

        // MCP oneshot
        if proto_msg.msg_type == "mcp:result" || proto_msg.msg_type == "mcp:exec_result" {
            if let Some(request_id) = proto_msg.payload.get("_mcp_request_id").and_then(|v| v.as_str()) {
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
            if let Some(request_id) = proto_msg.payload.get("_mcp_request_id").and_then(|v| v.as_str()) {
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
        let (session_id, tokens) = state.sessions.register(fixed_key.clone(), token_type.as_str()).await;

        let tokens_json: Vec<Value> = tokens.iter().map(|(token, perm)| {
            let perm_str = match perm { Permission::ReadWrite => "rw", Permission::ReadOnly => "ro" };
            json!({"token": token, "permission": perm_str})
        }).collect();

        let key_info = fixed_key.as_ref().map(|k| format!("key:{}", k)).unwrap_or_else(|| "temp".to_string());
        tracing::info!("Session {} created ({}) HTTP-mode", session_id, key_info);
        println!("Session: {}", session_id);
        println!("  {}", key_info);
        for (token, perm) in &tokens {
            let perm_str = match perm { Permission::ReadWrite => "rw", Permission::ReadOnly => "ro" };
            println!("  {} -> {}", perm_str, token);
        }

        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast.entry(session_id.clone()).or_insert_with(ChannelMap::new);
        }

        return Json(json!({
            "type": "agent:registered",
            "session_id": session_id,
            "payload": { "tokens": tokens_json }
        })).into_response();
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
                return (axum::http::StatusCode::UNAUTHORIZED, "Invalid server password").into_response();
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

// ── WebSocket agent handler ──────────────────────────────────────────

async fn handle_agent_ws(
    mut sender: futures_util::stream::SplitSink<axum::extract::ws::WebSocket, axum::extract::ws::Message>,
    mut receiver: futures_util::stream::SplitStream<axum::extract::ws::WebSocket>,
    state: Arc<SharedState>,
    register_msg: Value,
) {
    let fixed_key = register_msg["key"].as_str().map(|s| s.to_string());
    let token_type_str = register_msg["token_type"].as_str().unwrap_or("rw");
    let token_type = TokenType::from_str_val(token_type_str).unwrap_or(TokenType::Rw);

    let (session_id, tokens) = state.sessions.register(fixed_key.clone(), token_type.as_str()).await;

    let tokens_json: Vec<Value> = tokens.iter().map(|(token, perm)| {
        let perm_str = match perm { Permission::ReadWrite => "rw", Permission::ReadOnly => "ro" };
        json!({"token": token, "permission": perm_str})
    }).collect();

    let registered_msg = serde_json::to_string(&json!({
        "type": "agent:registered",
        "session_id": session_id,
        "payload": { "tokens": tokens_json }
    })).unwrap_or_default();

    if sender.send(axum::extract::ws::Message::Text(registered_msg)).await.is_err() {
        tracing::error!("Failed to send agent:registered to agent");
        return;
    }

    let key_info = fixed_key.as_ref().map(|k| format!("key:{}", k)).unwrap_or_else(|| "temp".to_string());
    tracing::info!("Session {} created ({}) WS-mode", session_id, key_info);
    println!("Session: {}", session_id);
    println!("  {}", key_info);
    for (token, perm) in &tokens {
        let perm_str = match perm { Permission::ReadWrite => "rw", Permission::ReadOnly => "ro" };
        println!("  {} -> {}", perm_str, token);
    }

    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<String>();

    {
        let mut broadcast = state.agent_broadcast.write().await;
        let entry = broadcast.entry(session_id.clone()).or_insert_with(ChannelMap::new);
        entry.agent = Some(agent_tx.clone());
    }

    let state_clone = state.clone();
    let session_id_clone = session_id.clone();

    let sender_task = tokio::spawn(async move {
        while let Some(msg) = agent_rx.recv().await {
            if sender.send(axum::extract::ws::Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    while let Some(msg_result) = receiver.next().await {
        match msg_result {
            Ok(axum::extract::ws::Message::Text(text)) => {
                let text_str = text.to_string();
                state_clone.last_activity.write().await.insert(session_id_clone.clone(), Instant::now());
                route_agent_message(&state_clone, &session_id_clone, &text_str).await;
            }
            Ok(axum::extract::ws::Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("WebSocket error from agent: {}", e);
                break;
            }
        }
    }

    {
        let mut pending = state_clone.pending_mcp.write().await;
        pending.retain(|_rid, (sid, _tx)| sid != &session_id);
    }

    sender_task.abort();

    let is_temporary = state.sessions.is_temporary(&session_id).await;
    if is_temporary {
        state.sessions.remove(&session_id).await;
    }

    let session_id = session_id;
    {
        let mut broadcast = state_clone.agent_broadcast.write().await;
        let disconnect_msg = json!({"type": "session:agent_disconnect", "session_id": session_id, "payload": {}}).to_string();
        if let Some(channel_map) = broadcast.get(&session_id) {
            for tx in channel_map.browsers.values() {
                let _ = tx.send(disconnect_msg.clone());
            }
        }
        broadcast.remove(&session_id);
    }

    tracing::info!("Agent disconnected for session {}", session_id);
}

// ── WebSocket browser handler ────────────────────────────────────────

async fn handle_browser_ws(
    mut sender: futures_util::stream::SplitSink<axum::extract::ws::WebSocket, axum::extract::ws::Message>,
    mut receiver: futures_util::stream::SplitStream<axum::extract::ws::WebSocket>,
    state: Arc<SharedState>,
    join_msg: Value,
) {
    let token = join_msg["payload"]["token"].as_str().unwrap_or("");
    let server_password = join_msg["payload"]["server_auth"].as_str().unwrap_or("");

    if !state.server_auth.is_empty() && !crate::relay::auth::constant_time_eq(server_password, &state.server_auth) {
        let error_msg = serde_json::to_string(&json!({
            "type": "error", "session_id": "", "payload": {"code": "AUTH_INVALID_PASSWORD", "message": "Invalid server password"}
        })).unwrap_or_default();
        let _ = sender.send(axum::extract::ws::Message::Text(error_msg)).await;
        return;
    }

    let (session_id, permission) = match state.sessions.authenticate(token).await {
        Some(result) => result,
        None => {
            let error_msg = serde_json::to_string(&json!({
                "type": "error", "session_id": "", "payload": {"code": "AUTH_INVALID_TOKEN", "message": "Invalid or expired token"}
            })).unwrap_or_default();
            let _ = sender.send(axum::extract::ws::Message::Text(error_msg)).await;
            return;
        }
    };

    let user_id = Uuid::new_v4().to_string();
    let perm_str = match permission { Permission::ReadWrite => "rw", Permission::ReadOnly => "ro" };

    let joined_msg = serde_json::to_string(&json!({
        "type": "session:join", "session_id": session_id, "payload": {"user_id": user_id, "permission": perm_str}
    })).unwrap_or_default();

    let agent_tx: Option<mpsc::UnboundedSender<String>> = {
        let broadcast = state.agent_broadcast.read().await;
        broadcast.get(&session_id).and_then(|cm| cm.agent.clone())
    };

    if let Some(ref tx) = agent_tx {
        let _ = tx.send(joined_msg);
    }

    let welcome_msg = serde_json::to_string(&json!({
        "type": "browser:connected", "session_id": session_id, "payload": {"user_id": user_id, "permission": perm_str}
    })).unwrap_or_default();

    if sender.send(axum::extract::ws::Message::Text(welcome_msg)).await.is_err() {
        tracing::warn!("Failed to send welcome message to browser");
        return;
    }

    let agent_tx_clone = agent_tx.clone();
    let session_id_clone = session_id.clone();
    let user_id_clone = user_id.clone();

    let (browser_tx, mut browser_rx) = mpsc::unbounded_channel::<String>();

    {
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(channel_map) = broadcast.get_mut(&session_id) {
            channel_map.browsers.insert(user_id.clone(), browser_tx.clone());
        }
    }

    let browser_count = {
        let broadcast = state.agent_broadcast.read().await;
        broadcast.get(&session_id).map(|cm| cm.browsers.len()).unwrap_or(0)
    };
    let users_msg = serde_json::to_string(&json!({
        "type": "session:users", "session_id": session_id, "payload": { "count": browser_count }
    })).unwrap_or_default();
    {
        let broadcast = state.agent_broadcast.read().await;
        if let Some(channel_map) = broadcast.get(&session_id) {
            for tx in channel_map.browsers.values() {
                let _ = tx.send(users_msg.clone());
            }
        }
    }

    let state_clone = state.clone();
    let session_clone2 = session_id.clone();

    let sender_task = tokio::spawn(async move {
        while let Some(msg) = browser_rx.recv().await {
            if sender.send(axum::extract::ws::Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    while let Some(msg_result) = receiver.next().await {
        match msg_result {
            Ok(axum::extract::ws::Message::Text(text)) => {
                let text_str = text.to_string();
                state_clone.last_activity.write().await.insert(session_id_clone.clone(), Instant::now());

                match serde_json::from_str::<ProtoMessage>(&text_str) {
                    Ok(proto_msg) => {
                        if crate::proto::requires_write(&proto_msg.msg_type) && permission == Permission::ReadOnly {
                            let perm_denied = serde_json::to_string(&json!({
                                "type": "error", "session_id": session_id_clone, "payload": {"code": "PERMISSION_DENIED", "message": "Read-only users cannot send write-type messages"}
                            })).unwrap_or_default();
                            let _ = browser_tx.send(perm_denied);
                            continue;
                        }
                        if let Some(ref tx) = agent_tx_clone {
                            let _ = tx.send(text_str.clone());
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Invalid message from browser: {}", e);
                    }
                }
            }
            Ok(axum::extract::ws::Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("WebSocket error from browser: {}", e);
                break;
            }
        }
    }

    sender_task.abort();

    {
        let mut broadcast = state_clone.agent_broadcast.write().await;
        if let Some(channel_map) = broadcast.get_mut(&session_clone2) {
            channel_map.browsers.remove(&user_id_clone);
        }
    }

    {
        let broadcast = state_clone.agent_broadcast.read().await;
        let count = broadcast.get(&session_clone2).map(|cm| cm.browsers.len()).unwrap_or(0);
        let users_msg = serde_json::to_string(&json!({
            "type": "session:users", "session_id": session_clone2, "payload": { "count": count }
        })).unwrap_or_default();
        if let Some(channel_map) = broadcast.get(&session_clone2) {
            for tx in channel_map.browsers.values() {
                let _ = tx.send(users_msg.clone());
            }
        }
    }

    let leave_msg = serde_json::to_string(&json!({
        "type": "session:leave", "session_id": session_clone2, "payload": {"user_id": user_id_clone, "permission": perm_str}
    })).unwrap_or_default();

    if let Some(ref tx) = agent_tx {
        let _ = tx.send(leave_msg);
    }

    tracing::info!("Browser {} disconnected from session {}", user_id, session_id);
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
        })
    }

    async fn insert_channel_map(state: &Arc<SharedState>, session_id: &str) -> (mpsc::UnboundedSender<String>, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let mut cm = ChannelMap::new();
        cm.agent = Some(tx.clone());
        state.agent_broadcast.write().await.insert(session_id.to_string(), cm);
        (tx, rx)
    }

    async fn add_browser(state: &Arc<SharedState>, session_id: &str, user_id: &str) -> mpsc::UnboundedReceiver<String> {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(cm) = broadcast.get_mut(session_id) {
            cm.browsers.insert(user_id.to_string(), tx);
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

        let msg = json!({"type":"terminal:output","session_id":"sid1","payload":{"data":"hello"}}).to_string();
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
        let msg = json!({"type":"terminal:output","session_id":"nonexistent","payload":{}}).to_string();
        route_agent_message(&state, "nonexistent", &msg).await;
    }

    #[tokio::test]
    async fn test_route_agent_message_mcp_result_oneshot() {
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;

        let (tx, mut rx) = oneshot::channel::<String>();
        state.pending_mcp.write().await.insert("req1".to_string(), ("sid1".to_string(), tx));

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
        state.pending_mcp.write().await.insert("fs1".to_string(), ("sid1".to_string(), tx));

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
        let resp = agent_send_handler(State(state), headers, Json(body)).await.into_response();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn test_agent_send_no_session_id_returns_400() {
        let state = make_state("");
        let body = json!({"type":"terminal:output","payload":{"data":"x"}});
        let headers = axum::http::HeaderMap::new();
        let resp = agent_send_handler(State(state), headers, Json(body)).await.into_response();
        assert_eq!(resp.status(), 400);
    }

    // ── agent_events_handler tests ───────────────────────────────────

    #[tokio::test]
    async fn test_agent_events_missing_session_returns_400() {
        let state = make_state("");
        let params = HashMap::new();
        let headers = axum::http::HeaderMap::new();
        let resp = agent_events_handler(State(state), headers, Query(params)).await.into_response();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn test_agent_events_nonexistent_session_returns_404() {
        let state = make_state("");
        let mut params = HashMap::new();
        params.insert("session".to_string(), "nonexistent".to_string());
        let headers = axum::http::HeaderMap::new();
        let resp = agent_events_handler(State(state), headers, Query(params)).await.into_response();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn test_agent_events_valid_session_returns_200() {
        let state = make_state("");
        state.agent_broadcast.write().await.insert("sid1".to_string(), ChannelMap::new());
        let mut params = HashMap::new();
        params.insert("session".to_string(), "sid1".to_string());
        let headers = axum::http::HeaderMap::new();
        let resp = agent_events_handler(State(state), headers, Query(params)).await.into_response();
        assert_eq!(resp.status(), 200);
    }
}
