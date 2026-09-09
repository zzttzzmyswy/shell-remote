// UI 二进制：仅 agent 模式（终端 + 桌面转发）。
//
// 与 CLI 二进制（shell-remote）的差别：
// - 只提供 agent 子命令；带全部 --desktop-* 参数（capture/codec/fps/码率/
//   选屏/灰度/LAN 直连等），供带桌面转发需求的设备 agent 使用。
// - 构建必须启用 desktop feature（`cargo build --bin shell-remote-ui` 或
//   默认 features）；lean 构建（--no-default-features）时该 bin 被跳过。
use clap::{Parser, Subcommand};

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    shell_remote::install_panic_hook();
    // `_log_guard` 必须活到 main 末尾（drop 会关闭 non-blocking writer）。
    let _log_guard = shell_remote::init_logging();

    let cli = Cli::parse();

    let version = env!("CARGO_PKG_VERSION");
    tracing::info!("shell-remote-ui v{}", version);

    match cli.command {
        Command::Agent {
            relay_url,
            relay_insecure,
            key,
            root,
            token_type,
            shell,
            session_id,
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
            agent::start(
                relay_url,
                key,
                root,
                token_type.as_str().to_string(),
                shell,
                desired,
                desktop_cfg,
                relay_insecure,
            )
            .await?;
        }
    }

    Ok(())
}
