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
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tokio_stream::StreamExt as _;
use uuid::Uuid;

use crate::proto::{Message as ProtoMessage, Permission};
use crate::relay::SharedState;

pub(crate) struct SseCleanup {
    pub inner: ReceiverStream<String>,
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
    let server_auth = state.server_auth.read().await.clone();
    if !server_auth.is_empty() {
        let header_auth = headers
            .get("x-auth")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let query_auth = params.get("auth").map(|s| s.as_str()).unwrap_or("");
        let auth = if header_auth.is_empty() {
            query_auth
        } else {
            header_auth
        };
        if !crate::relay::auth::constant_time_eq(auth, &server_auth) {
            return Sse::new(tokio_stream::once(Ok::<_, Infallible>(
                Event::default().event("error").data(
                    r#"{"code":"AUTH_INVALID_PASSWORD","message":"Invalid server password"}"#,
                ),
            )))
            .into_response();
        }
    }

    let mcp_session_id = Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::channel::<String>(crate::relay::SSE_CHANNEL_CAPACITY);

    {
        let mut channels = state.sse_sessions.write().await;
        channels.insert(mcp_session_id.clone(), tx);
    }

    let sid_for_stream = mcp_session_id.clone();
    let stream = async_stream::stream! {
        yield Ok::<_, Infallible>(Event::default()
            .event("endpoint")
            .data(format!("/agent/mcp/messages?sessionId={}", sid_for_stream)));

        let rx_stream = SseCleanup {
            inner: ReceiverStream::new(rx),
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
            return (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                "Too many requests",
            )
                .into_response();
        }
    }

    // Server auth check
    let server_auth = state.server_auth.read().await.clone();
    if !server_auth.is_empty() {
        let header_auth = headers
            .get("x-auth")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let query_auth = params.get("auth").map(|s| s.as_str()).unwrap_or("");
        let body_auth = body.get("auth").and_then(|v| v.as_str()).unwrap_or("");
        let auth = if !header_auth.is_empty() {
            header_auth
        } else if !query_auth.is_empty() {
            query_auth
        } else {
            body_auth
        };
        if !crate::relay::auth::constant_time_eq(auth, &server_auth) {
            return Json(json!({
                "jsonrpc": "2.0",
                "id": body.get("id").cloned().unwrap_or(Value::Null),
                "error": {"code": -32001, "message": "Invalid server password"}
            }))
            .into_response();
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
            None => {
                return (axum::http::StatusCode::NOT_FOUND, "SSE session not found").into_response()
            }
        }
    };

    let state_clone = state.clone();
    let body_clone = body.clone();
    let url_token = params.get("token").cloned();

    tokio::spawn(async move {
        if let Some(result) = process_mcp_request(&state_clone, url_token, &body_clone).await {
            let response_text = serde_json::to_string(&result).unwrap_or_default();
            crate::relay::ws::deliver(&sse_tx, "message", response_text);
        }
    });

    (axum::http::StatusCode::ACCEPTED, "").into_response()
}

