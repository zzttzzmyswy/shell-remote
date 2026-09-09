//! shell-remote-ui 终端状态界面（ratatui）。
//!
//! 显示：连接状态 / 会话 id / token(rw,ro) / relay 地址 / relay 延迟(心跳 RTT) /
//! 已运行时长。按键：
//! - `r`：手动刷新 token（会话 id 不变，经 status 信号触发重建会话重新注册）
//! - `q` / `Esc` / Ctrl-C：退出程序（干净停机：先还原终端，再请求 agent 停机）
//!
//! 仅在带 TTY 且代码以 `tui` feature 构建时由 shell-remote-ui 二进制启动；
//! 无 TTY（systemd/nohup）保持原有 headless 行为。

use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;

use crate::status::{AgentStatus, Phase};

/// 事件轮询 + 渲染主循环（阻塞；由 UI 二进制在独立线程中调用）。
pub fn run(status: AgentStatus) -> anyhow::Result<()> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use ratatui::crossterm::terminal;

    terminal::enable_raw_mode()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;
    terminal.hide_cursor()?;

    let result = loop {
        terminal.draw(|f| draw(f, &status))?;
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('r') => {
                        tracing::info!("TUI: token refresh requested");
                        status.request_refresh();
                    }
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('c') if key
                        .modifiers
                        .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        break Ok(())
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    };

    // 还原终端，再请求 agent 干净停机（PTY 子进程由 drop 回收，避免 orphan）。
    terminal.show_cursor()?;
    terminal::disable_raw_mode()?;
    terminal.clear()?;
    status.request_shutdown();
    result
}

fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

fn draw(f: &mut ratatui::Frame, status: &AgentStatus) {
    let phase = status.phase();
    let (phase_color, bold) = match phase {
        Phase::Connected => (Color::Green, true),
        Phase::Reconnecting => (Color::Yellow, true),
        Phase::Connecting => (Color::Cyan, true),
        Phase::Starting => (Color::Gray, false),
    };
    let phase_style = if bold {
        Style::default()
            .fg(phase_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(phase_color)
    };

    let tokens = status.tokens();
    let tokens_text: String = if tokens.is_empty() {
        "（未注册）".to_string()
    } else {
        tokens
            .iter()
            .map(|(tok, perm)| match perm.as_str() {
                "rw" => format!("  rw: {tok}"),
                "ro" => format!("  ro: {tok}"),
                other => format!("  {other}: {tok}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let latency = status
        .latency_ms()
        .map(|ms| format!("{ms} ms（经心跳 ping，每 15s 更新）"))
        .unwrap_or_else(|| "—".to_string());
    let uptime = status
        .connected_secs()
        .map(fmt_duration)
        .unwrap_or_else(|| "—".to_string());

    let block = Block::bordered()
        .title(" shell-remote-ui ")
        .title_bottom(" [r] 刷新 token（会话 id 不变）   [q] / Ctrl-C 退出 ")
        .border_style(Style::default().fg(Color::DarkGray));
    let outer = block.inner(f.area());
    f.render_widget(block, f.area());

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(outer);

    // 状态行：状态 / 会话 id / 运行时长。
    let line1 = Paragraph::new(format!(
        "状态: {}    会话 id: {}    已运行: {}",
        phase.label(),
        status.session_id(),
        uptime
    ))
    .style(phase_style);
    f.render_widget(line1, rows[0]);

    let line2 = Paragraph::new(format!("Relay: {}", status.relay_url()));
    f.render_widget(line2, rows[1]);

    let line3 = Paragraph::new(format!("延迟: {latency}"));
    f.render_widget(line3, rows[2]);

    let token_block = Paragraph::new(tokens_text)
        .block(Block::default().borders(Borders::TOP).title(" Token "))
        .wrap(ratatui::widgets::Wrap { trim: false });
    f.render_widget(token_block, rows[3]);

    let hint = Paragraph::new("agent 运行中：终端/文件/桌面转发接入 relay 后照常工作；本面板仅作状态查看。")
        .alignment(Alignment::Left)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(hint, rows[4]);
}