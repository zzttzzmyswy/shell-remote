use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};
use tokio::time;
use uuid::Uuid;

use crate::agent::fs;
use crate::proto::{ExecResultPayload, ExecSessionInfo};

const MAX_OUTPUT_BUF: usize = 1_000_000;

/// `sh` 的 argv（长会话 `exec_start` 与一次性 exec 共用同一策略）。
#[cfg(unix)]
fn shell_argv(cmd: &str) -> Vec<std::ffi::OsString> {
    crate::agent::login_env::sh_exec_argv(cmd)
}

/// Windows 保持原样：登录环境是 Unix 的 `/etc/profile` 语义，Windows 不适用。
#[cfg(not(unix))]
fn shell_argv(cmd: &str) -> Vec<std::ffi::OsString> {
    vec!["-c".into(), cmd.into()]
}

/// 测试用：暴露长会话路径的 argv，供 login_env 的执行级测试复用同一构造。
#[cfg(all(test, unix))]
pub(crate) fn shell_argv_for_test(cmd: &str) -> Vec<std::ffi::OsString> {
    shell_argv(cmd)
}

#[derive(Debug, Clone, PartialEq)]
enum ExecStatus {
    Running,
    Exited(i32),
    Killed,
    Timeout,
}

struct InnerSession {
    cmd: String,
    output_buf: Arc<Mutex<String>>,
    status: Arc<RwLock<ExecStatus>>,
    stdin_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Wakes the reader task so it kills the child (close/shutdown path).
    cancel: Arc<tokio::sync::Notify>,
    started_at: Instant,
}

impl Clone for InnerSession {
    fn clone(&self) -> Self {
        Self {
            cmd: self.cmd.clone(),
            output_buf: self.output_buf.clone(),
            status: self.status.clone(),
            stdin_tx: self.stdin_tx.clone(),
            cancel: self.cancel.clone(),
            started_at: self.started_at,
        }
    }
}

/// Keep only the last ~`max` bytes of `buf`, cutting on a char boundary
/// (`String::drain` panics mid-character, e.g. on CJK output > 1MB).
fn trim_front(buf: &mut String, max: usize) {
    if buf.len() <= max {
        return;
    }
    let mut keep_start = buf.len() - max;
    while !buf.is_char_boundary(keep_start) {
        keep_start += 1;
    }
    buf.drain(..keep_start);
}

/// Decode one raw output chunk as UTF-8, never splitting a multi-byte character
/// across chunk boundaries.
///
/// A 4096-byte read can land in the middle of a CJK character or an emoji, so
/// decoding each chunk on its own (`decode_bytes`) yields U+FFFD for both
/// halves. Instead, when the bytes end inside an incomplete UTF-8 sequence
/// (`error_len() == None`), the trailing partial character is held in `tail`
/// and prepended to the next chunk.
///
/// Genuinely invalid bytes (`error_len() == Some(_)`, e.g. GBK console output)
/// are not held back — they go through the platform decoder immediately.
fn decode_utf8_stream(tail: &mut Vec<u8>, chunk: &[u8]) -> String {
    tail.extend_from_slice(chunk);
    match std::str::from_utf8(tail) {
        Ok(_) => {
            let s = String::from_utf8_lossy(tail).into_owned();
            tail.clear();
            s
        }
        Err(e) if e.error_len().is_none() => {
            // Incomplete sequence at the end: emit everything before it and
            // keep the partial character for the next read.
            let cut = e.valid_up_to();
            let s = String::from_utf8_lossy(&tail[..cut]).into_owned();
            tail.drain(..cut);
            s
        }
        Err(_) => {
            // Not a split character — real legacy-encoded bytes. Use the
            // platform decoder (GBK on Windows), as before.
            let s = crate::agent::encoding::decode_bytes(tail);
            tail.clear();
            s
        }
    }
}

/// Flush whatever an incomplete multi-byte character left in `tail` at EOF.
fn flush_utf8_tail(tail: &mut Vec<u8>) -> String {
    if tail.is_empty() {
        return String::new();
    }
    let s = crate::agent::encoding::decode_bytes(tail);
    tail.clear();
    s
}

pub struct ExecSessionManager {
    sessions: RwLock<HashMap<String, InnerSession>>,
    max_sessions: usize,
    idle_timeout: Duration,
}

