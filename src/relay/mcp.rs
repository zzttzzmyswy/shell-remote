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
        let client_ip = crate::relay::auth::client_ip(&headers);
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&format!("mcp:{client_ip}"), 60, std::time::Duration::from_secs(60)) {
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
    process_mcp_request_with_grace(state, url_token, body, MCP_WAIT_GRACE).await
}

/// Extra time the relay waits beyond the agent's own `timeout_ms`.
///
/// The relay used to wait exactly `timeout_ms`, so it always won the race
/// against the agent's own timeout: the caller got a generic "request timed
/// out" and the agent's real answer (`Command timed out after Ns`, plus any
/// output produced before the deadline) was dropped, leaving the AI unable to
/// tell "command timed out" from "agent disconnected". Waiting a little longer
/// lets the agent's answer arrive and be reported as a normal result.
const MCP_WAIT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

async fn process_mcp_request_with_grace(
    state: &Arc<SharedState>,
    url_token: Option<String>,
    body: &Value,
    wait_grace: std::time::Duration,
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
                        "description": "Execute a shell command on the remote machine; returns stdout, stderr, exit code.

WINDOWS / SHELL SELECTION:
  `shell` picks the interpreter on Windows (ignored on Unix): \"auto\" (default) = \"powershell\" when PowerShell is available, otherwise cmd; \"powershell\" prefers PowerShell; \"cmd\" forces cmd.exe.
  On Windows the default shell is PowerShell, falling back to cmd.exe only when PowerShell is missing. Write PowerShell syntax: PowerShell 5.1 has no `&&` or `||`, so separate statements with `;`. For cmd-only syntax (`dir /b`, `%VAR%`, `&`) pass shell:\"cmd\" instead of nesting `powershell -Command \"...\"`.
  `cwd` sets the working directory; on Windows it defaults to the agent root. Output is decoded as UTF-8, falling back to GBK.

READING THE RESULT:
  A non-zero exit code is reported as a trailing `[exit code: N]` line; with no output at all you get `Exit code: N`. A non-zero code is not a tool error (`grep` returning 1 is normal).
  Every call is a fresh process: `cd` and environment changes do not persist across calls. `Command timed out after Ns` means the command hit timeout_ms; `no response from agent` means the agent itself never answered (it may be disconnected or busy).

FILE TRANSFER (bytes bypass LLM context — use for files of any size):\n  upload:   curl -T <localfile> -H \"X-SR-Token: <token>\" <origin>/agent/mcp/put?path=<remotepath>\n  download: curl -H \"X-SR-Token: <token>\" <origin>/agent/mcp/get?path=<remotepath> -o <localfile>
<origin> is this MCP server's origin (scheme://host:port, the base of /agent/mcp). <token> is the same session token used for this tool; upload needs read-write, download allows read-only.\nPATH: absolute paths are used as-is on the remote filesystem; relative paths resolve under the agent root (--root). Use an absolute path unless you mean the agent root.\nDownload supports HTTP Range (e.g. `-H \"Range: bytes=1000-\"`) for resumable downloads; a `bytes=-n` suffix range is not supported and returns the full file. Errors return proper status codes: 404 missing file, 400 directory/bad path, 403 permission, 401 bad token. Upload size is capped by the deployment's reverse proxy (commonly 50m–100m); see README.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "token": {"type": "string", "description": "shell_remote token for authentication (session token shown at agent startup)"},
                                "cmd": {"type": "string", "description": "The shell command to execute on the remote machine"},
                                "timeout_ms": {"type": "number", "description": "Optional timeout in milliseconds (default 30s)"},
                                "shell": {"type": "string", "enum": ["auto", "powershell", "cmd"], "default": "auto", "description": "Optional shell to run the command in (Windows only; ignored on Unix). Defaults to auto: on Windows the command runs in PowerShell when it is available, otherwise cmd. Use \"cmd\" to force cmd even when PowerShell exists, e.g. for cmd-only syntax such as `dir /b`, `%VAR%` or `&`."},
                                "cwd": {"type": "string", "description": "Optional working directory for the command. Defaults to the agent root (on Windows that is the agent's --root, so relative paths resolve under it rather than under C:\\windows\\system32). A `cd` inside the command does not persist to the next call."}
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
            // MYS-2114: optional shell selector + working directory, forwarded
            // to the agent verbatim. Absent (or empty) → field omitted from the
            // payload, so a call that doesn't use them is byte-identical to the
            // pre-MYS-2114 message and old agents keep working.
            let shell = arguments
                .get("shell")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let cwd = arguments
                .get("cwd")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());

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
            let mut payload = json!({
                "cmd": cmd,
                "timeout_ms": timeout_ms_val,
                "_mcp_request_id": mcp_req_id
            });
            if let Some(shell) = shell {
                payload["shell"] = json!(shell);
            }
            if let Some(cwd) = cwd {
                payload["cwd"] = json!(cwd);
            }

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

            // Wait `timeout_ms` plus a grace period: the agent applies the same
            // `timeout_ms` to the command, so waiting exactly that long would
            // make the relay's timeout fire first and swallow the agent's own
            // answer. Only when the grace expires too is the agent genuinely
            // unreachable.
            let timeout_dur = std::time::Duration::from_millis(timeout_ms_val) + wait_grace;
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
                    } else if exit_code != 0 {
                        // A failing command that still produced output (the
                        // common case on Windows: `findstr`, `dir` on a missing
                        // file) used to hide its exit code entirely, so the AI
                        // could not tell success from failure. Append it as a
                        // footer. `isError` deliberately stays unset: a non-zero
                        // exit code is a command result, not a tool failure
                        // (MCP clients treat isError as "the tool broke", which
                        // would mislabel `grep` returning 1).
                        text.push_str(&format!("\n[exit code: {}]", exit_code));
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
                    let waited_secs = timeout_dur.as_secs();
                    json!({"jsonrpc":"2.0","id":request_id,"result":{"content":[{"type":"text","text":format!("Error: no response from agent within {}s (agent may be disconnected or busy)", waited_secs)}],"isError":true}})
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

    /// Register a rw session with a connected agent channel and return
    /// `(token, rx)` — `rx` yields the raw messages the relay pushes to the agent.
    async fn register_with_agent(
        state: &Arc<SharedState>,
    ) -> (String, mpsc::Receiver<String>) {
        let (_sid, token, arx) = register_with_agent_full(state).await;
        (token, arx)
    }

    /// POST a raw JSON-RPC body and return the (ignored) HTTP status, without
    /// waiting for an SSE reply — used when the agent is expected to answer.
    async fn mcp_post_no_wait(state: &Arc<SharedState>, body: Value) {
        let (tx, _rx) = mpsc::channel::<String>(crate::relay::SSE_CHANNEL_CAPACITY);
        let sid = uuid::Uuid::new_v4().to_string();
        state.sse_sessions.write().await.insert(sid.clone(), tx);
        let mut p = HashMap::new();
        p.insert("sessionId".to_string(), sid);
        let resp = messages_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Query(p),
            axum::Json(body),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
    }

    /// Pull the next `mcp:exec` message from the agent channel as its payload.
    async fn next_mcp_exec_payload(rx: &mut mpsc::Receiver<String>) -> Value {
        let raw = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("relay must push an mcp:exec message to the agent")
            .expect("agent channel must stay open");
        let msg: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(msg["type"], "mcp:exec");
        msg["payload"].clone()
    }

    #[tokio::test]
    async fn test_tools_list_schema_exposes_shell_and_cwd() {
        // MYS-2114: the AI agent must be able to discover the new selectors.
        let state = make_state();
        let r = mcp_send_and_recv(
            &state,
            HashMap::new(),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;
        let props = &r["result"]["tools"][0]["inputSchema"]["properties"];
        assert_eq!(
            props["shell"]["enum"],
            json!(["auto", "powershell", "cmd"]),
            "shell must be an enum of auto/powershell/cmd, got {props}"
        );
        assert_eq!(
            props["shell"]["default"], "auto",
            "shell must default to auto (PowerShell when available), got {props}"
        );
        assert!(props["shell"]["type"] == "string");
        // The description must document the auto default + fallback, since that
        // is what tells the agent how to request cmd explicitly.
        let desc = props["shell"]["description"].as_str().unwrap();
        assert!(desc.contains("PowerShell"), "got {desc:?}");
        assert!(desc.contains("cmd"), "got {desc:?}");
        assert_eq!(props["cwd"]["type"], "string");
        // The new fields are optional: required must stay ["token","cmd"].
        assert_eq!(
            r["result"]["tools"][0]["inputSchema"]["required"],
            json!(["token", "cmd"])
        );
    }

    #[tokio::test]
    async fn test_tools_call_forwards_shell_and_cwd_to_agent() {
        let state = make_state();
        let (token, mut arx) = register_with_agent(&state).await;
        mcp_post_no_wait(&state, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"shell_remote",
            "arguments":{"token":token,"cmd":"echo hi","shell":"powershell","cwd":"C:\\Users\\me"}
        }})).await;

        let payload = next_mcp_exec_payload(&mut arx).await;
        assert_eq!(payload["cmd"], "echo hi");
        assert_eq!(payload["shell"], "powershell");
        assert_eq!(payload["cwd"], "C:\\Users\\me");
        assert!(payload["_mcp_request_id"].is_string());
    }

    #[tokio::test]
    async fn test_tools_call_omits_shell_and_cwd_when_absent() {
        // Backward compatibility: a call that doesn't use the new arguments
        // must produce the same payload as before (fields absent, not null) so
        // an older agent parses it unchanged.
        let state = make_state();
        let (token, mut arx) = register_with_agent(&state).await;
        mcp_post_no_wait(&state, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"shell_remote",
            "arguments":{"token":token,"cmd":"echo hello"}
        }})).await;

        let payload = next_mcp_exec_payload(&mut arx).await;
        assert_eq!(payload["cmd"], "echo hello");
        assert!(payload.get("shell").is_none(), "got {payload}");
        assert!(payload.get("cwd").is_none(), "got {payload}");
    }

    #[tokio::test]
    async fn test_tools_call_treats_empty_shell_and_cwd_as_absent() {
        let state = make_state();
        let (token, mut arx) = register_with_agent(&state).await;
        mcp_post_no_wait(&state, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"shell_remote",
            "arguments":{"token":token,"cmd":"echo hello","shell":"","cwd":""}
        }})).await;

        let payload = next_mcp_exec_payload(&mut arx).await;
        assert!(payload.get("shell").is_none(), "empty shell must be omitted, got {payload}");
        assert!(payload.get("cwd").is_none(), "empty cwd must be omitted, got {payload}");
    }

    /// Register a rw session with a connected agent and return
    /// `(session_id, token, rx)` — `rx` yields the raw messages the relay pushes
    /// to the agent.
    async fn register_with_agent_full(
        state: &Arc<SharedState>,
    ) -> (String, String, mpsc::Receiver<String>) {
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let token = r.tokens[0].0.clone();
        let (atx, arx) = mpsc::channel::<String>(crate::relay::SSE_CHANNEL_CAPACITY);
        let mut cm = crate::relay::ChannelMap::new();
        cm.agent = Some(atx);
        state.agent_broadcast.write().await.insert(r.session_id.clone(), cm);
        (r.session_id, token, arx)
    }

    /// Drive one `tools/call` end to end against a scripted agent.
    ///
    /// `reply` gets the `mcp:exec` payload the relay sent the agent and returns
    /// the `mcp:result` payload to answer with — or `None` to stay silent (the
    /// "agent never answered" case). `grace` is the relay's extra wait, injected
    /// so timeout tests finish in milliseconds instead of the production 5s.
    async fn call_with_scripted_agent<F, Fut>(
        state: &Arc<SharedState>,
        args: Value,
        grace: std::time::Duration,
        reply: F,
    ) -> Value
    where
        F: FnOnce(Value) -> Fut,
        Fut: std::future::Future<Output = Option<Value>>,
    {
        let (session_id, token, mut arx) = register_with_agent_full(state).await;
        let mut args = args;
        args["token"] = json!(token);
        let body = json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
                          "params":{"name":"shell_remote","arguments": args}});

        let task = {
            let state = state.clone();
            tokio::spawn(
                async move { process_mcp_request_with_grace(&state, None, &body, grace).await },
            )
        };

        let payload = next_mcp_exec_payload(&mut arx).await;
        if let Some(mut result) = reply(payload.clone()).await {
            result["_mcp_request_id"] = payload["_mcp_request_id"].clone();
            let msg = json!({
                "type": "mcp:result",
                "session_id": session_id,
                "payload": result
            })
            .to_string();
            crate::relay::ws::route_agent_message(state, &session_id, &msg).await;
        }
        task.await
            .unwrap()
            .expect("tools/call must always produce a response")
    }

    /// The text of a `tools/call` result content block.
    fn result_text(r: &Value) -> &str {
        r["result"]["content"][0]["text"].as_str().unwrap_or("")
    }

    #[tokio::test]
    async fn test_tools_call_nonzero_exit_appends_footer() {
        // A failing command that still produced output used to hide its exit
        // code, so the AI could not tell success from failure.
        let state = make_state();
        let r = call_with_scripted_agent(
            &state,
            json!({"cmd": "findstr nope file.txt"}),
            MCP_WAIT_GRACE,
            |_| async { Some(json!({"stdout":"x","stderr":"","exit_code":2})) },
        )
        .await;
        let text = result_text(&r);
        assert!(text.starts_with('x'), "got {text:?}");
        assert!(text.ends_with("[exit code: 2]"), "got {text:?}");
        // A non-zero exit code is a command result, not a tool failure:
        // `grep` returning 1 must not be reported as isError.
        assert_ne!(r["result"]["isError"], json!(true), "got {r}");
    }

    #[tokio::test]
    async fn test_tools_call_zero_exit_has_no_footer() {
        let state = make_state();
        let r = call_with_scripted_agent(
            &state,
            json!({"cmd": "echo hi"}),
            MCP_WAIT_GRACE,
            |_| async { Some(json!({"stdout":"hi","stderr":"","exit_code":0})) },
        )
        .await;
        let text = result_text(&r);
        assert_eq!(text, "hi", "exit code 0 must stay noise-free");
        assert!(!text.contains("exit code"), "got {text:?}");
    }

    #[tokio::test]
    async fn test_tools_call_empty_output_keeps_legacy_exit_code_line() {
        // Pre-existing behaviour: with no stdout/stderr at all, the exit code
        // is the whole answer.
        let state = make_state();
        let r = call_with_scripted_agent(
            &state,
            json!({"cmd": "exit 5"}),
            MCP_WAIT_GRACE,
            |_| async { Some(json!({"stdout":"","stderr":"","exit_code":5})) },
        )
        .await;
        assert_eq!(result_text(&r), "Exit code: 5");
    }

    #[tokio::test]
    async fn test_tools_call_result_arriving_after_timeout_ms_is_not_an_error() {
        // The agent applies `timeout_ms` to the command too, so its own timeout
        // answer lands *after* that deadline. The relay must still report it
        // rather than replacing it with a generic "no response" error.
        let state = make_state();
        let r = call_with_scripted_agent(
            &state,
            json!({"cmd": "sleep 10", "timeout_ms": 100}),
            std::time::Duration::from_secs(2),
            |_| async {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Some(json!({
                    "stdout": "",
                    "stderr": "Command timed out after 0s",
                    "exit_code": -1
                }))
            },
        )
        .await;
        assert_ne!(r["result"]["isError"], json!(true), "got {r}");
        let text = result_text(&r);
        assert!(
            text.contains("Command timed out after"),
            "the agent's own timeout reason must survive, got {text:?}"
        );
        assert!(text.ends_with("[exit code: -1]"), "got {text:?}");
    }

    #[tokio::test]
    async fn test_tools_call_silent_agent_reports_no_response() {
        // Only when even the grace period expires is the agent genuinely
        // unreachable — and that must read differently from a command timeout.
        let state = make_state();
        let r = call_with_scripted_agent(
            &state,
            json!({"cmd": "echo hi", "timeout_ms": 100}),
            std::time::Duration::from_millis(100),
            |_| async { None },
        )
        .await;
        assert_eq!(r["result"]["isError"], json!(true), "got {r}");
        let text = result_text(&r);
        assert!(text.contains("no response from agent"), "got {text:?}");
    }

    #[tokio::test]
    async fn test_tools_list_description_documents_windows_usage() {
        // The AI discovers the Windows rules (default shell, `&&` limitation,
        // how to read exit codes) only from the tool description.
        let state = make_state();
        let r = mcp_send_and_recv(
            &state,
            HashMap::new(),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;
        let desc = r["result"]["tools"][0]["description"].as_str().unwrap();
        for needle in ["powershell", "cmd", "cwd", "[exit code"] {
            assert!(desc.contains(needle), "description lacks {needle:?}: {desc}");
        }
        assert!(desc.contains("&&"), "must warn about PowerShell 5.1 `&&`: {desc}");
        assert!(desc.contains("Exit code:"), "must document the empty-output form: {desc}");
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
