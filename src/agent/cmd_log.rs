//! 命令日志：把 MCP 执行的命令与 Web 终端里用户敲下的命令行，按统一格式打印到日志。
//!
//! 两类来源格式对齐，都是单行、字段间两个空格：
//!
//! ```text
//! [MCP] exec        sess=ABCDE  shell=auto  cwd=/root  timeout=30s  $ ls -la
//! [MCP] exec_start  sess=ABCDE  $ tail -f /var/log/syslog
//! [MCP] stdin       sess=ABCDE  id=1a2b3c4d  bytes=4  "yes\n"
//! [WEB] cmd         sess=ABCDE  tab=tab-1  $ git status
//! [WEB] cmd~        sess=ABCDE  tab=tab-2  $ systemctl restart nginx
//! ```
//!
//! MCP 侧消息里本来就带 `cmd`，直接打印即可；Web 终端只在 `terminal:input`
//! 里收到原始按键，所以要在 agent 侧用 [`LineTracker`] 按键重建命令行（best-effort）：
//! 用方向键 / Tab 补全 / 历史检索改写过的行标 `cmd~`，提示内容可能与实际执行的不同。
//!
//! 日志走 `tracing::info!(target: "cmd", ...)`，与其它日志同源（默认 stderr，
//! 设 `SR_LOG_DIR` 时写滚动文件），不使用 println!/eprintln!。

use std::sync::atomic::{AtomicBool, Ordering};

/// 命令文本单行化后的最大字符数（超出截断并标注被截掉的字符数）。
pub const MAX_CMD_CHARS: usize = 1000;

/// 按键重建时缓冲行的最大字符数（超出后丢弃后续输入并标 approx）。
const MAX_LINE_CHARS: usize = 4096;

/// stdin 预览的最大字符数。
const MAX_STDIN_PREVIEW_CHARS: usize = 200;

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 命令文本单行化：`\n`→`\\n`，`\r`→`\\r`，`\t`→`\\t`，其他 C0 控制符与 0x7f
/// →`\\x{:02x}`；超过 [`MAX_CMD_CHARS`] 个字符时按字符截断并追加 `…(+N chars)`。
pub fn escape_cmd(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    let total = out.chars().count();
    if total <= MAX_CMD_CHARS {
        return out;
    }
    let kept: String = out.chars().take(MAX_CMD_CHARS).collect();
    format!("{kept}…(+{} chars)", total - MAX_CMD_CHARS)
}

/// `[TAG] action<pad>` 前缀：动作名左对齐填充到 11 列，再接一个空格。
fn head(tag: &str, action: &str) -> String {
    format!("[{tag}] {action:<11} ")
}

/// 超时按秒打印（整秒），否则按毫秒。
fn format_timeout(timeout_ms: u64) -> String {
    if timeout_ms % 1000 == 0 {
        format!("timeout={}s", timeout_ms / 1000)
    } else {
        format!("timeout={}ms", timeout_ms)
    }
}

pub fn format_mcp_exec(
    session: &str,
    shell: Option<&str>,
    cwd: Option<&str>,
    timeout_ms: u64,
    cmd: &str,
) -> String {
    format!(
        "{}sess={}  shell={}  cwd={}  {}  $ {}",
        head("MCP", "exec"),
        session,
        shell.unwrap_or("-"),
        cwd.unwrap_or("-"),
        format_timeout(timeout_ms),
        escape_cmd(cmd)
    )
}

pub fn format_mcp_exec_start(session: &str, cmd: &str) -> String {
    format!(
        "{}sess={}  $ {}",
        head("MCP", "exec_start"),
        session,
        escape_cmd(cmd)
    )
}

pub fn format_mcp_stdin(session: &str, exec_id: &str, data: &[u8]) -> String {
    // 只取 exec_id 前 8 个字符：完整 uuid 对阅读日志没有帮助。
    let short_id: String = exec_id.chars().take(8).collect();
    let preview: String = escape_cmd(&String::from_utf8_lossy(data))
        .chars()
        .take(MAX_STDIN_PREVIEW_CHARS)
        .collect();
    format!(
        "{}sess={}  id={}  bytes={}  \"{}\"",
        head("MCP", "stdin"),
        session,
        short_id,
        data.len(),
        preview
    )
}

pub fn format_web_cmd(session: &str, tab_id: &str, approx: bool, line: &str) -> String {
    format!(
        "{}sess={}  tab={}  $ {}",
        head("WEB", if approx { "cmd~" } else { "cmd" }),
        session,
        tab_id,
        escape_cmd(line)
    )
}

pub fn log_mcp_exec(
    session: &str,
    shell: Option<&str>,
    cwd: Option<&str>,
    timeout_ms: u64,
    cmd: &str,
) {
    if !enabled() {
        return;
    }
    tracing::info!(
        target: "cmd",
        "{}",
        format_mcp_exec(session, shell, cwd, timeout_ms, cmd)
    );
}

