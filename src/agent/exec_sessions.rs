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
    started_at: Instant,
}

impl Clone for InnerSession {
    fn clone(&self) -> Self {
        Self {
            cmd: self.cmd.clone(),
            output_buf: self.output_buf.clone(),
            status: self.status.clone(),
            stdin_tx: self.stdin_tx.clone(),
            started_at: self.started_at,
        }
    }
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
            if sessions.len() >= self.max_sessions {
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

        let exec_id = Uuid::new_v4().to_string();
        let cmd_str = cmd.to_string();

        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(cmd)
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
        let mut stdin_pipe = Some(child.stdin.take().unwrap());

        let output_buf = Arc::new(Mutex::new(String::new()));
        let status = Arc::new(RwLock::new(ExecStatus::Running));
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);

        let output_buf_clone = output_buf.clone();
        let status_clone = status.clone();
        let exec_id_clone = exec_id.clone();
        let idle_timeout = self.idle_timeout;

        tokio::spawn(async move {
            let mut stdout_buf = vec![0u8; 4096];
            let mut stderr_buf = vec![0u8; 4096];
            let check_interval = Duration::from_secs(30);
            let mut last_activity = Instant::now();
            let mut done = false;

            while !done {
                tokio::select! {
                    n = stdout.read(&mut stdout_buf) => {
                        match n {
                            Ok(0) => { done = true; }
                            Ok(n) => {
                                let s = String::from_utf8_lossy(&stdout_buf[..n]).to_string();
                                let mut buf = output_buf_clone.lock().await;
                                buf.push_str(&s);
                                last_activity = Instant::now();
                            }
                            Err(_) => { done = true; }
                        }
                    }
                    n = stderr.read(&mut stderr_buf) => {
                        match n {
                            Ok(0) => { done = true; }
                            Ok(n) => {
                                let s = String::from_utf8_lossy(&stderr_buf[..n]).to_string();
                                let mut buf = output_buf_clone.lock().await;
                                buf.push_str(&s);
                                last_activity = Instant::now();
                            }
                            Err(_) => { done = true; }
                        }
                    }
                    data = stdin_rx.recv() => {
                        match data {
                            Some(d) => {
                                use tokio::io::AsyncWriteExt;
                                if let Some(ref mut pipe) = stdin_pipe {
                                    let _ = pipe.write_all(&d).await;
                                    last_activity = Instant::now();
                                }
                            }
                            None => {
                                stdin_pipe = None;
                                done = true;
                            }
                        }
                    }
                    _ = time::sleep(check_interval) => {
                        if last_activity.elapsed() > idle_timeout {
                            tracing::warn!("Exec session {} idle timeout", exec_id_clone);
                            *status_clone.write().await = ExecStatus::Timeout;
                            done = true;
                        }
                    }
                }
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

    pub async fn close(
        &self,
        exec_id: &str,
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

        drop(session.stdin_tx);

        tokio::time::sleep(Duration::from_millis(200)).await;

        let output = {
            let buf = session.output_buf.lock().await;
            buf.clone()
        };

        let current_status = session.status.read().await.clone();
        let (status_str, exit_code_value) = match current_status {
            ExecStatus::Exited(code) => ("exited".to_string(), Some(code)),
            ExecStatus::Running => {
                let mut s = session.status.write().await;
                *s = ExecStatus::Killed;
                ("killed".to_string(), Some(-1))
            }
            ExecStatus::Killed => ("killed".to_string(), Some(-1)),
            ExecStatus::Timeout => ("timeout".to_string(), Some(-1)),
        };

        let mut sessions = self.sessions.write().await;
        sessions.remove(exec_id);

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
            .unwrap()
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

    #[allow(dead_code)]
    pub async fn shutdown_all(&self) {
        let exec_ids: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions.keys().cloned().collect()
        };
        for exec_id in exec_ids {
            let _ = self.close(&exec_id).await;
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
        assert!(result.unwrap_err().error.unwrap().contains("Max concurrent"));
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
}
