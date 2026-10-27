// CLI 二进制：relay + agent（仅终端转发）。
//
// 与 UI 二进制（shell-remote-ui）的差别：
// - 本二进制 agent 不暴露任何 --desktop-* 参数，desktop 能力恒为关闭
//   （capture="none"，浏览器端不显示桌面按钮）。
// - 发布产物建议用 `--no-default-features` lean 构建：不编译桌面代码
//   （无 openh264/x11rb/enigo/str0m/AV1 等重依赖），便于快速交叉编译。
use clap::{Parser, Subcommand, ValueEnum};

use shell_remote::{agent, proto::TokenType, relay};

/// `--upgrade-variant` 的取值：把 CLI 参数映射到 relay 的制品形态。
#[derive(Debug, Clone, Copy, ValueEnum)]
enum UpgradeVariantArg {
    /// lean CLI 二进制（shell-remote-<arch>）
    Cli,
    /// 桌面 UI 二进制（shell-remote-ui-<arch>）
    Ui,
}

impl From<UpgradeVariantArg> for shell_remote::relay::artifact_store::Variant {
    fn from(v: UpgradeVariantArg) -> Self {
        match v {
            UpgradeVariantArg::Cli => Self::Cli,
            UpgradeVariantArg::Ui => Self::Ui,
        }
    }
}

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
        /// (`shell-remote-<arch>[.exe]`, `shell-remote-ui-<arch>[.exe]`, plus an
        /// optional `<filename 去掉 .exe>.version` companion).
        ///
        /// 与 `--download-dir` 是**同一个**统一制品库的别名：只设一个即可，
        /// 两个都设且指向不同目录会被拒绝启动。
        #[arg(long)]
        agent_upgrade_dir: Option<String>,

        /// Directory with per-platform agent binaries served at
        /// `/download/<filename>` (e.g. shell-remote-x86_64,
        /// shell-remote-aarch64, shell-remote-armv7, shell-remote-x86_64.exe,
        /// shell-remote-darwin-aarch64). When set, the install scripts download
        /// from this relay first and only fall back to GitHub mirrors;
        /// binaries can be staged out-of-band (e.g. scp/CI) into this dir.
        ///
        /// 这是统一制品库的主参数：公开安装下载与 rw Token 鉴权的 agent 自升级
        /// 共享同一份字节。
        #[arg(long)]
        download_dir: Option<String>,

        /// 升级任务下发给 agent 的外部 CDN 基础地址（可重复）。每项是
        /// 「前缀 + 官方 release 相对路径」，如 `https://gh-proxy.com`。
        /// 未指定时用内置的官方 origin + 镜像列表。这些域名同时构成
        /// `/agent/resolve` 代解析的白名单。
        #[arg(long = "upgrade-cdn", value_delimiter = ',')]
        upgrade_cdn: Vec<String>,

        /// 允许为没有官方 digest 来源的本地镜像生成升级任务（任务会标记
        /// `source: custom`，且不提供外部 CDN）。默认关闭——不批准就不下发。
        #[arg(long)]
        allow_custom_upgrade: bool,

        /// 当 agent 未声明 `variant:cli` / `variant:ui` 能力时，按此形态挑升级
        /// 制品。**未指定时这类 agent 会被拒绝升级**（409 `variant_unknown`）：
        /// 升级是原地替换二进制，猜错就是能力丢失（把 UI 设备换成 lean CLI），
        /// 所以不设默认值。lean CLI 二进制也会上报 `desktop:*`，不能用它推断
        /// 桌面形态。阶段 2 的 agent 会自带 `variant:*`，届时无需本参数。
        #[arg(long, value_enum)]
        upgrade_variant: Option<UpgradeVariantArg>,

        /// 官方 release API 根（默认 https://api.github.com）。仅用于镜像/测试。
        #[arg(long)]
        manifest_api_base: Option<String>,

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

        /// Reject every agent uplink (`/agent/events`, `/agent/ws/send`,
        /// `/agent/send`) that does not carry `X-SR-Agent-Token`.
        /// 默认关闭以兼容存量旧版 agent；全部 agent 升级到 v0.56.0+ 后
        /// 可开启。开启后旧版 agent 无法注册上行（会被 401 拒绝）。
        #[arg(long)]
        strict_agent_auth: bool,

        /// relay 整体下行（relay 出站）带宽上限，单位 Mbit/s。0 = 不限。
        /// 优先级：settings 文件里的同名字段 > 本参数 > 不限；Web 后台
        /// 修改后即时生效并写回 settings 文件。
        #[arg(long)]
        bandwidth_down_mbps: Option<f64>,

        /// relay 整体上行（relay 入站）带宽上限，单位 Mbit/s。0 = 不限。
        #[arg(long)]
        bandwidth_up_mbps: Option<f64>,

        /// 带宽设置的持久化文件（JSON）。默认
        /// `~/.shell-remote/relay-settings.json`；Web 后台每次修改都会原子
        /// 写回这里，重启后以此为准。
        #[arg(long)]
        settings_file: Option<String>,
    },

    /// Run in agent mode (connects to a relay; terminal forwarding only —
    /// desktop sharing lives in the `shell-remote-ui` binary)
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

        /// 关闭命令日志（默认会把 MCP 执行的命令与 Web 终端输入的命令行打印到日志）
        #[arg(long)]
        no_cmd_log: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    shell_remote::install_panic_hook();
    // `_log_guard` 必须活到 main 末尾（drop 会关闭 non-blocking writer）。
    let _log_guard = shell_remote::init_logging();

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
            strict_agent_auth,
            bandwidth_down_mbps,
            bandwidth_up_mbps,
            settings_file,
            upgrade_cdn,
            allow_custom_upgrade,
            upgrade_variant,
            manifest_api_base,
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
                strict_agent_auth,
                bandwidth_down_mbps,
                bandwidth_up_mbps,
                settings_file,
                relay::UpgradeSettings {
                    cdn_base_urls: upgrade_cdn,
                    allow_custom_upgrade,
                    upgrade_variant_override: upgrade_variant.map(Into::into),
                    manifest_api_base,
                },
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
            no_cmd_log,
        } => {
            // 命令日志开关：默认开启，`--no-cmd-log` 关闭。
            agent::cmd_log::set_enabled(!no_cmd_log);
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
            #[cfg(feature = "desktop")]
            {
                // CLI 二进制 agent 恒为终端转发：desktop 能力强制关闭。
                let desktop_cfg = shell_remote::agent::desktop::DesktopConfig {
                    capture: "none".to_string(),
                    ..Default::default()
                };
                agent::start(
                    relay_url,
                    key,
                    root,
                    token_type.as_str().to_string(),
                    shell,
                    desired,
                    desktop_cfg,
                    #[cfg(feature = "tui")]
                    None,
                    relay_insecure,
                )
                .await?;
            }
            #[cfg(not(feature = "desktop"))]
            {
                agent::start(
                    relay_url,
                    key,
                    root,
                    token_type.as_str().to_string(),
                    shell,
                    desired,
                    #[cfg(feature = "tui")]
                    None,
                    relay_insecure,
                )
                .await?;
            }
        }
    }

    Ok(())
}
