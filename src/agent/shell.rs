use anyhow::Context;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::thread;
use tokio::sync::mpsc;

pub struct Shell {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    _reader_thread: thread::JoinHandle<()>,
    pub cols: u16,
    pub rows: u16,
}

/// 终端子进程的命令与环境。
///
/// 只注入 `TERM`/`COLORTERM`（网页终端做能力协商要用，目标机一般也不设）。
/// **不注入任何 locale 变量**（LANG/LC_*）：目标机（嵌入式 / initramfs / 精简容器）
/// 常常没有任何 locale 数据，此时一个加载不了的 UTF-8 locale 会让依赖 locale 的
/// 交互式程序启动即崩（静态 glibc 的 bash 直接 SIGSEGV），网页端只看到空白终端。
/// 子进程继承 agent 自身环境，运维在目标机上设好的 locale 原样生效。
fn pty_command(shell_path: &str) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(shell_path);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd
}

impl Shell {
    pub fn spawn(
        cols: u16,
        rows: u16,
        shell_path: &str,
        tab_id: &str,
        output_tx: mpsc::UnboundedSender<(String, Vec<u8>)>,
    ) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to open pty")?;

        let mut cmd = pty_command(shell_path);

        let home = crate::agent::home_dir();
        if home != "." {
            cmd.cwd(&home);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn shell process")?;

        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .context("Failed to take pty master writer")?;

        let mut master_reader = pair
            .master
            .try_clone_reader()
            .context("Failed to clone pty master reader")?;

        let tid = tab_id.to_string();
        let reader_thread = thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            loop {
                match master_reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if output_tx.send((tid.clone(), buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            master: pair.master,
            writer,
            _child: child,
            _reader_thread: reader_thread,
            cols,
            rows,
        })
    }

    pub fn write_input(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.writer
            .write_all(data)
            .context("Failed to write to pty master")?;
        self.writer.flush().context("Failed to flush pty master")?;
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize pty")?;
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = self._child.kill();
        let _ = self._child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    const TEST_SHELL: &str = "/bin/sh";
    #[cfg(not(unix))]
    const TEST_SHELL: &str = "cmd.exe";

    #[test]
    fn test_shell_spawn() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let shell = Shell::spawn(80, 24, TEST_SHELL, "test-tab", tx);
        assert!(shell.is_ok());
    }

    #[test]
    fn test_shell_write_input() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut shell = Shell::spawn(80, 24, TEST_SHELL, "test-tab", tx).unwrap();
        let result = shell.write_input(b"echo hello\n");
        assert!(result.is_ok());
    }

    #[test]
    fn test_shell_resize() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut shell = Shell::spawn(80, 24, TEST_SHELL, "test-tab", tx).unwrap();
        let result = shell.resize(120, 40);
        assert!(result.is_ok());
        assert_eq!(shell.cols, 120);
        assert_eq!(shell.rows, 40);
    }

    /// 终端子进程必须拿到 TERM/COLORTERM（网页终端能力协商依赖）。
    #[test]
    fn test_pty_command_injects_term() {
        let cmd = pty_command(TEST_SHELL);
        assert_eq!(
            cmd.get_env("TERM").and_then(|v| v.to_str()),
            Some("xterm-256color")
        );
        assert_eq!(
            cmd.get_env("COLORTERM").and_then(|v| v.to_str()),
            Some("truecolor")
        );
    }

    /// locale 变量一律不得由 agent 注入：目标机（嵌入式 / initramfs / 精简容器）
    /// 可能没有任何 locale 数据，注入一个加载不了的 UTF-8 locale 会让交互式 shell
    /// 启动即崩（静态 glibc bash SIGSEGV），网页端只看到空白终端。
    /// `iter_extra_env_as_str` 只列 agent 显式注入的变量（不含继承自 agent 环境的），
    /// 故这里能区分「注入」与「继承」。
    #[test]
    fn test_pty_command_does_not_inject_locale() {
        let cmd = pty_command(TEST_SHELL);
        let injected: Vec<&str> = cmd.iter_extra_env_as_str().map(|(k, _)| k).collect();
        assert_eq!(
            injected,
            vec!["COLORTERM", "TERM"],
            "agent 只应注入 TERM/COLORTERM，其余环境一律继承"
        );
        for key in ["LANG", "LC_ALL", "LC_CTYPE", "LC_MESSAGES", "LANGUAGE"] {
            assert!(
                !injected.contains(&key),
                "{key} 不得由 agent 注入（会覆盖目标机自身 locale）"
            );
        }
    }

    /// 端到端：经真实 pty 起一个 shell，子进程看到的 LANG 必须与 agent 进程一致
    /// （agent 不设 LANG 时子进程也不该凭空多出一个）。
    #[test]
    fn test_shell_child_locale_matches_agent_env() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut shell = Shell::spawn(80, 24, TEST_SHELL, "locale-tab", tx).unwrap();
        // 标记串在回显里被拆开（printf 格式串与实参分开），否则断言会命中 pty 回显的命令行本身
        shell
            .write_input(
                b"printf 'CHILD_%s=[%s]\\n' LANG \"${LANG-unset}\"; printf 'PRO%s\\n' BE_DONE\n",
            )
            .unwrap();

        let expected = std::env::var("LANG").unwrap_or_else(|_| "unset".to_string());
        let mut seen = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !seen.contains("PROBE_DONE") {
            match rx.try_recv() {
                Ok((_tab, data)) => seen.push_str(&String::from_utf8_lossy(&data)),
                Err(mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        assert!(
            seen.contains("PROBE_DONE"),
            "终端无输出（shell 可能已崩）: {seen:?}"
        );
        assert!(
            seen.contains(&format!("CHILD_LANG=[{expected}]")),
            "子进程 LANG 与 agent 环境不一致（期望 [{expected}]）: {seen:?}"
        );
    }
}