pub fn log_mcp_exec_start(session: &str, cmd: &str) {
    if !enabled() {
        return;
    }
    tracing::info!(target: "cmd", "{}", format_mcp_exec_start(session, cmd));
}

pub fn log_mcp_stdin(session: &str, exec_id: &str, data: &[u8]) {
    if !enabled() {
        return;
    }
    tracing::info!(target: "cmd", "{}", format_mcp_stdin(session, exec_id, data));
}

pub fn log_web_cmd(session: &str, tab_id: &str, approx: bool, line: &str) {
    if !enabled() {
        return;
    }
    tracing::info!(
        target: "cmd",
        "{}",
        format_web_cmd(session, tab_id, approx, line)
    );
}

/// 从按键流里重建出来的一行（回车时产出）。
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct TypedLine {
    pub text: String,
    /// 这一行在编辑中用过方向键 / Tab 补全 / 历史检索等无法精确还原的按键，
    /// `text` 可能与实际执行的不同。
    pub approx: bool,
}

/// 转义序列解析状态（必须跨 `feed` 调用延续：一个 CSI 序列可能被拆成两个包）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum EscState {
    #[default]
    Ground,
    /// 刚收到 ESC，等待下一个字节。
    Esc,
    /// `ESC [`，正在收集参数字节。
    Csi,
    /// `ESC O`，等待最后一个字节。
    Ss3,
}

/// 按键 → 命令行重建器（best-effort）。
#[derive(Default)]
pub struct LineTracker {
    buf: String,
    /// `buf` 的字符数（避免每字节都重算）。
    buf_chars: usize,
    approx: bool,
    esc: EscState,
    /// CSI 参数字节（如 `1;5`、`200`）。只保留判断用得到的少量字符。
    csi_params: String,
    /// 未完成的 UTF-8 前缀（多字节字符可能被拆在两个包里）。
    utf8_pending: Vec<u8>,
}

impl LineTracker {
    pub fn feed(&mut self, data: &[u8]) -> Vec<TypedLine> {
        let mut out = Vec::new();
        for &b in data {
            self.feed_byte(b, &mut out);
        }
        out
    }

