//! asciinema cast v2 session recording. One file per session, written by a
//! background task that drains an mpsc of RecordEvent. The hot path only does
//! a hashmap lookup + unbounded channel send.
//!
//! The `Recorder` also owns an `McpAuditor` (sharing the same `--record-dir`)
//! that appends one JSONL line per MCP `shell_remote` call to a per-session
//! `{sid}_{ts}.audit.jsonl` file. Auditing is on exactly when recording is on
//! (same opt-in, same directory); the audit writer for a session is opened
//! lazily on the first audited call and closed alongside the cast writer on
//! kick / idle-reap.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// Decode a terminal:output / terminal:input `payload.data` field into the raw
/// terminal bytes (as a lossy UTF-8 string for the cast file). Both the agent
/// (output) and the browser (input) base64-encode terminal data, so the cast
/// must decode it back or replays would show base64 gibberish. Falls back to
/// the raw string if it isn't valid base64.
pub fn decode_terminal_data(s: &str) -> String {
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    B64
        .decode(s)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_else(|| s.to_string())
}

/// One terminal event to append to a session's cast file.
#[derive(Debug, Clone)]
pub enum RecordEvent {
    /// terminal:output payload.data
    Output(String),
    /// terminal:input payload.data
    Input(String),
}

impl RecordEvent {
    fn stream_char(&self) -> &'static str {
        match self {
            RecordEvent::Output(_) => "o",
            RecordEvent::Input(_) => "i",
        }
    }
    fn data(&self) -> &str {
        match self {
            RecordEvent::Output(s) => s,
            RecordEvent::Input(s) => s,
        }
    }
}

/// Records terminal I/O to asciinema cast v2 files under `dir`. When `None`
/// (the field on SharedState is `Option<Arc<Recorder>>`), recording is fully
/// disabled and the hot path is a single `Option::is_some` check.
pub struct Recorder {
    dir: PathBuf,
    writers: RwLock<HashMap<String, mpsc::UnboundedSender<RecordEvent>>>,
    /// MCP command audit writers, sharing `dir`. Always present; only active
    /// for sessions that have had an audited call.
    audit: McpAuditor,
}

impl Recorder {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir: dir.clone(),
            writers: RwLock::new(HashMap::new()),
            audit: McpAuditor::new(dir),
        }
    }

    /// Record an event for a session. Non-blocking: the fast path is a read
    /// lock + unbounded send; the first event for a session spawns a writer
    /// task that owns the file handle.
    pub fn record(&self, session_id: &str, ev: RecordEvent) {
        // Fast path: existing writer.
        {
            if let Ok(w) = self.writers.read() {
                if let Some(tx) = w.get(session_id) {
                    let _ = tx.send(ev);
                    return;
                }
            }
        }
        // Slow path: spawn a writer for this session.
        let mut w = match self.writers.write() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(tx) = w.get(session_id) {
            let _ = tx.send(ev);
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel::<RecordEvent>();
        w.insert(session_id.to_string(), tx.clone());
        let _ = tx.send(ev); // first event
        let dir = self.dir.clone();
        let sid = session_id.to_string();
        tokio::spawn(async move {
            run_writer(dir, sid, rx).await;
        });
    }

    /// True if a writer task is currently open for this session.
    pub fn is_recording(&self, session_id: &str) -> bool {
        self.writers
            .read()
            .map(|w| w.contains_key(session_id))
            .unwrap_or(false)
    }

    /// True if an MCP audit writer is currently open for this session.
    pub fn is_auditing(&self, session_id: &str) -> bool {
        self.audit.is_auditing(session_id)
    }

    /// Append an MCP command-audit line for a session (no-op when the recorder
    /// is `None` at the SharedState level — the caller checks first).
    pub fn audit_mcp(&self, session_id: &str, line: AuditLine) {
        self.audit.audit(session_id, line);
    }

    /// Drop the sender for a session so the writer task drains, flushes, and
    /// exits. Safe to call for sessions that were never recorded. Also closes
    /// the session's MCP audit writer so the `.audit.jsonl` file flushes.
    pub fn close(&self, session_id: &str) {
        if let Ok(mut w) = self.writers.write() {
            w.remove(session_id);
        }
        self.audit.close(session_id);
    }
}

