use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt as _;
use uuid::Uuid;

use crate::proto::{Message as ProtoMessage, Permission};
use crate::relay::SharedState;

pub async fn sse_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if !state.server_auth.is_empty() {
        let header_auth = headers
            .get("x-auth")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let query_auth = params.get("auth").map(|s| s.as_str()).unwrap_or("");
        let auth = if header_auth.is_empty() { query_auth } else { header_auth };
        if !crate::relay::auth::constant_time_eq(auth, &state.server_auth) {
            return Sse::new(
                futures_util::stream::once(std::future::ready(Ok::<_, Infallible>(
                    Event::default()
                        .event("error")
                        .data(r#"{"code":"AUTH_INVALID_PASSWORD","message":"Invalid server password"}"#),
                ))),
            )
            .into_response();
        }
    }

    let mcp_session_id = Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::unbounded_channel::<String>();

    {
        let mut channels = state.mcp_sse_channels.write().await;
        channels.insert(mcp_session_id.clone(), tx);
    }

    let sid_for_stream = mcp_session_id.clone();
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(Event::default()
            .event("endpoint")
            .data(format!("/agent/mcp/messages?sessionId={}", sid_for_stream)));

        let mut rx_stream = UnboundedReceiverStream::new(rx);
        while let Some(msg) = tokio_stream::StreamExt::next(&mut rx_stream).await {
            yield Ok::<_, Infallible>(Event::default().event("message").data(msg));
        }
    };

    let state_clone = state.clone();
    let sid = mcp_session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        state_clone.mcp_sse_channels.write().await.remove(&sid);
    });

    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    response.headers_mut().insert(
        axum::http::header::HeaderName::from_static("x-accel-buffering"),
        axum::http::header::HeaderValue::from_static("no"),
    );
    response
}

pub async fn messages_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // Rate limit
    {
        let client_ip = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&client_ip, 60, std::time::Duration::from_secs(60)) {
            return (axum::http::StatusCode::TOO_MANY_REQUESTS, "Too many requests").into_response();
        }
    }

    // Server auth check
    if !state.server_auth.is_empty() {
        let header_auth = headers
            .get("x-auth")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let query_auth = params.get("auth").map(|s| s.as_str()).unwrap_or("");
        let body_auth = body.get("auth").and_then(|v| v.as_str()).unwrap_or("");
        let auth = if !header_auth.is_empty() { header_auth }
            else if !query_auth.is_empty() { query_auth }
            else { body_auth };
        if !crate::relay::auth::constant_time_eq(auth, &state.server_auth) {
            return Json(json!({
                "jsonrpc": "2.0",
                "id": body.get("id").cloned().unwrap_or(Value::Null),
                "error": {"code": -32001, "message": "Invalid server password"}
            })).into_response();
        }
    }

    let mcp_session_id = match params.get("sessionId") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return (axum::http::StatusCode::BAD_REQUEST, "Missing sessionId").into_response(),
    };

    let sse_tx = {
        let channels = state.mcp_sse_channels.read().await;
        match channels.get(&mcp_session_id).cloned() {
            Some(tx) => tx,
            None => return (axum::http::StatusCode::NOT_FOUND, "SSE session not found").into_response(),
        }
    };

    let state_clone = state.clone();
    let body_clone = body.clone();
    let headers_clone = headers.clone();
    let url_token = params.get("token").cloned();

    tokio::spawn(async move {
        let result = process_mcp_request(&state_clone, &headers_clone, url_token, &body_clone).await;
        let response_text = serde_json::to_string(&result).unwrap_or_default();
        let _ = sse_tx.send(response_text);
    });

    (axum::http::StatusCode::ACCEPTED, "").into_response()
}

