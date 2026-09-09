// UI 二进制：仅 agent 模式（终端 + 桌面转发）。
//
// 与 CLI 二进制（shell-remote）的差别：
// - 只提供 agent 子命令；带全部 --desktop-* 参数（capture/codec/fps/码率/
//   选屏/灰度/LAN 直连等），供带桌面转发需求的设备 agent 使用。
// - 构建必须启用 desktop feature（`cargo build --bin shell-remote-ui` 或
//   默认 features）；lean 构建（--no-default-features）时该 bin 被跳过。
use clap::{Parser, Subcommand};
use std::io::IsTerminal as _;

use shell_remote::{agent, proto::TokenType};

#[derive(Parser)]
#[command(name = "shell-remote-ui", about = "shell-remote UI agent (terminal + desktop sharing)", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run in agent mode (connects to a relay; terminal + desktop sharing)
    Agent {
        /// Relay 服务器地址（仅 http:// 或 https://；内部自动按
        /// http→ws / https→wss 用作视频上行，WS 不稳定时回退 http/https）
        #[arg(long, default_value = "http://localhost:3000")]
        relay_url: String,

        /// 信任自签证书（连 https/wss relay 的自签 TLS 时必填；
        /// relay 默认自签证书, 不信任则 register 握手即失败）
        #[arg(long)]
        relay_insecure: bool,

        /// Fixed authentication key (optional, random token used if omitted)
        #[arg(long)]
        key: Option<String>,

        /// Default directory for file manager (defaults to $HOME / %USERPROFILE%)
        #[arg(long)]
        root: Option<String>,

        /// Token type: rw, ro, or both
        #[arg(long, default_value = "rw")]
        token_type: TokenType,

        /// Shell path (e.g., /bin/bash, powershell.exe)
        #[cfg(windows)]
        #[arg(long, env = "SHELL", default_value = "cmd.exe")]
        shell: String,
        /// Shell path (e.g., /bin/bash, /usr/bin/zsh)
        #[cfg(not(windows))]
        #[arg(long, env = "SHELL", default_value = "/bin/bash")]
        shell: String,

        /// Stable session id (5-20 ASCII alphanumeric) shown in the admin
        /// panel to distinguish devices. If it collides with an in-use id the
        /// relay rejects registration and the agent exits. Omit for a random id.
        #[arg(long)]
        session_id: Option<String>,

        /// 本机状态面板视图：auto（默认：有 DISPLAY/Windows 开原生窗口，否则
        /// 有 TTY 开终端面板，两者皆无则 headless）/ window / tui / headless。
        #[arg(long, default_value = "auto")]
        view: String,

        /// Desktop capture backend: auto | dxgi | gdi | x11 | wayland | none.
        /// Windows: dxgi (Desktop Duplication, 60fps capable) with automatic
        /// GDI fallback; Linux: wayland portal (if built with --features
        /// wayland) then X11. `none` disables desktop sharing entirely.
        #[arg(long, default_value = "auto")]
        desktop_capture: String,

        /// Desktop encoder codec: av1 (libaom, default) or h264 (OpenH264)。
        /// MYS-954：VP8/VP9（libvpx）已移除。
        #[arg(long, default_value = "av1")]
        desktop_codec: String,

        /// Desktop capture frame rate (30 balances latency vs smoothness;
        /// 60 needs a strong CPU to keep encode time from inflating e2e).
        #[arg(long, default_value_t = 30.0)]
        desktop_fps: f64,

        /// 抓帧独立上限（fps，R3 乙83 / R5#135）：限制 capture 线程产帧率，
        /// 与编码 fps 解耦。0 = 不限制（默认，动态时全速抓帧由编码 min_gap
        /// 跳帧）。设值则动态桌面抓帧也按此节流（省 X/DXGI 往返，低配 CPU
        /// 友好）。静止桌面仍走 would-block 退避，不受此参数影响。
        #[arg(long, default_value_t = 0.0)]
        desktop_capture_fps: f64,

        /// Maximum encode bitrate in kbps. 0 = 自动按 rustdesk 模型
        /// （base_bitrate(分辨率) × 质量档，1080p balanced ≈1388kbps）。
        /// 显式设值则作为硬顶（向 rustdesk 配置靠拢, MYS-886）。
        #[arg(long, default_value_t = 0)]
        desktop_max_bitrate: u64,

        /// 编码质量档：speed / balanced / best（rustdesk BR_SPEED=0.5 /
        /// BR_BALANCED=0.67 / BR_BEST=1.5，决定目标码率与 QP 区间）。
        #[arg(long, default_value = "balanced")]
        desktop_quality: String,

        /// Minimum encode bitrate in kbps (static desktop ~80; dynamic raised by ABR).
        #[arg(long, default_value_t = 80)]
        desktop_min_bitrate: u64,

        /// X11 display to capture (defaults to $DISPLAY).
        #[arg(long)]
        desktop_display: Option<String>,

        /// LAN 直连桌面流监听端口（agent 本地 HTTP server，阶段2 基础）。
        /// 0 = 不启动（默认，不开任何端口）；显式指定则同局域网浏览器可直连
        /// `http://<agent-lan-ip>:<port>/agent/desktop/stream` 拉桌面流（绕开
        /// relay 中转，浏览器同网段探测在后续阶段接入）。
        #[arg(long, default_value_t = 0)]
        desktop_lan_port: u16,
    },
}

