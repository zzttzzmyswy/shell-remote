pub mod client;
pub mod exec_sessions;
pub mod fs;
pub mod shell;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use crate::agent::client::RelayClient;
use crate::agent::shell::Shell;
use crate::proto::{FsResultPayload, McpResultPayload, Message};

/// Returns the user's home directory, preferring `$HOME` (unix) and falling
/// back to `%USERPROFILE%` (Windows). Used for the file-manager root default
/// and the PTY child's cwd so the same code path works on both platforms.
pub(crate) fn home_dir() -> String {
    home_dir_from(std::env::var("HOME").ok(), std::env::var("USERPROFILE").ok())
}

fn home_dir_from(home: Option<String>, userprofile: Option<String>) -> String {
    home.or(userprofile).unwrap_or_else(|| ".".to_string())
}

struct TabState {
    shell: Shell,
    title: String,
    output_buf: Vec<u8>,
}

/// Outbound message handle. The main loop never blocks on HTTP: terminal
/// output is pushed to a bounded, coalesced channel; control/result messages
/// are pushed to a bounded channel drained with priority by a background task.
struct Out {
    control_tx: tokio::sync::mpsc::Sender<String>,
    output_tx: tokio::sync::mpsc::Sender<(String, Vec<u8>)>,
}

impl Out {
    /// Push a control/result message. Backpressures (rarely) instead of
    /// dropping — losing an mcp/fs result would break callers.
    async fn control(&self, msg: Message) {
        if let Ok(s) = serde_json::to_string(&msg) {
            let _ = self.control_tx.send(s).await;
        }
    }

    /// Push a terminal-output chunk. Non-blocking: under extreme flood we drop
    /// the chunk rather than stall input/command processing.
    fn output(&self, tab_id: String, data: Vec<u8>) {
        let _ = self.output_tx.try_send((tab_id, data));
    }
}

/// Background sender: drains control + output channels and POSTs to the relay.
/// Coalesces terminal:output per tab within a short window so a bursting
/// `cat kern.log` collapses into a handful of messages instead of thousands.
async fn sender_loop(
    client: reqwest::Client,
    send_url: String,
    session_id: String,
    mut control_rx: tokio::sync::mpsc::Receiver<String>,
    mut output_rx: tokio::sync::mpsc::Receiver<(String, Vec<u8>)>,
) {
    let mut pending: HashMap<String, Vec<u8>> = HashMap::new();
    let mut timer = tokio::time::interval(Duration::from_millis(16));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            ctrl = control_rx.recv() => match ctrl {
                Some(s) => {
                    flush_output(&client, &send_url, &session_id, &mut pending).await;
                    post_raw(&client, &send_url, &s).await;
                }
                None => break,
            },
            out = output_rx.recv() => match out {
                Some((tab, data)) => pending.entry(tab).or_default().extend(data),
                None => break,
            },
            _ = timer.tick() => {
                flush_output(&client, &send_url, &session_id, &mut pending).await;
            }
        }
    }
    flush_output(&client, &send_url, &session_id, &mut pending).await;
}

async fn flush_output(
    client: &reqwest::Client,
    send_url: &str,
    session_id: &str,
    pending: &mut HashMap<String, Vec<u8>>,
) {
    for (tab_id, data) in pending.drain() {
        if data.is_empty() {
            continue;
        }
        let encoded = fs::encode_b64(&data);
        let msg = Message {
            msg_type: "terminal:output".to_string(),
            session_id: session_id.to_string(),
            payload: serde_json::json!({ "data": encoded, "tab_id": tab_id }),
        };
        if let Ok(s) = serde_json::to_string(&msg) {
            post_raw(client, send_url, &s).await;
        }
    }
}

