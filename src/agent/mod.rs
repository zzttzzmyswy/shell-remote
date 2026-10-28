pub mod client;
pub mod cmd_log;
pub mod device;
                                #[cfg(feature = "desktop")]
pub mod desktop;
// MYS-2425 阶段2：制品下载器（CDN 优先 / 断点续传 / 有界预算）与 DoH 解析链路。
// 两者与桌面无关，lean 构建同样编译。
pub mod doh;
pub mod download;
pub mod encoding;
pub mod exec_sessions;
pub mod fs;
                                #[cfg(feature = "desktop")]
pub mod lan;
                                #[cfg(feature = "desktop")]
pub mod p2p;
pub mod shell;
pub mod tcp_tunnel;
// MYS-2425 阶段2 review：本地 TLS mock（测试 CA + 指定 SAN 证书），
// `doh` 与 `upgrade` 的测试共用，避免各自维护一份。
#[cfg(test)]
mod test_tls;
pub mod upgrade;
// MYS-2425 阶段3：隐藏换机助手（`update-apply`）、一次性身份交接与有界回滚。
// 与桌面无关，lean 构建同样编译。
pub mod update_apply;
pub mod web_proxy;

/// Unix 登录环境策略（网页终端与 MCP exec 共用，MYS-2421）。
#[cfg(unix)]
pub mod login_env;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::agent::client::RelayClient;
use crate::agent::shell::Shell;
use crate::proto::{McpResultPayload, Message};
#[cfg(feature = "tui")]
use crate::status::AgentStatus;

/// Returns the user's home directory, preferring `$HOME` (unix) and falling
/// back to `%USERPROFILE%` (Windows). Used for the file-manager root default
/// and the PTY child's cwd so the same code path works on both platforms.
pub fn home_dir() -> String {
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
    /// 按键 → 命令行重建器（命令日志用；`terminal:input` 只带原始按键）。
    line: cmd_log::LineTracker,
}

/// Per-tab replay buffer size (sent to a browser on join / tab switch).
const TAB_REPLAY_BYTES: usize = 65536;

/// Terminal tabs (PTYs) owned by [`start`] rather than by one relay session,
/// so they survive reconnects: a network blip re-registers the agent but the
/// user's shells and running commands keep going, and the browser's re-join
/// replays each tab's buffered output. Shells are only killed when the agent
/// process exits (or a tab is closed / its shell exits).
struct Terminals {
    tabs: HashMap<String, TabState>,
    active_tab_id: String,
    tab_counter: u32,
    shell_tx: tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
    shell_rx: tokio::sync::mpsc::UnboundedReceiver<(String, Vec<u8>)>,
}

/// Spawn a new tab and make it active. Free function over the `Terminals`
/// fields so `run_session` can call it on its destructured borrows.
fn spawn_tab(
    tabs: &mut HashMap<String, TabState>,
    active_tab_id: &mut String,
    tab_counter: &mut u32,
    shell_tx: &tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
    shell_path: &str,
) -> anyhow::Result<String> {
    *tab_counter += 1;
    let id = uuid::Uuid::new_v4().to_string();
    let shell = Shell::spawn(80, 24, shell_path, &id, shell_tx.clone())?;
    tabs.insert(
        id.clone(),
        TabState {
            shell,
            title: format!("Shell {}", tab_counter),
            output_buf: Vec::new(),
            line: cmd_log::LineTracker::default(),
        },
    );
    *active_tab_id = id.clone();
    Ok(id)
}

/// Drop a tab whose shell exited. Keeps at least one live tab (respawns when
/// the last one exits) and repoints the active tab. Returns whether the tab
/// set changed.
fn reap_exited_tab(
    tabs: &mut HashMap<String, TabState>,
    active_tab_id: &mut String,
    tab_counter: &mut u32,
    shell_tx: &tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
    tab_id: &str,
    shell_path: &str,
) -> bool {
    if tabs.remove(tab_id).is_none() {
        return false; // already closed by the user
    }
    tracing::info!(tab = %tab_id, "shell exited, tab closed");
    if tabs.is_empty() {
        if let Err(e) = spawn_tab(tabs, active_tab_id, tab_counter, shell_tx, shell_path) {
            tracing::error!("Failed to respawn shell after exit: {}", e);
            active_tab_id.clear(); // next session start respawns (tabs empty)
        }
    } else if active_tab_id == tab_id {
        *active_tab_id = tabs.keys().next().cloned().unwrap_or_default();
    }
    true
}

impl Terminals {
    fn new() -> Self {
        let (shell_tx, shell_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            tabs: HashMap::new(),
            active_tab_id: String::new(),
            tab_counter: 0,
            shell_tx,
            shell_rx,
        }
    }

    fn spawn_tab(&mut self, shell_path: &str) -> anyhow::Result<String> {
        spawn_tab(
            &mut self.tabs,
            &mut self.active_tab_id,
            &mut self.tab_counter,
            &self.shell_tx,
            shell_path,
        )
    }

    /// Append PTY output to the tab's bounded replay buffer. Returns false if
    /// the tab no longer exists.
    fn buffer_output(&mut self, tab_id: &str, data: &[u8]) -> bool {
        let Some(ts) = self.tabs.get_mut(tab_id) else {
            return false;
        };
        ts.output_buf.extend_from_slice(data);
        if ts.output_buf.len() > TAB_REPLAY_BYTES {
            let excess = ts.output_buf.len() - TAB_REPLAY_BYTES;
            ts.output_buf.drain(..excess);
        }
        true
    }

    fn reap_exited(&mut self, tab_id: &str, shell_path: &str) -> bool {
        reap_exited_tab(
            &mut self.tabs,
            &mut self.active_tab_id,
            &mut self.tab_counter,
            &self.shell_tx,
            tab_id,
            shell_path,
        )
    }

    /// Await `fut` while keeping PTY output flowing into the replay buffers,
    /// so a long reconnect gap neither blocks the shells nor queues unbounded
    /// output in the channel.
    async fn absorb_while<F: std::future::Future>(&mut self, shell_path: &str, fut: F) -> F::Output {
        tokio::pin!(fut);
        loop {
            tokio::select! {
                r = &mut fut => return r,
                Some((tab_id, data)) = self.shell_rx.recv() => {
                    if data.is_empty() {
                        self.reap_exited(&tab_id, shell_path);
                    } else {
                        self.buffer_output(&tab_id, &data);
                    }
                }
            }
        }
    }
}

/// Terminal size from an untrusted payload field: clamp into `1..=1000` before
/// narrowing to u16. A raw `as u16` cast wraps (65536 → 0), and a 0-column
/// terminal cannot render anything.
fn clamp_term_size(value: Option<u64>, default: u16) -> u16 {
    value.unwrap_or(default as u64).clamp(1, 1000) as u16
}

/// In-flight chunked-upload reassembly state. Holds the open destination file
/// across chunk messages so each chunk's decoded bytes append in order; the
/// last chunk flushes/closes and emits the fs:result reply.
struct UploadReassembly {
    file: std::fs::File,
    final_path: String,
    last_activity: std::time::Instant,
    /// Index the next chunk must carry. Chunks below it are duplicates (the
    /// relay may re-deliver after a timeout) and are ignored; chunks above it
    /// mean a gap — the upload is aborted rather than producing a file with a
    /// silent hole.
    next_index: u32,
}

/// Outbound message handle. The main loop never blocks on HTTP: terminal
/// output is pushed to a bounded, coalesced channel; control/result messages
/// are pushed to a bounded channel drained with priority by a background task.
#[derive(Clone)]
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
/// Consecutive failed heartbeat POSTs (15s apart, each bounded by
/// `POST_TIMEOUT`) after which the agent treats its uplink as dead.
const UPLINK_DEAD_AFTER_HEARTBEAT_FAILURES: u32 = 3;

async fn sender_loop(
    client: reqwest::Client,
    send_url: String,
    session_id: String,
    // 上行身份令牌（`X-SR-Agent-Token`），空串 = 不携带（旧 relay / 无 token）。
    agent_token: String,
    mut control_rx: tokio::sync::mpsc::Receiver<String>,
    mut output_rx: tokio::sync::mpsc::Receiver<(String, Vec<u8>)>,
    heartbeat: Duration,
    uplink_dead: Arc<tokio::sync::Notify>,
    #[cfg(feature = "desktop")]
    desktop: Option<std::sync::Arc<crate::agent::desktop::DesktopManager>>,
    #[cfg(feature = "tui")]
    status: Option<AgentStatus>,
) {
    let mut pending: HashMap<String, Vec<u8>> = HashMap::new();
    let mut timer = tokio::time::interval(Duration::from_millis(16));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_tick = tokio::time::interval(heartbeat);
    heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat_failures: u32 = 0;
    loop {
        tokio::select! {
            biased;
            ctrl = control_rx.recv() => match ctrl {
                Some(s) => {
                    flush_output(&client, &send_url, &agent_token, &session_id, &mut pending).await;
                    post_raw(&client, &send_url, &agent_token, &s).await;
                }
                None => break,
            },
            out = output_rx.recv() => match out {
                Some((tab, data)) => pending.entry(tab).or_default().extend(data),
                None => break,
            },
            _ = timer.tick() => {
                flush_output(&client, &send_url, &agent_token, &session_id, &mut pending).await;
            }
            _ = heartbeat_tick.tick() => {
                // 心跳扩展（R5#150）：桌面启用时附带运行态 KPI，relay/admin
                // 可观测不依赖浏览器面板。字段少、低频（15s），开销可忽略。
                #[cfg(feature = "desktop")]
                let mut ping = serde_json::json!({"type": "ping", "session_id": &session_id});
                #[cfg(not(feature = "desktop"))]
                let ping = serde_json::json!({"type": "ping", "session_id": &session_id});
                #[cfg(feature = "desktop")]
                if let Some(dm) = &desktop {
                    let k = dm.kpi_snapshot();
                    ping["kpi"] = serde_json::json!({
                        "running": k.running,
                        "codec": k.codec,
                        "fps": k.fps,
                        "quality_permille": k.quality_permille,
                        "bitrate_kbps": k.bitrate_kbps,
                        "encode_ms": k.encode_ms,
                        "active": k.active,
                        "bp_count": k.bp_count,
                        "rss_kb": self_rss_kb(),
                        "cpu_ms": self_cpu_ms(),
                        "cpu_temp": self_cpu_temp(),
                    });
                }
                let ping = ping.to_string();
                #[cfg(feature = "tui")]
                let ping_sent_at = std::time::Instant::now();
                if post_raw(&client, &send_url, &agent_token, &ping).await {
                    heartbeat_failures = 0;
                } else {
                    heartbeat_failures += 1;
                    // Uplink (agent→relay) dead while the downlink SSE may
                    // still look alive (half-open path, relay lost the
                    // session): end the session so it re-registers.
                    if heartbeat_failures == UPLINK_DEAD_AFTER_HEARTBEAT_FAILURES {
                        tracing::warn!(
                            "{} consecutive heartbeat POSTs failed — uplink considered dead, reconnecting",
                            heartbeat_failures
                        );
                        uplink_dead.notify_one();
                    }
                }
                // 心跳往返即 relay RTT：写入共享状态供 TUI 展示延迟。
                #[cfg(feature = "tui")]
                if let Some(st) = &status {
                    st.set_latency_ms(ping_sent_at.elapsed().as_millis() as u64);
                }
            }
        }
    }
    flush_output(&client, &send_url, &agent_token, &session_id, &mut pending).await;
}

/// 当前进程 RSS（KB）。读 `/proc/self/statm` 第 2 字段（resident pages）
/// × 4KB 页。非 Linux 回退 0（心跳 rss_kb 缺省）。用于 admin KPI 的
/// "agent 内存画像"时间线（R5#136-146 内存画像最小子集）。
                                #[cfg(feature = "desktop")]
fn self_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        .map(|pages| pages * 4)
        .unwrap_or(0)
}

/// 从 `/proc/self/stat` 文本解析累计 CPU 毫秒（utime+stime，字段 14/15；
/// comm 可含空格/括号 → `rsplit(") ")` 取后半，utime/stime 是后半字段
/// 11/12）。`hz` = 时钟 tick/秒（`sysconf(_SC_CLK_TCK)`）。纯函数便于单测。
                                #[cfg(feature = "desktop")]
fn cpu_ms_from_stat(stat: &str, hz: u64) -> u64 {
    let Some(tail) = stat.rsplit(") ").next() else {
        return 0;
    };
    let f: Vec<&str> = tail.split_whitespace().collect();
    if f.len() < 13 || hz == 0 {
        return 0;
    }
    let utime: u64 = f[11].parse().unwrap_or(0);
    let stime: u64 = f[12].parse().unwrap_or(0);
    (utime + stime) * 1000 / hz
}

