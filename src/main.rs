use crate::proto::TokenType;
use clap::{Parser, Subcommand};

mod agent;
#[cfg(test)]
mod integration_test;
mod proto;
mod relay;
mod web;
mod tlsutil;

#[derive(Parser)]
#[command(name = "shell-remote", about = "Collaborative remote shell tool", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run in relay (server) mode
    Relay {
        /// Address to bind the relay server
        #[arg(long, default_value = "0.0.0.0:3000")]
        bind: String,

        /// Server access password (required)
        #[arg(long)]
        auth: Option<String>,

        /// Secret sub-path that exposes the admin panel (e.g. /sr-admin-x7k).
        /// Unset by default — the panel is fully disabled. The homepage has no
        /// link to it; you must type this path manually.
        #[arg(long)]
        admin_path: Option<String>,

        /// Admin login username (defaults to "admin" when --admin-path is set)
        #[arg(long)]
        admin_user: Option<String>,

        /// Admin login password (required when --admin-path is set)
        #[arg(long)]
        admin_pass: Option<String>,

        /// Directory to record terminal sessions to (asciinema cast v2). Unset
        /// disables recording entirely.
        #[arg(long)]
        record_dir: Option<String>,

        /// Directory with staged agent upgrade artifacts
        /// (`shell-remote-<arch>[.exe]`, plus an optional
        /// `shell-remote-<arch>.version` companion). When set, the admin
        /// device panel can trigger atomic agent self-upgrades.
        #[arg(long)]
        agent_upgrade_dir: Option<String>,

        /// Directory with per-platform agent binaries served at
        /// `/download/<filename>` (e.g. shell-remote-x86_64,
        /// shell-remote-aarch64, shell-remote-armv7, shell-remote-x86_64.exe,
        /// shell-remote-darwin-aarch64). When set, the install scripts download
        /// from this relay first and only fall back to GitHub mirrors;
        /// binaries can be staged out-of-band (e.g. scp/CI) into this dir.
        #[arg(long)]
        download_dir: Option<String>,

        /// PEM certificate for HTTPS (must be paired with --tls-key).
        /// Unset = auto-generate a self-signed certificate and persist it
        /// under ~/.shell-remote/self-signed/ for reuse across restarts.
        #[arg(long)]
        tls_cert: Option<String>,

        /// PEM private key matching --tls-cert.
        #[arg(long)]
        tls_key: Option<String>,

        /// Disable TLS on the bind port and serve plain HTTP (same port).
        /// Default is HTTPS (self-signed) on the single listen port.
        #[arg(long)]
        no_tls: bool,

        /// Per-IP `agent:register` rate limit (registrations/minute).
        /// 同出口 IP 下多台 agent 同时注册/重连时放宽, 防 flood 的最低
        /// 保护仍保留。默认 120/min。
        #[arg(long, default_value_t = 120)]
        registration_rate_limit: usize,
    },

    /// Run in agent mode (connects to a relay)
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

        /// Desktop encoder codec: av1 (libaom, default), vp9 (libvpx) or
        /// h264 (OpenH264). av1/vp9 只在对应 feature 开启时可用。
        #[arg(long, default_value = "vp9")]
        desktop_codec: String,

        /// Desktop capture frame rate (30 balances latency vs smoothness;
        /// 60 needs a strong CPU to keep encode time from inflating e2e).
        #[arg(long, default_value_t = 30.0)]
        desktop_fps: f64,

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
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 关闭 ANSI 颜色控制符: 在不支持色彩的终端(重定向/日志文件/Windows 旧终端)
    // 里会产生大量转义序列垃圾。用户要求无法检测时直接关闭。
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    let version = env!("CARGO_PKG_VERSION");
    tracing::info!("shell-remote v{}", version);

    match cli.command {
        Command::Relay {
            bind,
            auth,
            admin_path,
            admin_user,
            admin_pass,
            record_dir,
            agent_upgrade_dir,
            download_dir,
            tls_cert,
            tls_key,
            no_tls,
            registration_rate_limit,
        } => {
            relay::start(
                bind,
                auth,
                admin_path,
                admin_user,
                admin_pass,
                record_dir,
                agent_upgrade_dir,
                download_dir,
                tls_cert,
                tls_key,
                no_tls,
                registration_rate_limit,
            )
            .await?;
        }
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
            desktop_max_bitrate,
            desktop_quality,
            desktop_min_bitrate,
            desktop_display,
        } => {
            let desired = match session_id.as_deref() {
                Some(s) => {
                    if !crate::proto::is_valid_custom_session_id(s) {
                        tracing::error!("--session-id must be 5-20 ASCII alphanumeric chars");
                        anyhow::bail!("invalid --session-id");
                    }
                    Some(s.to_string())
                }
                None => None,
            };
            let root = root.unwrap_or_else(agent::home_dir);
            let desktop_cfg = crate::agent::desktop::DesktopConfig {
                capture: desktop_capture,
                codec: desktop_codec,
                fps: desktop_fps,
                min_bps: desktop_min_bitrate * 1000,
                max_bps: desktop_max_bitrate * 1000,
                quality: match desktop_quality.as_str() {
                    "speed" => crate::agent::desktop::encoder::QUALITY_SPEED,
                    "best" => crate::agent::desktop::encoder::QUALITY_BEST,
                    _ => crate::agent::desktop::encoder::QUALITY_BALANCED,
                },
                display: desktop_display,
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