async fn process_mcp_request(
    state: &Arc<SharedState>,
    url_token: Option<String>,
    body: &Value,
) -> Option<Value> {
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");

    // Per JSON-RPC 2.0, a request without an `id` is a notification and the
    // server MUST NOT respond. MCP clients send `notifications/initialized`
    // (and other notifications) here; responding with an error would force
    // `id: null`, which the MCP Python SDK's pydantic model (id: int|str)
    // rejects. Drop notifications silently instead.
    let request_id = match body.get("id").cloned() {
        Some(id) => id,
        None => return None,
    };

    Some(match method {
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
                        "name": "shell_remote",
                        "description": "Execute a shell command on the remote machine; returns stdout, stderr, exit code.\n\nFILE TRANSFER (bytes bypass LLM context — use for files of any size):\n  upload:   curl -T <localfile> -H \"X-SR-Token: <token>\" <origin>/agent/mcp/put?path=<remotepath>\n  download: curl -H \"X-SR-Token: <token>\" <origin>/agent/mcp/get?path=<remotepath> -o <localfile>\n<origin> is this MCP server's origin (scheme://host:port, the base of /agent/mcp). <token> is the same session token used for this tool; upload needs read-write, download allows read-only.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "token": {"type": "string", "description": "shell_remote token for authentication (session token shown at agent startup)"},
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
            let tool_name = body
                .get("params")
                .and_then(|p| p.get("name").and_then(|n| n.as_str()))
                .unwrap_or("");
            if tool_name != "shell_remote" {
                return Some(json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":format!("Unknown tool: {}",tool_name)}}));
            }

            let empty_obj = json!({});
            let arguments = body
                .get("params")
                .and_then(|p| p.get("arguments"))
                .unwrap_or(&empty_obj);

            // Token from tool arguments (primary), fallback to query param
            let token = arguments
                .get("token")
                .and_then(|v| v.as_str())
                .or(url_token.as_deref())
                .unwrap_or("");

            // Parse the command up front so every outcome branch (including
            // rejections) can be audited. Without this, a rejected call would
            // have no cmd to log.
            let cmd = arguments.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
            let requested_timeout_ms = arguments.get("timeout_ms").and_then(|v| v.as_u64());

            let (session_id, permission) = match state.sessions.authenticate(token).await {
                Some(r) => r,
                None => {
                    // No session resolved → can't attribute to a per-session
                    // audit file, so nothing to record. (Invalid-token
                    // attempts aren't "commands run" on any session.)
                    return Some(json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32001,"message":"Invalid token"}}))
                }
            };

            // Default 30s, cap 300s (matches the tool description). Always
            // forward the effective timeout so the agent kills the command when
            // the relay gives up — otherwise long commands outlive the MCP
            // client's own timeout and surface as i/o errors.
            let timeout_ms_val = requested_timeout_ms.unwrap_or(30_000).min(300_000);

            if permission == Permission::ReadOnly {
                audit_mcp_call(
                    state, &session_id, token, permission, cmd, timeout_ms_val,
                    0, "rejected_readonly", None, "", "",
                );
                return Some(json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32002,"message":"Read-only token cannot call shell_remote"}}));
            }

            let mcp_req_id = Uuid::new_v4().to_string();
            let payload = json!({
                "cmd": cmd,
                "timeout_ms": timeout_ms_val,
                "_mcp_request_id": mcp_req_id
            });

            let proto_msg = ProtoMessage {
                msg_type: "mcp:exec".to_string(),
                session_id: session_id.clone(),
                payload,
            };

            let (tx, rx) = oneshot::channel();
            {
                state
                    .pending_mcp
                    .write()
                    .await
                    .insert(mcp_req_id.clone(), (session_id.clone(), tx));
            }

            {
                let agent_tx_option = {
                    state
                        .agent_broadcast
                        .read()
                        .await
                        .get(&session_id)
                        .and_then(|cm| cm.agent.clone())
                };
                match agent_tx_option {
                    Some(agent_tx) => {
                        crate::relay::ws::deliver(
                            &agent_tx,
                            "mcp:exec",
                            serde_json::to_string(&proto_msg).unwrap_or_default(),
                        );
                    }
                    None => {
                        state.pending_mcp.write().await.remove(&mcp_req_id);
                        audit_mcp_call(
                            state, &session_id, token, permission, cmd, timeout_ms_val,
                            0, "no_agent", None, "", "",
                        );
                        return Some(json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":"Error: No agent connected for this session"}],"isError":true}}));
                    }
                }
            }

            let timeout_dur = std::time::Duration::from_millis(timeout_ms_val);
            let started = std::time::Instant::now();
            match tokio::time::timeout(timeout_dur, rx).await {
                Ok(Ok(result)) => {
                    let value: Value = serde_json::from_str(&result).unwrap_or_default();
                    let stdout = value.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                    let stderr = value.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
                    let exit_code = value.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
                    let duration_ms = started.elapsed().as_millis() as u64;
                    audit_mcp_call(
                        state, &session_id, token, permission, cmd, timeout_ms_val,
                        duration_ms, "ok", Some(exit_code), stdout, stderr,
                    );
                    let mut text = String::new();
                    if !stdout.is_empty() {
                        text.push_str(stdout);
                    }
                    if !stderr.is_empty() {
                        text.push_str(&format!("\n[stderr]\n{}", stderr));
                    }
                    if text.is_empty() {
                        text = format!("Exit code: {}", exit_code);
                    }
                    json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":text.trim()}]}})
                }
                _ => {
                    state.pending_mcp.write().await.remove(&mcp_req_id);
                    let duration_ms = started.elapsed().as_millis() as u64;
                    audit_mcp_call(
                        state, &session_id, token, permission, cmd, timeout_ms_val,
                        duration_ms, "timeout", None, "", "",
                    );
                    json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":"Error: Request timed out or agent disconnected"}],"isError":true}})
                }
            }
        }

        _ => json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": format!("Unknown method: {}", method)}
        }),
    })
}