/// 当前进程累计 CPU 时间（ms，utime+stime）。非 Linux 回退 0。两次心跳
/// 采样差 = 区间 CPU 使用率——admin KPI 可见 agent 功耗画像
/// （R5#136-146 功耗采样最小子集）。
                                #[cfg(feature = "desktop")]
fn self_cpu_ms() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        std::fs::read_to_string("/proc/self/stat")
            .ok()
            .map(|s| cpu_ms_from_stat(&s, hz))
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Windows/macOS 无 /proc/self/stat 与 sysconf，功耗画像回退 0。
        0
    }
}

/// 当前 CPU/主板温度（℃，hwmon 传感器毫度 /1000；找不到回退 0.0）。
/// 功耗硬采样最小子集（R5#136-146）：扫描 `/sys/class/hwmon/*/temp*_input`
/// 取第一个非零温度（coretemp/dell_smm/nvme 均可）。无传感器环境回退 0。
                                #[cfg(feature = "desktop")]
fn self_cpu_temp() -> f64 {
    let Ok(dir) = std::fs::read_dir("/sys/class/hwmon") else {
        return 0.0;
    };
    for entry in dir.flatten() {
        let Ok(rd) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for f in rd.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            if !(name.starts_with("temp") && name.ends_with("_input")) {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(f.path()) {
                if let Ok(milli) = raw.trim().parse::<i64>() {
                    if milli > 0 {
                        return milli as f64 / 1000.0;
                    }
                }
            }
        }
    }
    0.0
}

async fn flush_output(
    client: &reqwest::Client,
    send_url: &str,
    agent_token: &str,
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
            post_raw(client, send_url, agent_token, &s).await;
        }
    }
}

/// POST one message to the relay. Returns whether the relay accepted it
/// (2xx); failures are logged. Bounded by [`client::POST_TIMEOUT`].
/// `agent_token` 非空时附带 `X-SR-Agent-Token`（relay 据此确认这条上行属于
/// 该会话；旧 relay 忽略该头）。
async fn post_raw(
    client: &reqwest::Client,
    send_url: &str,
    agent_token: &str,
    text: &str,
) -> bool {
    let body = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to parse outgoing message: {}", e);
            return false;
        }
    };
    match client::with_agent_token(client.post(send_url), agent_token)
        .timeout(client::POST_TIMEOUT)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if !resp.status().is_success() => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "Agent POST failed ({}): {}",
                status,
                client::snippet(&body, 200)
            );
            false
        }
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("Agent POST send error: {}", e);
            false
        }
    }
}

/// 同 [`post_raw`]，但入参已是**序列化好的 JSON 文本**，直接作为请求体发出。
///
/// 数据面（`tcp_tunnel` 的批量上行）用这个：批次可能有 1 MiB，`post_raw` 会先
/// `from_str` 成 `Value` 再 `json()` 序列化一遍，等于白白全量解析 + 重编码两次。
/// 解析失败的行在这里不再单独校验 —— 调用方拼的数组来自自己的 `serde_json`
/// 输出，本就合法。
async fn post_json_text(
    client: &reqwest::Client,
    send_url: &str,
    agent_token: &str,
    text: String,
) -> bool {
    match client::with_agent_token(client.post(send_url), agent_token)
        .timeout(client::POST_TIMEOUT)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(text)
        .send()
        .await
    {
        Ok(resp) if !resp.status().is_success() => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "Agent POST failed ({}): {}",
                status,
                client::snippet(&body, 200)
            );
            false
        }
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("Agent POST send error: {}", e);
            false
        }
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

/// 构造 WS 升级请求，附带 `X-SR-Agent-Token`（relay 侧 WS 上行路径据此
/// 判定身份）。`token` 为空时与直接传 URL 等价。
fn ws_request(url: &str, token: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url
        .into_client_request()
        .expect("ws_url is a well-formed ws:// URL built above");
    if !token.is_empty() {
        if let Ok(v) = token.parse() {
            req.headers_mut().insert(
                crate::agent::client::AGENT_TOKEN_HEADER,
                v,
            );
        }
    }
    req
}

