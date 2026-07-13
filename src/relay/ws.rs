use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response, Sse};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::proto::{requires_write, Message as ProtoMessage, Permission, TokenType};

use crate::relay::{ChannelMap, SharedState, MAX_SESSIONS, SSE_CHANNEL_CAPACITY};

/// Message types that are safe to silently drop when a downstream SSE channel
/// is full: they are either high-volume output (terminal frames) or state
/// broadcasts that are superseded by newer ones. Dropping them under a stuck
/// consumer degrades only that consumer; non-lossy types (input, file chunks,
/// results, control) are logged when dropped so a stuck session is visible
/// without stalling other sessions.
fn is_lossy_msg_type(t: &str) -> bool {
    matches!(
        t,
        "terminal:output"
            | "terminal:resize"
            | "session:users"
            | "session:tab_list"
            | "session:tab_switched"
    )
}

/// Non-blocking delivery to a bounded SSE channel. Never awaits, so it is safe
/// to call while holding a read lock. On overflow, lossy types are dropped
/// silently and everything else is dropped with a warning — the alternative
/// (unbounded buffering) would let one stuck session OOM the whole relay and
/// stall every other session.
pub fn deliver(tx: &mpsc::Sender<String>, msg_type: &str, msg: String) {
    match tx.try_send(msg) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            if !is_lossy_msg_type(msg_type) {
                tracing::warn!(
                    "SSE channel full; dropping non-lossy {} message for a stuck/downstream session",
                    msg_type
                );
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

// ── Shared agent message routing ─────────────────────────────────────

pub async fn route_agent_message(state: &Arc<SharedState>, session_id: &str, text_str: &str) {
    if let Ok(proto_msg) = serde_json::from_str::<ProtoMessage>(text_str) {
        // Recording: capture terminal:output for the session's cast file.
        if proto_msg.msg_type == "terminal:output" {
            if let Some(rec) = &state.recorder {
                if let Some(data) = proto_msg.payload.get("data").and_then(|v| v.as_str()) {
                    rec.record(
                        session_id,
                        crate::relay::recorder::RecordEvent::Output(crate::relay::recorder::decode_terminal_data(data)),
                    );
                }
            }
        }
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
            // fs:result is browser-facing (file manager reads + downloads).
            // A browser download reuses `_mcp_request_id` as its correlation
            // id, so it must still be broadcast — treating it as an MCP RPC
            // reply would swallow it (the download was never registered in
            // pending_mcp). MCP RPC replies use mcp:result / mcp:exec_result,
            // never fs:result. The fs:result oneshot block below only fires
            // for entries actually in pending_mcp, so it stays harmless.
            let is_mcp_rpc = proto_msg.msg_type != "fs:result"
                && proto_msg.payload.get("_mcp_request_id").is_some();
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
                                deliver(tx, &proto_msg.msg_type, text_str.to_string());
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
        // Rate limit registrations per client IP to prevent session-flooding DoS.
        let client_ip = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();
        {
            let mut rl = state.rate_limiter.write().await;
            if !rl.check(&client_ip, 10, std::time::Duration::from_secs(60)) {
                return (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    "Too many registrations from this address",
                )
                    .into_response();
            }
        }

        // Hard cap on total sessions.
        if state.sessions.count().await >= MAX_SESSIONS {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Session limit reached",
            )
                .into_response();
        }

        // If the agent supplied a cached token set (auto-reconnect), reuse
        // those exact tokens instead of minting fresh random ones.
        let cached_tokens: Option<Vec<(String, Permission)>> = body
            .get("tokens")
            .and_then(|t| t.as_array())
            .filter(|a| !a.is_empty())
            .and_then(|arr| {
                let mut v = Vec::with_capacity(arr.len());
                for t in arr {
                    let tok = t.get("token").and_then(|x| x.as_str())?;
                    let perm = match t.get("permission").and_then(|x| x.as_str())? {
                        "rw" => Permission::ReadWrite,
                        "ro" => Permission::ReadOnly,
                        _ => return None,
                    };
                    v.push((tok.to_string(), perm));
                }
                Some(v)
            });

        // Custom session id (--session-id on the agent). Validated by the
        // registry too; an empty/absent value means "relay picks a random id".
        let desired_session_id = body
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let register_result = if let Some(ct) = cached_tokens {
            state.sessions.register_existing(ct, desired_session_id).await
        } else {
            let fixed_key = body["key"].as_str().map(|s| s.to_string());
            let token_type_str = body["token_type"].as_str().unwrap_or("rw");
            let token_type = crate::proto::TokenType::from_str_val(token_type_str)
                .unwrap_or(crate::proto::TokenType::Rw);
            state
                .sessions
                .register(fixed_key.clone(), token_type.as_str(), desired_session_id)
                .await
        };
        let (session_id, tokens) = match register_result {
            Ok(v) => v,
            Err(crate::relay::session::RegisterError::IdTaken) => {
                return (
                    axum::http::StatusCode::CONFLICT,
                    "session_id already in use",
                )
                    .into_response();
            }
            Err(crate::relay::session::RegisterError::InvalidId) => {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    "invalid session_id (5-20 alphanumeric)",
                )
                    .into_response();
            }
        };

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

        let key_info = if body.get("tokens").map(|v| v.is_array()).unwrap_or(false) {
            "reconnect".to_string()
        } else {
            body["key"]
                .as_str()
                .map(|k| format!("key:{}", k))
                .unwrap_or_else(|| "temp".to_string())
        };
        tracing::info!(session = %session_id, key = %key_info, "session created (HTTP-mode)");
        for (token, perm) in &tokens {
            let perm_str = match perm {
                Permission::ReadWrite => "rw",
                Permission::ReadOnly => "ro",
            };
            tracing::info!(session = %session_id, permission = perm_str, "token: {}", token);
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
    let server_auth = state.server_auth.read().await.clone();
    if !server_auth.is_empty() {
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
            if !crate::relay::auth::constant_time_eq(auth_header, &server_auth)
                && !crate::relay::auth::constant_time_eq(body_auth, &server_auth)
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

/// Biased merge of the interactive and bulk agent channels into one stream
/// for the agent SSE. Interactive is drained first each cycle, so file chunks
/// (bulk) yield to terminal:input / mcp:exec / control messages. Both
/// receivers are bounded independently; bulk backpressure cannot stall
/// interactive.
fn merge_biased(
    mut interactive: tokio::sync::mpsc::Receiver<String>,
    mut bulk: tokio::sync::mpsc::Receiver<String>,
) -> impl tokio_stream::Stream<Item = String> {
    async_stream::stream! {
        loop {
            tokio::select! {
                biased;
                m = interactive.recv() => match m { Some(s) => yield s, None => {
                    // drain remaining bulk
                    while let Some(s) = bulk.recv().await { yield s; }
                    break;
                }},
                m = bulk.recv() => match m { Some(s) => yield s, None => {
                    // drain remaining interactive
                    while let Some(s) = interactive.recv().await { yield s; }
                    break;
                }},
            }
        }
    }
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

    let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);          // interactive
    let (bulk_tx, bulk_rx) = mpsc::channel::<String>(crate::relay::BULK_CHANNEL_CAPACITY);

    {
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(cm) = broadcast.get_mut(&session_id) {
            cm.agent = Some(tx);
            cm.agent_bulk = Some(bulk_tx);
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

        let mut merged = Box::pin(merge_biased(rx, bulk_rx));
        while let Some(msg) = merged.next().await {
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
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    // Prefer the Authorization header so tokens don't land in access logs via
    // the query string; fall back to ?token= for backward compatibility.
    let token = match crate::relay::auth::extract_token_from_headers_or_query(
        &headers,
        params.get("token"),
    ) {
        Some(t) => t,
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
    let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);

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
                deliver(agent_tx, "session:join", join_msg);
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
                    deliver(stx, "session:users", users_msg.clone());
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
        inner: ReceiverStream::new(rx),
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
                                deliver(stx, "session:users", users_msg.clone());
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
                        deliver(agent_tx, "session:leave", leave_msg);
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

    // Recording: capture terminal:input for the session's cast file.
    if msg_type == "terminal:input" {
        if let Some(rec) = &state.recorder {
            if let Some(data) = body["payload"]["data"].as_str() {
                rec.record(
                    &session_id,
                    crate::relay::recorder::RecordEvent::Input(crate::relay::recorder::decode_terminal_data(data)),
                );
            }
        }
    }

    {
        let mut activity = state.last_activity.write().await;
        activity.insert(session_id.clone(), Instant::now());
    }

    let forward_msg = json!({
        "type": msg_type,
        "session_id": session_id,
        "payload": body["payload"]
    })
    .to_string();

    {
        let broadcast = state.agent_broadcast.read().await;
        if let Some(cm) = broadcast.get(&session_id) {
            if let Some(ref agent_tx) = cm.agent {
                deliver(agent_tx, msg_type, forward_msg);
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
        Arc::new(SharedState::new(server_auth.to_string(), 100 * 1024 * 1024, None, String::new(), String::new(), None))
    }

    async fn insert_channel_map(
        state: &Arc<SharedState>,
        session_id: &str,
    ) -> (
        mpsc::Sender<String>,
        mpsc::Receiver<String>,
    ) {
        let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
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
    ) -> mpsc::Receiver<String> {
        let sse_sid = format!("bs_test_{}", Uuid::new_v4());
        let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        state.sse_sessions.write().await.insert(sse_sid.clone(), tx);
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(cm) = broadcast.get_mut(session_id) {
            cm.browser_sessions.insert(user_id.to_string(), sse_sid);
        }
        rx
    }

    // ── route_agent_message tests ────────────────────────────────────

    #[tokio::test]
    async fn test_deliver_drops_terminal_output_when_full() {
        // A bounded channel that's full must drop terminal:output silently
        // (lossy) so a stuck consumer can't stall the producer; non-lossy
        // messages are also dropped (can't block) but that's the degradation
        // bound that keeps other sessions alive.
        let (tx, mut rx) = mpsc::channel::<String>(2);
        deliver(&tx, "terminal:output", "a".into());
        deliver(&tx, "terminal:output", "b".into());
        // Channel now full (cap 2). Third deliver must drop, not block.
        deliver(&tx, "terminal:output", "c".into());
        assert_eq!(rx.recv().await.unwrap(), "a");
        assert_eq!(rx.recv().await.unwrap(), "b");
        // "c" was dropped; channel is empty now.
        assert!(rx.try_recv().is_err());
    }

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
    async fn test_route_agent_message_fs_result_download_broadcasts_to_browser() {
        // A browser download reuses `_mcp_request_id` as its correlation id.
        // The fs:result reply must still be broadcast to the browser (it was
        // never registered in pending_mcp), otherwise the download never
        // arrives. This is the download fix.
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;
        let mut rx = add_browser(&state, "sid1", "user1").await;

        let msg = json!({"type":"fs:result","session_id":"sid1","payload":{"success":true,"content":"aGk=","path":"/tmp/x","_mcp_request_id":"dl-1"}}).to_string();
        route_agent_message(&state, "sid1", &msg).await;

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await.unwrap().unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["type"], "fs:result");
        assert_eq!(v["payload"]["_mcp_request_id"], "dl-1");
        assert_eq!(v["payload"]["content"], "aGk=");
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
    async fn test_agent_send_register_reuses_cached_tokens() {
        let state = make_state("");
        let body = json!({
            "type": "agent:register",
            "tokens": [
                {"token": "reused-rw", "permission": "rw"},
                {"token": "reused-ro", "permission": "ro"}
            ]
        });
        let headers = axum::http::HeaderMap::new();
        let resp = agent_send_handler(State(state.clone()), headers, Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        // Returned tokens are exactly the ones supplied
        let tokens = v["payload"]["tokens"].as_array().unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0]["token"], "reused-rw");
        assert_eq!(tokens[1]["token"], "reused-ro");
        // They now authenticate against the new session
        let sid = v["session_id"].as_str().unwrap();
        let (auth_sid, _) = state.sessions.authenticate("reused-rw").await.unwrap();
        assert_eq!(auth_sid, sid);
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
        let (sid, tokens) = state.sessions.register(None, "ro", None).await.unwrap();
        let token = &tokens[0].0;
        state
            .agent_broadcast
            .write()
            .await
            .insert(sid.clone(), ChannelMap::new());
        let body = json!({"token": token, "type": "terminal:input", "payload": {}});
        let resp = browser_send_handler(State(state), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 403);
    }

    #[tokio::test]
    async fn test_agent_send_register_with_custom_id() {
        let state = make_state("");
        let body = json!({"type":"agent:register","token_type":"rw","session_id":"mydev01"});
        let resp = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
        let v: Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(v["session_id"], "mydev01");
    }

    #[tokio::test]
    async fn test_agent_send_register_custom_id_conflict() {
        let state = make_state("");
        let b1 = json!({"type":"agent:register","token_type":"rw","session_id":"mydev02"});
        let r1 = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(b1))
            .await
            .into_response();
        assert_eq!(r1.status(), 200);
        let b2 = json!({"type":"agent:register","token_type":"rw","session_id":"mydev02"});
        let r2 = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(b2))
            .await
            .into_response();
        assert_eq!(r2.status(), 409);
    }

    #[tokio::test]
    async fn test_agent_send_register_invalid_custom_id() {
        let state = make_state("");
        let body = json!({"type":"agent:register","token_type":"rw","session_id":"ab!"});
        let resp = agent_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 400);
    }

    fn make_state_with_recorder() -> (
        Arc<SharedState>,
        std::sync::Arc<crate::relay::recorder::Recorder>,
    ) {
        let dir = std::env::temp_dir().join(format!(
            "sr-rec-ws-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let rec = std::sync::Arc::new(crate::relay::recorder::Recorder::new(dir));
        let state = Arc::new(SharedState::new(
            "".to_string(),
            100 * 1024 * 1024,
            None,
            String::new(),
            String::new(),
            Some(rec.clone()),
        ));
        (state, rec)
    }

    #[tokio::test]
    async fn test_route_agent_message_records_output() {
        let (state, recorder) = make_state_with_recorder();
        insert_channel_map(&state, "sid1").await;
        let mut rx = add_browser(&state, "sid1", "u1").await;
        let msg = json!({"type":"terminal:output","session_id":"sid1","payload":{"data":"hi"}}).to_string();
        route_agent_message(&state, "sid1", &msg).await;
        // browser still received it
        assert!(rx.try_recv().is_ok());
        // recorder has an open writer
        assert!(recorder.is_recording("sid1"));
        recorder.close("sid1");
    }

    #[tokio::test]
    async fn test_browser_send_records_input() {
        let (state, recorder) = make_state_with_recorder();
        let (sid, tokens) = state.sessions.register(None, "rw", None).await.unwrap();
        state
            .agent_broadcast
            .write()
            .await
            .insert(sid.clone(), ChannelMap::new());
        let body = json!({"token": tokens[0].0, "type": "terminal:input", "payload": {"data": "ls"}});
        let resp = browser_send_handler(State(state), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 202);
        assert!(recorder.is_recording(&sid));
        recorder.close(&sid);
    }

    // ── merge_biased tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_merge_biased_drains_interactive_first() {
        use tokio::sync::mpsc;
        let (itx, irx) = mpsc::channel::<String>(8);
        let (btx, brx) = mpsc::channel::<String>(8);
        // pre-fill: 3 bulk, then 1 interactive, then 1 bulk
        for i in 0..3 { btx.send(format!("b{}", i)).await.unwrap(); }
        itx.send("i0".to_string()).await.unwrap();
        btx.send("b3".to_string()).await.unwrap();
        drop(itx); drop(btx); // close so merge terminates
        let mut out = Box::pin(merge_biased(irx, brx));
        let mut got = Vec::new();
        use tokio_stream::StreamExt as _;
        while let Some(s) = out.next().await { got.push(s); }
        // interactive i0 must come before all bulk (biased: interactive drained first each cycle)
        let i_pos = got.iter().position(|s| s == "i0").unwrap();
        let first_bulk = got.iter().position(|s| s.starts_with('b')).unwrap();
        assert!(i_pos < first_bulk, "interactive must precede bulk under biased merge; got {:?}", got);
    }
}
