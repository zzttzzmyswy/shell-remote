pub mod client;
pub mod device;
pub mod desktop;
pub mod encoding;
pub mod exec_sessions;
pub mod fs;
pub mod shell;
pub mod upgrade;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::agent::client::RelayClient;
use crate::agent::shell::Shell;
use crate::proto::{McpResultPayload, Message};

/// Returns the user's home directory, preferring `$HOME` (unix) and falling
/// back to `%USERPROFILE%` (Windows). Used for the file-manager root default
/// and the PTY child's cwd so the same code path works on both platforms.
pub(crate) fn home_dir() -> String {
    home_dir_from(
        std::env::var("HOME").ok(),
        std::env::var("USERPROFILE").ok(),
    )
}

fn home_dir_from(home: Option<String>, userprofile: Option<String>) -> String {
    home.or(userprofile).unwrap_or_else(|| ".".to_string())
}

struct TabState {
    shell: Shell,
    title: String,
    output_buf: Vec<u8>,
}

/// In-flight chunked-upload reassembly state. Holds the open destination file
/// across chunk messages so each chunk's decoded bytes append in order; the
/// last chunk flushes/closes and emits the fs:result reply.
struct UploadReassembly {
    file: std::fs::File,
    final_path: String,
    last_activity: std::time::Instant,
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
/// Also POSTs a lightweight `ping` every `heartbeat` so the outbound NAT
/// mapping stays alive (strict NATs only refresh on agent→relay traffic) and a
/// dead uplink shows up as repeating POST failures.
async fn sender_loop(
    client: reqwest::Client,
    send_url: String,
    session_id: String,
    mut control_rx: tokio::sync::mpsc::Receiver<String>,
    mut output_rx: tokio::sync::mpsc::Receiver<(String, Vec<u8>)>,
    heartbeat: Duration,
) {
    let mut pending: HashMap<String, Vec<u8>> = HashMap::new();
    let mut timer = tokio::time::interval(Duration::from_millis(16));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_tick = tokio::time::interval(heartbeat);
    heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
            _ = heartbeat_tick.tick() => {
                let ping = serde_json::json!({"type": "ping", "session_id": &session_id}).to_string();
                post_raw(&client, &send_url, &ping).await;
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
        // Decode subprocess/PTY bytes to UTF-8 before base64-ing so browsers
        // (which TextDecoder UTF-8 by default) render correctly even when the
        // agent's console emits a legacy encoding (e.g. GBK on Windows). On
        // POSIX this is a no-op (already UTF-8).
        let text = crate::agent::encoding::decode_bytes(&data);
        let encoded = fs::encode_b64(text.as_bytes());
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
            tracing::warn!(
                "Agent POST failed ({}): {}",
                status,
                &body[..body.len().min(200)]
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("Agent POST send error: {}", e),
    }
}

/// Map an I/O error to a coarse kind the relay can translate into an HTTP
/// status code (404 / 400 / 403 / 500).
fn kind_for_io(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::NotFound => "not_found",
        std::io::ErrorKind::PermissionDenied => "permission_denied",
        std::io::ErrorKind::IsADirectory | std::io::ErrorKind::NotADirectory => "is_directory",
        _ => "other",
    }
}

/// Post a single error `fs:result` (correlated by `_mcp_request_id`) for a
/// failed download; `kind` categorizes the failure for HTTP status mapping.
async fn post_fs_err(
    client: &reqwest::Client,
    send_url: &str,
    session_id: &str,
    path: &str,
    mcp_request_id: &Option<String>,
    kind: &str,
    err: &str,
) {
    let payload = serde_json::json!({
        "success": false, "error": err, "kind": kind, "path": path,
        "_mcp_request_id": mcp_request_id.clone()
    });
    let msg = serde_json::json!({"type":"fs:result","session_id":session_id,"payload":payload})
        .to_string();
    post_raw(client, send_url, &msg).await;
}

/// Build a chunked `fs:result` payload envelope for a single download chunk.
/// Pure function extracted from `stream_file_download` so it can be unit-tested
/// independently of I/O. `file_size` (full file length) rides every chunk so
/// the relay can emit Content-Length / Content-Range even for the first chunk.
fn build_download_chunk_payload(
    session_id: &str,
    path: &str,
    content_b64: &str,
    idx: u32,
    total: u32,
    mcp_request_id: Option<String>,
    file_size: u64,
) -> serde_json::Value {
    let is_last = idx + 1 >= total;
    let payload = serde_json::json!({
        "success": true,
        "content": content_b64,
        "chunk_index": idx,
        "total_chunks": total,
        "is_last": is_last,
        "file_size": file_size,
        "path": path,
        "_mcp_request_id": mcp_request_id.clone()
    });
    serde_json::json!({"type":"fs:result","session_id":session_id,"payload":payload})
}

/// Stream a file to the relay as chunked `fs:result` messages (one base64
/// chunk per POST), correlated by `_mcp_request_id`. Runs in its own task so
/// a large/slow download can't block the agent's main message loop (and thus
/// terminal input). Each message stays small so the relay can't be held by a
/// single giant message, and backpressure flows through the HTTP POST.
///
/// Each chunk is POSTed independently via `client.post()`, bypassing the
/// `sender_loop` control channel so download chunks don't compete with
/// terminal:output / mcp:result. Between chunks: `tokio::task::yield_now()`.
///
/// If `cancel` is supplied, the task checks `cancel.load(Ordering::Relaxed)`
/// before each chunk and aborts early when the peer disconnects.
/// `download_cancels` is the shared registry; this task removes its own entry
/// on completion (normal or error) so the map doesn't accumulate stale entries
/// after the download finishes.
#[allow(clippy::too_many_arguments)]
async fn stream_file_download(
    client: reqwest::Client,
    send_url: String,
    session_id: String,
    root: PathBuf,
    path: String,
    mcp_request_id: Option<String>,
    cancel: Option<Arc<AtomicBool>>,
    download_cancels: Arc<std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>>,
    offset: u64,
    limit: Option<u64>,
) {
    const CHUNK_SIZE: usize = 256 * 1024;
    use std::io::{Read, Seek, SeekFrom};

    let resolved = match crate::agent::fs::resolve_path(&root, &path) {
        Some(p) => p,
        None => {
            post_fs_err(
                &client,
                &send_url,
                &session_id,
                &path,
                &mcp_request_id,
                "invalid_path",
                "Invalid path",
            )
            .await;
            if let Some(ref cid) = mcp_request_id {
                let _ = download_cancels.lock().unwrap().remove(cid);
            }
            return;
        }
    };

    let meta = match std::fs::metadata(&resolved) {
        Ok(m) => m,
        Err(e) => {
            let kind = kind_for_io(&e);
            post_fs_err(
                &client,
                &send_url,
                &session_id,
                &path,
                &mcp_request_id,
                kind,
                &format!("Failed to read file: {}", e),
            )
            .await;
            if let Some(ref cid) = mcp_request_id {
                let _ = download_cancels.lock().unwrap().remove(cid);
            }
            return;
        }
    };
    if meta.is_dir() {
        post_fs_err(
            &client,
            &send_url,
            &session_id,
            &path,
            &mcp_request_id,
            "is_directory",
            "Path is a directory",
        )
        .await;
        if let Some(ref cid) = mcp_request_id {
            let _ = download_cancels.lock().unwrap().remove(cid);
        }
        return;
    }
    let file_size = meta.len();

    let mut f = match std::fs::File::open(&resolved) {
        Ok(f) => f,
        Err(e) => {
            let kind = kind_for_io(&e);
            post_fs_err(
                &client,
                &send_url,
                &session_id,
                &path,
                &mcp_request_id,
                kind,
                &format!("Failed to open file: {}", e),
            )
            .await;
            if let Some(ref cid) = mcp_request_id {
                let _ = download_cancels.lock().unwrap().remove(cid);
            }
            return;
        }
    };
    // Partial read (HTTP Range): seek to offset, cap the read to `limit`
    // bytes. An out-of-range offset reads 0 bytes → agent reports success with
    // an empty body; the relay's 416 check happens against file_size.
    if offset > 0 {
        if let Err(e) = f.seek(SeekFrom::Start(offset)) {
            post_fs_err(
                &client,
                &send_url,
                &session_id,
                &path,
                &mcp_request_id,
                "other",
                &format!("Failed to seek file: {}", e),
            )
            .await;
            if let Some(ref cid) = mcp_request_id {
                let _ = download_cancels.lock().unwrap().remove(cid);
            }
            return;
        }
    }
    let to_read: u64 = limit.map_or(file_size.saturating_sub(offset), |l| {
        l.min(file_size.saturating_sub(offset))
    });

    let total_chunks = (to_read.div_ceil(CHUNK_SIZE as u64) as u32).max(1);

    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut idx: u32 = 0;
    let mut remaining = to_read;
    loop {
        // Check for cancellation before reading/processing each chunk
        if let Some(ref c) = cancel {
            if c.load(Ordering::Relaxed) {
                tracing::debug!("Download cancelled for {}", path);
                break;
            }
        }
        let want = remaining.min(CHUNK_SIZE as u64) as usize;
        let n = if remaining == 0 {
            // Offset starts at/past EOF: still send one empty chunk so the
            // relay learns file_size and can answer 416 Range Not Satisfiable.
            0
        } else {
            match f.read(&mut buf[..want]) {
                Ok(n) => n,
                Err(e) => {
                    post_fs_err(
                        &client,
                        &send_url,
                        &session_id,
                        &path,
                        &mcp_request_id,
                        "other",
                        &format!("Failed to read file: {}", e),
                    )
                    .await;
                    if let Some(ref cid) = mcp_request_id {
                        let _ = download_cancels.lock().unwrap().remove(cid);
                    }
                    return;
                }
            }
        };
        if n > 0 {
            remaining -= n as u64;
        }
        let content_b64 = crate::agent::fs::encode_b64(&buf[..n]);
        let msg = build_download_chunk_payload(
            &session_id,
            &path,
            &content_b64,
            idx,
            total_chunks,
            mcp_request_id.clone(),
            file_size,
        );
        // Independent POST (not via sender_loop) so terminal:output/mcp:result
        // are not starved by download chunks.
        let _ = client.post(&send_url).json(&msg).send().await;
        tokio::task::yield_now().await;
        idx += 1;
        if idx >= total_chunks || remaining == 0 || n == 0 {
            break;
        }
    }
    // Remove from cancel registry now that the download has finished (normal
    // completion, cancellation, or early EOF). The spawned task holds its own
    // Arc clone, so a late fs:read_cancel is harmless (token already dropped).
    if let Some(ref cid) = mcp_request_id {
        let _ = download_cancels.lock().unwrap().remove(cid);
    }
}

/// Handle a relay-aborted upload: drop the open reassembly handle (so the
/// half-written file is closed) and best-effort delete the destination file
/// so no truncated artifact lingers. No reply is emitted — the relay's abort
/// uses a fresh `_mcp_request_id` not registered in `pending_mcp`.
fn handle_upload_abort(
    reassembly: &mut HashMap<String, UploadReassembly>,
    root: &Path,
    upload_id: &str,
    final_path: &str,
) {
    reassembly.remove(upload_id);
    if let Some(p) = crate::agent::fs::resolve_path(root, final_path) {
        let _ = std::fs::remove_file(&p);
    }
}

///
/// Assemble one base64 chunk of a chunked upload into `final_path`.
/// Chunk 0 opens (truncating) the destination and writes; subsequent chunks
/// append to the open file held in `reassembly` (keyed by `upload_id`); the
/// last chunk flushes, closes, and returns a terminal result. Returns
/// `(result, more_expected)` — when `more_expected` is true the caller should
/// wait for the final chunk before emitting `fs:result`.
fn assemble_upload_chunk(
    reassembly: &mut HashMap<String, UploadReassembly>,
    root: &Path,
    upload_id: &str,
    final_path: &str,
    content_b64: &str,
    chunk_index: u32,
    total_chunks: u32,
) -> (crate::proto::FsResultPayload, bool) {
    use std::io::Write;
    let is_last = total_chunks > 0 && chunk_index + 1 >= total_chunks;
    let decoded_opt = crate::agent::fs::decode_b64(content_b64);

    let err = |kind: &str, msg: &str| crate::proto::FsResultPayload {
        kind: Some(kind.to_string()),
        success: false,
        error: Some(msg.to_string()),
        entries: None,
        content: None,
        path: Some(final_path.to_string()),
        new_path: None,
    };
    let err_no_kind = |msg: &str| err("other", msg);
    let ok = || crate::proto::FsResultPayload {
        kind: None,
        success: true,
        error: None,
        entries: None,
        content: None,
        path: Some(final_path.to_string()),
        new_path: None,
    };

    if chunk_index == 0 {
        match decoded_opt {
            None => (err_no_kind("Invalid base64 content"), false),
            Some(decoded) => match crate::agent::fs::resolve_path(root, final_path) {
                None => (err("invalid_path", "Invalid destination path"), false),
                Some(p) => match std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&p)
                {
                    Err(e) => (err(kind_for_io(&e), &format!("Failed to open destination: {}", e)), false),
                    Ok(mut f) => {
                        if f.write_all(&decoded).is_err() {
                            (err_no_kind("Failed to write uploaded chunk"), false)
                        } else if is_last {
                            let _ = f.sync_all();
                            (ok(), false)
                        } else {
                            reassembly.insert(
                                upload_id.to_string(),
                                UploadReassembly {
                                    file: f,
                                    final_path: final_path.to_string(),
                                    last_activity: std::time::Instant::now(),
                                },
                            );
                            (ok(), true)
                        }
                    }
                },
            },
        }
    } else {
        match decoded_opt {
            None => (err_no_kind("Invalid base64 content"), false),
            Some(decoded) => match reassembly.remove(upload_id) {
                None => (
                    err_no_kind("Upload chunk received without a preceding chunk 0"),
                    false,
                ),
                Some(mut st) => {
                    let fp = st.final_path.clone();
                    if st.file.write_all(&decoded).is_err() {
                        (err_no_kind("Failed to write uploaded chunk"), false)
                    } else if is_last {
                        let _ = st.file.sync_all();
                        (
                            crate::proto::FsResultPayload {
                                kind: None,
                                success: true,
                                error: None,
                                entries: None,
                                content: None,
                                path: Some(fp),
                                new_path: None,
                            },
                            false,
                        )
                    } else {
                        let mut st = st;
                        st.last_activity = std::time::Instant::now();
                        reassembly.insert(upload_id.to_string(), st);
                        (
                            crate::proto::FsResultPayload {
                                kind: None,
                                success: true,
                                error: None,
                                entries: None,
                                content: None,
                                path: Some(fp),
                                new_path: None,
                            },
                            true,
                        )
                    }
                }
            },
        }
    }
}