/// Post a single error `fs:result` (correlated by `_mcp_request_id`) for a
/// failed download; `kind` categorizes the failure for HTTP status mapping.
async fn post_fs_err(
    client: &reqwest::Client,
    send_url: &str,
    agent_token: &str,
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
    post_raw(client, send_url, agent_token, &msg).await;
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
    agent_token: String,
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
                &agent_token,
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
                &agent_token,
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
            &agent_token,
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
                &agent_token,
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
                &agent_token,
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
                        &agent_token,
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
        // A lost chunk must not leave a silent hole in the file: the relay
        // streams chunks in arrival order without de-duplication, so we do
        // not retry (a timed-out-but-delivered chunk would be duplicated) —
        // abort instead and let the relay fail the download visibly.
        let delivered = matches!(
            client::with_agent_token(client.post(&send_url), &agent_token)
                .timeout(client::POST_TIMEOUT)
                .json(&msg)
                .send()
                .await,
            Ok(ref r) if r.status().is_success()
        );
        if !delivered {
            tracing::warn!("download chunk {} of {} not delivered — aborting {}", idx, total_chunks, path);
            break;
        }
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
    // nofollow：上传目标若是符号链接，删掉的必须是链接本身。
    if let Some(p) = crate::agent::fs::resolve_path_nofollow(root, final_path) {
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
///
/// Ordering (`UploadReassembly::next_index`): a chunk whose index is below the
/// expected one is a duplicate and is ignored (success); an index above it
/// means chunks were lost, so the upload is aborted and the partial file
/// removed rather than leaving a hole in the destination.
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
    let ok_path = |p: &str| crate::proto::FsResultPayload {
        kind: None,
        success: true,
        error: None,
        entries: None,
        content: None,
        path: Some(p.to_string()),
        new_path: None,
    };

    /// Drop the in-flight upload and remove the partially written file.
    fn abort_upload(
        reassembly: &mut HashMap<String, UploadReassembly>,
        upload_id: &str,
        final_path: &str,
        root: &Path,
    ) {
        reassembly.remove(upload_id);
        if let Some(p) = crate::agent::fs::resolve_path(root, final_path) {
            let _ = std::fs::remove_file(&p);
        }
    }

    if chunk_index == 0 {
        let decoded = match decoded_opt {
            None => return (err_no_kind("Invalid base64 content"), false),
            Some(d) => d,
        };
        let p = match crate::agent::fs::resolve_path(root, final_path) {
            None => return (err("invalid_path", "Invalid destination path"), false),
            Some(p) => p,
        };
        let mut f = match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&p)
        {
            Err(e) => {
                return (
                    err(kind_for_io(&e), &format!("Failed to open destination: {}", e)),
                    false,
                )
            }
            Ok(f) => f,
        };
        // An empty upload (total_chunks == 1, empty content) still has to
        // produce the file — created above by `truncate(true)`, zero bytes.
        if let Err(e) = f.write_all(&decoded) {
            drop(f);
            let _ = std::fs::remove_file(&p);
            return (
                err_no_kind(&format!("Failed to write uploaded chunk: {}", e)),
                false,
            );
        }
        if is_last {
            let _ = f.sync_all();
            return (ok(), false);
        }
        reassembly.insert(
            upload_id.to_string(),
            UploadReassembly {
                file: f,
                final_path: final_path.to_string(),
                last_activity: std::time::Instant::now(),
                next_index: 1,
            },
        );
        (ok(), true)
    } else {
        let decoded = match decoded_opt {
            None => return (err_no_kind("Invalid base64 content"), false),
            Some(d) => d,
        };
        let mut st = match reassembly.remove(upload_id) {
            None => {
                return (
                    err_no_kind("Upload chunk received without a preceding chunk 0"),
                    false,
                )
            }
            Some(st) => st,
        };
        let fp = st.final_path.clone();

        // Out-of-order: a gap means we can never rebuild a correct file.
        if chunk_index > st.next_index {
            drop(st.file);
            abort_upload(reassembly, upload_id, &fp, root);
            return (
                err_no_kind(&format!(
                    "Upload chunk {} arrived out of order (expected {}); upload aborted",
                    chunk_index, st.next_index
                )),
                false,
            );
        }
        // Duplicate: the relay re-sent a chunk we already appended. Ignore its
        // payload, keep the upload alive, and report progress as before.
        if chunk_index < st.next_index {
            st.last_activity = std::time::Instant::now();
            let more = !is_last;
            if more {
                reassembly.insert(upload_id.to_string(), st);
            } else {
                let _ = st.file.sync_all();
            }
            return (ok_path(&fp), more);
        }

        if let Err(e) = st.file.write_all(&decoded) {
            drop(st.file);
            abort_upload(reassembly, upload_id, &fp, root);
            return (
                err_no_kind(&format!("Failed to write uploaded chunk: {}", e)),
                false,
            );
        }
        st.next_index = chunk_index + 1;
        if is_last {
            let _ = st.file.sync_all();
            (ok_path(&fp), false)
        } else {
            st.last_activity = std::time::Instant::now();
            reassembly.insert(upload_id.to_string(), st);
            (ok_path(&fp), true)
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
    #[cfg(feature = "desktop")]
    desktop_cfg: crate::agent::desktop::DesktopConfig,
    #[cfg(feature = "tui")]
    status: Option<AgentStatus>,
    insecure_tls: bool,
) -> anyhow::Result<()> {
    start_with_restore(
        relay_url,
        key,
        root,
        token_type,
        shell_path,
        session_id,
        #[cfg(feature = "desktop")]
        desktop_cfg,
        #[cfg(feature = "tui")]
        status,
        insecure_tls,
        None,
    )
    .await
}

/// 带「一次性身份恢复」的启动入口（MYS-2425 阶段3）。普通启动传 `None`
/// 即完全走原路径——正常启动不落任何长期身份文件。
///
/// 恢复只在**进程内**生效：把交接来的 tokens + 实际 session_id 作为首次注册
/// 的初始 `cached_tokens`，之后的重连逻辑与平时完全一致。身份一旦进了内存，
/// 交接文件立即删除。
#[allow(clippy::too_many_arguments)]
pub async fn start_with_restore(
    relay_url: String,
    key: Option<String>,
    root: String,
    token_type: String,
    shell_path: String,
    session_id: Option<String>,
    #[cfg(feature = "desktop")]
    desktop_cfg: crate::agent::desktop::DesktopConfig,
    #[cfg(feature = "tui")]
    status: Option<AgentStatus>,
    insecure_tls: bool,
    restored: Option<crate::agent::update_apply::RestoredIdentity>,
) -> anyhow::Result<()> {
    let mut delay = Duration::from_secs(1);
    // Same 60s cap as registration retries (client.rs, R5#9): a 300s cap made
    // recovery after a flapping network take up to 5 minutes.
    let max_delay = Duration::from_secs(60);
    // A session that stayed up at least this long counts as healthy: the
    // next reconnect starts again from 1s instead of the grown backoff.
    const HEALTHY_SESSION: Duration = Duration::from_secs(60);
    let mut terminals = Terminals::new();
    // Tokens obtained on the first successful registration; replayed on every
    // reconnect so the relay reuses them instead of minting new random ones.
    //
    // 换机恢复：交接来的 (token, permission) 就是**第一次**注册的 cached
    // tokens。转成 `(String, String)` 与既有类型一致，后续重连/失效清理逻辑
    // 一行都不用改（401/409 仍会清缓存回退到 `--key` 全新注册）。
    let mut cached_tokens: Option<Vec<(String, String)>> = restored
        .as_ref()
        .filter(|r| r.restore.is_usable())
        .map(|r| r.restore.tokens.clone());
    // 会话 id：优先用交接来的**实际** id（临时 agent 没有 --session-id，
    // 只有这个能保住原会话）；其次才是 CLI 指定的。
    #[cfg(feature = "tui")]
    let mut effective_session_id = restored
        .as_ref()
        .filter(|r| r.restore.is_usable())
        .map(|r| r.restore.session_id.clone())
        .or(session_id);
    #[cfg(not(feature = "tui"))]
    let effective_session_id = restored
        .as_ref()
        .filter(|r| r.restore.is_usable())
        .map(|r| r.restore.session_id.clone())
        .or(session_id);
    // 恢复状态只在**首次**会话生效：注册成功即消费，之后的正常重连不再
    // 依赖它。
    let mut restore_pending = restored;
    if let Some(r) = restore_pending.as_ref() {
        if r.restore.is_usable() {
            tracing::info!(
                session = %r.restore.session_id,
                upgrade_id = %r.restore.upgrade_id,
                tokens = r.restore.tokens.len(),
                rollback = r.restore.rollback.is_some(),
                "recovering the previous identity after a self-upgrade"
            );
        } else {
            tracing::warn!(
                upgrade_id = %r.restore.upgrade_id,
                "the upgrade handoff carried no usable identity; registering as a new device"
            );
            restore_pending = None;
        }
    }
    // R5#11 会话级桌面状态：relay 重启/断线后自动恢复桌面流（跨 run_session
    // 重连传递"上次桌面是否在跑"）。
    #[cfg(feature = "desktop")]
    let mut desktop_want_running = false;

    loop {
        #[cfg(feature = "tui")]
        if let Some(st) = &status {
            st.set_phase(crate::status::Phase::Connecting);
        }
        let session_started = std::time::Instant::now();
        match run_session(
            &relay_url,
            &key,
            &root,
            &token_type,
            &shell_path,
            effective_session_id.as_deref(),
            &mut cached_tokens,
            #[cfg(feature = "desktop")]
            &desktop_cfg,
            #[cfg(feature = "tui")]
            status.clone(),
            insecure_tls,
            #[cfg(feature = "desktop")]
            &mut desktop_want_running,
            &mut terminals,
            // 交接文件的生命周期只到"首次注册成功"为止：注册后由
            // `RestoredIdentity::complete` 删除并回执助手。传引用是因为
            // 注册失败要继续在线重试（文件必须还在），成功后才消费。
            restore_pending.as_ref(),
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
        // 首次注册已回报（成功或已判定不可用）→ 后续重连不再带恢复语义。
        if let Some(r) = restore_pending.as_ref() {
            if r.consumed() {
                restore_pending = None;
            }
        }

        // 手动刷新 token：清掉缓存 token → 下次注册走全新注册路径 →
        // relay 为同一 session id 铸造新 token（旧会话被顶替）。延迟直接
        // 用 1s 快速重连，session id 取最近一次实际 id 保持不变。
        #[cfg(feature = "tui")]
        if let Some(st) = &status {
            if st.take_refresh() {
                tracing::info!("token refresh: re-registering with same session id (new tokens)");
                cached_tokens = None;
                // Refresh revokes the old tokens: do not hand the running
                // shells and their replay history to the new token holders.
                terminals.tabs.clear();
                let cur = st.session_id();
                if !cur.is_empty() {
                    effective_session_id = Some(cur);
                }
                delay = Duration::from_secs(1);
            }
        }
        // 手动停机（TUI q）：结束会话后正常退出进程。
        #[cfg(feature = "tui")]
        if let Some(st) = &status {
            if st.take_shutdown() {
                tracing::info!("shutdown requested by UI, exiting");
                return Ok(());
            }
        }

        #[cfg(feature = "tui")]
        if let Some(st) = &status {
            st.set_phase(crate::status::Phase::Reconnecting);
        }
        if session_started.elapsed() >= HEALTHY_SESSION {
            delay = Duration::from_secs(1);
        }
        let sleep_for = client::jittered(delay);
        terminals
            .absorb_while(&shell_path, tokio::time::sleep(sleep_for))
            .await;
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
    #[cfg(feature = "desktop")]
    desktop_cfg: &crate::agent::desktop::DesktopConfig,
    #[cfg(feature = "tui")]
    status: Option<AgentStatus>,
    insecure_tls: bool,
    // R5#11 会话级桌面状态：进入时若为 true（上次断线前桌面在跑）→ 自动
    // 恢复桌面流；退出时回写当前状态并停掉旧流（防孤儿 task 继续向已失效
    // 的 relay 连接发帧）。
    #[cfg(feature = "desktop")]
    desktop_want_running: &mut bool,
    terminals: &mut Terminals,
    // 换机后的一次性身份恢复（`None` = 普通启动）。注册成功即消费。
    restored: Option<&crate::agent::update_apply::RestoredIdentity>,
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

    // Task 4（阶段2 LAN 直连基础）：agent 本地 HTTP server 暴露与 relay 同构的
    // /agent/desktop/stream，同局域网浏览器直连 agent 拉桌面流、绕开中转。
    // 仅显式 --desktop-lan-port N（lan_port != 0）开启；默认 0 不开任何端口。
    // 会话结束 drop 时 LanDesktop 中止 server/feed 任务，端口释放供重连重新 bind。
    // 必须在 register 与 DesktopManager::new 之前：绑定端口/出口 IP 只有 spawn
    // 后才知道，注册消息的 lan_addr 与 desktop:capabilities.lan_addrs 都要它。
    #[cfg(feature = "desktop")]
    let lan_desktop = std::sync::Arc::new(if desktop_cfg.lan_port != 0 {
        // Task 6 CORS 收窄 + final-review #4 fail-closed：LAN 无认证端点只放行
        // relay 同源页面读流。relay_url 解析失败 → LanDesktop::spawn 拒绝启动
        //（不 serve 通配），这里走到 Err 分支记日志、不开服务。
        let relay_origin = crate::agent::lan::relay_origin(relay_url);
        match crate::agent::lan::LanDesktop::spawn(desktop_cfg.lan_port, relay_origin).await {
            Ok(lan) => {
                tracing::info!(
                    addr = %lan.addr_report(),
                    cors_origin = %lan.cors_origin(),
                    "LAN desktop stream server up (--desktop-lan-port)"
                );
                Some(lan)
            }
            Err(e) => {
                tracing::warn!(
                    lan_port = desktop_cfg.lan_port,
                    "LAN desktop server bind failed: {e}"
                );
                None
            }
        }
    } else {
        None
    });
    // 上报地址（"ip:port"，None = 未开启）：注册消息随 `lan_addr` 上报 relay，
    // 同时注入 DesktopManager 的 config（capabilities_json 的 lan_addrs）。
    #[cfg(feature = "desktop")]
    let lan_addr_report = lan_desktop.as_ref().as_ref().map(|lan| lan.addr_report());

    let connect = RelayClient::connect_with_retry(
        relay_url,
        key.clone(),
        token_type,
        session_id,
        cached_tokens,
        10,
        insecure_tls,
        #[cfg(feature = "desktop")]
        lan_addr_report.clone(),
        #[cfg(not(feature = "desktop"))]
        None,
    );
    let mut client = terminals.absorb_while(shell_path, connect).await?;

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

    // 换机恢复的回执：**注册成功之后**才做（"进程起来了"不算数）。
    //
    // 两件事：删交接文件（身份已进内存，文件留着只是风险）+ 向助手写
    // `registered` 标记。助手没收到标记就会回滚——所以这一步必须紧跟在注册
    // 成功之后，排在任何可能失败的后续初始化（PTY、桌面、代理）之前。
    if let Some(r) = restored {
        // 只有**身份真的回到原样**才允许上报 `registered`。
        //
        // 之前这里是"id 不一致只 warn，然后照样 complete()"：relay 失忆或
        // 拒绝这份身份时，临时设备已经拿到全新的 session_id/Token，助手却
        // 收到成功回执、删掉备份——把"设备变成了另一台"记成升级成功。
        //
        // 现在按 [`restore_verdict`] 判定：
        // - 完整一致 → 成功；
        // - 固定 key 设备保住 sid、Token 被 relay 重签 → 允许成功但记 warn
        //   （Token 由 key 派生，身份仍是同一台；旧 Token 的浏览器会掉线）；
        // - 其余（尤其纯临时设备换了 sid 或 Token）→ 不消费交接文件、
        //   不上报 registered，直接结束会话让助手按超时/提前退出回滚。
        let mut accepted = false;
        match crate::agent::update_apply::restore_verdict(
            r.restore.fixed_key.as_deref(),
            &r.restore.session_id,
            &r.restore.tokens,
            &client.session_id,
            &client.tokens,
        ) {
            Ok(crate::agent::update_apply::RollbackIdentity::Preserved) => {
                accepted = true;
            }
            Ok(verdict) => {
                tracing::warn!(
                    expected = %r.restore.session_id,
                    actual = %client.session_id,
                    verdict = ?verdict,
                    "the original session id was preserved but the relay re-issued its tokens; \
                     browsers holding the old tokens are logged out"
                );
                accepted = true;
            }
            Err(e) => {
                tracing::error!(
                    expected = %r.restore.session_id,
                    actual = %client.session_id,
                    error = %e,
                    "the relay did not accept the handed-over identity; refusing to report a successful upgrade"
                );
                // 不消费交接文件、不写 registered 标记：助手要么等超时，
                // 要么看到本进程退出，两条路都会走有界回滚。
                anyhow::bail!("upgrade identity not restored: {e}");
            }
        }
        if accepted {
            r.complete();
        }
    }

    // 把实际 session id 与 token 写入共享状态（TUI 展示；token 刷新后在此更新）。
    #[cfg(feature = "tui")]
    if let Some(st) = &status {
        st.set_connected(&client.session_id, client.tokens.clone());
    }

    // Outbound channel + background sender. The main loop must never block on
    // HTTP — otherwise high-volume terminal output starves input/command
    // processing (and MCP round-trips time out as "i/o error").
    let (control_tx, control_rx) = tokio::sync::mpsc::channel::<String>(64);
    let (output_tx, output_rx) = tokio::sync::mpsc::channel::<(String, Vec<u8>)>(64);
    // 回滚后的重启：把失败原因送回 relay。否则管理界面只会看到一个
    // "升级过、版本却没变"的状态，运维无从判断是没升成还是又退了回来。
    if let Some(r) = restored {
        // 成功 → `registered` 终结帧；回滚重启 → `rolled_back` + 原因。
        let frame = r
            .registered_frame(&client.session_id)
            .or_else(|| r.progress_frame(&client.session_id));
        if let Some(frame) = frame {
            let _ = control_tx.send(frame).await;
        }
    }
    let out = Out {
        control_tx: control_tx.clone(),
        output_tx,
    };
    // Desktop video sharing: control + frame messages are posted through a
    // single-task FIFO so the byte stream reaches the relay in order
    // (concurrent sends could reorder fragments and break playback).
    // 先建 DesktopManager，心跳扩展（R5#150）需要它的 KPI 快照。
    // lan_addr 已由上面的 LanDesktop::spawn 得出（None = 未开启阶段2）。
    #[cfg(feature = "desktop")]
    let mut dm_cfg = desktop_cfg.clone();
    #[cfg(feature = "desktop")]
    {
        dm_cfg.lan_addr = lan_addr_report.clone();
    }
    #[cfg(feature = "desktop")]
    let desktop = std::sync::Arc::new(crate::agent::desktop::DesktopManager::new(dm_cfg));
    // Task 2 P2P 信令会话槽：None = 无活动协商；offer 到达时宿主 peer +
    // 驱动任务，desktop:stop / 二次 offer / 会话结束 时回收。
    #[cfg(feature = "desktop")]
    let p2p_state = crate::agent::p2p::P2pState::default();
    let uplink_dead = Arc::new(tokio::sync::Notify::new());
    let sender_task = tokio::spawn(sender_loop(
        client.http_client().clone(),
        client.send_url().to_string(),
        client.session_id.clone(),
        client.agent_token(),
        control_rx,
        output_rx,
        Duration::from_secs(15),
        uplink_dead.clone(),
        #[cfg(feature = "desktop")]
        Some(desktop.clone()),
        #[cfg(feature = "tui")]
        status.clone(),
    ));
    // Keep a control sender for spawned long-running tasks (e.g. mcp:exec).
    let task_control_tx = control_tx;

    // 网页代理（MYS-2383 阶段1）：回包与 mcp/fs 结果共用这条 control 上行，
    // 抓取任务各自独立，主循环只做校验与派发（慢页面不阻塞终端/MCP）。
    let web_proxy = web_proxy::WebProxy::new(client.session_id.clone(), task_control_tx.clone());

    // TCP 字节管道（MYS-2383 阶段2）：原始字节量大且连续，**不能**挤 control
    // 通道（那条是单条逐个 POST，会把终端输出与 MCP 回包堵在后面）。这里起一条
    // 专用上行 task：有界队列 → 批量 JSON 数组 → 一次 POST。会话结束时 abort。
    let (tcp_up_tx, tcp_up_rx) = tokio::sync::mpsc::channel::<String>(tcp_tunnel::UPLINK_QUEUE);
    let tcp_tunnel = tcp_tunnel::TcpTunnel::new(client.session_id.clone(), tcp_up_tx);
    let tcp_uplink_task = tokio::spawn(tcp_tunnel::uplink_loop(
        tcp_tunnel::HttpPoster::new(
            client.http_client().clone(),
            client.send_url().to_string(),
            client.agent_token(),
        ),
        tcp_up_rx,
        tcp_tunnel.clone(),
    ));

    // 时钟校准：采样 relay /api/clock 求 (relay_epoch - 本地_epoch) 偏移，
    // 注入 DesktopManager，srtc 打点落在 relay 时基 —— e2e 延时从此不再
    // 依赖 agent/浏览器两机系统时钟同步（MYS-886 指标失真根因）。
    #[cfg(feature = "desktop")]
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
    // 桌面 video 上行链路（WS binary / HTTP 批量）+ P2P/LAN 镜像投递：
    // 全部 desktop-only，lean（无 desktop feature）构建不编译。
    #[cfg(feature = "desktop")]
    let (post_tx, mut post_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    #[cfg(feature = "desktop")]
    {
        let pc = client.http_client().clone();
        let base = client
            .send_url()
            .trim_end_matches("/agent/send")
            .to_string();
        let sid = client.session_id.clone();
        let server_auth = client.server_auth().to_string();
        // 上行身份令牌：HTTP 批量与 WS 两条路径都带（relay 侧 `Denied` 时
        // 直接 401，不会再退回「带错 token 等于没带」的旧行为）。
        let agent_token = client.agent_token();
        // R5 #41：relay 是否支持 desktop:video 二进制上行（register 响应
        // `desktop_binary: true`）。老 relay → false，WS 路径保持 JSON+base64。
        let desktop_binary = client.desktop_binary;
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
            };            let mut ws: Option<
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
                            ws_request(&ws_url, &agent_token),
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
                    // R5 #41 binary 上行（relay 支持时）：ctrl 消息合并一个
                    // text JSON 数组帧，desktop:video 逐帧发 binary 帧
                    // （[flags:u8][seq:u32 LE][len:u32 LE][fMP4 字节]，无
                    // base64 膨胀）；relay 侧 Message::Binary 分支直转。
                    // 老 relay（desktop_binary=false）走原 JSON 数组 text。
                    if desktop_binary {
                        let mut ws_ok = true;
                        let media_msgs: Vec<&serde_json::Value> =
                            out.iter().filter(|m| m["type"] == "desktop:video").collect();
                        tracing::debug!(out = %out.len(), media = %media_msgs.len(), "uplink WS binary dispatch");
                        let ctrl: Vec<serde_json::Value> = out
                            .iter()
                            .filter(|m| m["type"] != "desktop:video")
                            .cloned()
                            .collect();
                        if !ctrl.is_empty() {
                            let payload = serde_json::Value::Array(ctrl).to_string();
                            if stream.send(payload.into()).await.is_err() {
                                ws_ok = false;
                            }
                        }
                        if ws_ok {
                            for m in out.iter().filter(|m| m["type"] == "desktop:video") {
                                let kind = m["payload"]["kind"].as_str().unwrap_or("frag");
                                let seq = m["payload"]["seq"].as_u64().unwrap_or(0) as u32;
                                let is_key = m["payload"]["key"].as_bool().unwrap_or(false);
                                let data_b64 = m["payload"]["data"].as_str().unwrap_or("");
                                if let Some(bytes) = crate::agent::fs::decode_b64(data_b64) {
                                    let flags = if kind == "init" {
                                        crate::proto::BIN_FRAME_FLAG_INIT
                                    } else if is_key {
                                        crate::proto::BIN_FRAME_FLAG_KEY
                                    } else {
                                        0
                                    };
                                    let frame = crate::proto::encode_bin_frame(flags, seq, &bytes);
                                    match stream.send(frame.into()).await {
                                        Ok(()) => {
                                            tracing::debug!(kind = %kind, seq, bytes = %bytes.len(), "uplink WS binary frame sent");
                                        }
                                        Err(e) => {
                                            tracing::warn!("desktop WS binary send failed: {e}");
                                            ws_ok = false;
                                            break;
                                        }
                                    }
                                } else {
                                    tracing::warn!(kind = %kind, "uplink binary: base64 decode failed, dropped");
                                }
                            }
                        }
                        sent_via_ws = ws_ok;
                        if !ws_ok {
                            tracing::warn!("desktop WS binary send failed — falling back to HTTP");
                            ws = None;
                            ws_failures += 1;
                        }
                    } else {
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
                    client::with_agent_token(pc.post(format!("{base}/agent/send")), &agent_token)
                        .json(&batch)
                        .send(),
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
    // Task 3：P2P 已建连时把 desktop:video 的 fMP4 字节镜像一份进 DataChannel
    //（relay POST 行为/编码/QoS 零改动，仅加一条镜像投递）。闭包外 clone，
    // Arc 引用计数使闭包与消息循环共享同一个投递口。
    #[cfg(feature = "desktop")]
    let video_tx = p2p_state.video_tx.clone();
    #[cfg(feature = "desktop")]
    let last_init = p2p_state.last_init.clone();
    // LAN 投递口闭包专用 clone（Arc）：post_fn move 闭包持有它，外部消息
    // 循环保留原 Arc 供重建时 clear_init —— 避免整个 Option 被 move 进闭包。
    #[cfg(feature = "desktop")]
    let lan_desktop_pf = lan_desktop.clone();
    #[cfg(feature = "desktop")]
    let post_fn: crate::agent::desktop::PostFn = Arc::new(move |msg| {
        let t = msg["type"].as_str().unwrap_or("?").to_string();
        // fMP4 镜像投递：解析 desktop:video 的 base64 data（init=ftyp+moov /
        // frag=moof+mdat，不带 JSON 包裹），投给 P2P 消费循环写 DataChannel。
        // init 单独缓存一份（建连时补推，见 p2p 驱动任务 Connected 分支）。
        // 通道未建/未开时静默丢弃（丢旧保新，浏览器 reqkey/重连兜底）。p2p 侧
        // MirroredQueue::push 同步非阻塞（有界丢旧保新，满丢最旧；std RwLock
        // 读不跨 await，见 p2p.rs）。
        if t == "desktop:video" {
            let kind = msg["payload"]["kind"].as_str();
            let data_b64 = msg["payload"]["data"].as_str();
            if let Some(data_b64) = data_b64 {
                if let Some(bytes) = crate::agent::fs::decode_b64(data_b64) {
                    if kind == Some("init") {
                        *last_init.write().unwrap() = Some(bytes.clone());
                    }
                    // LAN 直连镜像（Task 4）：同源 fMP4 字节喂给本地 DesktopStream
                    // fan-out（init→set_init，frag→push_frag；关键帧标志取
                    // payload.key 或 flags=="key"，与 WS 二进制上行判定一致）。
                    if let Some(lan) = lan_desktop_pf.as_ref() {
                        let is_init = kind == Some("init");
                        let is_key = is_init
                            || msg["payload"]["key"].as_bool().unwrap_or(false)
                            || msg["payload"]["flags"].as_str() == Some("key");
                        lan.feed(is_init, is_key, bytes.clone());
                    }
                    if let Ok(guard) = video_tx.read() {
                        if let Some(q) = guard.as_ref() {
                            q.push(bytes);
                        }
                    }
                }
            }
        }
        let _ = post_tx.send(msg); // unbounded: 不会失败, 由 consumer 批内丢旧
        tracing::trace!("post_fn queued {t}");
    });

    // R5#11 会话恢复：上次断线前桌面在跑 → 重连后自动恢复桌面流（新
    // send_url 的 post_fn；start 的首帧强制 IDR 会重发 init 给 relay 新建的
    // DesktopStream——否则 relay 重启后 viewer 拿不到参数集黑屏）。
    #[cfg(feature = "desktop")]
    if *desktop_want_running && desktop_cfg.enabled() {
        tracing::warn!("reconnected with desktop previously running — auto-restoring desktop stream");
        desktop.start(post_fn.clone()).await;
    }

    let exec_sessions = crate::agent::exec_sessions::ExecSessionManager::new();

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

    // Tabs persist across reconnects (see `Terminals`); only the very first
    // session (or one after every shell exited) spawns the initial shell.
    if terminals.tabs.is_empty() {
        terminals.spawn_tab(shell_path)?;
    } else {
        tracing::info!(tabs = terminals.tabs.len(), "resuming existing shells after reconnect");
    }
    let Terminals {
        tabs,
        active_tab_id,
        tab_counter,
        shell_tx,
        shell_rx,
    } = terminals;

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
        payload: serde_json::json!({ "tabs": build_tab_infos(tabs, active_tab_id) }),
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
    // TUI 刷新/停机信号轮询。tokio::select! 是宏，无法在分支上 cfg，故 interval
    // 恒建：tui 构建 500ms 轮询（r 键刷新 token、q 停机），lean 构建 1h（不生效）。
    #[cfg(feature = "tui")]
    let mut ui_poll = tokio::time::interval(Duration::from_millis(500));
    #[cfg(not(feature = "tui"))]
    let mut ui_poll = tokio::time::interval(Duration::from_secs(3600));
    ui_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
                _ = uplink_dead.notified() => {
                    break;
                }
                // 刷新 token / 停机：结束本次会话，由 start() 决定重连(刷新)
                // 还是退出进程(停机)。二者都要求干净退出会话（回收 PTY）。
                _ = ui_poll.tick() => {
                    #[cfg(feature = "tui")]
                    {
                        if status.as_ref().map_or(false, AgentStatus::refresh_pending) {
                            tracing::info!("token refresh requested — ending session to re-register (same session id)");
                            break;
                        }
                        if status.as_ref().map_or(false, AgentStatus::shutdown_pending) {
                            tracing::info!("shutdown requested — ending session");
                            break;
                        }
                    }
                    #[cfg(not(feature = "tui"))]
                    {
                        let _ = 0;
                    }
                }
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
                        Some((tab_id, data)) if data.is_empty() => {
                            // Shell exited: reap (respawning the last tab) and
                            // tell browsers about the new tab set.
                            if reap_exited_tab(tabs, active_tab_id, tab_counter, shell_tx, &tab_id, shell_path) {
                                out.control(Message {
                                    msg_type: "session:tab_list".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload: serde_json::json!({ "tabs": build_tab_infos(tabs, active_tab_id) }),
                                }).await;
                                out.control(Message {
                                    msg_type: "session:tab_switched".to_string(),
                                    session_id: client.session_id.clone(),
                                    payload: serde_json::json!({ "tab_id": active_tab_id }),
                                }).await;
                            }
                        }
                        Some((tab_id, data)) => {
                            if let Some(ts) = tabs.get_mut(&tab_id) {
                                ts.output_buf.extend_from_slice(&data);
                                if ts.output_buf.len() > TAB_REPLAY_BYTES {
                                    let excess = ts.output_buf.len() - TAB_REPLAY_BYTES;
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
                            let is_readonly_blocked = is_readonly
                                && crate::proto::requires_write(&msg.msg_type)
                                && !web_proxy::readonly_allowed(&msg.msg_type);
                            if is_readonly_blocked {
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
                                            // 命令日志：按键重建命令行，回车时打印。必须
                                            // 在 write_input 之前喂——之后终端状态可能已变
                                            // （前台切到子进程），判定会误判成“不记录”。
                                            if cmd_log::enabled() {
                                                for l in ts.line.feed(&data) {
                                                    if ts.shell.should_log_typed_line() {
                                                        cmd_log::log_web_cmd(
                                                            &client.session_id,
                                                            &tab_id,
                                                            l.approx,
                                                            &l.text,
                                                        );
                                                    }
                                                }
                                            }
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
                                    // 先按 u64 读再 clamp：直接 `as u16` 会把
                                    // 65536 截断成 0（列数 0 的终端无法输出）。
                                    let cols = clamp_term_size(msg.payload["cols"].as_u64(), 80);
                                    let rows = clamp_term_size(msg.payload["rows"].as_u64(), 24);
                                    if let Some(ts) = tabs.get_mut(&tab_id) {
                                        if let Err(e) = ts.shell.resize(cols, rows) {
                                            tracing::error!("Failed to resize terminal: {}", e);
                                        }
                                    }
                                }

                                "session:tab_create" => {
                                    match spawn_tab(tabs, active_tab_id, tab_counter, shell_tx, shell_path) {
                                        Ok(_) => {
                                            let tab_msg = Message {
            msg_type: "session:tab_list".to_string(),
            session_id: client.session_id.clone(),
            payload: serde_json::json!({ "tabs": build_tab_infos(tabs, active_tab_id) }),
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
                                    if *active_tab_id == tab_id {
                                        *active_tab_id = tabs.keys().next().cloned().unwrap_or_default();
                                    }
                                    let tab_msg = Message {
            msg_type: "session:tab_list".to_string(),
            session_id: client.session_id.clone(),
            payload: serde_json::json!({ "tabs": build_tab_infos(tabs, active_tab_id) }),
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
                                        *active_tab_id = tab_id.clone();
                                        let sw_msg = Message {
                                            msg_type: "session:tab_switched".to_string(),
                                            session_id: client.session_id.clone(),
                                            payload: serde_json::json!({ "tab_id": tab_id }),
                                        };
                                        out.control(sw_msg).await;

                                        // Replay buffered output, routed to requesting user only
                                        if let Some(ts) = tabs.get(active_tab_id.as_str()) {
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
                                    let agent_token = client.agent_token();
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
                                        agent_token,
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
                                    // MYS-2114: optional shell selector + working
                                    // directory. Absent fields keep the pre-existing
                                    // behaviour (shell from `--shell`, inherited cwd).
                                    let shell_override = msg.payload["shell"]
                                        .as_str()
                                        .filter(|s| !s.is_empty())
                                        .map(|s| s.to_string());
                                    let cwd = msg.payload["cwd"]
                                        .as_str()
                                        .filter(|s| !s.is_empty())
                                        .map(|s| s.to_string());
                                    // Windows: absent `cwd` defaults to the agent
                                    // root (same base as file-transfer relative
                                    // paths) — an agent started as a service
                                    // inherits C:\windows\system32, where no
                                    // relative path resolves. Unix keeps
                                    // inheriting the agent's cwd, unchanged.
                                    #[cfg(windows)]
                                    let cwd = cwd.or_else(|| Some(root.to_string()));
                                    // 命令日志：日志里打印的是客户端显式指定的
                                    // shell（Windows 的 auto/powershell/cmd）；未指定
                                    // 时写 `shell=-`，与 `--shell` 无关——Unix 侧
                                    // execute_command 恒用 `sh -c`。
                                    cmd_log::log_mcp_exec(
                                        &client.session_id,
                                        shell_override.as_deref(),
                                        cwd.as_deref(),
                                        timeout_ms,
                                        &cmd,
                                    );
                                    let mcp_request_id = msg.payload["_mcp_request_id"].as_str().map(|s| s.to_string());
                                    let session_id = client.session_id.clone();
                                    let ctrl_tx = task_control_tx.clone();
                                    let shell = shell_path.to_string();
                                    // Spawn so a long-running command cannot freeze the main loop
                                    // (which would otherwise starve input and MCP round-trips).
                                    tokio::spawn(async move {
                                        let (stdout, stderr, exit_code) =
                                            execute_command(&cmd, timeout_ms, &shell, shell_override.as_deref(), cwd.as_deref()).await;
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
                                    cmd_log::log_mcp_exec_start(&client.session_id, cmd);
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
                                    cmd_log::log_mcp_stdin(&client.session_id, exec_id, &data);
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
                                        payload: serde_json::json!({ "tabs": build_tab_infos(tabs, active_tab_id) }),
                                    };
                                    out.control(tab_msg).await;

                                    // Replay buffered output AFTER tab_list so JS knows activeTabId.
                                    // Routed to the joining user only: other viewers
                                    // already show this output and would see it twice.
                                    for (tid, ts) in tabs.iter() {
                                        if !ts.output_buf.is_empty() {
                                            let text = crate::agent::encoding::decode_bytes(&ts.output_buf);
                                            let encoded = fs::encode_b64(text.as_bytes());
                                            let mut replay_payload = serde_json::json!({
                                                "data": encoded,
                                                "tab_id": tid
                                            });
                                            if !user_id.is_empty() {
                                                replay_payload["_target_user_id"] = serde_json::json!(user_id);
                                            }
                                            let replay_msg = Message {
                                                msg_type: "terminal:output".to_string(),
                                                session_id: client.session_id.clone(),
                                                payload: replay_payload,
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
                                    #[cfg(feature = "desktop")]
                                    let caps_msg = Message {
                                        msg_type: "desktop:capabilities".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: desktop.capabilities_json(),
                                    };
                                    #[cfg(feature = "desktop")]
                                    {
                                        out.control(caps_msg).await;
                                    }
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:start" => {
                                    tracing::info!("desktop:start requested");
                                    desktop.start(post_fn.clone()).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:stop" => {
                                    tracing::info!("desktop:stop requested");
                                    // 桌面上层流停止；P2P 信令会话一并回收（驱动
                                    // 任务 abort，浏览器回退 relay 路径，Task 3/6）。
                                    crate::agent::p2p::shutdown(&p2p_state).await;
                                    desktop.stop(post_fn.clone()).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:p2p-offer" => {
                                    // 浏览器作为 offerer 发起的 WebRTC 协商（Task 3
                                    // createOffer 产物）。agent 应答 → desktop:p2p-answer
                                    // {sdp, candidates[]}；建连/失败由驱动任务广播
                                    // desktop:p2p-state。窗口内二次 offer = last-wins。
                                    let sdp = msg
                                        .payload
                                        .get("sdp")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let candidates: Vec<String> = msg
                                        .payload
                                        .get("candidates")
                                        .and_then(|v| v.as_array())
                                        .map(|a| {
                                            a.iter()
                                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    tracing::info!(
                                        sdp_bytes = sdp.len(),
                                        cands = candidates.len(),
                                        "desktop:p2p-offer received"
                                    );
                                    let ctrl = task_control_tx.clone();
                                    crate::agent::p2p::handle_offer(
                                        &p2p_state,
                                        client.session_id.clone(),
                                        ctrl,
                                        &sdp,
                                        &candidates,
                                    )
                                    .await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:p2p-candidate" => {
                                    // trickle ICE：浏览器把候选中途到达的 remote
                                    // candidate 喂给正在握手的共享 peer。
                                    if let Some(c) = msg
                                        .payload
                                        .get("candidate")
                                        .and_then(|v| v.as_str())
                                    {
                                        crate::agent::p2p::handle_candidate(&p2p_state, c).await;
                                    }
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:codec" => {
                                    // 热切换编码方案（web 页编码器下拉）：
                                    // av1/h264，切换后自动重建桌面流。
                                    // R5#2 控制命令 ack：回 cmd-ack {seq, ok} 供
                                    // 浏览器确认操作结果（弱网/高负载下可见反馈）。
                                    let codec = msg
                                        .payload
                                        .get("codec")
                                        .and_then(|c| c.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let seq = msg
                                        .payload
                                        .get("seq")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    tracing::info!(codec = %codec, "desktop:codec requested");
                                    let result = desktop.set_codec(&codec, post_fn.clone()).await;
                                    // 重建竞态修复（MYS-954）：清空 LAN 缓存 init，
                                    // 避免新 viewer 拿到旧 codec init + 新格式帧。
                                    if result.is_ok() {
                                        if let Some(lan) = lan_desktop.as_ref() {
                                            lan.clear_init().await;
                                        }
                                    }
                                    let ack = Message {
                                        msg_type: "desktop:cmd-ack".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: match &result {
                                            Ok(()) => serde_json::json!({ "seq": seq, "ok": true }),
                                            Err(e) => serde_json::json!({ "seq": seq, "ok": false, "error": e }),
                                        },
                                    };
                                    out.control(ack).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:select-display" => {
                                    // 批次7 多显示器选屏（rustdesk 对齐）：切换
                                    // X11 捕获显示器。payload.display 为空 → 恢复
                                    // 启动默认（--desktop-display 或平台默认屏）。
                                    // 重建桌面流使新屏生效，回 cmd-ack {seq, ok}。
                                    let sel_disp = msg
                                        .payload
                                        .get("display")
                                        .and_then(|c| c.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let seq = msg
                                        .payload
                                        .get("seq")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    tracing::info!(sel_display = %sel_disp, "desktop:select-display requested");
                                    let result = desktop.select_display(&sel_disp, post_fn.clone()).await;
                                    // 重建竞态修复（MYS-954）：清空 LAN 缓存 init。
                                    if result.is_ok() {
                                        if let Some(lan) = lan_desktop.as_ref() {
                                            lan.clear_init().await;
                                        }
                                    }
                                    let ack = Message {
                                        msg_type: "desktop:cmd-ack".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: match &result {
                                            Ok(()) => serde_json::json!({ "seq": seq, "ok": true }),
                                            Err(e) => serde_json::json!({ "seq": seq, "ok": false, "error": e }),
                                        },
                                    };
                                    out.control(ack).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:qos" => {
                                    // 端到端延时 + 解码背压反馈 → QoS（内容驱动
                                    // fps：静态1fps/动态满帧/解码背压才降帧；码率由
                                    // 拥塞增量平滑缩放，rustdesk delay−RTT 同构）。
                                    let delay_ms = msg
                                        .payload
                                        .get("delay_ms")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as u32;
                                    // TestDelay 网络层探针（R4 甲 A1 / R5#148，
                                    // 对齐 rustdesk cm::TestDelay）：浏览器单调
                                    // 时钟往返、纯网络 RTT（0 = 未上报）。
                                    let probe_ms = msg
                                        .payload
                                        .get("probe_ms")
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
                                    let (fps, qos_scale, bitrate_kbps, qos_state) = desktop
                                        .on_qos_delay(delay_ms, probe_ms, decode_fps, decode_queue, ack_seq)
                                        .await;
                                    // 回传当前生效的 QoS 状态（对齐 rustdesk TestDelay
                                    // 携带 target_bitrate，MYS-886 #153）：浏览器可展示
                                    // 实际码率/帧率 + 质量状态（R4 甲A0 五态）。
                                    // active = 最近真实新帧 ≤1.5s（R5#25 空闲回收可见
                                    // 性：静止时 agent 已回收编码资源，面板显示"静止"）。
                                    let qos_ack = Message {
                                        msg_type: "desktop:qos-ack".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: serde_json::json!({
                                            "fps": fps,
                                            "qos_scale": qos_scale,
                                            "bitrate_kbps": bitrate_kbps,
                                            "qos_state": format!("{:?}", qos_state),
                                            "active": desktop.is_active(),
                                            "bp_count": desktop.backpressure_count(),
                                        }),
                                    };
                                    out.control(qos_ack).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:test-delay" => {
                                    // TestDelay 探针（R4 甲 A1 / R5#148，对齐
                                    // rustdesk cm::TestDelay）：浏览器单调时钟探测
                                    // 包，agent 收到即原样 echo——浏览器用本地单调
                                    // 时钟算往返 RTT（纯网络层，不含编码/解码/渲染
                                    // 管线、不依赖时钟校准）。t0 原样返回即可，agent
                                    // 侧处理时间被包含在 RTT 内（即时回包，微秒级）。
                                    let seq = msg
                                        .payload
                                        .get("seq")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let t0 = msg
                                        .payload
                                        .get("t0")
                                        .and_then(|v| v.as_f64())
                                        .unwrap_or(0.0);
                                    let ack = Message {
                                        msg_type: "test-delay-ack".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: serde_json::json!({
                                            "seq": seq,
                                            "t0": t0,
                                        }),
                                    };
                                    out.control(ack).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:congested" => {
                                    // R5#16 relay→浏览器 fan-out 拥塞回传
                                    // （relay viewer 缓冲满丢旧保新，≥5s 限频）：
                                    // 作为"传输段拥塞"证据记录并计数——浏览器
                                    // qos 上报的 e2e/dq 是浏览器段，relay drop
                                    // 是传输段，两者互补；此处不动 QoS 决策
                                    // （agent 的码率/帧率收敛仍由浏览器段主导），
                                    // 计数随 qos-ack 回传供面板显示。
                                    let dropped = msg
                                        .payload
                                        .get("dropped")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    desktop.bump_backpressure();
                                    tracing::warn!(
                                        session = %client.session_id,
                                        dropped,
                                        backpressure = desktop.backpressure_count(),
                                        "relay fan-out congested (backpressure, R5#16)"
                                    );
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:reqkey" => {
                                    // 浏览器请求关键帧（接入/参考链断裂/解码错误，
                                    // 对齐 rustdesk 控制端 refresh_video）：置 flag，
                                    // 编码循环下一拍 force_idr 即时重同步。
                                    desktop.request_idr();
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:quality" => {
                                    // 码率档切换（web 码率下拉）：speed/balanced/best
                                    // + 自定义 kbps。改档后重建桌面流。R5#2 命令 ack。
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
                                    let seq = msg
                                        .payload
                                        .get("seq")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    tracing::info!(quality = %quality, custom_kbps, "desktop:quality requested");
                                    let result = desktop
                                        .set_quality(&quality, custom_kbps, post_fn.clone())
                                        .await;
                                    let ack = Message {
                                        msg_type: "desktop:cmd-ack".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: match &result {
                                            Ok(()) => serde_json::json!({ "seq": seq, "ok": true }),
                                            Err(e) => serde_json::json!({ "seq": seq, "ok": false, "error": e }),
                                        },
                                    };
                                    out.control(ack).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:gray" => {
                                    // 灰度模式开关（web 桌面控制栏，弱网省带宽）：
                                    // H264 翻编码前 UV 置 128 flag（即时生效）；
                                    // AV1 切 monochrome 并重建流（码流无色度平面，
                                    // 省更多码率）。R5#2 命令 ack：失败带 error。
                                    let enabled = msg
                                        .payload
                                        .get("enabled")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);
                                    let seq = msg
                                        .payload
                                        .get("seq")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let result = desktop.set_gray(enabled, post_fn.clone()).await;
                                    // 重建竞态修复（MYS-954）：AV1 灰度切换会重建流
                                    // （monochrome 变化），清空 LAN 缓存 init。
                                    if result.is_ok() {
                                        if let Some(lan) = lan_desktop.as_ref() {
                                            lan.clear_init().await;
                                        }
                                    }
                                    let ack = Message {
                                        msg_type: "desktop:cmd-ack".to_string(),
                                        session_id: client.session_id.clone(),
                                        payload: match &result {
                                            Ok(()) => serde_json::json!({ "seq": seq, "ok": true }),
                                            Err(e) => serde_json::json!({ "seq": seq, "ok": false, "error": e }),
                                        },
                                    };
                                    out.control(ack).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:mouse" => {
                                    // 浏览器键鼠注入：desktop:mouse
                                    // {type,x,y,button,dx,dy}（RW 权限校验
                                    // 已在 relay requires_write 完成）。
                                    desktop.handle_mouse(&msg.payload).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:key" => {
                                    // desktop:key {code,down}（browser
                                    // KeyboardEvent.code 直传）。
                                    desktop.handle_key(&msg.payload).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:clipboard:set" => {
                                    // 浏览器把本地剪贴板文本推到远端。
                                    desktop.handle_clipboard_set(&msg.payload).await;
                                }

                                #[cfg(feature = "desktop")]
                                "desktop:clipboard:get" => {
                                    // 浏览器拉取远端剪贴板（回包经广播）。
                                    desktop.handle_clipboard_get(&post_fn).await;
                                }

                                #[cfg(feature = "desktop")]
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
                                    // download（CDN 多源 → relay 兜底，Range 续传）
                                    // → verify SHA-256 → smoke test → atomic replace
                                    // → restart with the same argv. Runs in a spawned
                                    // task so a slow download cannot freeze the
                                    // terminal; on success the process replaces itself.
                                    // 任何失败都只上报，旧 agent 继续在线。
                                    let req = match crate::agent::upgrade::UpgradeRequest::from_payload(&msg.payload) {
                                        Some(r) => r,
                                        None => {
                                            let err = Message {
                                                msg_type: "error".to_string(),
                                                session_id: client.session_id.clone(),
                                                payload: serde_json::json!({
                                                    "code": "UPGRADE_BAD_PAYLOAD",
                                                    "message": "agent:upgrade missing target_version/version, \
                                                                filename (or url) and a 64-hex sha256"
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
                                    // `--relay-insecure` 由上行客户端自身携带，直接取它，
                                    // 不在下载侧另行推断（推断错了就会让自签 relay 的兜底
                                    // 下载必然握手失败）。
                                    let relay_insecure = client.insecure_tls;
                                    match self_exe.clone() {
                                        Some(exe) => {
                                            // 升级上下文显式装配：relay 客户端与 Token
                                            // 只交给 UpgradeContext，外部 CDN 请求由下载器
                                            // 用另一个（无凭据的）客户端发出。
                                            let mut ctx = crate::agent::upgrade::UpgradeContext::new(
                                                http,
                                                relay_base,
                                                token,
                                                sid,
                                                ctrl,
                                                exe,
                                                relay_insecure,
                                            );
                                            // 换机要交接**完整**身份：`--token-type ro/both`
                                            // 的设备升级后权限必须一模一样，只交 rw 会悄悄提权。
                                            ctx.tokens = client.tokens.clone();
                                            // 回滚要用它把身份拉回原会话：旧版二进制
                                            // 不认识交接文件，但它认识自己的 `--key`。
                                            ctx.fixed_key = key.clone();
                                            ctx.token_type = token_type.to_string();
                                            // 助手是本二进制的隐藏子命令，路径就是 target 自己。
                                            ctx.helper_exe = ctx.target.clone();
                                            tokio::spawn(async move {
                                                crate::agent::upgrade::perform_upgrade(ctx, req)
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

                                // 网页代理（MYS-2383 阶段1）。两者都只做派发/取消，
                                // 真正的抓取在 WebProxy 内部的独立 task 里，慢页面
                                // 不会占住这个循环（终端输入与 MCP 调用照常）。
                                "web:request" => {
                                    web_proxy.handle_request(&msg.payload).await;
                                }

                                "web:cancel" => {
                                    web_proxy.handle_cancel(&msg.payload);
                                }

                                // TCP 字节管道（MYS-2383 阶段2）。与网页代理一样
                                // 只做转交：建连与 socket I/O 都在每连接自己的
                                // task 里，这个循环永远不会被慢目标或大流量占住。
                                "tcp:open" => {
                                    tcp_tunnel.handle_open(&msg.payload).await;
                                }

                                "tcp:data" => {
                                    tcp_tunnel.handle_data(&msg.payload).await;
                                }

                                "tcp:ack" => {
                                    tcp_tunnel.handle_ack(&msg.payload);
                                }

                                "tcp:eof" => {
                                    tcp_tunnel.handle_eof(&msg.payload);
                                }

                                "tcp:close" => {
                                    tcp_tunnel.handle_close(&msg.payload);
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
    // 网页代理会话退出清理：abort 全部在途抓取并停止回包（回包通道也随之
    // 失效，若不先关会在新会话里继续向旧 session 发 web:chunk）。
    web_proxy.shutdown();
    // TCP 管道会话退出清理：abort 全部连接 task 且不再发送消息，再收掉专用
    // 上行 task（否则它会继续向已失效的 relay 连接 POST 上一批残留数据）。
    tcp_tunnel.shutdown();
    tcp_uplink_task.abort();
    // Tabs are deliberately kept (owned by `start`): shells survive the
    // reconnect and are replayed to browsers on re-join.

    // R5#11 会话退出：记录桌面是否在跑（供重连恢复），并停止当前流——
    // run_desktop_loop 是 tokio::spawn 的孤儿 task（desktop drop 不停它），
    // 不显式 stop 会继续向已失效的 relay 连接发帧（浪费 CPU + 重连后与
    // 新流双发冲突）。P2P 驱动任务同是孤儿 task：先回收再断连，防其继续
    // 向失效连接广播 desktop:p2p-state。
    #[cfg(feature = "desktop")]
    {
        *desktop_want_running = desktop.is_running();
        if desktop.is_running() {
            tracing::warn!("session ending with desktop running — stopping stream for reconnect recovery");
            desktop.stop(post_fn.clone()).await;
        }
        crate::agent::p2p::shutdown(&p2p_state).await;
    }

    // The sender only exits once every control sender is dropped; spawned
    // tasks may still hold clones, and on a dead uplink it may sit in a POST.
    // Its session is over either way.
    sender_task.abort();

    // Tokens were already cached into `cached_tokens` right after registration,
    // so `start` can replay them on reconnect — nothing to return here.
    Ok(())
}

/// Which Windows shell a `shell_remote` command runs under.
///
/// Deliberately *not* cfg-gated: the invocation builder below is pure, so the
/// Windows quoting logic stays unit-testable on Linux (the Windows exec path
/// itself can't run in CI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinShell {
    Cmd,
    PowerShell,
}

/// Pick the shell for a Windows MCP command.
///
/// Pure (takes the probe result rather than doing it) so the decision table is
/// unit-testable on Linux, where PowerShell can't be probed at all.
///
/// - `cmd` → always cmd, even when PowerShell is available.
/// - `powershell`/`pwsh` → PowerShell when available, otherwise **cmd** without
///   erroring: a command that fails to run is worse than one that runs in the
///   fallback shell.
/// - `auto`, absent, or unrecognised → PowerShell when available, else cmd.
///
/// The MCP path deliberately ignores `--shell`: its default is `cmd.exe`, so
/// it cannot distinguish "operator chose cmd" from "defaulted to cmd". It only
/// drives the interactive PTY terminal.
pub fn pick_windows_shell(requested: Option<&str>, ps_path: Option<PathBuf>) -> WinShell {
    // Only `cmd` overrides the preference; `powershell`/`pwsh`/`auto`/absent/
    // unrecognised all mean "PowerShell if we have one" — an explicit request
    // for a missing PowerShell degrades to cmd rather than failing.
    let wants_cmd = matches!(requested.map(str::trim), Some(s) if s.eq_ignore_ascii_case("cmd"));
    if !wants_cmd && ps_path.is_some() {
        WinShell::PowerShell
    } else {
        WinShell::Cmd
    }
}

/// Locate a PowerShell executable on Windows, preferring the system-shipped
/// Windows PowerShell 5.1 over a separately installed pwsh 7.
///
/// Order: `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`, then
/// `powershell.exe` on PATH, then `pwsh.exe` on PATH. Cached in a `OnceLock`:
/// the probe touches the filesystem and the answer cannot change in a way that
/// matters for a process's lifetime.
#[cfg(not(unix))]
fn find_powershell() -> Option<PathBuf> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            // 1. System Windows PowerShell 5.1 — present on every stock install.
            if let Some(root) = std::env::var_os("SystemRoot") {
                let p = Path::new(&root)
                    .join("System32")
                    .join("WindowsPowerShell")
                    .join("v1.0")
                    .join("powershell.exe");
                if p.is_file() {
                    return Some(p);
                }
            }
            // 2. PATH, preferring powershell.exe (5.1) over pwsh.exe (7).
            let path = std::env::var_os("PATH")?;
            for exe in ["powershell.exe", "pwsh.exe"] {
                for dir in std::env::split_paths(&path) {
                    let candidate = dir.join(exe);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
            None
        })
        .clone()
}

/// Render a PowerShell script as `-EncodedCommand`'s value: base64 of the
/// script encoded in UTF-16LE. Shipping the whole script encoded sidesteps
/// cmd/PowerShell quote, `$` and `%` parsing entirely — the root cause of
/// MYS-2114 defect 1 (nested `powershell -Command "..."` was the worst case).
///
/// The script forces UTF-8 console output so the agent's `encoding::decode_bytes`
/// (UTF-8 first, GBK fallback) renders non-ASCII output correctly, and appends
/// an explicit `$LASTEXITCODE` propagation because `-EncodedCommand` otherwise
/// reports PowerShell's own exit status rather than the command's.
///
/// Head and tail are separated from the user's command by **newlines**, not
/// `;`: a `;` after a command ending in a `#` comment (`echo 1 # c`) sits on the
/// commented-out remainder of that line and is swallowed, so `$LASTEXITCODE`
/// would never be propagated. A newline terminates the comment and keeps the
/// tail on its own line regardless of what the command ends with.
pub fn encode_powershell_command(cmd: &str) -> String {
    let script = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8\n$OutputEncoding=[Text.Encoding]::UTF8\n{}\nif ($LASTEXITCODE -ne $null) {{ exit $LASTEXITCODE }}",
        cmd
    );
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    crate::agent::fs::encode_b64(&utf16le)
}

/// Build `(program, raw_args)` for a Windows command. `raw_args` is the
/// command-line fragment to append verbatim via `Command::raw_arg`; Rust's
/// default Windows argument escaping would rewrite every `"` as `\"`, which
/// cmd.exe and PowerShell then take literally.
///
/// - Cmd: `/D /S /C "<cmd>"` — `/S` makes cmd strip only the outermost quote
///   pair and keep inner quotes intact, `/D` skips AutoRun scripts (which
///   would otherwise echo into stdout).
/// - PowerShell: the encoded script from [`encode_powershell_command`], run on
///   `ps_exe` when one was probed, else on the bare `powershell.exe` name.
pub fn build_windows_invocation(shell: WinShell, ps_exe: Option<&Path>, cmd: &str) -> (String, String) {
    match shell {
        WinShell::Cmd => ("cmd.exe".to_string(), format!("/D /S /C \"{}\"", cmd)),
        WinShell::PowerShell => {
            let exe = ps_exe
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "powershell.exe".to_string());
            (
                exe,
                format!(
                    "-NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}",
                    encode_powershell_command(cmd)
                ),
            )
        }
    }
}

#[cfg(unix)]
async fn execute_command(
    cmd: &str,
    timeout_ms: u64,
    _shell: &str,
    _shell_override: Option<&str>,
    cwd: Option<&str>,
) -> (String, String, i32) {
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

    let mut command = if has_script {
        // script -c 只收一个字符串，再由 $SHELL 解释一遍，所以登录环境在内层
        // 再套一层 `sh -l -c <用户命令>`（策略见 agent::login_env）。外层串是
        // 固定常量，用户命令经环境变量传入——这样它只被解释一次，引号 / 换行 /
        // $() 的语义与不加 script 时一致。
        //
        // `-e` 让 script 返回命令自身的退出码（util-linux 默认恒返回 0，会把失败
        // 的命令报成成功），但 **busybox 的 script 不认 `-e`**，收到就 usage + exit 1。
        // busybox 环境里 `which script` 一定成功，所以这里拦不住，必须先探测再加：
        // 不然每条 MCP 命令都会变成「exit 1 + 空 stdout」。
        let mut c = tokio::process::Command::new("script");
        c.args(crate::agent::login_env::script_command_argv())
            .env(crate::agent::login_env::MCP_CMD_ENV, &cmd);
        c
    } else {
        // 没有 script（精简容器）时直接 `sh [-l] -c <用户命令>`，
        // 用户命令同样是一个独立 argv、原样传入。
        let mut c = tokio::process::Command::new("sh");
        c.args(crate::agent::login_env::sh_exec_argv(&cmd));
        c
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Explicit `cwd` only: when absent the child keeps inheriting the agent's
    // directory, exactly as before MYS-2114.
    if let Some(dir) = cwd.filter(|d| !d.is_empty()) {
        command.current_dir(dir);
    }

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

/// Kill a Windows process *tree* on timeout. `kill_on_drop` only reaches the
/// direct child (cmd.exe/powershell.exe), so its children (`ping -n 60`, a
/// detached build, …) would outlive the tool call; `taskkill /T` walks them.
#[cfg(not(unix))]
async fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    // Bounded: a wedged taskkill must not outlive the tool call either.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
}

#[cfg(not(unix))]
async fn execute_command(
    cmd: &str,
    timeout_ms: u64,
    _shell_path: &str,
    shell_override: Option<&str>,
    cwd: Option<&str>,
) -> (String, String, i32) {
    let timeout = std::time::Duration::from_millis(timeout_ms);
    // PowerShell is the MCP default on Windows; `--shell` plays no part here.
    let ps_path = find_powershell();
    let shell = pick_windows_shell(shell_override, ps_path.clone());
    let (program, raw_args) = build_windows_invocation(shell, ps_path.as_deref(), cmd);

    let mut command = tokio::process::Command::new(program);
    command.raw_arg(raw_args);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Deliberately *not* kill_on_drop: dropping the `wait_with_output`
        // future on timeout would kill only the direct child and orphan its
        // children *before* `taskkill /T` can walk the tree. The timeout arm
        // below owns the kill instead.
        .kill_on_drop(false);
    if let Some(dir) = cwd.filter(|d| !d.is_empty()) {
        command.current_dir(dir);
    }

    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                String::new(),
                format!("Failed to execute command: {}", e),
                -1,
            )
        }
    };
    let pid = child.id();

    let result = tokio::time::timeout(timeout, child.wait_with_output()).await;

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
        Err(_) => {
            kill_process_tree(pid).await;
            (
                String::new(),
                format!("Command timed out after {}s", timeout_ms / 1000),
                -1,
            )
        }
    }
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn test_sender_loop_signals_dead_uplink_after_failed_heartbeats() {
        // Relay keeps rejecting heartbeats (uplink broken / session lost)
        // → sender_loop must signal run_session to reconnect.
        let app = axum::Router::new().route(
            "/agent/send",
            axum::routing::post(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = tokio::spawn(async move { axum::serve(listener, app).await });
        let (_control_tx, control_rx) = tokio::sync::mpsc::channel::<String>(64);
        let (_output_tx, output_rx) = tokio::sync::mpsc::channel::<(String, Vec<u8>)>(64);
        let dead = std::sync::Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(sender_loop(
            reqwest::Client::new(),
            format!("http://127.0.0.1:{}/agent/send", port),
            "sess1".to_string(),
            String::new(),
            control_rx,
            output_rx,
            std::time::Duration::from_millis(30),
            dead.clone(),
            #[cfg(feature = "desktop")]
            None,
            #[cfg(feature = "tui")]
            None,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), dead.notified())
            .await
            .expect("dead uplink must be signalled");
        task.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_terminals_survive_and_buffer_during_reconnect_gap() {
        // PTYs are owned by `start` (not one relay session): while the agent
        // is between sessions, shell output keeps landing in the replay buffer.
        let mut t = Terminals::new();
        let id = t.spawn_tab("/bin/sh").unwrap();
        t.tabs.get_mut(&id).unwrap().shell.write_input(b"echo gap-marker\n").unwrap();
        t.absorb_while("/bin/sh", tokio::time::sleep(Duration::from_millis(800)))
            .await;
        let buf = String::from_utf8_lossy(&t.tabs[&id].output_buf).to_string();
        assert!(buf.contains("gap-marker"), "output buffered while disconnected: {buf:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_terminals_respawn_after_last_shell_exits() {
        // The only shell exiting must not leave a dead, persisted tab: it is
        // reaped and a fresh shell takes its place.
        let mut t = Terminals::new();
        let id = t.spawn_tab("/bin/sh").unwrap();
        t.tabs.get_mut(&id).unwrap().shell.write_input(b"exit\n").unwrap();
        t.absorb_while("/bin/sh", async {
            for _ in 0..50 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert_eq!(t.tabs.len(), 1);
        assert!(!t.tabs.contains_key(&id), "exited tab replaced");
        assert!(t.tabs.contains_key(&t.active_tab_id));
    }

    #[test]
    fn test_buffer_output_bounded() {
        let mut t = Terminals::new();
        assert!(!t.buffer_output("missing", b"x"));
    }
    use super::*;

    #[cfg(feature = "desktop")]
    #[test]
    fn test_self_rss_kb_reports_positive() {
        // R5#136-146 内存画像最小子集：Linux 下 /proc/self/statm 必然给出
        // 非零 RSS（本测试进程占内存）。
        let rss = self_rss_kb();
        assert!(rss > 0, "self_rss_kb must be >0 on Linux, got {rss}");
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn test_cpu_ms_from_stat_parses_ticks() {
        // R5#136-146 功耗采样：构造 stat（comm 含空格/括号，utime 字段14=100、
        // stime 字段15=200 ticks，hz=100）→ 应换算 3000ms。
        let stat = "1234 (shell-remote test) R 0 0 0 0 0 0 0 0 0 0 100 200";
        assert_eq!(cpu_ms_from_stat(stat, 100), 3000);
        // 字段不足 / hz=0 回退 0
        assert_eq!(cpu_ms_from_stat("1 (x) R 0", 100), 0);
        assert_eq!(cpu_ms_from_stat(stat, 0), 0);
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn test_self_cpu_temp_safe_or_positive() {
        // R5#136-146 功耗硬采样：有 hwmon 传感器环境应返回非负温度（≥0，
        // 本机 coretemp ≈ 77℃）；无传感器环境回退 0 不 panic。
        let t = self_cpu_temp();
        assert!(t >= 0.0 && t < 150.0, "cpu_temp 应在合理范围, got {t}");
    }

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
            next_index: 1,
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
            next_index: 1,
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
                next_index: 1,
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
                next_index: 1,
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

    #[test]
    fn test_assemble_upload_chunk_ignores_duplicate_chunk() {
        // C5：重复块（index < next_index）被忽略并返回成功，内容不重复写入；
        // 后续块照常追加，最终文件内容正确。
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();

        let (res, more) = assemble_upload_chunk(
            &mut reassembly,
            &root,
            "dup-uid",
            "dup.bin",
            &crate::agent::fs::encode_b64(b"AAAA"),
            0,
            3,
        );
        assert!(res.success && more);

        // chunk 1 written twice — the second delivery must be a no-op.
        let (res, more) = assemble_upload_chunk(
            &mut reassembly,
            &root,
            "dup-uid",
            "dup.bin",
            &crate::agent::fs::encode_b64(b"BBBB"),
            1,
            3,
        );
        assert!(res.success && more);
        let (res, more) = assemble_upload_chunk(
            &mut reassembly,
            &root,
            "dup-uid",
            "dup.bin",
            &crate::agent::fs::encode_b64(b"BBBB"),
            1,
            3,
        );
        assert!(res.success, "duplicate chunk must report success");
        assert!(more, "duplicate must not terminate the upload");
        assert!(
            reassembly.contains_key("dup-uid"),
            "duplicate must keep the upload alive"
        );

        let (res, more) = assemble_upload_chunk(
            &mut reassembly,
            &root,
            "dup-uid",
            "dup.bin",
            &crate::agent::fs::encode_b64(b"CCCC"),
            2,
            3,
        );
        assert!(res.success);
        assert!(!more, "last chunk is terminal");
        assert!(reassembly.is_empty());

        assert_eq!(
            std::fs::read(root.join("dup.bin")).unwrap(),
            b"AAAABBBBCCCC".to_vec(),
            "duplicate chunk must not be appended twice"
        );
    }

    #[test]
    fn test_assemble_upload_chunk_out_of_order_chunk_aborts() {
        // C5：跳号（index > next_index）说明中间丢块，无法拼出正确文件——
        // 必须报错、终止该上传并删除半成品。
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();

        let (res, more) = assemble_upload_chunk(
            &mut reassembly,
            &root,
            "ooo-uid",
            "ooo.bin",
            &crate::agent::fs::encode_b64(b"AAAA"),
            0,
            3,
        );
        assert!(res.success && more);

        // chunk 2 arrives while chunk 1 is missing.
        let (res, more) = assemble_upload_chunk(
            &mut reassembly,
            &root,
            "ooo-uid",
            "ooo.bin",
            &crate::agent::fs::encode_b64(b"CCCC"),
            2,
            3,
        );
        assert!(!res.success, "out-of-order chunk must fail");
        assert!(!more, "upload must terminate");
        assert!(
            res.error.as_deref().unwrap_or("").contains("out of order"),
            "unexpected error: {:?}",
            res.error
        );
        assert!(reassembly.is_empty(), "reassembly entry must be dropped");
        assert!(
            !root.join("ooo.bin").exists(),
            "partial file must be removed"
        );
    }

    #[test]
    fn test_assemble_upload_chunk_empty_upload_creates_zero_byte_file() {
        // C5：空上传（total_chunks=1、content 为空）必须产生 0 字节文件。
        // 卡 B 的 relay 会发出这种块。
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();

        let (res, more) = assemble_upload_chunk(&mut reassembly, &root, "empty-uid", "empty.bin", "", 0, 1);
        assert!(res.success, "empty upload must succeed: {:?}", res.error);
        assert!(!more);
        let meta = std::fs::metadata(root.join("empty.bin")).expect("0-byte file must exist");
        assert_eq!(meta.len(), 0);
    }

    #[test]
    fn test_clamp_term_size() {
        // C4：u64 → u16 直接 as 转换会回绕（65536 → 0，终端无法输出）。
        assert_eq!(clamp_term_size(Some(65536), 80), 1000);
        assert_eq!(clamp_term_size(Some(1000), 80), 1000);
        assert_eq!(clamp_term_size(Some(0), 80), 1);
        assert_eq!(clamp_term_size(Some(1), 80), 1);
        assert_eq!(clamp_term_size(Some(120), 80), 120);
        assert_eq!(clamp_term_size(None, 80), 80);
        assert_eq!(clamp_term_size(None, 24), 24);
        assert_ne!(clamp_term_size(Some(65536), 80), 0);
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
            String::new(),
            control_rx,
            output_rx,
            std::time::Duration::from_millis(50),
            std::sync::Arc::new(tokio::sync::Notify::new()),
            #[cfg(feature = "desktop")]
            None,
            #[cfg(feature = "tui")]
            None,
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

    #[cfg(feature = "desktop")]
    #[tokio::test]
    async fn test_sender_loop_heartbeat_carries_desktop_kpi() {
        // R5#150：桌面启用时心跳必须附带 KPI（fps/codec/bitrate 等），
        // relay/admin 侧可观测不依赖浏览器面板。
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
        let dm = std::sync::Arc::new(crate::agent::desktop::DesktopManager::new(
            crate::agent::desktop::DesktopConfig::default(),
        ));
        let task = tokio::spawn(sender_loop(
            client,
            url,
            "sess1".to_string(),
            String::new(),
            control_rx,
            output_rx,
            std::time::Duration::from_millis(50),
            std::sync::Arc::new(tokio::sync::Notify::new()),
            Some(dm.clone()),
            #[cfg(feature = "tui")]
            None,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        drop(control_tx);
        drop(output_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), task).await;

        let calls = calls.lock().unwrap();
        let ping = calls
            .iter()
            .find(|b| b["type"] == "ping")
            .expect("heartbeat ping");
        assert!(
            ping["kpi"].is_object(),
            "heartbeat must carry desktop KPI, got {ping:?}"
        );
        assert!(
            ping["kpi"]["fps"].is_u64() && ping["kpi"]["codec"].is_string(),
            "KPI must include fps + codec, got {ping:?}"
        );
        assert!(
            ping["kpi"]["active"].is_boolean() && ping["kpi"]["bp_count"].is_u64(),
            "heartbeat KPI must carry active/bp_count (R5#25/#16 admin 可见), got {ping:?}"
        );
    }

    /// End-to-end check of the real Windows exec path against a live cmd.exe.
    /// Only compiles on Windows targets; run it under wine with the
    /// x86_64-pc-windows-gnu test binary (there is no Windows CI runner).
    #[cfg(not(unix))]
    #[tokio::test]
    async fn test_execute_command_windows_preserves_quotes() {
        // Defect 1: `"` used to arrive as `\"` because Rust escaped the arg.
        let (out, err, code) = execute_command(
            "echo \"quoted\"", 10_000, "cmd.exe", Some("cmd"), None,
        )
        .await;
        assert_eq!(code, 0, "stderr={err}");
        assert!(
            out.contains("\"quoted\""),
            "quotes must survive cmd.exe, got {out:?} (stderr={err:?})"
        );
        assert!(
            !out.contains("\\\""),
            "output must not contain escaped quotes, got {out:?}"
        );

        // The other commands from the MYS-2114 report: `%`, `&` and a pipe all
        // used to break because the whole line was over-escaped.
        let (out, err, code) =
            execute_command("echo 100%done & echo a | find /i \"a\"", 10_000, "cmd.exe", Some("cmd"), None).await;
        assert_eq!(code, 0, "stderr={err}");
        assert!(out.contains("100%done"), "got {out:?} (stderr={err:?})");
        assert!(out.contains("a"), "pipe must run, got {out:?} (stderr={err:?})");
        assert!(
            !err.contains("\\"),
            "no stray backslash-escaping in stderr, got {err:?}"
        );
    }

    #[cfg(not(unix))]
    #[tokio::test]
    async fn test_execute_command_windows_cmd_honours_cwd() {
        // Defect 3: the agent service inherits C:\windows\system32, so a
        // relative path only works if `cwd` is actually applied.
        let dir = std::env::temp_dir();
        let (out, err, code) = execute_command(
            "echo %CD%", 10_000, "cmd.exe", Some("cmd"), Some(&dir.to_string_lossy()),
        )
        .await;
        assert_eq!(code, 0, "stderr={err}");
        // cmd prints the path in Windows form; compare case-insensitively and
        // separator-insensitively rather than assuming backslashes.
        let norm = |s: &str| s.trim().replace('/', "\\").to_ascii_lowercase();
        let want = norm(&dir.to_string_lossy());
        assert!(
            !want.is_empty() && norm(&out).ends_with(want.trim_end_matches('\\')),
            "cwd must be applied: want suffix {want:?}, got {out:?} (stderr={err:?})"
        );
    }

    #[cfg(not(unix))]
    #[tokio::test]
    async fn test_execute_command_windows_ignores_shell_path() {
        // 【用户变更】The MCP path must not consult `--shell`: its default is
        // cmd.exe, so it cannot express an explicit choice. Passing a
        // *PowerShell* `--shell` with `shell: "cmd"` must still run cmd (here
        // `%CD%` expands and quotes survive — both are cmd semantics).
        let (out, err, code) = execute_command(
            "echo %CD%", 10_000, "powershell.exe", Some("cmd"), None,
        )
        .await;
        assert_eq!(code, 0, "stderr={err}");
        assert!(
            !out.trim().is_empty() && !out.contains('%'),
            "shell:cmd must win over --shell=powershell.exe, got {out:?} (stderr={err:?})"
        );

        // And the reverse: `--shell=cmd.exe` with `shell: "auto"` must not force
        // cmd — it resolves solely from find_powershell().
        let expected = pick_windows_shell(None, find_powershell());
        let (_, _, code) = execute_command(
            "Write-Output x", 30_000, "cmd.exe", Some("auto"), None,
        )
        .await;
        if expected == WinShell::Cmd {
            // cmd has no Write-Output: a non-zero exit proves cmd was used.
            assert_ne!(code, 0, "auto without PowerShell must fall back to cmd");
        }
    }

    #[cfg(not(unix))]
    #[tokio::test]
    async fn test_execute_command_windows_powershell_override() {
        // Defect 2: an explicit `shell: "powershell"` must reach PowerShell
        // regardless of `--shell`. Script construction is unit-tested
        // separately; this checks the exec wiring.
        //
        // Wine ships a *stub* powershell.exe that swallows -EncodedCommand and
        // prints nothing, so probe first and skip rather than fail there.
        // A real Windows host always runs the probe.
        let (probe, _, _) = execute_command(
            "Write-Output probe-ok", 30_000, "cmd.exe", Some("powershell"), None,
        )
        .await;
        if !probe.contains("probe-ok") {
            eprintln!(
                "SKIP: no functional PowerShell in this environment (probe output {probe:?}) — \
                 wine's powershell.exe is a stub; validated on a real Windows host instead"
            );
            return;
        }
        // Non-ASCII + `$` + quotes are exactly what nested
        // `powershell -Command "..."` used to mangle.
        let (out, err, code) = execute_command(
            "Write-Output \"a b\"; Write-Output \"$($null)中文\"",
            30_000,
            "cmd.exe",
            Some("powershell"),
            None,
        )
        .await;
        assert_eq!(code, 0, "stderr={err}");
        assert!(out.contains("a b"), "got {out:?} (stderr={err:?})");
        assert!(out.contains("中文"), "UTF-8 output must survive, got {out:?}");
    }

    // ── MYS-2114: Windows command invocation (pure, runs on Linux) ──────────

    /// Decode the `-EncodedCommand` payload back into the PowerShell script.
    fn decode_encoded_command(raw_args: &str) -> String {
        let b64 = raw_args
            .split(" -EncodedCommand ")
            .nth(1)
            .expect("-EncodedCommand must be present");
        let bytes = crate::agent::fs::decode_b64(b64).expect("payload must be valid base64");
        assert_eq!(bytes.len() % 2, 0, "UTF-16LE payload must be even-length");
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).expect("payload must be valid UTF-16")
    }

    #[test]
    fn test_build_windows_invocation_cmd_preserves_command_verbatim() {
        // Defect 1 regression: every one of these was mangled by Rust's Windows
        // arg escaping (`"` → `\"`). raw_args must carry the command untouched,
        // wrapped in exactly one `/D /S /C "<cmd>"`.
        for cmd in [
            "echo \"quoted\"",
            "python -c \"print('x')\"",
            "echo 100%done",
            "echo a & echo b",
            "tasklist | find /i \"ping\"",
            "powershell -NoProfile -Command \"$x = 1; echo $x\"",
            "echo 中文 ok",
        ] {
            let (program, raw_args) = build_windows_invocation(WinShell::Cmd, None, cmd);
            assert_eq!(program, "cmd.exe");
            assert_eq!(
                raw_args,
                format!("/D /S /C \"{}\"", cmd),
                "cmd raw_args must be verbatim for {cmd:?}"
            );
            assert!(
                !raw_args.contains("\\\""),
                "raw_args must not contain escaped quotes for {cmd:?}: {raw_args}"
            );
        }
    }

    #[test]
    fn test_build_windows_invocation_powershell_roundtrips_script() {
        for cmd in [
            "Write-Output \"hello world\"",
            "Get-ChildItem -Path 'C:\\Program Files'",
            "Write-Output \"$env:USERPROFILE\"",
            "Write-Output 'it''s'",
            "Write-Output 中文",
        ] {
            let (program, raw_args) = build_windows_invocation(
                WinShell::PowerShell,
                Some(Path::new("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe")),
                cmd,
            );
            assert!(program.ends_with("powershell.exe"), "got {program}");
            for flag in [
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy Bypass",
                "-EncodedCommand",
            ] {
                assert!(raw_args.contains(flag), "missing {flag} in {raw_args}");
            }
            let script = decode_encoded_command(&raw_args);
            assert!(
                script.starts_with(
                    "[Console]::OutputEncoding=[Text.Encoding]::UTF8\n$OutputEncoding=[Text.Encoding]::UTF8\n"
                ),
                "script must force UTF-8 output first, got {script:?}"
            );
            assert!(
                script.contains(cmd),
                "command must survive encoding verbatim: {cmd:?} not in {script:?}"
            );
            // Newline-separated, so a trailing `#` comment in the user's command
            // cannot swallow the exit-code propagation.
            assert!(
                script.ends_with("\nif ($LASTEXITCODE -ne $null) { exit $LASTEXITCODE }"),
                "script must propagate $LASTEXITCODE on its own line, got {script:?}"
            );
        }
    }

    #[test]
    fn test_powershell_encoded_command_is_utf16le_base64() {
        // Non-ASCII must survive: the payload is UTF-16LE, not UTF-8 or GBK.
        let (_, raw_args) = build_windows_invocation(
            WinShell::PowerShell,
            Some(Path::new("pwsh.exe")),
            "echo 中文",
        );
        assert_eq!(decode_encoded_command(&raw_args), format!(
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8\n$OutputEncoding=[Text.Encoding]::UTF8\necho 中文\nif ($LASTEXITCODE -ne $null) {{ exit $LASTEXITCODE }}"
        ));
    }

    #[test]
    fn test_pick_windows_shell_auto_prefers_powershell_then_falls_back() {
        // 【用户变更】auto (the default) uses PowerShell when it is available.
        let ps = Some(PathBuf::from("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"));
        assert_eq!(pick_windows_shell(None, ps.clone()), WinShell::PowerShell);
        assert_eq!(pick_windows_shell(Some("auto"), ps.clone()), WinShell::PowerShell);
        assert_eq!(pick_windows_shell(Some("AUTO"), ps.clone()), WinShell::PowerShell);
        // No PowerShell → cmd.
        assert_eq!(pick_windows_shell(None, None), WinShell::Cmd);
        assert_eq!(pick_windows_shell(Some("auto"), None), WinShell::Cmd);
    }

    #[test]
    fn test_pick_windows_shell_explicit_requests() {
        let ps = Some(PathBuf::from("pwsh.exe"));
        // Explicit `cmd` forces cmd even when PowerShell is available.
        assert_eq!(pick_windows_shell(Some("cmd"), ps.clone()), WinShell::Cmd);
        assert_eq!(pick_windows_shell(Some("CMD"), ps.clone()), WinShell::Cmd);
        assert_eq!(pick_windows_shell(Some(" cmd "), ps.clone()), WinShell::Cmd);
        // Explicit powershell/pwsh uses it when available…
        assert_eq!(pick_windows_shell(Some("powershell"), ps.clone()), WinShell::PowerShell);
        assert_eq!(pick_windows_shell(Some("pwsh"), ps.clone()), WinShell::PowerShell);
        // …and falls back to cmd (no error) when it is not.
        assert_eq!(pick_windows_shell(Some("powershell"), None), WinShell::Cmd);
        assert_eq!(pick_windows_shell(Some("pwsh"), None), WinShell::Cmd);
        // Blank / unrecognised values behave like auto, not like an error.
        assert_eq!(pick_windows_shell(Some(""), ps.clone()), WinShell::PowerShell);
        assert_eq!(pick_windows_shell(Some("bash"), ps.clone()), WinShell::PowerShell);
        assert_eq!(pick_windows_shell(Some("bash"), None), WinShell::Cmd);
    }

    #[test]
    fn test_powershell_trailing_comment_does_not_swallow_exit_code() {
        // A command ending in a `#` comment used to swallow the `;`-joined
        // `$LASTEXITCODE` tail on the same line, so the real exit code was
        // never propagated. The tail must sit on its own line.
        let (_, raw_args) = build_windows_invocation(
            WinShell::PowerShell,
            Some(Path::new("powershell.exe")),
            "echo 1 # c",
        );
        let script = decode_encoded_command(&raw_args);
        assert_eq!(
            script,
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8\n$OutputEncoding=[Text.Encoding]::UTF8\necho 1 # c\nif ($LASTEXITCODE -ne $null) { exit $LASTEXITCODE }"
        );
        // The `$LASTEXITCODE` line is its own line, not part of the comment.
        let tail_line = script.lines().last().unwrap();
        assert_eq!(tail_line, "if ($LASTEXITCODE -ne $null) { exit $LASTEXITCODE }");
        assert!(
            !script.contains("# c;if"),
            "the comment must not swallow the tail on the same line, got {script:?}"
        );
        // Regression guard for the same hazard on the head: the encoding setup
        // lines are newline-separated too, so a leading `#` in the user's
        // command cannot comment them out either.
        assert!(
            script.lines().nth(1).unwrap().starts_with("$OutputEncoding="),
            "head must be newline-separated too, got {script:?}"
        );
    }

    #[test]
    fn test_pick_windows_shell_unrecognised_values_behave_like_auto() {
        // Explicitly covered per review: unknown strings are `auto`, not errors.
        let ps = Some(PathBuf::from("powershell.exe"));
        for v in ["auto", "AUTO", "", "bash", "sh", "cmd.exe", "powershell.exe"] {
            if v.eq_ignore_ascii_case("cmd") {
                continue;
            }
            assert_eq!(
                pick_windows_shell(Some(v), ps.clone()),
                WinShell::PowerShell,
                "unrecognised {v:?} must behave like auto when PowerShell exists"
            );
            assert_eq!(
                pick_windows_shell(Some(v), None),
                WinShell::Cmd,
                "unrecognised {v:?} must fall back to cmd without PowerShell"
            );
        }
    }

    #[test]
    fn test_build_windows_invocation_powershell_uses_probed_exe() {
        // The probed path is run as-is; with no probe result the bare name is
        // used so the OS resolves it from PATH.
        let probed = PathBuf::from("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
        let (program, _) = build_windows_invocation(WinShell::PowerShell, Some(&probed), "echo hi");
        assert_eq!(program, probed.to_string_lossy());
        let (program, _) = build_windows_invocation(WinShell::PowerShell, None, "echo hi");
        assert_eq!(program, "powershell.exe");
    }
}