async fn post_raw(client: &reqwest::Client, send_url: &str, text: &str) {
    let body = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to parse outgoing message: {}", e);
            return;
        }
    };
    match client.post(send_url).json(&body).send().await {
        Ok(resp) if !resp.status().is_success() => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!("Agent POST failed ({}): {}", status, &body[..body.len().min(200)]);
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("Agent POST send error: {}", e),
    }
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
    // Tokens obtained on the first successful registration; replayed on every
    // reconnect so the relay reuses them instead of minting new random ones.
    let mut cached_tokens: Option<Vec<(String, String)>> = None;

    loop {
        match run_session(
            &relay_url,
            &key,
            &root,
            &token_type,
            &shell_path,
            cached_tokens.as_deref(),
        )
        .await
        {
            Ok(tokens) => {
                if let Some(t) = tokens {
                    cached_tokens = Some(t);
                }
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
    cached_tokens: Option<&[(String, String)]>,
) -> anyhow::Result<Option<Vec<(String, String)>>> {
    let mut client =
        RelayClient::connect_with_retry(relay_url, key.clone(), token_type, cached_tokens, 10)
            .await?;

    tracing::info!(session = %client.session_id, "agent session established");
    for (token, perm) in &client.tokens {
        tracing::info!(session = %client.session_id, permission = %perm, "token: {}", token);
    }

    // Outbound channel + background sender. The main loop must never block on
    // HTTP — otherwise high-volume terminal output starves input/command
    // processing (and MCP round-trips time out as "i/o error").
    let (control_tx, control_rx) = tokio::sync::mpsc::channel::<String>(64);
    let (output_tx, output_rx) = tokio::sync::mpsc::channel::<(String, Vec<u8>)>(64);
    let out = Out {
        control_tx: control_tx.clone(),
        output_tx,
    };
    tokio::spawn(sender_loop(
        client.http_client().clone(),
        client.send_url().to_string(),
        client.session_id.clone(),
        control_rx,
        output_rx,
    ));
    // Keep a control sender for spawned long-running tasks (e.g. mcp:exec).
    let task_control_tx = control_tx;

    let root_path = PathBuf::from(root);
    if !root_path.is_dir() {
        anyhow::bail!(
            "Root directory does not exist or is not a directory: {}",
            root
        );
    }
    let exec_sessions = crate::agent::exec_sessions::ExecSessionManager::new();

    let (shell_tx, mut shell_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();

    let is_readonly = token_type == "ro";

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
    out.control(tab_msg).await;

    let sw_msg = Message {
        msg_type: "session:tab_switched".to_string(),
        session_id: client.session_id.clone(),
        payload: serde_json::json!({ "tab_id": active_tab_id }),
    };
    out.control(sw_msg).await;

    loop {
        tokio::select! {
                shell_output = shell_rx.recv() => {
                    match shell_output {
                        Some((tab_id, data)) => {
                            if let Some(ts) = tabs.get_mut(&tab_id) {
                                ts.output_buf.extend_from_slice(&data);
                                if ts.output_buf.len() > 65536 {
                                    let excess = ts.output_buf.len() - 65536;
                                    ts.output_buf.drain(..excess);
                                }
                            }
                            // Non-blocking push to the coalescing sender. Never
                            // stalls the loop — disconnect is detected via the
                            // relay→agent SSE stream closing (recv below).
                            out.output(tab_id, data);
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
                            if is_readonly && crate::proto::requires_write(&msg.msg_type) {
                                let err_resp = Message {
                                    msg_type: "error".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload: serde_json::json!({
                                        "code": "PERMISSION_DENIED",
                                        "message": "Agent is read-only, write-type messages rejected"
                                    }),
                                };
                                out.control(err_resp).await;
                                continue;
                            }

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
        out.control(tab_msg).await;
                                            let sw_msg_inner = Message {
                                                msg_type: "session:tab_switched".to_string(),
                                                session_id: client.session_id.clone(),
                                                payload: serde_json::json!({ "tab_id": active_tab_id }),
                                            };
                                            out.control(sw_msg_inner).await;
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
        out.control(tab_msg).await;

                                    let sw_msg = Message {
                                        msg_type: "session:tab_switched".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: serde_json::json!({ "tab_id": active_tab_id }),
                                    };
                                    out.control(sw_msg).await;
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
                                        out.control(sw_msg).await;

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
                                                out.control(replay_msg).await;
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
                                    out.control(resp).await;
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
                                    out.control(resp).await;
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
                                    out.control(resp).await;
                                }

                                "fs:upload" => {
                                    let temp_path = msg.payload["temp_path"].as_str().unwrap_or("");
                                    let final_path = msg.payload["final_path"].as_str().unwrap_or("");
                                    let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());

                                    // Validate temp_path is under our known upload directory
                                    let temp = std::path::Path::new(temp_path);
                                    if !temp.starts_with("/tmp/opencode/uploads/") {
                                        let result = FsResultPayload {
                                            success: false,
                                            error: Some("Invalid temporary upload path".into()),
                                            entries: None, content: None,
                                            path: Some(final_path.to_string()), new_path: None,
                                        };
                                        let payload = serde_json::to_value(&result).unwrap();
                                        let resp = Message { msg_type: "fs:result".into(), session_id: client.session_id.clone(), payload };
                                        out.control(resp).await;
                                        continue;
                                    }

                                    let result = match std::fs::read(temp_path) {
                                        Ok(data) => {
                                            let r = fs::write_file_bytes(&root_path, final_path, &data);
                                            let _ = std::fs::remove_file(temp_path);
                                            r
                                        }
                                        Err(e) => FsResultPayload {
                                            success: false,
                                            error: Some(format!("Failed to read uploaded file: {}", e)),
                                            entries: None, content: None,
                                            path: Some(final_path.to_string()), new_path: None,
                                        }
                                    };
                                    if let Some(ref req_id) = mcp_request_id {
                                        let mut payload = serde_json::to_value(&result).unwrap();
                                        if let serde_json::Value::Object(ref mut map) = payload {
                                            map.insert("_mcp_request_id".to_string(), serde_json::Value::String(req_id.clone()));
                                        }
                                        let resp = Message { msg_type: "fs:result".into(), session_id: client.session_id.clone(), payload };
                                        out.control(resp).await;
                                        continue;
                                    }
                                    let payload = serde_json::to_value(&result).unwrap();
                                    let resp = Message { msg_type: "fs:result".into(), session_id: client.session_id.clone(), payload };
                                    out.control(resp).await;
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
                                    out.control(resp).await;
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
                                    out.control(resp).await;
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
                                    out.control(resp).await;
                                }

                                "mcp:exec" => {
                                    let cmd = msg.payload["cmd"].as_str().unwrap_or("").to_string();
                                    let timeout_ms = msg.payload["timeout_ms"].as_u64().unwrap_or(30_000);
                                    let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                    let session_id = client.session_id.clone();
                                    let ctrl_tx = task_control_tx.clone();
                                    // Spawn so a long-running command cannot freeze the main loop
                                    // (which would otherwise starve input and MCP round-trips).
                                    tokio::spawn(async move {
                                        let (stdout, stderr, exit_code) = execute_command(&cmd, timeout_ms).await;
                                        let result = McpResultPayload { stdout, stderr, exit_code };
                                        let mut payload = serde_json::to_value(&result).unwrap();
                                        if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                            (mcp_request_id, &mut payload)
                                        {
                                            map.insert("_mcp_request_id".to_string(), serde_json::Value::String(req_id));
                                        }
                                        let resp = Message { msg_type: "mcp:result".to_string(), session_id, payload };
                                        let _ = ctrl_tx.send(serde_json::to_string(&resp).unwrap_or_default()).await;
                                    });
                                }

                                "mcp:exec_start" => {
                                    let cmd = msg.payload["cmd"].as_str().unwrap_or("");
                                    let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                    let mut result = match exec_sessions.spawn(cmd).await { Ok(r) => r, Err(r) => r };
                                    result._mcp_request_id = mcp_request_id;
                                    let resp = Message { msg_type: "mcp:exec_result".to_string(), session_id: client.session_id.clone(), payload: serde_json::to_value(&result).unwrap() };
                                    out.control(resp).await;
                                }

                                "mcp:exec_input" => {
                                    let exec_id = msg.payload["exec_id"].as_str().unwrap_or("");
                                    let data_b64 = msg.payload["data_b64"].as_str().unwrap_or("");
                                    let data = fs::decode_b64(data_b64).unwrap_or_default();
                                    let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                    let mut result = match exec_sessions.write_stdin(exec_id, &data).await { Ok(r) => r, Err(r) => r };
                                    result._mcp_request_id = mcp_request_id;
                                    let resp = Message { msg_type: "mcp:exec_result".to_string(), session_id: client.session_id.clone(), payload: serde_json::to_value(&result).unwrap() };
                                    out.control(resp).await;
                                }

                                "mcp:exec_close" => {
                                    let exec_id = msg.payload["exec_id"].as_str().unwrap_or("");
                                    let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                    let mut result = match exec_sessions.close(exec_id).await { Ok(r) => r, Err(r) => r };
                                    result._mcp_request_id = mcp_request_id;
                                    let resp = Message { msg_type: "mcp:exec_result".to_string(), session_id: client.session_id.clone(), payload: serde_json::to_value(&result).unwrap() };
                                    out.control(resp).await;
                                }

                                "mcp:exec_list" => {
                                    let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                    let mut result = exec_sessions.list().await;
                                    result._mcp_request_id = mcp_request_id;
                                    let resp = Message { msg_type: "mcp:exec_result".to_string(), session_id: client.session_id.clone(), payload: serde_json::to_value(&result).unwrap() };
                                    out.control(resp).await;
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
                                    out.control(tab_msg).await;

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
                                            out.control(replay_msg).await;
                                        }
                                    }

                                    let sw_msg = Message {
                                        msg_type: "session:tab_switched".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: serde_json::json!({ "tab_id": active_tab_id }),
                                    };
                                    out.control(sw_msg).await;
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

    exec_sessions.shutdown_all().await;
    tabs.clear(); // Drop all tabs - shells kill child processes via Drop

    // Return the tokens used this session so `start` can replay them on reconnect.
    Ok(Some(client.tokens.clone()))
}

async fn execute_command(cmd: &str, timeout_ms: u64) -> (String, String, i32) {
    let cmd = cmd.to_string();
    let timeout = std::time::Duration::from_millis(timeout_ms);

    // Prefer `script` for PTY allocation so interactive prompts (sudo, gh, etc.)
    // are captured instead of leaking to the agent host terminal via /dev/tty.
    // Fall back to direct `sh -c` if `script` is unavailable (minimal containers).
    let has_script = tokio::process::Command::new("which")
        .arg("script")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);

    let output = if has_script {
        tokio::process::Command::new("script")
            .arg("-q")
            .arg("-c")
            .arg(&cmd)
            .arg("/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
    } else {
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
    };

    let result = tokio::time::timeout(timeout, output).await;

    match result {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let exit_code = out.status.code().unwrap_or(-1);
            (stdout, stderr, exit_code)
        }
        Ok(Err(e)) => (
            String::new(),
            format!("Failed to execute command: {}", e),
            -1,
        ),
        Err(_) => (
            String::new(),
            format!("Command timed out after {}s", timeout_ms / 1000),
            -1,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_coalescing_per_tab() {
        // sender_loop accumulates chunks per tab; a flush emits one message
        // per tab with the concatenated bytes — collapsing a `cat kern.log`
        // burst into a handful of POSTs.
        let mut pending: HashMap<String, Vec<u8>> = HashMap::new();
        for chunk in [b"a".as_slice(), b"b", b"c"] {
            pending.entry("tab1".to_string()).or_default().extend(chunk);
        }
        pending.entry("tab2".to_string()).or_default().extend(b"xy");
        let mut drained: HashMap<String, Vec<u8>> =
            pending.drain().filter(|(_, d)| !d.is_empty()).collect();
        assert_eq!(drained.remove("tab1").unwrap(), b"abc".to_vec());
        assert_eq!(drained.remove("tab2").unwrap(), b"xy".to_vec());
    }

    #[tokio::test]
    async fn test_out_control_delivers_serialized_message() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(4);
        let out = Out {
            control_tx: tx,
            output_tx: tokio::sync::mpsc::channel::<(String, Vec<u8>)>(4).0,
        };
        let msg = Message {
            msg_type: "mcp:result".to_string(),
            session_id: "s1".to_string(),
            payload: serde_json::json!({"stdout":"hi","exit_code":0}),
        };
        out.control(msg).await;
        let received = rx.recv().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&received).unwrap();
        assert_eq!(v["type"], "mcp:result");
        assert_eq!(v["payload"]["stdout"], "hi");
    }

    #[tokio::test]
    async fn test_out_output_drops_instead_of_blocking() {
        // Bounded output channel: flooding past capacity drops chunks (try_send
        // returns Err) rather than stalling the main loop.
        let (tx, _rx) = tokio::sync::mpsc::channel::<(String, Vec<u8>)>(1);
        let out = Out {
            control_tx: tokio::sync::mpsc::channel::<String>(1).0,
            output_tx: tx,
        };
        // Fill + overflow; must return promptly without awaiting.
        for _ in 0..1000 {
            out.output("t".to_string(), b"x".to_vec());
        }
    }

    #[test]
    fn test_home_dir_prefers_home() {
        assert_eq!(
            super::home_dir_from(Some("/home/u".to_string()), None),
            "/home/u"
        );
    }

    #[test]
    fn test_home_dir_falls_back_to_userprofile() {
        assert_eq!(
            super::home_dir_from(None, Some("C:\\Users\\u".to_string())),
            "C:\\Users\\u"
        );
    }

    #[test]
    fn test_home_dir_defaults_to_dot() {
        assert_eq!(super::home_dir_from(None, None), ".");
    }
}