/// Append one MCP command-audit line for a session's `.audit.jsonl` file.
/// No-op when `--record-dir` is unset (recorder is `None`). The MCP hot path
/// never blocks on auditing: the recorder's audit writer is fed via an
/// unbounded channel, so a slow disk can't stall a tool call.
#[allow(clippy::too_many_arguments)]
fn audit_mcp_call(
    state: &SharedState,
    session_id: &str,
    token: &str,
    permission: Permission,
    cmd: &str,
    timeout_ms: u64,
    duration_ms: u64,
    status: &str,
    exit_code: Option<i64>,
    stdout: &str,
    stderr: &str,
) {
    use crate::relay::recorder::{
        truncate_output, unix_ms, unix_ms_to_iso, token_prefix, AuditLine, AUDIT_OUTPUT_CAP,
    };
    let Some(recorder) = &state.recorder else { return; };
    let (stdout_t, stdout_len) = truncate_output(stdout, AUDIT_OUTPUT_CAP);
    let (stderr_t, stderr_len) = truncate_output(stderr, AUDIT_OUTPUT_CAP);
    let perm_str = match permission {
        Permission::ReadWrite => "rw",
        Permission::ReadOnly => "ro",
    };
    let ms = unix_ms();
    recorder.audit_mcp(
        session_id,
        AuditLine {
            ts: unix_ms_to_iso(ms),
            unix_ms: ms,
            session_id: session_id.to_string(),
            token_prefix: token_prefix(token),
            permission: perm_str.to_string(),
            cmd: cmd.to_string(),
            timeout_ms,
            duration_ms,
            status: status.to_string(),
            exit_code,
            stdout_len,
            stderr_len,
            stdout: stdout_t,
            stderr: stderr_t,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::session::SessionRegistry;
    use crate::relay::RateLimiter;
    use axum::extract::{Query, State};
    use axum::response::IntoResponse;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot, RwLock};

    fn make_state() -> Arc<SharedState> {
        Arc::new(SharedState::new(String::new(), 100 * 1024 * 1024, None, String::new(), String::new(), None))
    }

    fn make_state_with_recorder(dir: std::path::PathBuf) -> Arc<SharedState> {
        let recorder = std::sync::Arc::new(crate::relay::recorder::Recorder::new(dir));
        Arc::new(SharedState::new(
            String::new(),
            100 * 1024 * 1024,
            None,
            String::new(),
            String::new(),
            Some(recorder),
        ))
    }

    fn audit_tempdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sr-mcp-audit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Read the single `.audit.jsonl` line written for `sid` under `dir`.
    async fn read_audit_line(dir: &std::path::Path, sid: &str) -> serde_json::Value {
        let entry = std::fs::read_dir(dir)
            .unwrap()
            .into_iter()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(&format!("{}_", sid)))
                    .unwrap_or(false)
            })
            .expect("audit file for session must exist");
        let content = tokio::fs::read_to_string(&entry).await.unwrap();
        let line = content.lines().next().expect("audit file non-empty");
        serde_json::from_str(line).unwrap()
    }

    async fn mcp_send_and_recv(
        state: &Arc<SharedState>,
        params: HashMap<String, String>,
        body: Value,
    ) -> Value {
        let (tx, mut rx) = mpsc::channel::<String>(crate::relay::SSE_CHANNEL_CAPACITY);
        let sid = uuid::Uuid::new_v4().to_string();
        state.sse_sessions.write().await.insert(sid.clone(), tx);
        let mut p = params;
        p.insert("sessionId".into(), sid);
        let resp = messages_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Query(p),
            axum::Json(body),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
        let raw = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[tokio::test]
    async fn test_sse_handler_valid_token_returns_200() {
        let state = make_state();
        let response = sse_handler(
            State(state),
            axum::http::HeaderMap::new(),
            Query(HashMap::new()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn test_sse_handler_no_token_returns_200() {
        let state = make_state();
        let response = sse_handler(
            State(state),
            axum::http::HeaderMap::new(),
            Query(HashMap::new()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn test_messages_handler_initialize() {
        let state = make_state();
        let r = mcp_send_and_recv(
            &state,
            HashMap::new(),
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        )
        .await;
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
    }

    #[tokio::test]
    async fn test_messages_handler_tools_list() {
        let state = make_state();
        let r = mcp_send_and_recv(
            &state,
            HashMap::new(),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(
            r["result"]["tools"][0]["name"], "shell_remote",
            "tool must be named shell_remote"
        );
    }

    #[tokio::test]
    async fn test_messages_handler_unknown_method() {
        let state = make_state();
        let r = mcp_send_and_recv(
            &state,
            HashMap::new(),
            json!({"jsonrpc":"2.0","id":3,"method":"unknown"}),
        )
        .await;
        assert_eq!(r["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn test_messages_handler_invalid_token_returns_error() {
        let state = make_state();
        let r = mcp_send_and_recv(&state, HashMap::new(),
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"shell_remote","arguments":{"token":"bad","cmd":"echo hello"}}})).await;
        assert_eq!(r["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn test_messages_handler_shell_remote_without_agent() {
        let state = make_state();
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
        let r = mcp_send_and_recv(&state, HashMap::new(),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"shell_remote","arguments":{"token":tokens[0].0,"cmd":"echo hello"}}})).await;
        assert!(r["result"]["isError"].as_bool().unwrap_or(false));
    }

    #[tokio::test]
    async fn test_mcp_call_audited_no_agent() {
        // When --record-dir is enabled, a tool call that reaches the dispatch
        // stage but finds no connected agent must still be audited (status
        // "no_agent"), so the attempt is visible in the audit log.
        let dir = audit_tempdir();
        let state = make_state_with_recorder(dir.clone());
        let r = state.sessions.register(None, "rw", Some("auditbot".to_string())).await.unwrap();
            let _sid = r.session_id;
            let tokens = r.tokens;
        let r = mcp_send_and_recv(
            &state,
            HashMap::new(),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"shell_remote","arguments":{"token":tokens[0].0,"cmd":"echo hello"}}}),
        )
        .await;
        assert!(r["result"]["isError"].as_bool().unwrap_or(false));

        state.recorder.as_ref().unwrap().close("auditbot");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        let line = read_audit_line(&dir, "auditbot").await;
        assert_eq!(line["session_id"], "auditbot");
        assert_eq!(line["cmd"], "echo hello");
        assert_eq!(line["status"], "no_agent");
        assert_eq!(line["permission"], "rw");
        assert_eq!(line["token_prefix"], &tokens[0].0[..8]);
    }

    #[tokio::test]
    async fn test_mcp_call_audited_rejected_readonly() {
        // A read-only token attempting shell_remote is audited as
        // "rejected_readonly" (the command was attempted, not executed).
        let dir = audit_tempdir();
        let state = make_state_with_recorder(dir.clone());
        let r = state.sessions.register(None, "ro", Some("robot".to_string())).await.unwrap();
            let _sid = r.session_id;
            let tokens = r.tokens;
        let r = mcp_send_and_recv(
            &state,
            HashMap::new(),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"shell_remote","arguments":{"token":tokens[0].0,"cmd":"ls"}}}),
        )
        .await;
        assert_eq!(r["error"]["code"], -32002);

        state.recorder.as_ref().unwrap().close("robot");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        let line = read_audit_line(&dir, "robot").await;
        assert_eq!(line["session_id"], "robot");
        assert_eq!(line["cmd"], "ls");
        assert_eq!(line["status"], "rejected_readonly");
        assert_eq!(line["permission"], "ro");
    }

    #[tokio::test]
    async fn test_mcp_call_not_audited_when_recording_disabled() {
        // No --record-dir → recorder is None → no audit file is ever written.
        let dir = audit_tempdir();
        let state = make_state(); // recorder = None
        let r = state.sessions.register(None, "rw", Some("noaudit".to_string())).await.unwrap();
            let _sid = r.session_id;
            let tokens = r.tokens;
        let _ = mcp_send_and_recv(
            &state,
            HashMap::new(),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"shell_remote","arguments":{"token":tokens[0].0,"cmd":"echo hi"}}}),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        // Directory (a temp dir we created but the relay never writes to) has
        // no audit files.
        let any = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.path().to_string_lossy().ends_with(".audit.jsonl"));
        assert!(!any, "no audit file when --record-dir is unset");
    }

    #[tokio::test]
    async fn test_notifications_get_no_response() {
        // JSON-RPC notifications (no `id`) must not produce an SSE message.
        // The MCP client sends notifications/initialized after initialize;
        // responding would force id:null which the SDK rejects.
        let state = make_state();
        let (tx, mut rx) = mpsc::channel::<String>(crate::relay::SSE_CHANNEL_CAPACITY);
        let sid = uuid::Uuid::new_v4().to_string();
        state.sse_sessions.write().await.insert(sid.clone(), tx);

        let mut params = HashMap::new();
        params.insert("sessionId".into(), sid);
        let resp = messages_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Query(params),
            axum::Json(json!({"jsonrpc":"2.0","method":"notifications/initialized"})),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

        // Give the spawned task time to (not) send. No message should arrive.
        let got = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;
        assert!(got.is_err(), "notification must not produce a response");
    }

    #[tokio::test]
    async fn test_unknown_method_echoes_request_id() {
        // Errors for real requests (with id) must echo that id, never null.
        let state = make_state();
        let r = mcp_send_and_recv(
            &state,
            HashMap::new(),
            json!({"jsonrpc":"2.0","id":"req-42","method":"nope"}),
        )
        .await;
        assert_eq!(r["error"]["code"], -32601);
        assert_eq!(r["id"], "req-42");
    }

    #[tokio::test]
    async fn test_tools_list_description_mentions_file_transfer() {
        let state = make_state();
        let r = mcp_send_and_recv(
            &state,
            HashMap::new(),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;
        let desc = r["result"]["tools"][0]["description"].as_str().unwrap();
        assert!(desc.contains("/agent/mcp/put"));
        assert!(desc.contains("/agent/mcp/get"));
        assert!(desc.contains("X-SR-Token"));
    }
}
