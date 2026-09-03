use crate::proto::TokenType;
use clap::{Parser, Subcommand};

mod agent;
#[cfg(test)]
mod integration_test;
mod proto;
mod relay;
mod web;

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
    },

    /// Run in agent mode (connects to a relay)
    Agent {
        /// WebSocket URL of the relay server
        #[arg(long, default_value = "ws://localhost:3000")]
        relay_url: String,

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

        /// Desktop capture backend: auto | x11 | wayland | gdi | none.
        /// `none` disables desktop sharing entirely.
        #[arg(long, default_value = "auto")]
        desktop_capture: String,

        /// Desktop encoder codec (only h264 is currently supported).
        #[arg(long, default_value = "h264")]
        desktop_codec: String,

        /// Desktop capture frame rate.
        #[arg(long, default_value_t = 15.0)]
        desktop_fps: f64,

        /// Maximum encode bitrate in kbps (user request: 最高 800).
        #[arg(long, default_value_t = 800)]
        desktop_max_bitrate: u64,

        /// Minimum encode bitrate in kbps (user request: 最低 200).
        #[arg(long, default_value_t = 200)]
        desktop_min_bitrate: u64,

        /// X11 display to capture (defaults to $DISPLAY).
        #[arg(long)]
        desktop_display: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
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
        } => {
            relay::start(
                bind,
                auth,
                admin_path,
                admin_user,
                admin_pass,
                record_dir,
                agent_upgrade_dir,
            )
            .await?;
        }
        Command::Agent {
            relay_url,
            key,
            root,
            token_type,
            shell,
            session_id,
            desktop_capture,
            desktop_codec,
            desktop_fps,
            desktop_max_bitrate,
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
                display: desktop_display,
            };
            agent::start(
                relay_url, key, root, token_type.as_str().to_string(), shell, desired, desktop_cfg,
            )
            .await?;
        }
    }

    Ok(())
}
