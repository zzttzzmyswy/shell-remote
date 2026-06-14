pub mod client;
pub mod fs;
pub mod shell;
pub mod exec_sessions;

use std::path::PathBuf;
use std::process::Stdio;

use crate::agent::client::RelayClient;
use crate::agent::shell::Shell;
use crate::proto::{Message, McpResultPayload};

pub async fn start(
    relay_url: String,
    key: Option<String>,
    root: String,
    token_type: String,
) -> anyhow::Result<()> {
    let mut client = RelayClient::connect_with_retry(&relay_url, key.clone(), &token_type, 10).await?;

    println!("session_id: {}", client.session_id);
    for (token, perm) in &client.tokens {
        println!("  {}: {}", perm, token);
    }

    let root_path = PathBuf::from(&root);

    let mut shell = Shell::spawn(80, 24)?;

    loop {
        tokio::select! {
            shell_output = async { shell.read_output() } => {
                match shell_output {
                    Some(data) => {
                        let encoded = fs::encode_b64(&data);
                        let msg = Message {
                            msg_type: "terminal:output".to_string(),
                            session_id: client.session_id.clone(),
                            payload: serde_json::json!({
                                "data": encoded
                            }),
                        };
                        if client.send(&msg).await.is_err() {
                            tracing::error!("Failed to send terminal output, disconnecting");
                            break;
                        }
                    }
                    None => {
                        tracing::info!("Shell pty closed, disconnecting");
                        break;
                    }
                }
            }

            relay_msg = client.recv() => {
                match relay_msg {
                    Some(mut msg) => {
                        match msg.msg_type.as_str() {
                            "terminal:input" => {
                                let data_b64 = msg.payload["data"]
                                    .as_str()
                                    .unwrap_or("");
                                if let Some(data) = fs::decode_b64(data_b64) {
                                    if let Err(e) = shell.write_input(&data) {
                                        tracing::error!("Failed to write terminal input: {}", e);
                                    }
                                }
                            }

                            "terminal:resize" => {
                                let cols = msg.payload["cols"].as_u64().unwrap_or(80) as u16;
                                let rows = msg.payload["rows"].as_u64().unwrap_or(24) as u16;
                                if let Err(e) = shell.resize(cols, rows) {
                                    tracing::error!("Failed to resize terminal: {}", e);
                                }
                            }

                            "fs:list" => {
                                let path = msg.payload["path"].as_str().unwrap_or(".");
                                let mcp_request_id = msg.payload["_mcp_request_id"]
                                    .as_str()
                                    .map(|s| s.to_string());

                                let result = fs::list_dir(&root_path, path);
                                if let Some(ref _req_id) = mcp_request_id {
                                    if let serde_json::Value::Object(ref mut map) = msg.payload {
                                        map.remove("_mcp_request_id");
                                    }
                                }
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(),
                                        serde_json::Value::String(req_id));
                                }
                                let resp = Message {
                                    msg_type: "fs:result".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload,
                                };
                                let _ = client.send(&resp).await;
                            }

                            "fs:read" => {
                                let path = msg.payload["path"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"]
                                    .as_str()
                                    .map(|s| s.to_string());

                                let result = fs::read_file(&root_path, path);
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(),
                                        serde_json::Value::String(req_id));
                                }
                                let resp = Message {
                                    msg_type: "fs:result".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload,
                                };
                                let _ = client.send(&resp).await;
                            }

                            "fs:write" => {
                                let path = msg.payload["path"].as_str().unwrap_or("");
                                let content = msg.payload["content"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"]
                                    .as_str()
                                    .map(|s| s.to_string());

                                let result = fs::write_file(&root_path, path, content);
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(),
                                        serde_json::Value::String(req_id));
                                }
                                let resp = Message {
                                    msg_type: "fs:result".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload,
                                };
                                let _ = client.send(&resp).await;
                            }

                            "fs:delete" => {
                                let path = msg.payload["path"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"]
                                    .as_str()
                                    .map(|s| s.to_string());

                                let result = fs::delete_path(&root_path, path);
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(),
                                        serde_json::Value::String(req_id));
                                }
                                let resp = Message {
                                    msg_type: "fs:result".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload,
                                };
                                let _ = client.send(&resp).await;
                            }

                            "fs:rename" => {
                                let from = msg.payload["from"].as_str().unwrap_or("");
                                let to = msg.payload["to"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"]
                                    .as_str()
                                    .map(|s| s.to_string());

                                let result = fs::rename_path(&root_path, from, to);
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(),
                                        serde_json::Value::String(req_id));
                                }
                                let resp = Message {
                                    msg_type: "fs:result".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload,
                                };
                                let _ = client.send(&resp).await;
                            }

                            "mcp:exec" => {
                                let cmd = msg.payload["cmd"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"]
                                    .as_str()
                                    .map(|s| s.to_string());

                                let (stdout, stderr, exit_code) =
                                    execute_command(cmd);

                                let result = McpResultPayload {
                                    stdout,
                                    stderr,
                                    exit_code,
                                };
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(),
                                        serde_json::Value::String(req_id));
                                }
                                let resp = Message {
                                    msg_type: "mcp:result".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload,
                                };
                                let _ = client.send(&resp).await;
                            }

                            "session:join" => {
                                let user_id = msg.payload["user_id"].as_str().unwrap_or("");
                                let perm = msg.payload["permission"].as_str().unwrap_or("");
                                tracing::info!("User {} joined (permission: {})", user_id, perm);
                            }

                            "session:leave" => {
                                let user_id = msg.payload["user_id"].as_str().unwrap_or("");
                                tracing::info!("User {} left", user_id);
                            }

                            other => {
                                tracing::debug!("Unknown message type: {}", other);
                            }
                        }
                    }
                    None => {
                        tracing::info!("Relay connection closed, shutting down");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

fn execute_command(cmd: &str) -> (String, String, i32) {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let exit_code = out.status.code().unwrap_or(-1);
            (stdout, stderr, exit_code)
        }
        Err(e) => {
            (String::new(), format!("Failed to execute command: {}", e), -1)
        }
    }
}