/// Compute unix seconds for the cast header timestamp.
fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Writer task: opens `{dir}/{sid}_{unix}.cast`, writes the v2 header, then
/// drains the channel writing one JSONL event per message with a 250ms
/// periodic flush. When the channel closes, final-flush and exit.
async fn run_writer(dir: PathBuf, sid: String, mut rx: mpsc::UnboundedReceiver<RecordEvent>) {
    let ts = unix_secs();
    let path = dir.join(format!("{}_{}.cast", sid, ts));

    let file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(session = %sid, path = %path.display(), err = %e, "recorder: open failed");
            return;
        }
    };
    let mut buf = tokio::io::BufWriter::new(file);

    // Header. Each writer owns a fresh file (the filename includes the spawn
    // timestamp), so always emit the v2 header as the first line.
    let header = serde_json::json!({
        "version": 2,
        "width": 80,
        "height": 24,
        "timestamp": ts,
    });
    if let Err(e) = buf
        .write_all(format!("{}\n", header).as_bytes())
        .await
    {
        tracing::error!(session = %sid, err = %e, "recorder: header write failed");
        return;
    }
    let _ = buf.flush().await;

    let start = Instant::now();
    let mut flush = tokio::time::interval(tokio::time::Duration::from_millis(250));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            ev = rx.recv() => match ev {
                Some(ev) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    let line = serde_json::json!([elapsed, ev.stream_char(), ev.data()]);
                    if let Err(e) = buf.write_all(format!("{}\n", line).as_bytes()).await {
                        tracing::error!(session = %sid, err = %e, "recorder: event write failed");
                        break;
                    }
                }
                None => break,
            },
            _ = flush.tick() => {
                let _ = buf.flush().await;
            }
        }
    }
    let _ = buf.flush().await;
}

// ── MCP command audit ───────────────────────────────────────────────

/// Cap on how much of a command's stdout/stderr is written to the audit log.
/// The full lengths are still recorded (`stdout_len`/`stderr_len`); only the
/// inlined text is truncated so a chatty command can't blow up the audit file.
pub const AUDIT_OUTPUT_CAP: usize = 4 * 1024;

/// One audited MCP `shell_remote` call, serialized as a single JSONL line.
#[derive(Debug, Clone, Serialize)]
pub struct AuditLine {
    /// Human-readable UTC timestamp, e.g. `2026-07-06T12:34:56Z`.
    pub ts: String,
    /// Same instant as `ts`, as epoch milliseconds (for sorting/diffing).
    pub unix_ms: u64,
    pub session_id: String,
    /// First 8 chars of the calling token — enough to correlate with the
    /// admin token list without logging the full secret.
    pub token_prefix: String,
    /// `"rw"` / `"ro"`.
    pub permission: String,
    pub cmd: String,
    pub timeout_ms: u64,
    /// Wall-clock time spent waiting for the agent, in ms.
    pub duration_ms: u64,
    /// `ok` | `timeout` | `disconnected` | `no_agent` | `rejected_readonly`.
    pub status: String,
    pub exit_code: Option<i64>,
    /// Full length of stdout (bytes).
    pub stdout_len: usize,
    /// Full length of stderr (bytes).
    pub stderr_len: usize,
    /// stdout, truncated to [`AUDIT_OUTPUT_CAP`] bytes.
    pub stdout: String,
    /// stderr, truncated to [`AUDIT_OUTPUT_CAP`] bytes.
    pub stderr: String,
}

/// Appends MCP command-audit lines to `{dir}/{sid}_{ts}.audit.jsonl`, one
/// writer task per session (mirroring the cast writer lifecycle). The hot path
/// is a read lock + unbounded send.
pub struct McpAuditor {
    dir: PathBuf,
    writers: RwLock<HashMap<String, mpsc::UnboundedSender<AuditLine>>>,
}

