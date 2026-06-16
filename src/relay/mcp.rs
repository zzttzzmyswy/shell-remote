use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt as _;
use tokio_stream::Stream;
use uuid::Uuid;

use crate::proto::{Message as ProtoMessage, Permission};
use crate::relay::SharedState;

pub(crate) struct SseCleanup {
    pub inner: UnboundedReceiverStream<String>,
    pub state: Arc<SharedState>,
    pub sid: String,
    pub on_drop: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl tokio_stream::Stream for SseCleanup {
    type Item = String;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<String>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl Drop for SseCleanup {
    fn drop(&mut self) {
        let state = self.state.clone();
        let sid = self.sid.clone();
        let on_drop = self.on_drop.take();
        tokio::spawn(async move {
            state.sse_sessions.write().await.remove(&sid);
            if let Some(f) = on_drop {
                f();
            }
        });
    }
}

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
                tokio_stream::once(Ok::<_, Infallible>(
                    Event::default()
                        .event("error")
                        .data(r#"{"code":"AUTH_INVALID_PASSWORD","message":"Invalid server password"}"#),
                )),
            )
            .into_response();
        }
    }

    let mcp_session_id = Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::unbounded_channel::<String>();

    {
        let mut channels = state.sse_sessions.write().await;
        channels.insert(mcp_session_id.clone(), tx);
    }

    let sid_for_stream = mcp_session_id.clone();
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(Event::default()
            .event("endpoint")
            .data(format!("/agent/mcp/messages?sessionId={}", sid_for_stream)));

        yield Ok::<_, Infallible>(Event::default()
            .event("connected")
            .data("{}"));

        let rx_stream = SseCleanup {
            inner: UnboundedReceiverStream::new(rx),
            state: state.clone(),
            sid: mcp_session_id,
            on_drop: None,
        };
        let mut rx_stream = rx_stream;
        while let Some(msg) = tokio_stream::StreamExt::next(&mut rx_stream).await {
            yield Ok::<_, Infallible>(Event::default().event("message").data(msg));
        }
    };

    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(5)))
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
        let channels = state.sse_sessions.read().await;
        match channels.get(&mcp_session_id).cloned() {
            Some(tx) => tx,
            None => return (axum::http::StatusCode::NOT_FOUND, "SSE session not found").into_response(),
        }
    };

    let state_clone = state.clone();
    let body_clone = body.clone();
    let url_token = params.get("token").cloned();

    tokio::spawn(async move {
        let result = process_mcp_request(&state_clone, url_token, &body_clone).await;
        let response_text = serde_json::to_string(&result).unwrap_or_default();
        let _ = sse_tx.send(response_text);
    });

    (axum::http::StatusCode::ACCEPTED, "").into_response()
}

async fn process_mcp_request(
    state: &Arc<SharedState>,
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
                        "description": "Execute a shell command on the remote target machine. Returns stdout, stderr, and exit code.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "token": {"type": "string", "description": "Session token for authentication"},
                                "cmd": {"type": "string", "description": "The shell command to execute on the remote machine"},
                                "timeout_ms": {"type": "number", "description": "Optional timeout in milliseconds (default 30s)"}
                            },
                            "required": ["token", "cmd"]
                        }
                    }
                ]
            }
        }),

        "tools/call" => {
            let tool_name = body.get("params").and_then(|p| p.get("name").and_then(|n| n.as_str())).unwrap_or("");
            if tool_name != "exec_remote" {
                return json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":format!("Unknown tool: {}",tool_name)}});
            }

            let empty_obj = json!({});
            let arguments = body.get("params").and_then(|p| p.get("arguments")).unwrap_or(&empty_obj);

            // Token from tool arguments (primary), fallback to query param
            let token = arguments.get("token").and_then(|v| v.as_str())
                .or_else(|| url_token.as_deref())
                .unwrap_or("");

            let (session_id, permission) = match state.sessions.authenticate(token).await {
                Some(r) => r,
                None => return json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32001,"message":"Invalid token"}}),
            };

            if permission == Permission::ReadOnly {
                return json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32002,"message":"Read-only token cannot call exec_remote"}});
            }

            let cmd = arguments.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
            let timeout_ms = arguments.get("timeout_ms").and_then(|v| v.as_u64());

            let mcp_req_id = Uuid::new_v4().to_string();
            let payload = if let Some(t) = timeout_ms {
                json!({"cmd":cmd,"timeout_ms":t,"_mcp_request_id":mcp_req_id})
            } else {
                json!({"cmd":cmd,"_mcp_request_id":mcp_req_id})
            };

            let proto_msg = ProtoMessage {
                msg_type: "mcp:exec".to_string(),
                session_id: session_id.clone(),
                payload,
            };

            let (tx, rx) = oneshot::channel();
            { state.pending_mcp.write().await.insert(mcp_req_id.clone(), (session_id.clone(), tx)); }

            {
                let agent_tx_option = { state.agent_broadcast.read().await.get(&session_id).and_then(|cm| cm.agent.clone()) };
                match agent_tx_option {
                    Some(agent_tx) => {
                        let _ = agent_tx.send(serde_json::to_string(&proto_msg).unwrap_or_default());
                    }
                    None => {
                        state.pending_mcp.write().await.remove(&mcp_req_id);
                        return json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":"Error: No agent connected for this session"}],"isError":true}});
                    }
                }
            }

            let timeout_ms_val = timeout_ms.unwrap_or(300_000).min(1_800_000);
            let timeout_dur = std::time::Duration::from_millis(timeout_ms_val);
            match tokio::time::timeout(timeout_dur, rx).await {
                Ok(Ok(result)) => {
                    let value: Value = serde_json::from_str(&result).unwrap_or_default();
                    let stdout = value.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                    let stderr = value.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
                    let exit_code = value.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
                    let mut text = String::new();
                    if !stdout.is_empty() {
                        text.push_str(stdout);
                    }
                    if !stderr.is_empty() {
                        text.push_str(&format!("\n[stderr]\n{}", stderr));
                    }
                    if text.is_empty() { text = format!("Exit code: {}", exit_code); }
                    json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":text.trim()}]}})
                }
                _ => {
                    state.pending_mcp.write().await.remove(&mcp_req_id);
                    json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":"Error: Request timed out or agent disconnected"}],"isError":true}})
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
            sse_sessions: RwLock::new(HashMap::new()),
        })
    }

    async fn mcp_send_and_recv(state: &Arc<SharedState>, params: HashMap<String, String>, body: Value) -> Value {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let sid = uuid::Uuid::new_v4().to_string();
        state.sse_sessions.write().await.insert(sid.clone(), tx);
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
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 1);
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
        let r = mcp_send_and_recv(&state, HashMap::new(),
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"exec_remote","arguments":{"token":"bad","cmd":"echo hello"}}})).await;
        assert_eq!(r["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn test_messages_handler_exec_remote_without_agent() {
        let state = make_state();
        let (_sid, tokens) = state.sessions.register(None, "rw").await;
        let r = mcp_send_and_recv(&state, HashMap::new(),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"exec_remote","arguments":{"token":tokens[0].0,"cmd":"echo hello"}}})).await;
        assert!(r["result"]["isError"].as_bool().unwrap_or(false));
    }
}