/// 本机状态面板视图。auto：有桌面（DISPLAY / Windows）→ window；否则有 TTY →
/// tui；否则 headless。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Auto,
    Window,
    Tui,
    Headless,
}

impl View {
    fn resolve(s: &str) -> Self {
        match s {
            "window" => View::Window,
            "tui" => View::Tui,
            "headless" => View::Headless,
            _ => View::Auto,
        }
    }

    fn effective(self) -> Self {
        match self {
            v @ (View::Window | View::Tui | View::Headless) => v,
            View::Auto => {
                if cfg!(windows)
                    || std::env::var("DISPLAY")
                        .map(|d| !d.trim().is_empty())
                        .unwrap_or(false)
                {
                    View::Window
                } else if std::io::stdout().is_terminal() {
                    View::Tui
                } else {
                    View::Headless
                }
            }
        }
    }

    fn is_interactive(self) -> bool {
        matches!(self, View::Window | View::Tui)
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let view = View::resolve(match &cli.command {
        Command::Agent { view, .. } => view,
    });
    let view = view.effective();

    // 交互视图（窗口/TUI）下业务日志默认落 ~/.shell-remote/，避免刷屏污染界面；
    // headless（systemd/nohup）保持原 stderr 行为。
    if view.is_interactive() && std::env::var("SR_LOG_DIR").map(|d| d.trim().is_empty()).unwrap_or(true) {
        let dir = std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".shell-remote"))
            .unwrap_or_else(|_| std::path::PathBuf::from(".shell-remote"));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                std::env::set_var("SR_LOG_DIR", &dir);
            }
            Err(e) => {
                eprintln!("WARN: 创建默认日志目录失败 {:?}: {e}", dir.display());
            }
        }
    }
    shell_remote::install_panic_hook();
    // `_log_guard` 必须活到 main 末尾（drop 会关闭 non-blocking writer）。
    let _log_guard = shell_remote::init_logging();

    let version = env!("CARGO_PKG_VERSION");
    tracing::info!("shell-remote-ui v{} (view={view:?})", version);

    match cli.command {
        Command::Agent {
            relay_url,
            relay_insecure,
            key,
            root,
            token_type,
            shell,
            session_id,
            view: _,
            desktop_capture,
            desktop_codec,
            desktop_fps,
            desktop_capture_fps,
            desktop_max_bitrate,
            desktop_quality,
            desktop_min_bitrate,
            desktop_display,
            desktop_lan_port,
        } => {
            let desired = match session_id.as_deref() {
                Some(s) => {
                    if !shell_remote::proto::is_valid_custom_session_id(s) {
                        tracing::error!("--session-id must be 5-20 ASCII alphanumeric chars");
                        anyhow::bail!("invalid --session-id");
                    }
                    Some(s.to_string())
                }
                None => None,
            };
            let root = root.unwrap_or_else(agent::home_dir);
            // 共享状态 + 视图调度：window（原生窗口，主线程事件循环）/ tui（终端面板
            // 线程）/ headless。三者共用同一 AgentStatus（状态/会话id/token/延迟 +
            // 刷新/退出信号）。
            let agent_status = {
                let st = shell_remote::status::AgentStatus::new(relay_url.clone());
                match view {
                    View::Window => {
                        // 窗口事件循环在下方主线程运行（winit 要求主线程创建）；
                        // 这里只把 agent 放到 tokio 后台任务。
                        tracing::info!("native window panel (main-thread event loop)");
                    }
                    View::Tui => {
                        #[cfg(feature = "tui")]
                        {
                            let st2 = st.clone();
                            std::thread::Builder::new()
                                .name("sr-status-tui".into())
                                .spawn(move || {
                                    if let Err(e) = shell_remote::tui::run(st2) {
                                        tracing::warn!("TUI exited with error: {e:?}");
                                    }
                                })
                                .expect("spawn TUI thread");
                            tracing::info!("TUI status panel active");
                        }
                    }
                    View::Headless => {
                        tracing::info!("headless mode (no status panel)");
                    }
                    View::Auto => unreachable!("effective() already resolved"),
                }
                st
            };
            let desktop_cfg = shell_remote::agent::desktop::DesktopConfig {
                capture: desktop_capture,
                codec: desktop_codec,
                fps: desktop_fps,
                capture_fps: desktop_capture_fps,
                min_bps: desktop_min_bitrate * 1000,
                max_bps: desktop_max_bitrate * 1000,
                quality: match desktop_quality.as_str() {
                    "speed" => shell_remote::agent::desktop::encoder::QUALITY_SPEED,
                    "best" => shell_remote::agent::desktop::encoder::QUALITY_BEST,
                    _ => shell_remote::agent::desktop::encoder::QUALITY_BALANCED,
                },
                display: desktop_display,
                monochrome: false,
                lan_port: desktop_lan_port,
                lan_addr: None, // 由 run_session 在 LanDesktop::spawn 后注入
            };
            // agent 在独立 std::thread + 专属 tokio runtime 上运行（agent::start 的
            // future 含非 Send 的 PTY 句柄，不能 tokio::spawn 跨线程）；window 视图
            // 把主线程让给 winit 事件循环（Linux 创建 EventLoop 必须在主线程）。
            let status_for_agent = agent_status.clone();
            let agent_thread = std::thread::Builder::new()
                .name("sr-agent".into())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?;
                    rt.block_on(agent::start(
                        relay_url,
                        key,
                        root,
                        token_type.as_str().to_string(),
                        shell,
                        desired,
                        desktop_cfg,
                        Some(status_for_agent),
                        relay_insecure,
                    ))
                })
                .expect("spawn agent thread");

            match view {
                View::Window => {
                    #[cfg(feature = "gui")]
                    {
                        if let Err(e) = shell_remote::gui::run(agent_status.clone()) {
                            tracing::warn!("GUI exited with error: {e:?}");
                        }
                        // 窗口关闭/退出 → 请求 agent 停机并等它收尾（返回后进程退出）。
                        agent_status.request_shutdown();
                    }
                    let _ = agent_thread.join();
                }
                View::Tui | View::Headless | View::Auto => {
                    let _ = agent_thread.join();
                }
            }
        }
    }

    Ok(())
}
