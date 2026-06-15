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

use crate::proto::Message as ProtoMessage;
use crate::relay::SharedState;

pub async fn sse_handler(
    State(state): State<Arc<SharedState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let token = params.get("token").cloned();

    if let Some(ref t) = token {
        if !t.is_empty() && state.sessions.authenticate(t).await.is_none() {
            return Sse::new(
                futures_util::stream::once(std::future::ready(Ok::<_, Infallible>(
                    Event::default()
                        .event("error")
                        .data(r#"{"code":"AUTH_INVALID_TOKEN","message":"Invalid token"}"#),
                ))),
            )
            .into_response();
        }
    }

    let stream = async_stream::stream! {
        let endpoint_url = if let Some(ref t) = token {
            format!("/mcp/messages?token={}", t)
        } else {
            "/mcp/messages".to_string()
        };

        yield Ok::<_, Infallible>(Event::default()
            .event("endpoint")
            .data(endpoint_url));

        let init_result = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "shell-remote",
                                        "version": "0.1.3"
                },
                "capabilities": {
                    "tools": {}
                }
            }
        }).to_string();

        yield Ok::<_, Infallible>(Event::default()
            .event("message")
            .data(init_result));

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
            yield Ok::<_, Infallible>(Event::default()
                .event("heartbeat")
                .data("{}"));
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