    fn feed_byte(&mut self, b: u8, out: &mut Vec<TypedLine>) {
        // 转义序列优先：序列中间的字节一律不当作可打印字符。
        match self.esc {
            EscState::Esc => {
                self.esc = match b {
                    b'[' => {
                        self.csi_params.clear();
                        EscState::Csi
                    }
                    b'O' => EscState::Ss3,
                    // 连续的 ESC：重新开始等下一个字节。
                    0x1b => {
                        self.approx = true;
                        EscState::Esc
                    }
                    // Alt+键 等：无法还原，吞掉这个字节。
                    _ => {
                        self.approx = true;
                        EscState::Ground
                    }
                };
                return;
            }
            EscState::Ss3 => {
                // ESC O x（应用模式方向键等）。
                self.approx = true;
                self.esc = EscState::Ground;
                return;
            }
            EscState::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    // 终止字节。bracketed paste 的起止（ESC[200~ / ESC[201~）
                    // 只是包裹粘贴内容的标记，不算“编辑”，不置 approx。
                    let paste_marker =
                        b == b'~' && (self.csi_params == "200" || self.csi_params == "201");
                    if !paste_marker {
                        self.approx = true;
                    }
                    self.csi_params.clear();
                    self.esc = EscState::Ground;
                } else if self.csi_params.len() < 16 {
                    // 参数字节（含中间字节 0x20..=0x2f）；限制长度防止异常流量把内存撑大。
                    self.csi_params.push(b as char);
                }
                return;
            }
            EscState::Ground => {}
        }

        match b {
            0x1b => {
                self.esc = EscState::Esc;
            }
            // 回车 / 换行：产出一行（`\r\n` 的第二个字节因 buf 已空不会再产出）。
            b'\r' | b'\n' => {
                if !self.buf.trim().is_empty() {
                    out.push(TypedLine {
                        text: self.buf.trim().to_string(),
                        approx: self.approx,
                    });
                }
                self.reset_line();
            }
            // 退格 / DEL：删除最后一个字符。
            0x7f | 0x08 => {
                if self.buf.pop().is_some() {
                    self.buf_chars -= 1;
                }
            }
            // Ctrl-U：清空当前行。
            0x15 => self.reset_line(),
            // Ctrl-W：先删尾部空白，再删到前一个空白为止。
            0x17 => {
                let trimmed = self.buf.trim_end().len();
                self.buf.truncate(trimmed);
                let cut = match self
                    .buf
                    .char_indices()
                    .rev()
                    .find(|(_, c)| c.is_whitespace())
                {
                    Some((i, c)) => i + c.len_utf8(),
                    None => 0,
                };
                self.buf.truncate(cut);
                let trimmed = self.buf.trim_end().len();
                self.buf.truncate(trimmed);
                self.buf_chars = self.buf.chars().count();
            }
            // Ctrl-C：放弃当前行。
            0x03 => {
                self.reset_line();
            }
            // Tab 补全：内容无法还原（可能是文件名/命令补全）。
            b'\t' => self.approx = true,
            // 其它 C0 控制符（Ctrl-R 历史检索、Ctrl-A 行首等）：忽略但标 approx。
            b if b < 0x20 => self.approx = true,
            // 可打印字节（含多字节 UTF-8 的各个字节）：增量解码。
            _ => self.push_utf8(b),
        }
    }

    fn reset_line(&mut self) {
        self.buf.clear();
        self.buf_chars = 0;
        self.approx = false;
    }

    fn push_utf8(&mut self, b: u8) {
        self.utf8_pending.push(b);
        loop {
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(s) => {
                    let s = s.to_string();
                    self.utf8_pending.clear();
                    self.push_str(&s);
                    return;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // 前缀合法，先落进 buf，再重新判断剩下的字节。
                        let s = std::str::from_utf8(&self.utf8_pending[..valid])
                            .expect("valid_up_to 前缀必然合法")
                            .to_string();
                        self.utf8_pending.drain(..valid);
                        self.push_str(&s);
                        continue;
                    }
                    match e.error_len() {
                        Some(n) => {
                            // 非法字节：用 U+FFFD 代替，继续处理后面的字节。
                            self.utf8_pending.drain(..n);
                            self.push_str("\u{fffd}");
                        }
                        // 序列还没收全，等下一个包。
                        None => return,
                    }
                }
            }
        }
    }

    fn push_str(&mut self, s: &str) {
        if self.buf_chars >= MAX_LINE_CHARS {
            // 超过上限：丢弃后续输入，并标注内容不完整。
            self.approx = true;
            return;
        }
        let room = MAX_LINE_CHARS - self.buf_chars;
        let n = s.chars().count();
        if n <= room {
            self.buf.push_str(s);
            self.buf_chars += n;
        } else {
            self.buf.extend(s.chars().take(room));
            self.buf_chars = MAX_LINE_CHARS;
            self.approx = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(chunks: &[&[u8]]) -> Vec<TypedLine> {
        let mut t = LineTracker::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(t.feed(c));
        }
        out
    }

    fn lines(chunks: &[&[u8]]) -> Vec<(String, bool)> {
        feed_all(chunks)
            .into_iter()
            .map(|l| (l.text, l.approx))
            .collect()
    }

    // ---- LineTracker ----

    #[test]
    fn test_plain_line() {
        assert_eq!(
            feed_all(&[b"ls -la\r"]),
            vec![TypedLine {
                text: "ls -la".to_string(),
                approx: false
            }]
        );
    }

    #[test]
    fn test_backspace() {
        assert_eq!(
            lines(&[b"lsx\x7f -l\r"]),
            vec![("ls -l".to_string(), false)]
        );
        // 0x08 与 0x7f 等价
        assert_eq!(
            lines(&[b"lsx\x08 -l\r"]),
            vec![("ls -l".to_string(), false)]
        );
    }

    #[test]
    fn test_ctrl_u_clears_line() {
        assert_eq!(
            lines(&[b"rm -rf /tmp\x15ls\r"]),
            vec![("ls".to_string(), false)]
        );
    }

    #[test]
    fn test_ctrl_w_deletes_last_word() {
        assert_eq!(
            lines(&[b"git status now\x17\r"]),
            vec![("git status".to_string(), false)]
        );
        // 尾随空白也要先吃掉：删到前一个空白为止
        assert_eq!(
            lines(&[b"git status   \x17\r"]),
            vec![("git".to_string(), false)]
        );
    }

    #[test]
    fn test_ctrl_c_discards_line() {
        assert_eq!(
            lines(&[b"echo oops\x03echo ok\r"]),
            vec![("echo ok".to_string(), false)]
        );
    }

    #[test]
    fn test_arrow_up_on_empty_buffer_emits_nothing() {
        assert!(feed_all(&[b"\x1b[A\r"]).is_empty());
    }

    #[test]
    fn test_arrow_left_marks_approx() {
        assert_eq!(
            lines(&[b"git st\x1b[D\r"]),
            vec![("git st".to_string(), true)]
        );
    }

    #[test]
    fn test_tab_marks_approx() {
        // Tab 本身不入 buf（补全出来的内容是 shell 侧行为，agent 看不见），
        // 所以重建文本里没有它；但这一行内容已无法精确还原，标 approx。
        assert_eq!(
            lines(&[b"git sta\tus\r"]),
            vec![("git staus".to_string(), true)]
        );
    }

    #[test]
    fn test_bracketed_paste_is_not_approx() {
        assert_eq!(
            lines(&[b"\x1b[200~echo hi\x1b[201~\r"]),
            vec![("echo hi".to_string(), false)]
        );
    }

    #[test]
    fn test_escape_sequence_across_feeds() {
        assert_eq!(
            lines(&[b"ab\x1b", b"[Ac\r"]),
            vec![("abc".to_string(), true)]
        );
    }

    #[test]
    fn test_utf8_split_across_feeds() {
        let s = "echo 中文\r";
        let bytes = s.as_bytes();
        // “中”= E4 B8 AD，在它中间切开
        let idx = s.find('中').unwrap() + 2;
        assert_eq!(
            lines(&[&bytes[..idx], &bytes[idx..]]),
            vec!(("echo 中文".to_string(), false))
        );
    }

    #[test]
    fn test_crlf_and_blank_lines() {
        assert_eq!(
            lines(&[b"a\r\nb\r"]),
            vec![("a".to_string(), false), ("b".to_string(), false)]
        );
        assert!(feed_all(&[b"\r\n\r\n"]).is_empty());
    }

    #[test]
    fn test_over_long_line_is_capped() {
        let mut data = vec![b'a'; 5000];
        data.push(b'\r');
        let out = feed_all(&[&data]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text.chars().count(), MAX_LINE_CHARS);
        assert!(out[0].approx);
    }

    // ---- escape_cmd ----

    #[test]
    fn test_escape_cmd_control_chars() {
        assert_eq!(escape_cmd("a\nb"), "a\\nb");
        assert_eq!(escape_cmd("a\rb"), "a\\rb");
        assert_eq!(escape_cmd("a\tb"), "a\\tb");
        assert_eq!(escape_cmd("a\x1b[Ab"), "a\\x1b[Ab");
        assert_eq!(escape_cmd("a\x07b"), "a\\x07b");
        assert_eq!(escape_cmd("a\x7fb"), "a\\x7fb");
    }

    #[test]
    fn test_escape_cmd_truncates() {
        let s = "a".repeat(1200);
        let out = escape_cmd(&s);
        assert_eq!(out, format!("{}…(+200 chars)", "a".repeat(1000)));
    }

    #[test]
    fn test_escape_cmd_truncates_utf8_by_chars() {
        // 1200 个汉字：按字符截断，不能切坏 UTF-8（也不该 panic）
        let s = "中".repeat(1200);
        let out = escape_cmd(&s);
        let tail = "…(+200 chars)";
        assert!(out.ends_with(tail));
        let kept = out.strip_suffix(tail).unwrap();
        assert_eq!(kept.chars().count(), 1000);
        assert_eq!(kept, "中".repeat(1000));
    }

    #[test]
    fn test_escape_cmd_keeps_short_text_as_is() {
        assert_eq!(escape_cmd("ls -la"), "ls -la");
    }

    // ---- 格式（逐字符相等，含对齐空格）----

    #[test]
    fn test_format_mcp_exec() {
        assert_eq!(
            format_mcp_exec("ABCDE", Some("auto"), Some("/root"), 30_000, "ls -la"),
            "[MCP] exec        sess=ABCDE  shell=auto  cwd=/root  timeout=30s  $ ls -la"
        );
        // None → `-`
        assert_eq!(
            format_mcp_exec("ABCDE", None, None, 1500, "ls"),
            "[MCP] exec        sess=ABCDE  shell=-  cwd=-  timeout=1500ms  $ ls"
        );
    }

    #[test]
    fn test_format_mcp_exec_start() {
        assert_eq!(
            format_mcp_exec_start("ABCDE", "tail -f /var/log/syslog"),
            "[MCP] exec_start  sess=ABCDE  $ tail -f /var/log/syslog"
        );
    }

    #[test]
    fn test_format_mcp_stdin() {
        assert_eq!(
            format_mcp_stdin("ABCDE", "1a2b3c4d5e6f", b"yes\n"),
            "[MCP] stdin       sess=ABCDE  id=1a2b3c4d  bytes=4  \"yes\\n\""
        );
    }

    #[test]
    fn test_format_web_cmd() {
        assert_eq!(
            format_web_cmd("ABCDE", "tab-1", false, "git status"),
            "[WEB] cmd         sess=ABCDE  tab=tab-1  $ git status"
        );
        assert_eq!(
            format_web_cmd("ABCDE", "tab-2", true, "systemctl restart nginx"),
            "[WEB] cmd~        sess=ABCDE  tab=tab-2  $ systemctl restart nginx"
        );
    }

    // ---- 开关 ----

    #[test]
    fn test_enabled_toggle() {
        assert!(enabled(), "命令日志默认开启");
        set_enabled(false);
        assert!(!enabled());
        set_enabled(true);
    }
}