impl McpAuditor {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            writers: RwLock::new(HashMap::new()),
        }
    }

    /// Append an audit line for a session. Non-blocking: the fast path is a
    /// read lock + unbounded send; the first line for a session spawns a
    /// writer task that owns the file handle.
    pub fn audit(&self, session_id: &str, line: AuditLine) {
        {
            if let Ok(w) = self.writers.read() {
                if let Some(tx) = w.get(session_id) {
                    let _ = tx.send(line);
                    return;
                }
            }
        }
        let mut w = match self.writers.write() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(tx) = w.get(session_id) {
            let _ = tx.send(line);
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel::<AuditLine>();
        w.insert(session_id.to_string(), tx.clone());
        let _ = tx.send(line);
        let dir = self.dir.clone();
        let sid = session_id.to_string();
        tokio::spawn(async move { run_audit_writer(dir, sid, rx).await });
    }

    /// True if an audit writer is currently open for this session.
    pub fn is_auditing(&self, session_id: &str) -> bool {
        self.writers
            .read()
            .map(|w| w.contains_key(session_id))
            .unwrap_or(false)
    }

    /// Drop the sender for a session so the writer task drains, flushes, and
    /// exits. Safe to call for sessions that were never audited.
    pub fn close(&self, session_id: &str) {
        if let Ok(mut w) = self.writers.write() {
            w.remove(session_id);
        }
    }
}

async fn run_audit_writer(dir: PathBuf, sid: String, mut rx: mpsc::UnboundedReceiver<AuditLine>) {
    let ts = unix_secs();
    let path = dir.join(format!("{}_{}.audit.jsonl", sid, ts));

    let file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(session = %sid, path = %path.display(), err = %e, "mcp audit: open failed");
            return;
        }
    };
    let mut buf = tokio::io::BufWriter::new(file);

    let start = Instant::now();
    let mut flush = tokio::time::interval(tokio::time::Duration::from_millis(250));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            line = rx.recv() => match line {
                Some(line) => {
                    let json = match serde_json::to_string(&line) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if let Err(e) = buf.write_all(format!("{}\n", json).as_bytes()).await {
                        tracing::error!(session = %sid, err = %e, "mcp audit: write failed");
                        break;
                    }
                    let _ = start.elapsed(); // keep start used; per-line timing is unix_ms
                }
                None => break,
            },
            _ = flush.tick() => {
                let _ = buf.flush().await;
            }
        }
    }
    let _ = buf.flush().await;
}

/// Truncate to at most `cap` chars, returning (text, full_len).
pub fn truncate_output(s: &str, cap: usize) -> (String, usize) {
    let len = s.chars().count();
    if len <= cap {
        (s.to_string(), len)
    } else {
        (s.chars().take(cap).collect(), len)
    }
}

/// First 8 chars of a token, for audit correlation without logging the secret.
pub fn token_prefix(token: &str) -> String {
    token.chars().take(8).collect()
}