pub async fn messages_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let url_token = params.get("token").cloned();

    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let request_id = body.get("id").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => Json(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "shell-remote",
                                        "version": "0.1.3"
                },
                "capabilities": {
                    "tools": {}
                }
            }
        }))
        .into_response(),

        "tools/list" => Json(json!({
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
                                "cmd": {
                                    "type": "string",
                                    "description": "The shell command to execute on the remote machine"
                                },
                                "timeout_ms": {
                                    "type": "number",
                                    "description": "Optional timeout in milliseconds"
                                }
                            },
                            "required": ["cmd"]
                        }
                    },
                    {
                        "name": "exec_remote_start",
                        "description": "Start an interactive command session ON THE REMOTE TARGET MACHINE. Returns exec_id and initial output. The command runs in the background; use exec_remote_input to interact, exec_remote_close to terminate.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "cmd": {"type": "string", "description": "Shell command to execute on the remote machine"}
                            },
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
                                "data": {"type": "string", "description": "Text to write to the remote process stdin"}
                            },
                            "required": ["exec_id", "data"]
                        }
                    },
                    {
                        "name": "exec_remote_close",
                        "description": "Close an exec session ON THE REMOTE TARGET MACHINE. Kills the process if still running. Returns final output and exit code.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "exec_id": {"type": "string", "description": "ID of the exec session to close"}
                            },
                            "required": ["exec_id"]
                        }
                    },
                    {
                        "name": "exec_remote_list",
                        "description": "List all active exec sessions on the REMOTE TARGET MACHINE with their status.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    },
                    {
                        "name": "file_remote_read",
                        "description": "Read the content of a file from the REMOTE TARGET MACHINE (NOT the AI's local filesystem).",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Absolute path to the file on the remote machine"
                                }
                            },
                            "required": ["path"]
                        }
                    },
                    {
                        "name": "file_remote_write",
                        "description": "Write content to a file on the REMOTE TARGET MACHINE (NOT the AI's local filesystem).",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Absolute path to the file on the remote machine"
                                },
                                "content": {
                                    "type": "string",
                                    "description": "Content to write to the file on the remote machine"
                                }
                            },
                            "required": ["path", "content"]
                        }
                    },
                    {
                        "name": "file_remote_list",
                        "description": "List contents of a directory on the REMOTE TARGET MACHINE (NOT the AI's local filesystem).",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Absolute path to the directory on the remote machine"
                                }
                            },
                            "required": ["path"]
                        }
                    }
                ]
            }
        }))
        .into_response(),

        "tools/call" => {
            let params_obj = body.get("params").unwrap_or(&Value::Null);
            let tool_name = params_obj
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let arguments = params_obj.get("arguments").unwrap_or(&Value::Null);

            let auth = {
                let mut result = None;

                if let Some(t) = crate::relay::auth::extract_bearer_token(&headers) {
                    result = state.sessions.authenticate(&t).await;
                }

                if result.is_none() {
                    if let Some(ref t) = url_token {
                        if !t.is_empty() {
                            result = state.sessions.authenticate(t).await;
                        }
                    }
                }
                if result.is_none() {
                    if let Some(t) = arguments.get("token").and_then(|v| v.as_str()) {
                        if !t.is_empty() {
                            result = state.sessions.authenticate(t).await;
                        }
                    }
                }
                result
            };

            let (session_id, _permission) = match auth {
                Some(result) => result,
                None => {
                    return Json(json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {
                            "code": -32001,
                            "message": "Invalid token"
                        }
                    }))
                    .into_response();
                }
            };

            if matches!(tool_name, "exec_remote_start" | "exec_remote_input" | "exec_remote_close" | "exec_remote_list") {
                let request_id = Uuid::new_v4().to_string();

                let proto_message: crate::proto::Message;
                let exec_timeout: u64;

                match tool_name {
                    "exec_remote_start" => {
                        let cmd = arguments.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
                        proto_message = crate::proto::Message {
                            msg_type: "mcp:exec_start".to_string(),
                            session_id: session_id.clone(),
                            payload: json!({
                                "cmd": cmd,
                                "_mcp_request_id": request_id
                            }),
                        };
                        exec_timeout = 10000;
                    }
                    "exec_remote_input" => {
                        let exec_id = arguments.get("exec_id").and_then(|v| v.as_str()).unwrap_or("");
                        let data = arguments.get("data").and_then(|v| v.as_str()).unwrap_or("");
                        let data_b64 = crate::agent::fs::encode_b64(data.as_bytes());
                        proto_message = crate::proto::Message {
                            msg_type: "mcp:exec_input".to_string(),
                            session_id: session_id.clone(),
                            payload: json!({
                                "exec_id": exec_id,
                                "data_b64": data_b64,
                                "_mcp_request_id": request_id
                            }),
                        };
                        exec_timeout = 10000;
                    }
                    "exec_remote_close" => {
                        let exec_id = arguments.get("exec_id").and_then(|v| v.as_str()).unwrap_or("");
                        proto_message = crate::proto::Message {
                            msg_type: "mcp:exec_close".to_string(),
                            session_id: session_id.clone(),
                            payload: json!({
                                "exec_id": exec_id,
                                "_mcp_request_id": request_id
                            }),
                        };
                        exec_timeout = 5000;
                    }
                    "exec_remote_list" => {
                        proto_message = crate::proto::Message {
                            msg_type: "mcp:exec_list".to_string(),
                            session_id: session_id.clone(),
                            payload: json!({
                                "_mcp_request_id": request_id
                            }),
                        };
                        exec_timeout = 5000;
                    }
                    _ => unreachable!(),
                }

                let (tx, rx) = oneshot::channel();

                {
                    let mut pending = state.pending_mcp.write().await;
                    pending.insert(request_id.clone(), (session_id.clone(), tx));
                }

                {
                    let agent_tx_option = {
                        let broadcast = state.agent_broadcast.read().await;
                        broadcast
                            .get(&session_id)
                            .and_then(|cm| cm.agent.clone())
                    };

                    match agent_tx_option {
                        Some(agent_tx) => {
                            let text = serde_json::to_string(&proto_message).unwrap_or_default();
                            let _ = agent_tx.send(text);
                        }
                        None => {
                            let mut pending = state.pending_mcp.write().await;
                            pending.remove(&request_id);
                            return Json(json!({
                                "jsonrpc": "2.0",
                                "id": request_id,
                                "result": {
                                    "content": [{
                                        "type": "text",
                                        "text": "Error: No agent connected for this session"
                                    }],
                                    "isError": true
                                }
                            }))
                            .into_response();
                        }
                    }
                }

                match tokio::time::timeout(
                    tokio::time::Duration::from_millis(exec_timeout),
                    rx,
                )
                .await
                {
                    Ok(Ok(result)) => {
                        let value: Value = serde_json::from_str(&result).unwrap_or_default();
                        let stdout = value
                            .get("stdout")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let stderr = value
                            .get("stderr")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let status = value
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let exit_code = value
                            .get("exit_code")
                            .and_then(|v| v.as_i64());

                        let mut text = format!("Status: {}\n", status);
                        if let Some(code) = exit_code {
                            text.push_str(&format!("Exit code: {}\n", code));
                        }
                        if !stdout.is_empty() {
                            if let Some(decoded) = crate::agent::fs::decode_b64(stdout) {
                                if let Ok(s) = String::from_utf8(decoded) {
                                    text.push_str(&s);
                                }
                            }
                        }
                        if !stderr.is_empty() {
                            if let Some(decoded) = crate::agent::fs::decode_b64(stderr) {
                                if let Ok(s) = String::from_utf8(decoded) {
                                    text.push_str(&format!("\n[stderr]\n{}", s));
                                }
                            }
                        }

                        let exec_id_val = value
                            .get("exec_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");

                        return Json(json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": text.trim()
                                }],
                                "exec_id": exec_id_val
                            }
                        }))
                        .into_response();
                    }
                    _ => {
                        let mut pending = state.pending_mcp.write().await;
                        pending.remove(&request_id);
                        return Json(json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "Error: Request timed out or agent disconnected"
                                }],
                                "isError": true
                            }
                        }))
                        .into_response();
                    }
                }
            }

            let (msg_type, payload) = match tool_name {
                "exec_remote" => {
                    let cmd = arguments
                        .get("cmd")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let timeout_ms = arguments
                        .get("timeout_ms")
                        .and_then(|v| v.as_u64());
                    let payload = if let Some(timeout) = timeout_ms {
                        json!({"cmd": cmd, "timeout_ms": timeout})
                    } else {
                        json!({"cmd": cmd})
                    };
                    ("mcp:exec", payload)
                }
                "file_remote_read" => {
                    let path = arguments
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    ("fs:read", json!({"path": path}))
                }
                "file_remote_write" => {
                    let path = arguments
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let content = arguments
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    ("fs:write", json!({"path": path, "content": content}))
                }
                "file_remote_list" => {
                    let path = arguments
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    ("fs:list", json!({"path": path}))
                }
                _ => {
                    return Json(json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {
                            "code": -32601,
                            "message": format!("Unknown tool: {}", tool_name)
                        }
                    }))
                    .into_response();
                }
            };

            let (response_tx, mut response_rx) = oneshot::channel::<String>();
            let mcp_request_id = Uuid::new_v4().to_string();

            {
                let mut pending = state.pending_mcp.write().await;
                pending.insert(mcp_request_id.clone(), (session_id.clone(), response_tx));
            }

            let mcp_request_msg = json!({
                "type": msg_type,
                "session_id": session_id,
                "payload": payload,
                "_mcp_request_id": mcp_request_id
            })
            .to_string();

            let sent = {
                let broadcast = state.agent_broadcast.read().await;
                if let Some(channel_map) = broadcast.get(&session_id) {
                    if let Some(agent_tx) = &channel_map.agent {
                        let _ = agent_tx.send(mcp_request_msg.clone());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if !sent {
                let mut pending = state.pending_mcp.write().await;
                pending.remove(&mcp_request_id);
                return Json(json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": "Error: No agent connected for this session"
                        }],
                        "isError": true
                    }
                }))
                .into_response();
            }

            let timeout_duration = match tool_name {
                "exec_remote" => {
                    let timeout_ms = arguments
                        .get("timeout_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(30000);
                    tokio::time::Duration::from_millis(timeout_ms)
                }
                _ => tokio::time::Duration::from_secs(30),
            };

            match tokio::time::timeout(timeout_duration, &mut response_rx).await {
                Ok(Ok(response_text)) => Json(json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": response_text
                        }],
                        "isError": false
                    }
                }))
                .into_response(),
                _ => {
                    let mut pending = state.pending_mcp.write().await;
                    pending.remove(&mcp_request_id);
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": "Error: Request timed out waiting for agent response"
                            }],
                            "isError": true
                        }
                    }))
                    .into_response()
                }
            }
        }

        _ => Json(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {
                "code": -32601,
                "message": format!("Unknown method: {}", method)
            }
        }))
        .into_response(),
    }
}
