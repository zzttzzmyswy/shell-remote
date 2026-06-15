pub mod client;
pub mod fs;
pub mod shell;
pub mod exec_sessions;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use crate::agent::client::RelayClient;
use crate::agent::shell::Shell;
use crate::proto::{Message, McpResultPayload, FsResultPayload};

struct TabState {
    shell: Shell,
    title: String,
    output_buf: Vec<u8>,
}

pub async fn start(
    relay_url: String,
    key: Option<String>,
    root: String,
    token_type: String,
    shell_path: String,
) -> anyhow::Result<()> {
    let mut delay = Duration::from_secs(1);
    let max_delay = Duration::from_secs(300);

    loop {
        match run_session(&relay_url, &key, &root, &token_type, &shell_path).await {
            Ok(()) => {
                tracing::warn!("Agent session ended, reconnecting in {:?}...", delay);
            }
            Err(e) => {
                tracing::warn!("Agent session error: {}, reconnecting in {:?}...", e, delay);
            }
        }
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, max_delay);
    }
}

async fn run_session(
    relay_url: &str,
    key: &Option<String>,
    root: &str,
    token_type: &str,
    shell_path: &str,
) -> anyhow::Result<()> {
    let mut client = RelayClient::connect_with_retry(relay_url, key.clone(), token_type, 10).await?;

    println!("session: {}", client.session_id);
    for (token, perm) in &client.tokens {
        println!("  {}: {}", perm, token);
    }

    let root_path = PathBuf::from(root);
    let exec_sessions = crate::agent::exec_sessions::ExecSessionManager::new();

    let (shell_tx, mut shell_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();

    let first_tab_id = uuid::Uuid::new_v4().to_string();
    let mut tabs: HashMap<String, TabState> = HashMap::new();
    let mut active_tab_id = first_tab_id.clone();
    let mut tab_counter: u32 = 1;

    let initial_shell = Shell::spawn(80, 24, shell_path, &first_tab_id, shell_tx.clone())?;
    tabs.insert(
        first_tab_id.clone(),
        TabState {
            shell: initial_shell,
            title: "Shell 1".to_string(),
            output_buf: Vec::new(),
        },
    );

    fn build_tab_infos(tabs: &HashMap<String, TabState>, active: &str) -> Vec<serde_json::Value> {
        tabs.iter()
            .map(|(id, ts)| {
                serde_json::json!({
                    "tab_id": id,
                    "title": ts.title,
                    "active": id == active
                })
            })
            .collect()
    }

    let tab_msg = Message {
        msg_type: "session:tab_list".to_string(),
        session_id: client.session_id.clone(),
        payload: serde_json::json!({ "tabs": build_tab_infos(&tabs, &active_tab_id) }),
    };
    let _ = client.send(&tab_msg).await;

    let sw_msg = Message {
        msg_type: "session:tab_switched".to_string(),
        session_id: client.session_id.clone(),
        payload: serde_json::json!({ "tab_id": active_tab_id }),
    };
    let _ = client.send(&sw_msg).await;

    loop {
        tokio::select! {
            shell_output = shell_rx.recv() => {
                match shell_output {
                    Some((tab_id, data)) => {
                        let encoded = fs::encode_b64(&data);
                        if let Some(ts) = tabs.get_mut(&tab_id) {
                            ts.output_buf.extend_from_slice(&data);
                            if ts.output_buf.len() > 65536 {
                                let excess = ts.output_buf.len() - 65536;
                                ts.output_buf.drain(..excess);
                            }
                        }
                        let msg = Message {
                            msg_type: "terminal:output".to_string(),
                            session_id: client.session_id.clone(),
                            payload: serde_json::json!({
                                "data": encoded,
                                "tab_id": tab_id
                            }),
                        };
                        if client.send(&msg).await.is_err() {
                            tracing::error!("Failed to send terminal output, disconnecting");
                            break;
                        }
                    }
                    None => {
                        tracing::info!("All shells closed, disconnecting");
                        break;
                    }
                }
            }

            relay_msg = client.recv() => {
                match relay_msg {
                    Some(msg) => {
                        match msg.msg_type.as_str() {
                            "terminal:input" => {
                                let tab_id = msg.payload["tab_id"]
                                    .as_str()
                                    .unwrap_or(&active_tab_id)
                                    .to_string();
                                let data_b64 = msg.payload["data"]
                                    .as_str()
                                    .unwrap_or("");
                                if let Some(data) = fs::decode_b64(data_b64) {
                                    if let Some(ts) = tabs.get_mut(&tab_id) {
                                        if let Err(e) = ts.shell.write_input(&data) {
                                            tracing::error!("Failed to write terminal input: {}", e);
                                        }
                                    }
                                }
                            }

                            "terminal:resize" => {
                                let tab_id = msg.payload["tab_id"]
                                    .as_str()
                                    .unwrap_or(&active_tab_id)
                                    .to_string();
                                let cols = msg.payload["cols"].as_u64().unwrap_or(80) as u16;
                                let rows = msg.payload["rows"].as_u64().unwrap_or(24) as u16;
                                if let Some(ts) = tabs.get_mut(&tab_id) {
                                    if let Err(e) = ts.shell.resize(cols, rows) {
                                        tracing::error!("Failed to resize terminal: {}", e);
                                    }
                                }
                            }

                            "session:tab_create" => {
                                tab_counter += 1;
                                let new_id = uuid::Uuid::new_v4().to_string();
                                let title = format!("Shell {}", tab_counter);

                                match Shell::spawn(80, 24, shell_path, &new_id, shell_tx.clone()) {
                                    Ok(shell) => {
                                        tabs.insert(new_id.clone(), TabState { shell, title, output_buf: Vec::new() });
                                        active_tab_id = new_id.clone();
                                        let tab_msg = Message {
        msg_type: "session:tab_list".to_string(),
        session_id: client.session_id.clone(),
        payload: serde_json::json!({ "tabs": build_tab_infos(&tabs, &active_tab_id) }),
    };
    let _ = client.send(&tab_msg).await;
                                        let sw_msg_inner = Message {
                                            msg_type: "session:tab_switched".to_string(),
                                            session_id: client.session_id.clone(),
                                            payload: serde_json::json!({ "tab_id": active_tab_id }),
                                        };
                                        let _ = client.send(&sw_msg_inner).await;
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to spawn shell for new tab: {}", e);
                                    }
                                }
                            }

                            "session:tab_close" => {
                                let tab_id = msg.payload["tab_id"].as_str().unwrap_or("").to_string();
                                if tabs.len() <= 1 || tab_id.is_empty() {
                                    continue;
                                }
                                tabs.remove(&tab_id);
                                if active_tab_id == tab_id {
                                    active_tab_id = tabs.keys().next().cloned().unwrap_or_default();
                                }
                                let tab_msg = Message {
        msg_type: "session:tab_list".to_string(),
        session_id: client.session_id.clone(),
        payload: serde_json::json!({ "tabs": build_tab_infos(&tabs, &active_tab_id) }),
    };
    let _ = client.send(&tab_msg).await;

                                let sw_msg = Message {
                                    msg_type: "session:tab_switched".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload: serde_json::json!({ "tab_id": active_tab_id }),
                                };
                                let _ = client.send(&sw_msg).await;
                            }

                            "session:tab_switch" => {
                                let tab_id = msg.payload["tab_id"].as_str().unwrap_or("").to_string();
                                let target_user = msg.payload["_user_id"].as_str().map(|s| s.to_string());
                                if tabs.contains_key(&tab_id) {
                                    active_tab_id = tab_id.clone();
                                    let sw_msg = Message {
                                        msg_type: "session:tab_switched".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: serde_json::json!({ "tab_id": tab_id }),
                                    };
                                    let _ = client.send(&sw_msg).await;

                                    // Replay buffered output, routed to requesting user only
                                    if let Some(ts) = tabs.get(&active_tab_id) {
                                        if !ts.output_buf.is_empty() {
                                            let encoded = fs::encode_b64(&ts.output_buf);
                                            let mut replay_payload = serde_json::json!({
                                                "data": encoded,
                                                "tab_id": active_tab_id
                                            });
                                            if let Some(ref uid) = target_user {
                                                replay_payload["_target_user_id"] = serde_json::json!(uid);
                                            }
                                            let replay_msg = Message {
                                                msg_type: "terminal:output".to_string(),
                                                session_id: client.session_id.clone(),
                                                payload: replay_payload,
                                            };
                                            let _ = client.send(&replay_msg).await;
                                        }
                                    }
                                }
                            }

                            "fs:list" => {
                                let path = msg.payload["path"].as_str().unwrap_or(".");
                                let mcp_request_id = msg.payload["_mcp_request_id"]
                                    .as_str()
                                    .map(|s| s.to_string());
                                let result = fs::list_dir(&root_path, path);
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(), serde_json::Value::String(req_id));
                                }
                                let resp = Message { msg_type: "fs:result".to_string(), session_id: client.session_id.clone(), payload };
                                let _ = client.send(&resp).await;
                            }

                            "fs:read" => {
                                let path = msg.payload["path"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                let result = fs::read_file(&root_path, path);
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(), serde_json::Value::String(req_id));
                                }
                                let resp = Message { msg_type: "fs:result".to_string(), session_id: client.session_id.clone(), payload };
                                let _ = client.send(&resp).await;
                            }

                            "fs:write" => {
                                let path = msg.payload["path"].as_str().unwrap_or("");
                                let content = msg.payload["content"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                let result = fs::write_file(&root_path, path, content);
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(), serde_json::Value::String(req_id));
                                }
                                let resp = Message { msg_type: "fs:result".to_string(), session_id: client.session_id.clone(), payload };
                                let _ = client.send(&resp).await;
                            }

                            "fs:upload" => {
                                let temp_path = msg.payload["temp_path"].as_str().unwrap_or("");
                                let final_path = msg.payload["final_path"].as_str().unwrap_or("");
                                let result = match std::fs::read(temp_path) {
                                    Ok(data) => {
                                        let content = fs::encode_b64(&data);
                                        let r = fs::write_file(&root_path, final_path, &content);
                                        let _ = std::fs::remove_file(temp_path);
                                        r
                                    }
                                    Err(e) => FsResultPayload {
                                        success: false,
                                        error: Some(format!("Failed to read uploaded file: {}", e)),
                                        entries: None,
                                        content: None,
                                        path: Some(final_path.to_string()),
                                        new_path: None,
                                    }
                                };
                                let payload = serde_json::to_value(&result).unwrap();
                                let resp = Message { msg_type: "fs:result".to_string(), session_id: client.session_id.clone(), payload };
                                let _ = client.send(&resp).await;
                            }

                            "fs:delete" => {
                                let path = msg.payload["path"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                let result = fs::delete_path(&root_path, path);
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(), serde_json::Value::String(req_id));
                                }
                                let resp = Message { msg_type: "fs:result".to_string(), session_id: client.session_id.clone(), payload };
                                let _ = client.send(&resp).await;
                            }

                            "fs:rename" => {
                                let from = msg.payload["from"].as_str().unwrap_or("");
                                let to = msg.payload["to"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                let result = fs::rename_path(&root_path, from, to);
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(), serde_json::Value::String(req_id));
                                }
                                let resp = Message { msg_type: "fs:result".to_string(), session_id: client.session_id.clone(), payload };
                                let _ = client.send(&resp).await;
                            }

                            "fs:mkdir" => {
                                let path = msg.payload["path"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                let result = fs::create_dir(&root_path, path);
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) = (mcp_request_id, &mut payload) {
                                    map.insert("_mcp_request_id".to_string(), serde_json::Value::String(req_id));
                                }
                                let resp = Message { msg_type: "fs:result".to_string(), session_id: client.session_id.clone(), payload };
                                let _ = client.send(&resp).await;
                            }

                            "mcp:exec" => {
                                let cmd = msg.payload["cmd"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                let (stdout, stderr, exit_code) = execute_command(cmd).await;
                                let result = McpResultPayload { stdout, stderr, exit_code };
                                let mut payload = serde_json::to_value(&result).unwrap();
                                if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                    (mcp_request_id, &mut payload)
                                {
                                    map.insert("_mcp_request_id".to_string(), serde_json::Value::String(req_id));
                                }
                                let resp = Message { msg_type: "mcp:result".to_string(), session_id: client.session_id.clone(), payload };
                                let _ = client.send(&resp).await;
                            }

                            "mcp:exec_start" => {
                                let cmd = msg.payload["cmd"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                let mut result = match exec_sessions.spawn(cmd).await { Ok(r) => r, Err(r) => r };
                                result._mcp_request_id = mcp_request_id;
                                let resp = Message { msg_type: "mcp:exec_result".to_string(), session_id: client.session_id.clone(), payload: serde_json::to_value(&result).unwrap() };
                                let _ = client.send(&resp).await;
                            }

                            "mcp:exec_input" => {
                                let exec_id = msg.payload["exec_id"].as_str().unwrap_or("");
                                let data_b64 = msg.payload["data_b64"].as_str().unwrap_or("");
                                let data = fs::decode_b64(data_b64).unwrap_or_default();
                                let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                let mut result = match exec_sessions.write_stdin(exec_id, &data).await { Ok(r) => r, Err(r) => r };
                                result._mcp_request_id = mcp_request_id;
                                let resp = Message { msg_type: "mcp:exec_result".to_string(), session_id: client.session_id.clone(), payload: serde_json::to_value(&result).unwrap() };
                                let _ = client.send(&resp).await;
                            }

                            "mcp:exec_close" => {
                                let exec_id = msg.payload["exec_id"].as_str().unwrap_or("");
                                let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                let mut result = match exec_sessions.close(exec_id).await { Ok(r) => r, Err(r) => r };
                                result._mcp_request_id = mcp_request_id;
                                let resp = Message { msg_type: "mcp:exec_result".to_string(), session_id: client.session_id.clone(), payload: serde_json::to_value(&result).unwrap() };
                                let _ = client.send(&resp).await;
                            }

                            "mcp:exec_list" => {
                                let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                let mut result = exec_sessions.list().await;
                                result._mcp_request_id = mcp_request_id;
                                let resp = Message { msg_type: "mcp:exec_result".to_string(), session_id: client.session_id.clone(), payload: serde_json::to_value(&result).unwrap() };
                                let _ = client.send(&resp).await;
                            }

                            "session:join" => {
                                let user_id = msg.payload["user_id"].as_str().unwrap_or("");
                                let perm = msg.payload["permission"].as_str().unwrap_or("");
                                tracing::info!("User {} joined (permission: {})", user_id, perm);

                                let tab_msg = Message {
                                    msg_type: "session:tab_list".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload: serde_json::json!({ "tabs": build_tab_infos(&tabs, &active_tab_id) }),
                                };
                                let _ = client.send(&tab_msg).await;

                                // Replay buffered output AFTER tab_list so JS knows activeTabId
                                for (tid, ts) in &tabs {
                                    if !ts.output_buf.is_empty() {
                                        let encoded = fs::encode_b64(&ts.output_buf);
                                        let replay_msg = Message {
                                            msg_type: "terminal:output".to_string(),
                                            session_id: client.session_id.clone(),
                                            payload: serde_json::json!({
                                                "data": encoded,
                                                "tab_id": tid
                                            }),
                                        };
                                        let _ = client.send(&replay_msg).await;
                                    }
                                }

                                let sw_msg = Message {
                                    msg_type: "session:tab_switched".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload: serde_json::json!({ "tab_id": active_tab_id }),
                                };
                                let _ = client.send(&sw_msg).await;
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

async fn execute_command(cmd: &str) -> (String, String, i32) {
    let cmd = cmd.to_string();
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(Stdio::null())
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
            Err(e) => (String::new(), format!("Failed to execute command: {}", e), -1),
        }
    })
    .await
    .unwrap_or_else(|_| (String::new(), "Command execution panicked".to_string(), -1))
}