/// Epoch milliseconds.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Format epoch milliseconds as a UTC ISO-8601 string `YYYY-MM-DDTHH:MM:SSZ`.
/// Uses the civil-from-days algorithm (Hinnant) so we don't pull in chrono
/// just for audit timestamps.
pub fn unix_ms_to_iso(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hour, min, sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_file(path: &std::path::Path) -> String {
        tokio::fs::read_to_string(path).await.unwrap()
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("sr-rec-test-{}", unix_nanos()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn unix_nanos() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| format!("{}{}", d.as_secs(), d.subsec_nanos()))
            .unwrap_or_else(|_| "0".to_string())
    }

    #[tokio::test]
    async fn test_records_output_and_input_events() {
        let dir = tempdir();
        let rec = Recorder::new(dir.clone());
        rec.record("s1", RecordEvent::Output("hello\n".to_string()));
        rec.record("s1", RecordEvent::Input("ls".to_string()));
        rec.close("s1");
        // Give the writer task time to drain + flush.
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        let p = files[0].path(); let content = read_file(&p).await;
        let mut lines = content.lines();
        let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 80);
        assert_eq!(header["height"], 24);

        let ev1: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(ev1[1], "o");
        assert_eq!(ev1[2], "hello\n");
        let ev2: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(ev2[1], "i");
        assert_eq!(ev2[2], "ls");
    }

    #[tokio::test]
    async fn test_json_escapes_special_chars() {
        let dir = tempdir();
        let rec = Recorder::new(dir.clone());
        rec.record("s2", RecordEvent::Output("a\"b\\c\n".to_string()));
        rec.close("s2");
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let p = files[0].path(); let content = read_file(&p).await;
        // The event line must be valid JSON with the data round-tripping.
        let evline = content.lines().nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(evline).unwrap();
        assert_eq!(v[2], "a\"b\\c\n");
    }

    #[tokio::test]
    async fn test_is_recording_lifecycle() {
        let dir = tempdir();
        let rec = Recorder::new(dir.clone());
        assert!(!rec.is_recording("s3"));
        rec.record("s3", RecordEvent::Output("x".to_string()));
        assert!(rec.is_recording("s3"));
        rec.close("s3");
        assert!(!rec.is_recording("s3"));
    }

    #[test]
    fn test_decode_terminal_data_base64() {
        // "hello\n" base64
        assert_eq!(decode_terminal_data("aGVsbG8K"), "hello\n");
        // invalid base64 (hyphen) falls back to raw
        assert_eq!(decode_terminal_data("hello-world"), "hello-world");
        // empty
        assert_eq!(decode_terminal_data(""), "");
    }

    fn make_audit_line(session_id: &str, cmd: &str, status: &str) -> AuditLine {
        AuditLine {
            ts: "2026-07-06T12:00:00Z".to_string(),
            unix_ms: 1_783_339_200_000,
            session_id: session_id.to_string(),
            token_prefix: "b4e2ec42".to_string(),
            permission: "rw".to_string(),
            cmd: cmd.to_string(),
            timeout_ms: 30_000,
            duration_ms: 12,
            status: status.to_string(),
            exit_code: Some(0),
            stdout_len: 5,
            stderr_len: 0,
            stdout: "hello".to_string(),
            stderr: "".to_string(),
        }
    }

    #[tokio::test]
    async fn test_mcp_audit_writes_jsonl_lines() {
        let dir = tempdir();
        let rec = Recorder::new(dir.clone());
        rec.audit_mcp(
            "s1",
            make_audit_line("s1", "ls -la", "ok"),
        );
        rec.audit_mcp(
            "s1",
            AuditLine {
                status: "timeout".to_string(),
                exit_code: None,
                ..make_audit_line("s1", "sleep 100", "timeout")
            },
        );
        rec.close("s1");
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(files.len(), 1, "one audit file per session");
        let p = files[0].path();
        assert!(
            p.to_string_lossy().ends_with(".audit.jsonl"),
            "audit file name: {}",
            p.display()
        );
        let content = read_file(&p).await;
        let mut lines = content.lines();
        let l1: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(l1["cmd"], "ls -la");
        assert_eq!(l1["status"], "ok");
        assert_eq!(l1["exit_code"], 0);
        assert_eq!(l1["token_prefix"], "b4e2ec42");
        assert_eq!(l1["stdout"], "hello");
        assert_eq!(l1["stdout_len"], 5);
        let l2: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(l2["status"], "timeout");
        assert!(l2["exit_code"].is_null());
    }

    #[tokio::test]
    async fn test_is_auditing_lifecycle() {
        let dir = tempdir();
        let rec = Recorder::new(dir.clone());
        assert!(!rec.is_auditing("s4"));
        rec.audit_mcp("s4", make_audit_line("s4", "echo hi", "ok"));
        assert!(rec.is_auditing("s4"));
        rec.close("s4");
        assert!(!rec.is_auditing("s4"));
    }

    #[test]
    fn test_truncate_output_caps_and_keeps_full_len() {
        let big = "x".repeat(10_000);
        let (t, len) = truncate_output(&big, AUDIT_OUTPUT_CAP);
        assert_eq!(t.chars().count(), AUDIT_OUTPUT_CAP);
        assert_eq!(len, 10_000);
        let (t, len) = truncate_output("abc", AUDIT_OUTPUT_CAP);
        assert_eq!(t, "abc");
        assert_eq!(len, 3);
    }

    #[test]
    fn test_token_prefix_is_8_chars() {
        assert_eq!(token_prefix("abcdef1234567890"), "abcdef12");
        assert_eq!(token_prefix("short"), "short");
        assert_eq!(token_prefix(""), "");
    }

    #[test]
    fn test_unix_ms_to_iso_known_instant() {
        // 2026-07-06T12:00:00Z = 1783339200 s = 1783339200000 ms
        assert_eq!(unix_ms_to_iso(1_783_339_200_000), "2026-07-06T12:00:00Z");
        // Unix epoch
        assert_eq!(unix_ms_to_iso(0), "1970-01-01T00:00:00Z");
        // A known leap-day instant: 2024-02-29T00:00:00Z = 1709164800 s
        assert_eq!(unix_ms_to_iso(1_709_164_800_000), "2024-02-29T00:00:00Z");
    }
}