impl ExecSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            max_sessions: 8,
            idle_timeout: Duration::from_secs(300),
        }
    }

    pub async fn spawn(&self, cmd: &str) -> Result<ExecResultPayload, ExecResultPayload> {
        {
            let sessions = self.sessions.read().await;
            let running_count = sessions
                .values()
                .filter(|s| {
                    if let Ok(status) = s.status.try_read() {
                        *status == ExecStatus::Running
                    } else {
                        true // Can't read lock, treat as running for safety
                    }
                })
                .count();
            if running_count >= self.max_sessions {
                return Err(ExecResultPayload {
                    exec_id: String::new(),
                    stdout: String::new(),
                    stderr: String::new(),
                    status: "error".to_string(),
                    exit_code: None,
                    error: Some(
                        "Max concurrent sessions (8) reached. Close some sessions first."
                            .to_string(),
                    ),
                    _mcp_request_id: None,
                });
            }
        }

        // Reap terminated sessions before spawning
        self.reap_terminated().await;

        let exec_id = Uuid::new_v4().to_string();
        let cmd_str = cmd.to_string();

        // 与一次性 exec 共用同一条登录环境策略（agent::login_env）：Unix 上是
        // `sh [-l] -c <用户命令>`，用户命令原样作为一个 argv；Windows 行为不变。
        let mut shell_cmd = Command::new("sh");
        shell_cmd.args(shell_argv(cmd));

        let mut child = match shell_cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return Err(ExecResultPayload {
                    exec_id,
                    stdout: String::new(),
                    stderr: format!("Failed to spawn: {}", e),
                    status: "error".to_string(),
                    exit_code: None,
                    error: Some(format!("Failed to spawn process: {}", e)),
                    _mcp_request_id: None,
                });
            }
        };

        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let stdin_pipe = child.stdin.take().unwrap();

        let output_buf = Arc::new(Mutex::new(String::new()));
        let status = Arc::new(RwLock::new(ExecStatus::Running));
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
        let cancel = Arc::new(tokio::sync::Notify::new());

        let output_buf_clone = output_buf.clone();
        let status_clone = status.clone();
        let exec_id_clone = exec_id.clone();
        let cancel_clone = cancel.clone();
        let idle_timeout = self.idle_timeout;

        // stdin 写入独立成 task（C2）：`write_all` 在子进程不读管道时会阻塞
        // （管道缓冲区满）。原先它在 select! 分支里 await，会把 stdout/stderr
        // 读取一起卡住——输出停摆、看门狗误判空闲。写入活动通过
        // `stdin_last_activity` 回传，避免有输入的会话被判空闲超时。
        let stdin_cancel = cancel.clone();
        let stdin_last_activity = Arc::new(std::sync::Mutex::new(Instant::now()));
        let stdin_last_activity_tx = stdin_last_activity.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut pipe = stdin_pipe;
            loop {
                let data = tokio::select! {
                    d = stdin_rx.recv() => d,
                    _ = stdin_cancel.notified() => break,
                };
                let Some(d) = data else {
                    // 通道关闭 = 释放写端：子进程读到 EOF 后自行结束，
                    // 读取任务从管道 EOF 正常收尾。
                    let _ = pipe.shutdown().await;
                    break;
                };
                // 写入失败（管道已关闭）只说明对端不再读，不影响读取侧。
                let _ = pipe.write_all(&d).await;
                let _ = pipe.flush().await;
                if let Ok(mut t) = stdin_last_activity_tx.lock() {
                    *t = Instant::now();
                }
            }
        });

        tokio::spawn(async move {
            let mut stdout_buf = vec![0u8; 4096];
            let mut stderr_buf = vec![0u8; 4096];
            // 跨块 UTF-8 尾部：一个 4096 字节读可能正好切在多字节字符中间，
            // 各自独立 decode 会把两半都变成 U+FFFD。
            let mut stdout_tail: Vec<u8> = Vec::new();
            let mut stderr_tail: Vec<u8> = Vec::new();
            let check_interval = Duration::from_secs(30);
            let mut last_activity = Instant::now();
            // stdout/stderr 分别记 EOF：两者都关闭才说明进程不再产出，
            // 此时才能 wait()。原先任一管道 EOF 就 kill 子进程，会截断
            // 另一个管道里尚未读出的输出。
            let mut stdout_eof = false;
            let mut stderr_eof = false;
            let mut kill_requested = false;

            while !(stdout_eof && stderr_eof) {
                tokio::select! {
                    n = stdout.read(&mut stdout_buf), if !stdout_eof => {
                        match n {
                            Ok(0) => {
                                stdout_eof = true;
                                let s = flush_utf8_tail(&mut stdout_tail);
                                if !s.is_empty() {
                                    let mut buf = output_buf_clone.lock().await;
                                    buf.push_str(&s);
                                    trim_front(&mut buf, MAX_OUTPUT_BUF);
                                }
                            }
                            Ok(n) => {
                                let s = decode_utf8_stream(&mut stdout_tail, &stdout_buf[..n]);
                                if !s.is_empty() {
                                    let mut buf = output_buf_clone.lock().await;
                                    buf.push_str(&s);
                                    trim_front(&mut buf, MAX_OUTPUT_BUF);
                                }
                                last_activity = Instant::now();
                            }
                            Err(_) => {
                                stdout_eof = true;
                                let s = flush_utf8_tail(&mut stdout_tail);
                                if !s.is_empty() {
                                    let mut buf = output_buf_clone.lock().await;
                                    buf.push_str(&s);
                                    trim_front(&mut buf, MAX_OUTPUT_BUF);
                                }
                            }
                        }
                    }
                    n = stderr.read(&mut stderr_buf), if !stderr_eof => {
                        match n {
                            Ok(0) => {
                                stderr_eof = true;
                                let s = flush_utf8_tail(&mut stderr_tail);
                                if !s.is_empty() {
                                    let mut buf = output_buf_clone.lock().await;
                                    buf.push_str(&s);
                                    trim_front(&mut buf, MAX_OUTPUT_BUF);
                                }
                            }
                            Ok(n) => {
                                let s = decode_utf8_stream(&mut stderr_tail, &stderr_buf[..n]);
                                if !s.is_empty() {
                                    let mut buf = output_buf_clone.lock().await;
                                    buf.push_str(&s);
                                    trim_front(&mut buf, MAX_OUTPUT_BUF);
                                }
                                last_activity = Instant::now();
                            }
                            Err(_) => {
                                stderr_eof = true;
                                let s = flush_utf8_tail(&mut stderr_tail);
                                if !s.is_empty() {
                                    let mut buf = output_buf_clone.lock().await;
                                    buf.push_str(&s);
                                    trim_front(&mut buf, MAX_OUTPUT_BUF);
                                }
                            }
                        }
                    }
                    _ = cancel_clone.notified() => {
                        // close()/shutdown：主动终止，不等管道 EOF。
                        *status_clone.write().await = ExecStatus::Killed;
                        kill_requested = true;
                        break;
                    }
                    _ = time::sleep(check_interval) => {
                        // 写入活动也刷新空闲计时（stdin 任务独立跑，不再在
                        // 这里 await）。
                        if let Ok(t) = stdin_last_activity.lock() {
                            if *t > last_activity {
                                last_activity = *t;
                            }
                        }
                        if last_activity.elapsed() > idle_timeout {
                            tracing::warn!("Exec session {} idle timeout", exec_id_clone);
                            *status_clone.write().await = ExecStatus::Timeout;
                            kill_requested = true;
                            break;
                        }
                    }
                }
            }

            // 只在超时/被取消时 kill：正常 EOF 说明进程自己结束了，
            // 此时把它的输出读完再拿退出码即可。
            if kill_requested {
                let _ = child.start_kill();
            }

            // Wait for process to fully exit
            match child.wait().await {
                Ok(exit_status) => {
                    let code = exit_status.code();
                    let mut s = status_clone.write().await;
                    if *s == ExecStatus::Running {
                        *s = ExecStatus::Exited(code.unwrap_or(-1));
                    }
                }
                Err(_) => {
                    let mut s = status_clone.write().await;
                    if *s == ExecStatus::Running {
                        *s = ExecStatus::Killed;
                    }
                }
            }
        });

        let inner = InnerSession {
            cmd: cmd_str,
            output_buf: output_buf.clone(),
            status,
            stdin_tx,
            cancel,
            started_at: Instant::now(),
        };

        let mut sessions = self.sessions.write().await;
        sessions.insert(exec_id.clone(), inner);

        let initial_output = {
            let buf = output_buf.lock().await;
            buf.clone()
        };

        Ok(ExecResultPayload {
            exec_id,
            stdout: fs::encode_b64(initial_output.as_bytes()),
            stderr: String::new(),
            status: "running".to_string(),
            exit_code: None,
            error: None,
            _mcp_request_id: None,
        })
    }

    pub async fn write_stdin(
        &self,
        exec_id: &str,
        data: &[u8],
    ) -> Result<ExecResultPayload, ExecResultPayload> {
        let session = {
            let sessions = self.sessions.read().await;
            match sessions.get(exec_id) {
                Some(s) => s.clone(),
                None => {
                    return Err(ExecResultPayload {
                        exec_id: exec_id.to_string(),
                        stdout: String::new(),
                        stderr: String::new(),
                        status: "error".to_string(),
                        exit_code: None,
                        error: Some(format!("Session {} not found", exec_id)),
                        _mcp_request_id: None,
                    });
                }
            }
        };

        {
            let current_status = session.status.read().await.clone();
            match &current_status {
                ExecStatus::Exited(code) => {
                    return Err(ExecResultPayload {
                        exec_id: exec_id.to_string(),
                        stdout: String::new(),
                        stderr: String::new(),
                        status: "exited".to_string(),
                        exit_code: Some(*code),
                        error: Some(format!("Process has exited (code: {})", code)),
                        _mcp_request_id: None,
                    });
                }
                ExecStatus::Killed => {
                    return Err(ExecResultPayload {
                        exec_id: exec_id.to_string(),
                        stdout: String::new(),
                        stderr: String::new(),
                        status: "killed".to_string(),
                        exit_code: Some(-1),
                        error: Some("Session was killed".to_string()),
                        _mcp_request_id: None,
                    });
                }
                ExecStatus::Timeout => {
                    return Err(ExecResultPayload {
                        exec_id: exec_id.to_string(),
                        stdout: String::new(),
                        stderr: String::new(),
                        status: "timeout".to_string(),
                        exit_code: Some(-1),
                        error: Some("Session timed out".to_string()),
                        _mcp_request_id: None,
                    });
                }
                ExecStatus::Running => {}
            }
        }

        if session.stdin_tx.send(data.to_vec()).await.is_err() {
            return Err(ExecResultPayload {
                exec_id: exec_id.to_string(),
                stdout: String::new(),
                stderr: String::new(),
                status: "error".to_string(),
                exit_code: None,
                error: Some("Failed to send input: process may have exited".to_string()),
                _mcp_request_id: None,
            });
        }

        tokio::time::sleep(Duration::from_millis(50)).await;

        let output = {
            let buf = session.output_buf.lock().await;
            buf.clone()
        };

        let current_status = session.status.read().await.clone();
        let (status_str, exit_code) = match current_status {
            ExecStatus::Running => ("running".to_string(), None),
            ExecStatus::Exited(code) => ("exited".to_string(), Some(code)),
            ExecStatus::Killed => ("killed".to_string(), Some(-1)),
            ExecStatus::Timeout => ("timeout".to_string(), Some(-1)),
        };

        Ok(ExecResultPayload {
            exec_id: exec_id.to_string(),
            stdout: fs::encode_b64(output.as_bytes()),
            stderr: String::new(),
            status: status_str,
            exit_code,
            error: None,
            _mcp_request_id: None,
        })
    }

    pub async fn close(&self, exec_id: &str) -> Result<ExecResultPayload, ExecResultPayload> {
        // 从 session map 里移除，这样后续 write_stdin/close 立刻报 not found；
        // 本地拿到的 owned 句柄仍持有 cancel/status/output_buf（含 stdin 发送端
        // 的最后一个克隆——函数结束 drop 它即关闭写端）。
        let session = {
            let mut sessions = self.sessions.write().await;
            sessions.remove(exec_id)
        };
        let session = match session {
            Some(s) => s,
            None => {
                return Err(ExecResultPayload {
                    exec_id: exec_id.to_string(),
                    stdout: String::new(),
                    stderr: String::new(),
                    status: "error".to_string(),
                    exit_code: None,
                    error: Some(format!("Session {} not found", exec_id)),
                    _mcp_request_id: None,
                });
            }
        };

        // 拆开结构体：取出 stdin 发送端并 drop，等于关闭写端——进程读到
        // EOF 后自行退出（cat/交互式 shell 等）。
        let InnerSession {
            cmd: _,
            output_buf,
            status,
            stdin_tx,
            cancel,
            started_at: _,
        } = session;
        drop(stdin_tx);

        // 自然收尾给 500ms；仍在跑就主动取消（读取任务收到通知后 kill 子进程）。
        let settled = {
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                if *status.read().await != ExecStatus::Running {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                time::sleep(Duration::from_millis(20)).await;
            }
        };
        if !settled {
            cancel.notify_one();
            let deadline = Instant::now() + Duration::from_millis(1000);
            while *status.read().await == ExecStatus::Running && Instant::now() < deadline {
                time::sleep(Duration::from_millis(20)).await;
            }
        }
        // 给读取任务一点时间把 EOF 前的最后一块输出写进缓冲。
        tokio::time::sleep(Duration::from_millis(100)).await;

        let output = {
            let buf = output_buf.lock().await;
            buf.clone()
        };

        let current_status = status.read().await.clone();
        let (status_str, exit_code_value) = match current_status {
            ExecStatus::Exited(code) => ("exited".to_string(), Some(code)),
            ExecStatus::Running => {
                let mut s = status.write().await;
                *s = ExecStatus::Killed;
                ("killed".to_string(), Some(-1))
            }
            ExecStatus::Killed => ("killed".to_string(), Some(-1)),
            ExecStatus::Timeout => ("timeout".to_string(), Some(-1)),
        };

        Ok(ExecResultPayload {
            exec_id: exec_id.to_string(),
            stdout: fs::encode_b64(output.as_bytes()),
            stderr: String::new(),
            status: status_str,
            exit_code: exit_code_value,
            error: None,
            _mcp_request_id: None,
        })
    }

    pub async fn list(&self) -> ExecResultPayload {
        let sessions = self.sessions.read().await;
        let mut infos = Vec::new();
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        for (exec_id, session) in sessions.iter() {
            let status = session.status.read().await;
            let status_str = match &*status {
                ExecStatus::Running => "running".to_string(),
                ExecStatus::Exited(code) => format!("exited({})", code),
                ExecStatus::Killed => "killed".to_string(),
                ExecStatus::Timeout => "timeout".to_string(),
            };

            let started_at_secs = now_secs - session.started_at.elapsed().as_secs();

            infos.push(ExecSessionInfo {
                exec_id: exec_id.clone(),
                cmd: session.cmd.clone(),
                status: status_str,
                started_at: started_at_secs,
            });
        }

        let list_json = serde_json::to_value(&infos).unwrap();

        ExecResultPayload {
            exec_id: String::new(),
            stdout: fs::encode_b64(list_json.to_string().as_bytes()),
            stderr: String::new(),
            status: "ok".to_string(),
            exit_code: None,
            error: None,
            _mcp_request_id: None,
        }
    }

    /// Wait until the session leaves `Running` (or `timeout` elapses).
    /// Returns true when it settled.
    async fn wait_settled(&self, session: &InnerSession, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if *session.status.read().await != ExecStatus::Running {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn shutdown_all(&self) {
        let exec_ids: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions.keys().cloned().collect()
        };
        for exec_id in exec_ids {
            let _ = self.close(&exec_id).await;
        }
    }

    pub async fn reap_terminated(&self) {
        let mut sessions = self.sessions.write().await;
        let mut to_remove = Vec::new();
        for (id, s) in sessions.iter() {
            if let Ok(status) = s.status.try_read() {
                if *status != ExecStatus::Running {
                    to_remove.push(id.clone());
                }
            }
        }
        for id in to_remove {
            sessions.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_simple_command() {
        let mgr = ExecSessionManager::new();
        let result = mgr.spawn("echo hello").await.unwrap();
        assert!(!result.exec_id.is_empty());
        assert_eq!(result.status, "running");
    }

    #[tokio::test]
    async fn test_spawn_max_limit() {
        let mgr = ExecSessionManager::new();
        let mut ids = Vec::new();
        for _ in 0..8 {
            let result = mgr.spawn("sleep 10").await.unwrap();
            ids.push(result.exec_id);
        }
        let result = mgr.spawn("echo fail").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .error
            .unwrap()
            .contains("Max concurrent"));
        for id in ids {
            let _ = mgr.close(&id).await;
        }
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let mgr = ExecSessionManager::new();
        let r1 = mgr.spawn("sleep 60").await.unwrap();
        let r2 = mgr.spawn("sleep 60").await.unwrap();

        let list = mgr.list().await;
        assert_eq!(list.status, "ok");
        let decoded = fs::decode_b64(&list.stdout).unwrap();
        let sessions: Vec<ExecSessionInfo> = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(sessions.len(), 2);

        let _ = mgr.close(&r1.exec_id).await;
        let _ = mgr.close(&r2.exec_id).await;

        let list2 = mgr.list().await;
        let decoded2 = fs::decode_b64(&list2.stdout).unwrap();
        let sessions2: Vec<ExecSessionInfo> = serde_json::from_slice(&decoded2).unwrap();
        assert!(sessions2.is_empty());
    }

    #[tokio::test]
    async fn test_close_session() {
        let mgr = ExecSessionManager::new();
        let result = mgr.spawn("sleep 60").await.unwrap();
        let exec_id = result.exec_id.clone();

        let close_result = mgr.close(&exec_id).await.unwrap();
        assert_eq!(close_result.exec_id, exec_id);
        assert!(close_result.status == "killed" || close_result.status == "exited");

        let close2 = mgr.close(&exec_id).await;
        assert!(close2.is_err());
    }

    #[tokio::test]
    async fn test_shutdown_all() {
        let mgr = ExecSessionManager::new();
        mgr.spawn("sleep 60").await.unwrap();
        mgr.spawn("sleep 60").await.unwrap();

        mgr.shutdown_all().await;

        let list = mgr.list().await;
        let decoded = fs::decode_b64(&list.stdout).unwrap();
        let sessions: Vec<ExecSessionInfo> = serde_json::from_slice(&decoded).unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_write_stdin_to_running() {
        let mgr = ExecSessionManager::new();
        let r = mgr.spawn("cat").await.unwrap();
        let result = mgr.write_stdin(&r.exec_id, b"hello\n").await.unwrap();
        assert_eq!(result.status, "running");
        // After close, check output
        let close = mgr.close(&r.exec_id).await.unwrap();
        let decoded = fs::decode_b64(&close.stdout).unwrap();
        let s = String::from_utf8_lossy(&decoded);
        assert!(s.contains("hello"));
    }

    #[tokio::test]
    async fn test_write_stdin_to_nonexistent() {
        let mgr = ExecSessionManager::new();
        let result = mgr.write_stdin("nonexistent", b"data").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_write_stdin_to_exited() {
        let mgr = ExecSessionManager::new();
        let r = mgr.spawn("true").await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let result = mgr.write_stdin(&r.exec_id, b"data").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.status == "exited" || err.status == "killed");
    }

    #[tokio::test]
    async fn test_close_nonexistent_session() {
        let mgr = ExecSessionManager::new();
        let result = mgr.close("nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_close_already_exited() {
        let mgr = ExecSessionManager::new();
        let r = mgr.spawn("true").await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let result = mgr.close(&r.exec_id).await.unwrap();
        assert_eq!(result.status, "exited");
        assert_eq!(result.exit_code, Some(0));
    }

    #[tokio::test]
    async fn test_spawn_invalid_command() {
        let mgr = ExecSessionManager::new();
        let r = mgr.spawn("nonexistent_command_xyz_123").await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let result = mgr.close(&r.exec_id).await.unwrap();
        assert!(result.status == "exited" || result.status == "killed");
    }

    #[test]
    fn test_trim_front_char_boundary() {
        // "中" is 3 bytes: a byte-offset cut would land mid-character.
        let mut s = "中".repeat(10);
        trim_front(&mut s, 4);
        assert_eq!(s, "中");
        let mut short = String::from("abc");
        trim_front(&mut short, 8);
        assert_eq!(short, "abc");
    }

    /// Drain the session output buffer as text.
    async fn read_output(mgr: &ExecSessionManager, exec_id: &str) -> String {
        let sessions = mgr.sessions.read().await;
        let buf = sessions.get(exec_id).unwrap().output_buf.lock().await;
        buf.clone()
    }

    #[tokio::test]
    async fn test_stdout_and_stderr_both_captured() {
        // C2：两条管道都要读到；原先任一管道 EOF 就 start_kill 会截断另一条。
        let mgr = ExecSessionManager::new();
        let r = mgr.spawn("sh -c 'echo out; echo err >&2'").await.unwrap();
        let close = mgr.close(&r.exec_id).await.unwrap();
        assert_eq!(close.status, "exited");
        assert_eq!(close.exit_code, Some(0));
        let out = String::from_utf8_lossy(&fs::decode_b64(&close.stdout).unwrap()).into_owned();
        assert!(out.contains("out"), "stdout missing: {out:?}");
        assert!(out.contains("err"), "stderr missing: {out:?}");
    }

    #[tokio::test]
    async fn test_closed_stdout_does_not_kill_process() {
        // C2：进程主动关掉 stdout 后仍在跑；此时不能 kill，stderr 后续输出要能收到。
        let mgr = ExecSessionManager::new();
        let r = mgr
            .spawn("sh -c 'exec 1>&-; sleep 1; echo e >&2'")
            .await
            .unwrap();
        // 1.2s > sleep 1：进程应已自然退出（说明中途没有被 kill）。
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let out = read_output(&mgr, &r.exec_id).await;
        assert!(out.contains('e'), "stderr output missing: {out:?}");
        let close = mgr.close(&r.exec_id).await.unwrap();
        assert_eq!(
            close.status, "exited",
            "process must exit on its own, not be killed: {:?}",
            close.error
        );
        assert_eq!(close.exit_code, Some(0));
    }

    #[test]
    fn test_decode_utf8_stream_across_chunk_boundary() {
        // C3：「你好」6 字节，在中间拆成两块——各自独立解码会得到两个 U+FFFD。
        let bytes = "你好".as_bytes();
        let mut tail = Vec::new();
        let first = decode_utf8_stream(&mut tail, &bytes[..3]);
        assert_eq!(first, "你", "first chunk should decode its complete char");
        assert_eq!(tail.len(), 0, "no partial char is pending here");

        // 真正切在多字节字符中间：前 4 字节 = "你" + "好" 的首字节。
        let mut tail = Vec::new();
        let first = decode_utf8_stream(&mut tail, &bytes[..4]);
        assert_eq!(first, "你");
        assert_eq!(tail, &bytes[3..4], "the split byte stays buffered");
        let second = decode_utf8_stream(&mut tail, &bytes[4..]);
        assert_eq!(second, "好");
        assert!(tail.is_empty());
    }

    #[test]
    fn test_decode_utf8_stream_three_byte_split_and_eof_flush() {
        // 3 字节字符被切成 1+2，且最后一块不完整时 EOF flush 用 lossy。
        let bytes = "好".as_bytes(); // 3 bytes
        let mut tail = Vec::new();
        assert_eq!(decode_utf8_stream(&mut tail, &bytes[..1]), "");
        assert_eq!(tail.len(), 1);
        assert_eq!(decode_utf8_stream(&mut tail, &bytes[1..]), "好");
        assert!(tail.is_empty());

        // 进程在字符中途退出：剩余字节在 EOF 时 lossy 输出，不吞掉。
        let mut tail = Vec::new();
        assert_eq!(decode_utf8_stream(&mut tail, &bytes[..1]), "");
        let flushed = flush_utf8_tail(&mut tail);
        assert_eq!(flushed, "\u{FFFD}");
        assert!(tail.is_empty());
        assert_eq!(flush_utf8_tail(&mut tail), "");
    }

    #[test]
    fn test_decode_utf8_stream_passes_invalid_bytes_to_platform_decoder() {
        // 非 UTF-8 的遗留编码字节（error_len == Some）不能被当成"跨块字符"
        // 无限缓存，必须立刻交给平台解码器。
        let mut tail = Vec::new();
        let out = decode_utf8_stream(&mut tail, b"AB\xFFCD");
        assert!(tail.is_empty(), "invalid bytes must not be buffered");
        assert!(out.starts_with("AB"));
        assert!(out.ends_with("CD"));
    }
}