async fn process_mcp_request(
    state: &Arc<SharedState>,
    headers: &axum::http::HeaderMap,
    url_token: Option<String>,
    body: &Value,
) -> Value {
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let request_id = body.get("id").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "shell-remote",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "tools": {}
                }
            }
        }),


        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "tools": [
                    {
                        "name": "exec_remote",
                        "description": "Execute a shell command on the REMOTE TARGET MACHINE (NOT the AI's local sandbox). Returns stdout, stderr, and exit code.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "cmd": {"type": "string", "description": "The shell command to execute on the remote machine"},
                                "timeout_ms": {"type": "number", "description": "Optional timeout in milliseconds"}
                            },
                            "required": ["cmd"]
                        }
                    },
                    {
                        "name": "exec_remote_start",
                        "description": "Start an interactive command session ON THE REMOTE TARGET MACHINE. Returns exec_id and initial output.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"cmd": {"type": "string", "description": "Shell command to execute"}},
                            "required": ["cmd"]
                        }
                    },
                    {
                        "name": "exec_remote_input",
                        "description": "Send input to a running exec session ON THE REMOTE TARGET MACHINE and get accumulated output.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "exec_id": {"type": "string", "description": "ID of the exec session"},
                                "data": {"type": "string", "description": "Text to write to stdin"}
                            },
                            "required": ["exec_id", "data"]
                        }
                    },
                    {
                        "name": "exec_remote_close",
                        "description": "Close an exec session ON THE REMOTE TARGET MACHINE. Kills the process if still running.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"exec_id": {"type": "string", "description": "ID of the exec session to close"}},
                            "required": ["exec_id"]
                        }
                    },
                    {
                        "name": "exec_remote_list",
                        "description": "List all active exec sessions on the REMOTE TARGET MACHINE with their status.",
                        "inputSchema": {"type": "object", "properties": {}}
                    },
                    {
                        "name": "file_remote_read",
                        "description": "Read the content of a file from the REMOTE TARGET MACHINE.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"path": {"type": "string", "description": "Absolute path to the file on the remote machine"}},
                            "required": ["path"]
                        }
                    },
                    {
                        "name": "file_remote_write",
                        "description": "Write content to a file on the REMOTE TARGET MACHINE.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "description": "Absolute path to the file on the remote machine"},
                                "content": {"type": "string", "description": "Content to write to the file"}
                            },
                            "required": ["path", "content"]
                        }
                    },
                    {
                        "name": "file_remote_list",
                        "description": "List contents of a directory on the REMOTE TARGET MACHINE.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"path": {"type": "string", "description": "Absolute path to the directory on the remote machine"}},
                            "required": ["path"]
                        }
                    }
                ]
            }
        }),

        "tools/call" => {
            let empty_obj = json!({});
            let arguments = body.get("params").and_then(|p| p.get("arguments")).unwrap_or(&empty_obj);
            let tool_name = body.get("params").and_then(|p| p.get("name").and_then(|n| n.as_str())).unwrap_or("");

            let auth = {
                let mut result = None;
                if let Some(t) = crate::relay::auth::extract_bearer_token(&headers) {
                    result = state.sessions.authenticate(&t).await;
                }
                if result.is_none() {
                    if let Some(ref t) = url_token {
                        if !t.is_empty() { result = state.sessions.authenticate(t).await; }
                    }
                }
                if result.is_none() {
                    if let Some(t) = arguments.get("token").and_then(|v| v.as_str()) {
                        if !t.is_empty() { result = state.sessions.authenticate(t).await; }
                    }
                }
                result
            };

            let (session_id, permission) = match auth {
                Some(r) => r,
                None => return json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32001,"message":"Invalid token"}}),
            };

            let write_tools = [
                "exec_remote", "exec_remote_start", "exec_remote_input",
                "exec_remote_close", "file_remote_write", "file_remote_rename",
                "file_remote_delete",
            ];
            if write_tools.contains(&tool_name) && permission == Permission::ReadOnly {
                return json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32002,"message":"Read-only token cannot call write-type tools"}});
            }

            if matches!(tool_name, "exec_remote_start" | "exec_remote_input" | "exec_remote_close" | "exec_remote_list") {
                let request_id = Uuid::new_v4().to_string();
                let proto_message: crate::proto::Message;
                let exec_timeout: u64;

                match tool_name {
                    "exec_remote_start" => {
                        let cmd = arguments.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
                        proto_message = crate::proto::Message { msg_type: "mcp:exec_start".to_string(), session_id: session_id.clone(), payload: json!({"cmd":cmd,"_mcp_request_id":request_id}) };
                        exec_timeout = 10000;
                    }
                    "exec_remote_input" => {
                        let exec_id = arguments.get("exec_id").and_then(|v| v.as_str()).unwrap_or("");
                        let data = arguments.get("data").and_then(|v| v.as_str()).unwrap_or("");
                        let data_b64 = crate::agent::fs::encode_b64(data.as_bytes());
                        proto_message = crate::proto::Message { msg_type: "mcp:exec_input".to_string(), session_id: session_id.clone(), payload: json!({"exec_id":exec_id,"data_b64":data_b64,"_mcp_request_id":request_id}) };
                        exec_timeout = 10000;
                    }
                    "exec_remote_close" => {
                        let exec_id = arguments.get("exec_id").and_then(|v| v.as_str()).unwrap_or("");
                        proto_message = crate::proto::Message { msg_type: "mcp:exec_close".to_string(), session_id: session_id.clone(), payload: json!({"exec_id":exec_id,"_mcp_request_id":request_id}) };
                        exec_timeout = 5000;
                    }
                    "exec_remote_list" => {
                        proto_message = crate::proto::Message { msg_type: "mcp:exec_list".to_string(), session_id: session_id.clone(), payload: json!({"_mcp_request_id":request_id}) };
                        exec_timeout = 5000;
                    }
                    _ => unreachable!(),
                }

                let (tx, rx) = oneshot::channel();
                { state.pending_mcp.write().await.insert(request_id.clone(), (session_id.clone(), tx)); }
                {
                    let agent_tx_option = { state.agent_broadcast.read().await.get(&session_id).and_then(|cm| cm.agent.clone()) };
                    match agent_tx_option {
                        Some(agent_tx) => {
                            let text = serde_json::to_string(&proto_message).unwrap_or_default();
                            let _ = agent_tx.send(text);
                        }
                        None => {
                            state.pending_mcp.write().await.remove(&request_id);
                            return json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":"Error: No agent connected for this session"}],"isError":true}});
                        }
                    }
                }

                match tokio::time::timeout(tokio::time::Duration::from_millis(exec_timeout), rx).await {
                    Ok(Ok(result)) => {
                        let value: Value = serde_json::from_str(&result).unwrap_or_default();
                        let stdout = value.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                        let stderr = value.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
                        let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let exit_code = value.get("exit_code").and_then(|v| v.as_i64());
                        let mut text = format!("Status: {}\n", status);
                        if let Some(code) = exit_code { text.push_str(&format!("Exit code: {}\n", code)); }
                        if !stdout.is_empty() {
                            if let Some(decoded) = crate::agent::fs::decode_b64(stdout) {
                                if let Ok(s) = String::from_utf8(decoded) { text.push_str(&s); }
                            }
                        }
                        if !stderr.is_empty() {
                            if let Some(decoded) = crate::agent::fs::decode_b64(stderr) {
                                if let Ok(s) = String::from_utf8(decoded) { text.push_str(&format!("\n[stderr]\n{}", s)); }
                            }
                        }
                        let exec_id_val = value.get("exec_id").and_then(|v| v.as_str()).unwrap_or("");
                        json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":text.trim()}],"exec_id":exec_id_val}})
                    }
                    _ => {
                        state.pending_mcp.write().await.remove(&request_id);
                        json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":"Error: Request timed out or agent disconnected"}],"isError":true}})
                    }
                }
            } else {
                let (msg_type, payload) = match tool_name {
                    "exec_remote" => {
                        let cmd = arguments.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
                        let timeout_ms = arguments.get("timeout_ms").and_then(|v| v.as_u64());
                        let p = if let Some(t) = timeout_ms { json!({"cmd":cmd,"timeout_ms":t}) } else { json!({"cmd":cmd}) };
                        ("mcp:exec", p)
                    }
                    "file_remote_read" => {
                        let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        ("fs:read", json!({"path":path}))
                    }
                    "file_remote_write" => {
                        let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        let content = arguments.get("content").and_then(|v| v.as_str()).unwrap_or("");
                        ("fs:write", json!({"path":path,"content":content}))
                    }
                    "file_remote_list" => {
                        let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        ("fs:list", json!({"path":path}))
                    }
                    _ => return json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":format!("Unknown tool: {}",tool_name)}}),
                };

                let (response_tx, mut response_rx) = oneshot::channel::<String>();
                let mcp_request_id = Uuid::new_v4().to_string();
                { state.pending_mcp.write().await.insert(mcp_request_id.clone(), (session_id.clone(), response_tx)); }

                let _mcp_request_msg = json!({"_mcp_request_id":&mcp_request_id});
                let mut full_payload = payload.clone();
                if let Some(obj) = full_payload.as_object_mut() { obj.insert("_mcp_request_id".to_string(), json!(mcp_request_id)); }
                let proto_msg = ProtoMessage { msg_type: msg_type.to_string(), session_id: session_id.clone(), payload: full_payload };

                {
                    let agent_tx_option = { state.agent_broadcast.read().await.get(&session_id).and_then(|cm| cm.agent.clone()) };
                    match agent_tx_option {
                        Some(agent_tx) => {
                            let text = serde_json::to_string(&proto_msg).unwrap_or_default();
                            let _ = agent_tx.send(text);
                        }
                        None => {
                            state.pending_mcp.write().await.remove(&mcp_request_id);
                            return json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":"Error: No agent connected for this session"}],"isError":true}});
                        }
                    }
                }

                match tokio::time::timeout(tokio::time::Duration::from_secs(60), &mut response_rx).await {
                    Ok(result) => {
                        let text = result.unwrap_or_default();
                        let value: Value = serde_json::from_str(&text).unwrap_or_default();
                        let stdout = value.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                        let stderr = value.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
                        let exit_code = value.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
                        let mut text = String::new();
                        if !stdout.is_empty() {
                            if let Some(decoded) = crate::agent::fs::decode_b64(stdout) {
                                if let Ok(s) = String::from_utf8(decoded) { text.push_str(&s); }
                            }
                        }
                        if !stderr.is_empty() {
                            if let Some(decoded) = crate::agent::fs::decode_b64(stderr) {
                                if let Ok(s) = String::from_utf8(decoded) { text.push_str(&format!("\n[stderr]\n{}", s)); }
                            }
                        }
                        if text.is_empty() { text = format!("Exit code: {}", exit_code); }
                        json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":text.trim()}]}})
                    }
                    _ => {
                        state.pending_mcp.write().await.remove(&mcp_request_id);
                        json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":"Error: Request timed out waiting for agent response"}],"isError":true}})
                    }
                }
            }
        },

        _ => json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": format!("Unknown method: {}", method)}
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::session::SessionRegistry;
    use crate::relay::RateLimiter;
    use std::sync::Arc;
    use tokio::sync::{oneshot, RwLock, mpsc};
    use axum::extract::{Query, State};
    use axum::response::IntoResponse;
    use std::collections::HashMap;
    use serde_json::{json, Value};

    fn make_state() -> Arc<SharedState> {
        Arc::new(SharedState {
            sessions: SessionRegistry::new(),
            agent_broadcast: RwLock::new(HashMap::new()),
            pending_mcp: RwLock::new(HashMap::new()),
            last_activity: RwLock::new(HashMap::new()),
            server_auth: String::new(),
            bin_dir: None,
            agent_event_buffers: RwLock::new(HashMap::new()),
            rate_limiter: RwLock::new(RateLimiter::new()),
            max_upload_size: 100 * 1024 * 1024,
            mcp_sse_channels: RwLock::new(HashMap::new()),
        })
    }

    async fn mcp_send_and_recv(state: &Arc<SharedState>, params: HashMap<String, String>, body: Value) -> Value {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let sid = uuid::Uuid::new_v4().to_string();
        state.mcp_sse_channels.write().await.insert(sid.clone(), tx);
        let mut p = params;
        p.insert("sessionId".into(), sid);
        let resp = messages_handler(State(state.clone()), axum::http::HeaderMap::new(), Query(p), axum::Json(body)).await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
        let raw = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[tokio::test]
    async fn test_sse_handler_valid_token_returns_200() {
        let state = make_state();
        let response = sse_handler(State(state), axum::http::HeaderMap::new(), Query(HashMap::new())).await.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn test_sse_handler_no_token_returns_200() {
        let state = make_state();
        let response = sse_handler(State(state), axum::http::HeaderMap::new(), Query(HashMap::new())).await.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn test_messages_handler_initialize() {
        let state = make_state();
        let r = mcp_send_and_recv(&state, HashMap::new(), json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).await;
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
    }

    #[tokio::test]
    async fn test_messages_handler_tools_list() {
        let state = make_state();
        let r = mcp_send_and_recv(&state, HashMap::new(), json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 8);
    }

    #[tokio::test]
    async fn test_messages_handler_unknown_method() {
        let state = make_state();
        let r = mcp_send_and_recv(&state, HashMap::new(), json!({"jsonrpc":"2.0","id":3,"method":"unknown"})).await;
        assert_eq!(r["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn test_messages_handler_invalid_token_returns_error() {
        let state = make_state();
        let r = mcp_send_and_recv(&state, HashMap::from([("token".into(),"bad".into())]),
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"file_remote_read","arguments":{"path":"/etc/hostname"}}})).await;
        assert_eq!(r["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn test_messages_handler_exec_remote_without_agent() {
        let state = make_state();
        let (_sid, tokens) = state.sessions.register(None, "rw").await;
        let r = mcp_send_and_recv(&state, HashMap::from([("token".into(), tokens[0].0.clone())]),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"exec_remote","arguments":{"cmd":"echo hello"}}})).await;
        assert!(r["result"]["isError"].as_bool().unwrap_or(false));
    }

    #[tokio::test]
    async fn test_messages_handler_file_remote_read_without_agent() {
        let state = make_state();
        let (_sid, tokens) = state.sessions.register(None, "rw").await;
        let r = mcp_send_and_recv(&state, HashMap::from([("token".into(), tokens[0].0.clone())]),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"file_remote_read","arguments":{"path":"/etc/hostname"}}})).await;
        assert!(r["result"]["isError"].as_bool().unwrap_or(false));
    }
}