pub async fn start(
    relay_url: String,
    key: Option<String>,
    root: String,
    token_type: String,
    shell_path: String,
    session_id: Option<String>,
    desktop_cfg: crate::agent::desktop::DesktopConfig,
    insecure_tls: bool,
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
            session_id.as_deref(),
            &mut cached_tokens,
            &desktop_cfg,
            insecure_tls,
        )
        .await
        {
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
    session_id: Option<&str>,
    cached_tokens: &mut Option<Vec<(String, String)>>,
    desktop_cfg: &crate::agent::desktop::DesktopConfig,
    insecure_tls: bool,
) -> anyhow::Result<()> {
    // Validate the root directory BEFORE registering with the relay. A bad
    // root must fail fast without minting a session — otherwise the relay
    // keeps a ghost session entry that blocks re-registration with the same
    // --session-id (HTTP 409) on every reconnect attempt.
    let root_path = PathBuf::from(root);
    if !root_path.is_dir() {
        anyhow::bail!(
            "Root directory does not exist or is not a directory: {}",
            root
        );
    }

    let mut client = RelayClient::connect_with_retry(
        relay_url,
        key.clone(),
        token_type,
        session_id,
        cached_tokens.as_deref(),
        10,
        insecure_tls,
    )
    .await?;

    // Cache the tokens the moment registration succeeds — before anything
    // later in this function can bail (e.g. shell spawn failure). On the next
    // reconnect, `connect_with_retry` replays them so the relay takes the
    // `register_existing` path, which evicts this session's stale prior
    // incarnation instead of rejecting with 409 "session_id already in use".
    *cached_tokens = Some(client.tokens.clone());

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
        Duration::from_secs(15),
    ));
    // Keep a control sender for spawned long-running tasks (e.g. mcp:exec).
    let task_control_tx = control_tx;

    // Desktop video sharing: control + frame messages are posted through a
    // single-task FIFO so the byte stream reaches the relay in order
    // (concurrent sends could reorder fragments and break playback).
    let desktop = std::sync::Arc::new(crate::agent::desktop::DesktopManager::new(desktop_cfg.clone()));
    // 时钟校准：采样 relay /api/clock 求 (relay_epoch - 本地_epoch) 偏移，
    // 注入 DesktopManager，srtc 打点落在 relay 时基 —— e2e 延时从此不再
    // 依赖 agent/浏览器两机系统时钟同步（MYS-886 指标失真根因）。
    if desktop_cfg.enabled() {
        let cc = client.http_client().clone();
        let clock_base = client.send_url().trim_end_matches("/agent/send").to_string();
        let dm2 = desktop.clone();
        tokio::spawn(async move {
            let mut samples: Vec<i64> = Vec::with_capacity(3);
            for _ in 0..3 {
                let t0 = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                match cc.get(format!("{}/api/clock", clock_base)).send().await {
                    Ok(r) => {
                        if let Ok(j) = r.json::<serde_json::Value>().await {
                            if let Some(ep) = j["epoch_ms"].as_u64() {
                                let t1 = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as i64)
                                    .unwrap_or(0);
                                let rtt = t1 - t0;
                                let relay_at_t0 = ep as i64 - rtt / 2;
                                samples.push(relay_at_t0 - t0);
                                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                            }
                        }
                    }
                    Err(e) => tracing::warn!("clock calibrate failed: {e}"),
                }
            }
            if !samples.is_empty() {
                samples.sort_unstable();
                let offset = samples[samples.len() / 2];
                tracing::info!(clock_offset_ms = offset, "relay clock calibrated");
                dm2.set_clock_offset(offset);
            }
        });
    }
    let (post_tx, mut post_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    {
        let pc = client.http_client().clone();
        let base = client
            .send_url()
            .trim_end_matches("/agent/send")
            .to_string();
        let sid = client.session_id.clone();
        let server_auth = client.server_auth().to_string();
        // 自签 https relay 模式：桌面 WS 上行同样跳过证书校验（与 register
        // 通道保持一致——relay 是自签证书, 不信任则 wss 握手必失败）。
        let insecure_tls = client.insecure_tls();
        if insecure_tls {
            crate::tlsutil::install_rustls_provider();
        }
        // 当前的上行链路方式（ws | http）——浏览器指标面板展示用。
        // WS 连接建立/失败回退时更新并广播 desktop:uplink。
        let uplink_mode = Arc::new(std::sync::atomic::AtomicU8::new(0)); // 0=未知 1=ws 2=http
        {
            let uplink_mode = uplink_mode.clone();
            tokio::spawn(async move {
            // 上行通道（v0.21）: WebSocket 长连接逐帧发送 —— 无每批 HTTP
            // 握手、无 80ms 攒批窗口、拥塞窗口跨帧保持热态，公网/弱网下
            // 桌面帧的单帧上行时延显著低于批量 POST。WS 不可用（老 relay）
            // 或连接失败时自动回退到 HTTP 批量 POST 路径。
            // 积压保护仍保留：队列深度超过阈值时丢最旧的非关键帧，
            // 控制消息（started/stopped/error）绝不丢弃。
            const MAX_PENDING_FRAMES: usize = 24; // ≈ 30fps×0.8s：稳态积压有界（旧 90 ≈3s）
            let ws_url = {
                // 配置是 http/https，视频上行内部映射 http→ws / https→wss。
                let mut u = base.replace("http://", "ws://").replace("https://", "wss://");
                u.push_str("/agent/ws/send?session=");
                u.push_str(&sid);
                if !server_auth.is_empty() {
                    u.push_str("&auth=");
                    u.push_str(&server_auth);
                }
                u
            };
            let mut ws: Option<
                tokio_tungstenite::WebSocketStream<
                    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
                >,
            > = None;
            let mut ws_failures: u32 = 0;
            loop {
                let first = match post_rx.recv().await {
                    Some(m) => m,
                    None => return, // 通道关闭（会话结束）
                };
                let mut batch = vec![first];
                // 微批窗口（≤8ms）只用于合并同一 tick 内的帧, 不是延迟发送
                // 的节流器: 队列里有积压立即全取。
                let deadline =
                    tokio::time::Instant::now() + Duration::from_millis(8);
                loop {
                    match post_rx.try_recv() {
                        Ok(m) => {
                            batch.push(m);
                            if batch.len() >= 64 {
                                break;
                            }
                        }
                        Err(_) => {
                            if tokio::time::Instant::now() < deadline && batch.len() < 64 {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                            } else {
                                break;
                            }
                        }
                    }
                }
                // 跨批丢帧（MYS-886 延迟修复）: channel 是 unbounded 的, 生产端
                // （编码循环 60fps）持续快于消费端（WS send / relay 逐条路由）
                // 时会**无限积压**——实测 Windows GDI 路径稳态积压 ~4s, 表现为
                // 端到端延迟 4s 且不增长（生产消费恰好平衡在积压点）。批内
                // MAX_PENDING_FRAMES 检查永远看不到跨批积压。这里在每轮发送前
                // 主动排水: 队列深度超过 90 帧时丢掉最旧的 media 帧（控制消息
                // 保留）, 保证上行端到端延迟上限 ≈ 1.5s 而非无界。
                if post_rx.len() > MAX_PENDING_FRAMES {
                    let mut dropped = 0usize;
                    while post_rx.len() > MAX_PENDING_FRAMES {
                        match post_rx.try_recv() {
                            Ok(m) => {
                                if m["type"] == "desktop:video" {
                                    dropped += 1;
                                } else {
                                    // 控制消息插回本批发送（数量少, 不会放大）。
                                    batch.push(m);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    if dropped > 0 {
                        tracing::debug!("uplink drained {dropped} stale frames (cross-batch backpressure)");
                    }
                }
                // 丢旧保新：media 只留最新 N 条，控制消息全保留。
                let mut media: Vec<serde_json::Value> = Vec::new();
                let mut ctrl: Vec<serde_json::Value> = Vec::new();
                for m in batch {
                    if m["type"] == "desktop:video" {
                        media.push(m);
                    } else {
                        ctrl.push(m);
                    }
                }
                if media.len() > MAX_PENDING_FRAMES {
                    let drop = media.len() - MAX_PENDING_FRAMES;
                    media.drain(..drop);
                    tracing::debug!("uplink dropped {drop} oldest media frames (backpressure)");
                }
                let mut out: Vec<serde_json::Value> = ctrl;
                out.extend(media);
                if out.is_empty() {
                    continue;
                }
                for m in &mut out {
                    m["session_id"] = serde_json::Value::String(sid.clone());
                }
                // WS 路径: 逐帧（每条消息一个 text frame）。连接失败按指数
                // 退避重试; 三次失败后停止尝试一段时间, 本批走 HTTP 兜底。
                let mut sent_via_ws = false;
                let ws_connected_now = if ws.is_none() && ws_failures < 3 {
                    match tokio::time::timeout(
                        Duration::from_secs(3),
                        tokio_tungstenite::connect_async_tls_with_config(
                            &ws_url,
                            None,
                            true,
                            if insecure_tls {
                                Some(tokio_tungstenite::Connector::Rustls(
                                    crate::tlsutil::no_verify_client_config(),
                                ))
                            } else {
                                None
                            },
                        ),
                    )
                    .await
                    {
                        Ok(Ok((stream, _resp))) => {
                            tracing::info!("desktop WS uplink connected");
                            ws = Some(stream);
                            ws_failures = 0;
                            true
                        }
                        Ok(Err(e)) => {
                            ws_failures += 1;
                            tracing::warn!("desktop WS uplink connect failed ({ws_failures}): {e}");
                            tokio::time::sleep(Duration::from_millis(
                                200u64.saturating_mul(1 << ws_failures.min(5)),
                            ))
                            .await;
                            false
                        }
                        Err(_) => {
                            ws_failures += 1;
                            tracing::warn!("desktop WS uplink connect timed out ({ws_failures})");
                            false
                        }
                    }
                } else {
                    ws.is_some()
                };
                // 链路方式上报（浏览器指标面板显示 ws/http）。两种时机:
                // ① 方式变化时(WS 建立失败回退)——但首次翻转可能发生在浏览器
                //   加入前, 广播无人接收; ② 批内含 desktop:started 时总是重发
                //   ——started 必然有浏览器在等, 保证面板能拿到当前值。
                // 必须在发送**之前**插入（此前放在 send 之后, WS 路径下 out
                // 已发出, 只有 HTTP 回退路径能带上——真机验证踩过的坑）。
                {
                    use std::sync::atomic::Ordering as O;
                    let expected: u8 = if ws_connected_now { 1 } else { 2 };
                    let has_started = out.iter().any(|m| m["type"] == "desktop:started");
                    if uplink_mode.swap(expected, O::Relaxed) != expected || has_started {
                        out.insert(
                            0,
                            serde_json::json!({
                                "type": "desktop:uplink",
                                "payload": { "uplink": if ws_connected_now { "ws" } else { "http" } },
                                "session_id": sid.clone(),
                            }),
                        );
                    }
                }
                if let Some(stream) = ws.as_mut() {
                    use futures_util::SinkExt;
                    let payload = serde_json::Value::Array(out.clone()).to_string();
                    match stream.send(payload.into()).await {
                        Ok(()) => sent_via_ws = true,
                        Err(e) => {
                            tracing::warn!("desktop WS send failed: {e} — falling back to HTTP");
                            ws = None;
                            ws_failures += 1;
                            // 本批按实际结果改走 HTTP（uplink 标记已按 WS 发出,
                            // 下批会纠正）。
                        }
                    }
                }
                if ws.is_none() && sent_via_ws == false && ws_failures >= 3 {
                    // WS 持续失败: 冷却 30s 后重新允许尝试（长会话中 relay
                    // 升级后 WS 恢复可用）。
                    tokio::time::sleep(Duration::ZERO).await;
                    if post_rx.is_empty() {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        ws_failures = 0;
                    }
                }
                if sent_via_ws {
                    continue;
                }
                // HTTP 批量回退（老 relay / WS 持续失败）。
                let batch: serde_json::Value = serde_json::Value::Array(out);
                // 视频发送超时对齐 rustdesk SEND_TIMEOUT_VIDEO=12s：桌面帧在
                // 弱网/大帧时允许更长发送窗口，避免误判断连触发回退（MYS-886）。
                let send_outcome = tokio::time::timeout(
                    Duration::from_secs(12),
                    pc.post(format!("{base}/agent/send")).json(&batch).send(),
                )
                .await;
                match send_outcome {
                    Ok(Ok(r)) => {
                        let status = r.status();
                        let body_len = tokio::time::timeout(Duration::from_secs(2), r.bytes())
                            .await
                            .map(|b| b.map(|b| b.len()).unwrap_or(0))
                            .unwrap_or(0);
                        tracing::trace!("desk POST batch {status} body={body_len}");
                    }
                    Ok(Err(e)) => tracing::warn!("desk POST batch failed: {e}"),
                    Err(_) => {
                        tracing::warn!("desk POST batch timed out after 5s, dropping batch")
                    }
                }
                tokio::task::yield_now().await;
            }
            });
        }
    }
    let post_fn: crate::agent::desktop::PostFn = Arc::new(move |msg| {
        let t = msg["type"].as_str().unwrap_or("?").to_string();
        let _ = post_tx.send(msg); // unbounded: 不会失败, 由 consumer 批内丢旧
        tracing::trace!("post_fn queued {t}");
    });

    let exec_sessions = crate::agent::exec_sessions::ExecSessionManager::new();

    let (shell_tx, mut shell_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();

    let is_readonly = token_type == "ro";

    // Only one self-upgrade may run per session; the flag is held by the whole
    // run_session lifetime and cleared by the task on failure (on success the
    // process replaces itself via exec, so nothing to clear).
    let upgrade_in_progress = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Canonical path of the running executable, resolved once at startup so a
    // later in-place rebuild/rotation of this file cannot make the
    // self-upgrade resolver (`/proc/self/exe`) fail mid-upgrade.
    let self_exe: Option<std::path::PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());

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

    // In-flight chunked upload reassembly, keyed by upload_id. Chunks for a
    // transfer arrive interleaved with other messages on the SSE stream, so
    // we keep an open file handle per upload_id and append each decoded
    // chunk; the last chunk flushes, closes, and replies.
    let mut upload_reassembly: HashMap<String, UploadReassembly> = HashMap::new();

    // Cancel registry for in-flight downloads, keyed by correlation_id
    // (_mcp_request_id from the fs:read request). When the relay sends
    // fs:read_cancel{correlation_id} (client disconnected), we set the
    // AtomicBool so the spawned stream_file_download task stops early.
    let download_cancels: Arc<std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    let mut cleanup_tick = tokio::time::interval(Duration::from_secs(60));
    cleanup_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
                _ = cleanup_tick.tick() => {
                    let now = std::time::Instant::now();
                    upload_reassembly.retain(|_, r| {
                        if now.duration_since(r.last_activity) > Duration::from_secs(300) {
                            let _ = std::fs::remove_file(&r.final_path);
                            false
                        } else {
                            true
                        }
                    });
                }
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
                                                let text = crate::agent::encoding::decode_bytes(&ts.output_buf);
                                                let encoded = fs::encode_b64(text.as_bytes());
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
                                    let path = msg.payload["path"].as_str().unwrap_or("").to_string();
                                    let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                    // Optional HTTP-Range fields: `offset` (bytes to skip)
                                    // and `limit` (max bytes). Absent → full file.
                                    let offset = msg.payload.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let limit = msg.payload.get("limit").and_then(|v| v.as_u64());
                                    // Stream the file as chunked fs:result messages
                                    // in a separate task so a large/slow download
                                    // can't block the main loop (terminal input).
                                    let client_req = client.http_client().clone();
                                    let send_url = client.send_url().to_string();
                                    let sid = client.session_id.clone();
                                    let root_clone = root_path.clone();
                                    // Register a cancel token for this correlation_id so the
                                    // relay can abort the download via fs:read_cancel.
                                    let cancel = Arc::new(AtomicBool::new(false));
                                    let cancel_token = cancel.clone();
                                    if let Some(ref cid) = mcp_request_id {
                                        download_cancels.lock().unwrap().insert(cid.clone(), cancel);
                                    }
                                    let dc = download_cancels.clone();
                                    tokio::spawn(stream_file_download(
                                        client_req,
                                        send_url,
                                        sid,
                                        root_clone,
                                        path,
                                        mcp_request_id,
                                        Some(cancel_token),
                                        dc,
                                        offset,
                                        limit,
                                    ));
                                }

                                "fs:read_cancel" => {
                                    let mcp_request_id = msg.payload["_mcp_request_id"]
                                        .as_str()
                                        .map(|s| s.to_string());
                                    if let Some(ref cid) = mcp_request_id {
                                        if let Some(cancel) = download_cancels.lock().unwrap().remove(cid) {
                                            cancel.store(true, Ordering::Relaxed);
                                            tracing::debug!("Cancelled download for correlation_id={}", cid);
                                        }
                                    }
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
                                    // Chunked reassembly (see assemble_upload_chunk).
                                    // Keeps each message small so a big upload
                                    // can't block terminal I/O or blow memory.
                                    let upload_id = msg.payload["upload_id"].as_str().unwrap_or("").to_string();
                                    let final_path = msg.payload["final_path"].as_str().unwrap_or("").to_string();
                                    let content_b64 = msg.payload["content"].as_str().unwrap_or("");
                                    let chunk_index = msg.payload["chunk_index"].as_u64().unwrap_or(0) as u32;
                                    let total_chunks = msg.payload["total_chunks"].as_u64().unwrap_or(0) as u32;
                                    let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());

                                    // Abort: the relay detected a premature client
                                    // close and sent {"aborted":true} with a fresh
                                    // _mcp_request_id (not registered in pending_mcp).
                                    // Drop the open reassembly handle and delete the
                                    // half-written destination file; no reply (the
                                    // relay's abort uses an unregistered id, so no
                                    // oneshot leaks).
                                    if msg.payload.get("aborted").and_then(|v| v.as_bool()) == Some(true) {
                                        handle_upload_abort(&mut upload_reassembly, &root_path, &upload_id, &final_path);
                                        continue;
                                    }

                                    let (result, more_expected) = assemble_upload_chunk(
                                        &mut upload_reassembly,
                                        &root_path,
                                        &upload_id,
                                        &final_path,
                                        content_b64,
                                        chunk_index,
                                        total_chunks,
                                    );

                                    if more_expected {
                                        // More chunks pending; don't emit fs:result yet.
                                        continue;
                                    }
                                    let mut payload = serde_json::to_value(&result).unwrap();
                                    if let (Some(req_id), serde_json::Value::Object(ref mut map)) =
                                        (mcp_request_id, &mut payload)
                                    {
                                        map.insert("_mcp_request_id".to_string(), serde_json::Value::String(req_id));
                                    }
                                    let resp = Message { msg_type: "fs:result".to_string(), session_id: client.session_id.clone(), payload };
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
                                    let shell = shell_path.to_string();
                                    // Spawn so a long-running command cannot freeze the main loop
                                    // (which would otherwise starve input and MCP round-trips).
                                    tokio::spawn(async move {
                                        let (stdout, stderr, exit_code) = execute_command(&cmd, timeout_ms, &shell).await;
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
                                            let text = crate::agent::encoding::decode_bytes(&ts.output_buf);
                                            let encoded = fs::encode_b64(text.as_bytes());
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

                                    // Desktop capability snapshot so the web UI can
                                    // enable/disable the 桌面 button accordingly.
                                    let caps_msg = Message {
                                        msg_type: "desktop:capabilities".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: desktop.capabilities_json(),
                                    };
                                    out.control(caps_msg).await;
                                }

                                "desktop:start" => {
                                    tracing::info!("desktop:start requested");
                                    desktop.start(post_fn.clone()).await;
                                }

                                "desktop:stop" => {
                                    tracing::info!("desktop:stop requested");
                                    desktop.stop(post_fn.clone()).await;
                                }

                                "desktop:codec" => {
                                    // 热切换编码方案（web 页编码器下拉）：
                                    // av1/vp9/h264，切换后自动重建桌面流。
                                    let codec = msg
                                        .payload
                                        .get("codec")
                                        .and_then(|c| c.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    tracing::info!(codec = %codec, "desktop:codec requested");
                                    if let Err(e) = desktop.set_codec(&codec, post_fn.clone()).await {
                                        let err = Message {
                                            msg_type: "error".to_string(),
                                            session_id: client.session_id.clone(),
                                            payload: serde_json::json!({
                                                "code": "DESKTOP_BAD_CODEC",
                                                "message": e
                                            }),
                                        };
                                        out.control(err).await;
                                    }
                                }

                                "desktop:qos" => {
                                    // 端到端延时 + 解码背压反馈 → QoS（内容驱动
                                    // fps：静态1fps/动态满帧/解码背压才降帧；码率由
                                    // 拥塞增量平滑缩放，rustdesk delay−RTT 同构）。
                                    let delay_ms = msg
                                        .payload
                                        .get("delay_ms")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as u32;
                                    let decode_fps = msg
                                        .payload
                                        .get("dfps")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as u32;
                                    let decode_queue = msg
                                        .payload
                                        .get("dq")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as u32;
                                    let ack_seq = msg
                                        .payload
                                        .get("lseq")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let (fps, qos_scale, bitrate_kbps) =
                                        desktop.on_qos_delay(delay_ms, decode_fps, decode_queue, ack_seq).await;
                                    // 回传当前生效的 QoS 状态（对齐 rustdesk TestDelay
                                    // 携带 target_bitrate，MYS-886 #153）：浏览器可展示
                                    // 实际码率/帧率。
                                    let qos_ack = Message {
                                        msg_type: "desktop:qos-ack".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: serde_json::json!({
                                            "fps": fps,
                                            "qos_scale": qos_scale,
                                            "bitrate_kbps": bitrate_kbps,
                                        }),
                                    };
                                    out.control(qos_ack).await;
                                }

                                "desktop:reqkey" => {
                                    // 浏览器请求关键帧（接入/参考链断裂/解码错误，
                                    // 对齐 rustdesk 控制端 refresh_video）：置 flag，
                                    // 编码循环下一拍 force_idr 即时重同步。
                                    desktop.request_idr();
                                }

                                "desktop:quality" => {
                                    // 码率档切换（web 码率下拉）：speed/balanced/best
                                    // + 自定义 kbps。改档后重建桌面流。
                                    let quality = msg
                                        .payload
                                        .get("quality")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("balanced")
                                        .to_string();
                                    let custom_kbps = msg
                                        .payload
                                        .get("bitrate_kbps")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    tracing::info!(quality = %quality, custom_kbps, "desktop:quality requested");
                                    if let Err(e) = desktop
                                        .set_quality(&quality, custom_kbps, post_fn.clone())
                                        .await
                                    {
                                        let err = Message {
                                            msg_type: "error".to_string(),
                                            session_id: client.session_id.clone(),
                                            payload: serde_json::json!({
                                                "code": "DESKTOP_BAD_QUALITY",
                                                "message": e
                                            }),
                                        };
                                        out.control(err).await;
                                    }
                                }

                                "desktop:gray" => {
                                    // 灰度模式开关（web 桌面控制栏，弱网省带宽）：
                                    // 只翻编码前降色度 flag，即时生效不重建流。
                                    let enabled = msg
                                        .payload
                                        .get("enabled")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);
                                    desktop.set_gray(enabled);
                                }

                                "desktop:mouse" => {
                                    // 浏览器键鼠注入：desktop:mouse
                                    // {type,x,y,button,dx,dy}（RW 权限校验
                                    // 已在 relay requires_write 完成）。
                                    desktop.handle_mouse(&msg.payload).await;
                                }

                                "desktop:key" => {
                                    // desktop:key {code,down}（browser
                                    // KeyboardEvent.code 直传）。
                                    desktop.handle_key(&msg.payload).await;
                                }

                                "desktop:clipboard:set" => {
                                    // 浏览器把本地剪贴板文本推到远端。
                                    desktop.handle_clipboard_set(&msg.payload).await;
                                }

                                "desktop:clipboard:get" => {
                                    // 浏览器拉取远端剪贴板（回包经广播）。
                                    desktop.handle_clipboard_get(&post_fn).await;
                                }

                                "desktop:bitrate" => {
                                    // 浏览器周期性上报的实测可用带宽 → 弱网自适应
                                    // (把编码码率天花板 clamp 到网络可承受范围)。
                                    if let Some(kbps) =
                                        msg.payload["kbps"].as_u64()
                                    {
                                        if kbps > 0 {
                                            desktop.set_bandwidth_bps(kbps * 1000);
                                        }
                                    } else if let Some(bps) =
                                        msg.payload["bps"].as_u64()
                                    {
                                        if bps > 0 {
                                            desktop.set_bandwidth_bps(bps);
                                        }
                                    }
                                }

                                "session:leave" => {
                                    let user_id = msg.payload["user_id"].as_str().unwrap_or("");
                                    tracing::info!("User {} left", user_id);
                                }

                                "agent:upgrade" => {
                                    // Admin panel triggered an atomic self-upgrade:
                                    // download → verify SHA-256 → smoke test → atomic
                                    // replace → restart with the same argv. Runs in a
                                    // spawned task so a slow download cannot freeze the
                                    // terminal; on success the process replaces itself.
                                    let req = match crate::agent::upgrade::UpgradeRequest::from_payload(&msg.payload) {
                                        Some(r) => r,
                                        None => {
                                            let err = Message {
                                                msg_type: "error".to_string(),
                                                session_id: client.session_id.clone(),
                                                payload: serde_json::json!({
                                                    "code": "UPGRADE_BAD_PAYLOAD",
                                                    "message": "agent:upgrade missing version/url/sha256"
                                                }),
                                            };
                                            out.control(err).await;
                                            continue;
                                        }
                                    };
                                    if upgrade_in_progress.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                        let err = Message {
                                            msg_type: "error".to_string(),
                                            session_id: client.session_id.clone(),
                                            payload: serde_json::json!({
                                                "code": "UPGRADE_IN_FLIGHT",
                                                "message": "an upgrade is already running"
                                            }),
                                        };
                                        out.control(err).await;
                                        continue;
                                    }
                                    let guard = upgrade_in_progress.clone();
                                    let http = client.http_client().clone();
                                    let relay_base = client
                                        .send_url()
                                        .trim_end_matches("/agent/send")
                                        .to_string();
                                    let token = client
                                        .tokens
                                        .iter()
                                        .find(|(_, p)| p == "rw")
                                        .map(|(t, _)| t.clone())
                                        .or_else(|| client.tokens.first().map(|(t, _)| t.clone()))
                                        .unwrap_or_default();
                                    let sid = client.session_id.clone();
                                    let ctrl = task_control_tx.clone();
                                    match self_exe.clone() {
                                        Some(exe) => {
                                            tokio::spawn(async move {
                                                crate::agent::upgrade::perform_upgrade(
                                                    http, relay_base, token, sid, ctrl, req, exe,
                                                )
                                                .await;
                                                // Only reached on failure — success exits the process.
                                                guard.store(
                                                    false,
                                                    std::sync::atomic::Ordering::SeqCst,
                                                );
                                            });
                                        }
                                        None => {
                                            guard.store(false, std::sync::atomic::Ordering::SeqCst);
                                            let err = Message {
                                                msg_type: "error".to_string(),
                                                session_id: client.session_id.clone(),
                                                payload: serde_json::json!({
                                                    "code": "UPGRADE_NO_EXE",
                                                    "message": "cannot resolve the running executable path"
                                                }),
                                            };
                                            out.control(err).await;
                                            continue;
                                        }
                                    }
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

    // Tokens were already cached into `cached_tokens` right after registration,
    // so `start` can replay them on reconnect — nothing to return here.
    Ok(())
}

#[cfg(unix)]
async fn execute_command(cmd: &str, timeout_ms: u64, _shell: &str) -> (String, String, i32) {
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
            let stdout = crate::agent::encoding::decode_bytes(&out.stdout);
            let stderr = crate::agent::encoding::decode_bytes(&out.stderr);
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

#[cfg(not(unix))]
async fn execute_command(cmd: &str, timeout_ms: u64, shell: &str) -> (String, String, i32) {
    let timeout = std::time::Duration::from_millis(timeout_ms);
    let lower = shell.to_ascii_lowercase();
    let mut command = if lower.contains("powershell") || lower.contains("pwsh") {
        let mut c = tokio::process::Command::new(shell);
        c.arg("-NoProfile").arg("-Command").arg(cmd);
        c
    } else {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.arg("/c").arg(cmd);
        c
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let result = tokio::time::timeout(timeout, command.output()).await;

    match result {
        Ok(Ok(out)) => {
            let stdout = crate::agent::encoding::decode_bytes(&out.stdout);
            let stderr = crate::agent::encoding::decode_bytes(&out.stderr);
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
    fn test_download_chunk_payload_is_last_flag() {
        let p0 = build_download_chunk_payload("s", "/x", "AAAA", 0, 3, Some("r".to_string()), 15);
        let p2 = build_download_chunk_payload("s", "/x", "AAAA", 2, 3, Some("r".to_string()), 15);
        assert_eq!(p0["payload"]["is_last"], serde_json::json!(false));
        assert_eq!(p2["payload"]["is_last"], serde_json::json!(true));
        assert_eq!(p2["payload"]["chunk_index"], serde_json::json!(2));
        assert_eq!(p2["payload"]["total_chunks"], serde_json::json!(3));
    }

    #[test]
    fn test_assemble_upload_chunk_reassembles_multiple_chunks() {
        // Feed a 3-chunk upload through the reassembly state machine and
        // verify the final file equals the concatenation, only the last
        // chunk produces a terminal result, and intermediate chunks ask for
        // more (no premature fs:result).
        let tmp = std::env::temp_dir().join(format!("sr-upload-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let root = tmp.clone();
        let dest = root.join("out.bin");
        let final_path = dest.to_string_lossy().to_string();

        let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();
        let chunks: Vec<Vec<u8>> = vec![vec![1; 100_000], vec![2; 100_000], vec![3; 50_000]];
        let total = chunks.len() as u32;
        let mut got_terminal = false;
        for (i, ch) in chunks.iter().enumerate() {
            let b64 = crate::agent::fs::encode_b64(ch);
            let (res, more) = assemble_upload_chunk(
                &mut reassembly,
                &root,
                "uid-1",
                &final_path,
                &b64,
                i as u32,
                total,
            );
            if i as u32 + 1 == total {
                assert!(!more, "last chunk must be terminal");
                assert!(res.success, "last chunk success");
                got_terminal = true;
            } else {
                assert!(more, "intermediate chunk must expect more");
            }
        }
        assert!(got_terminal);
        assert!(reassembly.is_empty(), "final state should be consumed");

        let written = std::fs::read(&dest).unwrap();
        let mut expected = Vec::new();
        for ch in &chunks {
            expected.extend_from_slice(ch);
        }
        assert_eq!(written, expected);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_assemble_upload_chunk_single_chunk() {
        let tmp = std::env::temp_dir().join(format!("sr-upload-single-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let root = tmp.clone();
        let dest = root.join("one.bin");
        let final_path = dest.to_string_lossy().to_string();
        let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();
        let b64 = crate::agent::fs::encode_b64(b"hello");
        let (res, more) =
            assemble_upload_chunk(&mut reassembly, &root, "uid-2", &final_path, &b64, 0, 1);
        assert!(!more);
        assert!(res.success);
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_assemble_upload_chunk_missing_chunk0_errors() {
        let tmp = std::env::temp_dir().join(format!("sr-upload-missing-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();
        let b64 = crate::agent::fs::encode_b64(b"x");
        let (res, more) =
            assemble_upload_chunk(&mut reassembly, &tmp, "uid-3", "/tmp/none", &b64, 1, 2);
        assert!(!more);
        assert!(!res.success);
        let _ = std::fs::remove_dir_all(&tmp);
    }

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

    #[test]
    fn test_stale_upload_reassembly_reaped() {
        // Simulate an old entry in upload_reassembly and verify the 5min
        // cleanup removes it and deletes the half-written file.
        let tmp = std::env::temp_dir().join(format!("sr-stale-upload-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let dest = tmp.join("stale.bin");
        // Write some bytes so the file exists and removal must succeed.
        std::fs::write(&dest, b"partial").unwrap();
        let final_path = dest.to_string_lossy().to_string();

        let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();
        // Create a stale entry: last_activity set to 10 minutes in the past.
        let old_entry = UploadReassembly {
            file: std::fs::File::open(&dest).unwrap(),
            final_path: final_path.clone(),
            last_activity: std::time::Instant::now() - Duration::from_secs(600),
        };
        reassembly.insert("stale-uid".to_string(), old_entry);

        // Run the retain logic.
        let now = std::time::Instant::now();
        reassembly.retain(|_, r| {
            if now.duration_since(r.last_activity) > Duration::from_secs(300) {
                let _ = std::fs::remove_file(&r.final_path);
                false
            } else {
                true
            }
        });

        assert!(reassembly.is_empty(), "stale entry must be removed");
        assert!(!dest.exists(), "half-written file must be deleted");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_fresh_upload_reassembly_not_reaped() {
        // A fresh entry (just created) must survive the cleanup.
        let tmp = std::env::temp_dir().join(format!("sr-fresh-upload-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let dest = tmp.join("fresh.bin");
        std::fs::write(&dest, b"data").unwrap();
        let final_path = dest.to_string_lossy().to_string();

        let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();
        let fresh_entry = UploadReassembly {
            file: std::fs::File::open(&dest).unwrap(),
            final_path: final_path.clone(),
            last_activity: std::time::Instant::now(),
        };
        reassembly.insert("fresh-uid".to_string(), fresh_entry);

        let now = std::time::Instant::now();
        reassembly.retain(|_, r| {
            if now.duration_since(r.last_activity) > Duration::from_secs(300) {
                let _ = std::fs::remove_file(&r.final_path);
                false
            } else {
                true
            }
        });

        assert_eq!(reassembly.len(), 1, "fresh entry must survive");
        assert!(dest.exists(), "fresh file must not be deleted");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_handle_upload_abort_removes_reassembly_and_file() {
        // Finding 3: an fs:upload with aborted:true must drop the open
        // reassembly handle AND delete the half-written destination file.
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        // Pre-existing partial file at final_path (relative to root).
        let dest = root.join("partial.bin");
        fs::write(&dest, b"partial-data").unwrap();
        assert!(dest.exists());

        // Simulate a prior chunk-0 that opened the file for appending.
        let final_path = "partial.bin".to_string();
        let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();
        reassembly.insert(
            "uid-1".to_string(),
            UploadReassembly {
                file: fs::OpenOptions::new().append(true).open(&dest).unwrap(),
                final_path: final_path.clone(),
                last_activity: std::time::Instant::now(),
            },
        );

        handle_upload_abort(&mut reassembly, &root, "uid-1", &final_path);

        assert!(
            reassembly.is_empty(),
            "reassembly entry must be removed on abort"
        );
        assert!(!dest.exists(), "half-written file must be deleted on abort");

        // Idempotent: a second abort (no entry) is a no-op, doesn't panic.
        handle_upload_abort(&mut reassembly, &root, "uid-1", &final_path);
    }

    #[test]
    fn test_assemble_upload_chunk_refreshes_last_activity_on_append() {
        // Finding 4 (agent): a non-final append chunk must refresh
        // UploadReassembly.last_activity so a slow-but-progressing upload isn't
        // reaped mid-stream.
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let dest = root.join("big.bin");
        // chunk 0: open + write, non-final (total_chunks=2) → inserts entry.
        let (res, more) = assemble_upload_chunk(
            &mut HashMap::new(),
            &root,
            "uid-2",
            "big.bin",
            &crate::agent::fs::encode_b64(b"AAAA"),
            0,
            2,
        );
        assert!(res.success);
        assert!(more);

        let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();
        // Stale entry (as if created long ago, then chunk 1 arrives now).
        let stale = std::time::Instant::now() - Duration::from_secs(600);
        reassembly.insert(
            "uid-2".to_string(),
            UploadReassembly {
                file: fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&dest)
                    .unwrap(),
                final_path: "big.bin".to_string(),
                last_activity: stale,
            },
        );
        let before = reassembly.get("uid-2").unwrap().last_activity;
        assert!(std::time::Instant::now().duration_since(before) > Duration::from_secs(5));

        // Non-final append (chunk_index=1, total_chunks=3 → not last) refreshes.
        let (_res, more2) = assemble_upload_chunk(
            &mut reassembly,
            &root,
            "uid-2",
            "big.bin",
            &crate::agent::fs::encode_b64(b"BBBB"),
            1,
            3,
        );
        assert!(more2, "chunk 1/3 is not last → more expected");
        let after = reassembly.get("uid-2").unwrap().last_activity;
        assert!(
            std::time::Instant::now().duration_since(after) < Duration::from_secs(1),
            "last_activity must be refreshed on non-final append"
        );
    }

    #[tokio::test]
    async fn test_sender_loop_sends_periodic_heartbeat() {
        // The agent must periodically POST a lightweight ping so the outbound
        // NAT mapping stays alive and a dead uplink becomes visible via POST
        // failures — otherwise strict NATs silently kill the relay→agent SSE
        // and the agent never notices.
        use std::sync::{Arc as StdArc, Mutex};
        let calls: StdArc<Mutex<Vec<serde_json::Value>>> = StdArc::new(Mutex::new(Vec::new()));
        let app = {
            let calls = calls.clone();
            axum::Router::new().route(
                "/agent/send",
                axum::routing::post(
                    move |axum::Json(b): axum::Json<serde_json::Value>| {
                        let calls = calls.clone();
                        async move {
                            calls.lock().unwrap().push(b);
                            axum::http::StatusCode::OK
                        }
                    },
                ),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = tokio::spawn(async move { axum::serve(listener, app).await });

        let client = reqwest::Client::new();
        let (control_tx, control_rx) = tokio::sync::mpsc::channel::<String>(64);
        let (output_tx, output_rx) = tokio::sync::mpsc::channel::<(String, Vec<u8>)>(64);
        let url = format!("http://127.0.0.1:{}/agent/send", port);
        let task = tokio::spawn(sender_loop(
            client,
            url,
            "sess1".to_string(),
            control_rx,
            output_rx,
            std::time::Duration::from_millis(50),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        drop(control_tx);
        drop(output_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), task).await;

        let calls = calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|b| b["type"] == "ping" && b["session_id"] == "sess1"),
            "sender_loop must send a periodic ping heartbeat, got {:?}",
            *calls
        );
    }
}
