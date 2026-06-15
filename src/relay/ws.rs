use axum::extract::{Query, State, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::proto::{Message as ProtoMessage, Permission};
use crate::relay::ChannelMap;
use crate::relay::SharedState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<SharedState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    ws.max_message_size(256 * 1024 * 1024)
        .max_frame_size(64 * 1024 * 1024)
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

    let parsed: serde_json::Value = match serde_json::from_str(&first_msg) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Invalid JSON in first message: {}", e);
            return;
        }
    };

    let msg_type = parsed["type"].as_str().unwrap_or("");

    match msg_type {
        "agent:register" => {
            handle_agent(sender, receiver, state, parsed).await;
        }
        "browser:join" => {
            handle_browser(sender, receiver, state, parsed).await;
        }
        other => {
            tracing::warn!("Unknown first message type: {}", other);
            let _ = sender
                .send(axum::extract::ws::Message::Text(
                    json!({
                        "type":"error",
                        "payload":{
                            "code":"UNKNOWN_ROLE",
                            "message":format!("Unknown role: {}", other)
                        }
                    })
                    .to_string(),
                ))
                .await;
        }
    }
}

async fn handle_agent(
    mut sender: futures_util::stream::SplitSink<
        axum::extract::ws::WebSocket,
        axum::extract::ws::Message,
    >,
    mut receiver: futures_util::stream::SplitStream<axum::extract::ws::WebSocket>,
    state: Arc<SharedState>,
    register_msg: serde_json::Value,
) {
    let fixed_key = register_msg["key"].as_str().map(|s| s.to_string());
    let token_type = register_msg["token_type"].as_str().unwrap_or("rw");

    let (session_id, tokens) = state
        .sessions
        .register(fixed_key.clone(), token_type)
        .await;

    let tokens_json: Vec<serde_json::Value> = tokens
        .iter()
        .map(|(token, perm)| {
            let perm_str = match perm {
                Permission::ReadWrite => "rw",
                Permission::ReadOnly => "ro",
            };
            json!({"token": token, "permission": perm_str})
        })
        .collect();

    let registered_msg = serde_json::to_string(&json!({
        "type": "agent:registered",
        "session_id": session_id,
        "payload": {
            "tokens": tokens_json
        }
    }))
    .unwrap_or_default();

    if sender
        .send(axum::extract::ws::Message::Text(registered_msg))
        .await
        .is_err()
    {
        tracing::error!("Failed to send agent:registered to agent");
        return;
    }

    let key_info = fixed_key
        .as_ref()
        .map(|k| format!("key:{}", k))
        .unwrap_or_else(|| "temp".to_string());
    tracing::info!(
        "Session {} created ({}), tokens: {:?}",
        session_id,
        key_info,
        tokens.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>()
    );

    println!("Session: {}", session_id);
    println!("  {}", key_info);
    for (token, perm) in &tokens {
        let perm_str = match perm {
            Permission::ReadWrite => "rw",
            Permission::ReadOnly => "ro",
        };
        println!("  {} -> {}", perm_str, token);
    }

    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<String>();

    {
        let mut broadcast = state.agent_broadcast.write().await;
        let entry = broadcast
            .entry(session_id.clone())
            .or_insert_with(ChannelMap::new);
        entry.agent = Some(agent_tx.clone());
    }

    let state_clone = state.clone();
    let session_id_clone = session_id.clone();

    let sender_task = tokio::spawn(async move {
        while let Some(msg) = agent_rx.recv().await {
            if sender
                .send(axum::extract::ws::Message::Text(msg))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    while let Some(msg_result) = receiver.next().await {
        match msg_result {
            Ok(axum::extract::ws::Message::Text(text)) => {
                let text_str = text.to_string();

                state_clone.last_activity.write().await.insert(session_id_clone.clone(), Instant::now());

                if let Ok(proto_msg) = serde_json::from_str::<ProtoMessage>(&text_str) {
                    if proto_msg.msg_type == "session:users"
                        || proto_msg.msg_type == "session:tab_list"
                        || proto_msg.msg_type == "session:tab_switched"
                        || proto_msg.msg_type == "terminal:output"
                        || proto_msg.msg_type == "fs:result"
                        || proto_msg.msg_type == "fs:mkdir"
                        || proto_msg.msg_type == "mcp:result"
                    {
                        let broadcast = state_clone.agent_broadcast.read().await;
                        if let Some(channel_map) = broadcast.get(&session_id_clone) {
                            let target_user = proto_msg.payload
                                .get("_target_user_id")
                                .and_then(|v| v.as_str());

                            for (uid, tx) in &channel_map.browsers {
                                if target_user.map_or(true, |t| t == uid.as_str()) {
                                    let _ = tx.send(text_str.clone());
                                }
                            }
                        }
                    }

                    if proto_msg.msg_type == "mcp:result" || proto_msg.msg_type == "mcp:exec_result" {
                        if let Some(request_id) = proto_msg.payload.get("_mcp_request_id")
                            .and_then(|v| v.as_str())
                        {
                            let mut pending = state_clone.pending_mcp.write().await;
                            if let Some((_sid, tx)) = pending.remove(request_id) {
                                let result_text = if proto_msg.msg_type == "mcp:exec_result" {
                                    serde_json::to_string(&proto_msg.payload).unwrap_or_default()
                                } else {
                                    serde_json::to_string(&json!({
                                        "stdout": proto_msg.payload.get("stdout").and_then(|v| v.as_str()).unwrap_or(""),
                                        "stderr": proto_msg.payload.get("stderr").and_then(|v| v.as_str()).unwrap_or(""),
                                        "exit_code": proto_msg.payload.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0)
                                    }))
                                    .unwrap_or_default()
                                };
                                let _ = tx.send(result_text);
                            }
                        }
                    }

                    if proto_msg.msg_type == "fs:result" {
                        if let Some(request_id) = proto_msg.payload.get("_mcp_request_id")
                            .and_then(|v| v.as_str())
                        {
                            let mut pending = state_clone.pending_mcp.write().await;
                            if let Some((_sid, tx)) = pending.remove(request_id) {
                                let result_text = serde_json::to_string(&json!({
                                    "success": proto_msg.payload.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
                                    "error": proto_msg.payload.get("error").and_then(|v| v.as_str()).unwrap_or(""),
                                    "content": proto_msg.payload.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                                    "entries": proto_msg.payload.get("entries"),
                                    "path": proto_msg.payload.get("path").and_then(|v| v.as_str()).unwrap_or(""),
                                    "new_path": proto_msg.payload.get("new_path").and_then(|v| v.as_str()).unwrap_or("")
                                }))
                                .unwrap_or_default();
                                let _ = tx.send(result_text);
                            }
                        }
                    }
                }
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

    {
        let mut broadcast = state_clone.agent_broadcast.write().await;
        let disconnect_msg = json!({
            "type": "session:agent_disconnect",
            "session_id": session_id,
            "payload": {}
        })
        .to_string();
        if let Some(channel_map) = broadcast.get(&session_id) {
            for tx in channel_map.browsers.values() {
                let _ = tx.send(disconnect_msg.clone());
            }
        }
        broadcast.remove(&session_id);
    }

    tracing::info!("Agent disconnected for session {}", session_id);
}

async fn handle_browser(
    mut sender: futures_util::stream::SplitSink<
        axum::extract::ws::WebSocket,
        axum::extract::ws::Message,
    >,
    mut receiver: futures_util::stream::SplitStream<axum::extract::ws::WebSocket>,
    state: Arc<SharedState>,
    join_msg: serde_json::Value,
) {
    let token = join_msg["payload"]["token"].as_str().unwrap_or("");
    let server_password = join_msg["payload"]["server_auth"].as_str().unwrap_or("");

    if !state.server_auth.is_empty() && server_password != state.server_auth {
        let error_msg = serde_json::to_string(&json!({
            "type": "error",
            "session_id": "",
            "payload": {
                "code": "AUTH_INVALID_PASSWORD",
                "message": "Invalid server password"
            }
        }))
        .unwrap_or_default();
        let _ = sender.send(axum::extract::ws::Message::Text(error_msg)).await;
        return;
    }

    let (session_id, permission) = match state.sessions.authenticate(token).await {
        Some(result) => result,
        None => {
            let error_msg = serde_json::to_string(&json!({
                "type": "error",
                "session_id": "",
                "payload": {
                    "code": "AUTH_INVALID_TOKEN",
                    "message": "Invalid or expired token"
                }
            }))
            .unwrap_or_default();
            let _ = sender
                .send(axum::extract::ws::Message::Text(error_msg))
                .await;
            return;
        }
    };

    let user_id = Uuid::new_v4().to_string();
    let perm_str = match permission {
        Permission::ReadWrite => "rw",
        Permission::ReadOnly => "ro",
    };

    let joined_msg = serde_json::to_string(&json!({
        "type": "session:join",
        "session_id": session_id,
        "payload": {
            "user_id": user_id,
            "permission": perm_str
        }
    }))
    .unwrap_or_default();

    let agent_tx: Option<mpsc::UnboundedSender<String>> = {
        let broadcast = state.agent_broadcast.read().await;
        broadcast
            .get(&session_id)
            .and_then(|cm| cm.agent.clone())
    };

    if let Some(ref tx) = agent_tx {
        let _ = tx.send(joined_msg);
    }

    let welcome_msg = serde_json::to_string(&json!({
        "type": "browser:connected",
        "session_id": session_id,
        "payload": {
            "user_id": user_id,
            "permission": perm_str
        }
    }))
    .unwrap_or_default();

    if sender
        .send(axum::extract::ws::Message::Text(welcome_msg))
        .await
        .is_err()
    {
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
        broadcast
            .get(&session_id)
            .map(|cm| cm.browsers.len())
            .unwrap_or(0)
    };
    let users_msg = serde_json::to_string(&json!({
        "type": "session:users",
        "session_id": session_id,
        "payload": { "count": browser_count }
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
            if sender
                .send(axum::extract::ws::Message::Text(msg))
                .await
                .is_err()
            {
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
                        let msg_type = &proto_msg.msg_type;

                        if crate::proto::requires_write(msg_type)
                            && permission == Permission::ReadOnly
                        {
                            let perm_denied = serde_json::to_string(&json!({
                                "type": "error",
                                "session_id": session_id_clone,
                                "payload": {
                                    "code": "PERMISSION_DENIED",
                                    "message": "Read-only users cannot send write-type messages"
                                }
                            }))
                            .unwrap_or_default();
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
        let count = broadcast
            .get(&session_clone2)
            .map(|cm| cm.browsers.len())
            .unwrap_or(0);
        let users_msg = serde_json::to_string(&json!({
            "type": "session:users",
            "session_id": session_clone2,
            "payload": { "count": count }
        })).unwrap_or_default();
        if let Some(channel_map) = broadcast.get(&session_clone2) {
            for tx in channel_map.browsers.values() {
                let _ = tx.send(users_msg.clone());
            }
        }
    }

    let leave_msg = serde_json::to_string(&json!({
        "type": "session:leave",
        "session_id": session_clone2,
        "payload": {
            "user_id": user_id_clone,
            "permission": perm_str
        }
    }))
    .unwrap_or_default();

    if let Some(ref tx) = agent_tx {
        let _ = tx.send(leave_msg);
    }

    tracing::info!(
        "Browser {} disconnected from session {}",
        user_id,
        session_id
    );
}
