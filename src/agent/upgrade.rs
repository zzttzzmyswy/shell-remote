//! Agent-mode atomic self-upgrade.
//!
//! 升级分两步，边界刻意拆开：
//!
//! - [`download`](crate::agent::download)：**取字节**——多 CDN 顺序 + relay 兜底、
//!   Range 断点续传、代理/DoH 解析、有界预算。它不碰文件系统之外的东西，也不知道
//!   relay 协议；
//! - 本模块：**装字节**——校验制品身份 → 冒烟测试 → 原子替换 → 重启，以及把每个
//!   阶段转成 `agent:upgrade_progress` 上报。
//!
//! 不变式：**下载与验证全部成功之前，旧 agent 不动**。任何失败路径都只上报
//! `failed` 并返回，进程继续在线；只有确认新二进制能在这台机器上跑起来，才会
//! 替换并退出。
//!
//! 安装细节（沿用既有实现，阶段 2 不改换机语义）：
//!
//! 1. 校验 SHA-256 与制品身份（variant/arch/OS 必须就是本机这一份），
//! 2. unix `--version` 冒烟测试，坏包/错架构在这里被拦住，
//! 3. 原子 rename 覆盖（同目录同文件系统），
//! 4. 用原 argv 拉起新进程并退出。
//!
//! Windows 无法覆盖运行中的 `.exe`，沿用既有 `.bat` 助手路径（阶段 3 会用
//! `update-apply` 取代）。

use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc::Sender;

use crate::agent::doh::{DohConfig, RelayResolver};
use crate::agent::download::{
    self, DownloadBudget, Downloader, ProgressFn, ProxyConfig, RelaySource, UpgradeTask,
};
use crate::proto::Message;

/// 下载总预算：制品是单个二进制（几十 MB），弱网下 10 分钟足够，再久就该
/// 明确失败让状态机解锁，而不是无限挂着。
const DOWNLOAD_TOTAL_BUDGET: Duration = Duration::from_secs(600);
/// 首字节预算：包含建连 + TLS 握手 + 响应头。
const DOWNLOAD_FIRST_BYTE_BUDGET: Duration = Duration::from_secs(20);
/// 分块静默预算：弱网断流要尽快失败并续传。
const DOWNLOAD_CHUNK_BUDGET: Duration = Duration::from_secs(45);
/// 终结帧（`installing` / `failed`）发出后留给发送循环的时间：它是异步排空
/// 的，进程立刻退出会丢掉还没发出去的帧。
const EXIT_FLUSH_GRACE: Duration = Duration::from_millis(400);

/// 升级指令。阶段 2 起来自 relay 的完整任务（契约 1）；存量 relay 的三个老键
/// 仍由 [`UpgradeTask::from_payload`] 兼容解析。
pub use crate::agent::download::UpgradeTask as UpgradeRequest;

/// 升级所需的运行上下文。字段都显式传入而不读全局，便于测试与阶段 3 复用。
#[derive(Clone)]
pub struct UpgradeContext {
    /// 复用 relay 上行客户端（带 `--relay-insecure` 时能连自签 relay）。
    pub relay_http: reqwest::Client,
    /// relay 根地址（`http(s)://host:port`，无尾斜杠）。
    pub relay_base: String,
    /// rw Token：**只**用于 relay 镜像与代解析，绝不出现在外部 CDN 请求里。
    pub token: String,
    pub session_id: String,
    pub ctrl_tx: Sender<String>,
    /// 正在运行的可执行文件的规范路径（启动时解析好）。
    pub target: std::path::PathBuf,
    /// 本进程的制品形态；`None` 表示无法判定（不校验 variant）。
    pub own_variant: Option<download::AgentVariant>,
    /// 下载预算（测试可收紧）。
    pub budget: DownloadBudget,
    /// 代理配置（默认读环境）。
    pub proxy: ProxyConfig,
    /// DoH 配置（默认预置两家）。
    pub doh: DohConfig,
    /// 额外信任根（PEM）——仅测试注入本地 CA。
    pub extra_roots: Vec<Vec<u8>>,
    /// `--relay-insecure`：relay 用自签/过期证书时跳过校验。上行客户端按此
    /// 配置，**下载客户端也必须按此配置**——否则「CDN 全部不可用 → 走 relay」
    /// 这条最需要兜底的路径会在握手阶段必然失败。
    pub relay_insecure: bool,
}

impl std::fmt::Debug for UpgradeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Token 永不进日志。
        f.debug_struct("UpgradeContext")
            .field("relay_base", &self.relay_base)
            .field("token", &"<redacted>")
            .field("session_id", &self.session_id)
            .field("target", &self.target)
            .field("own_variant", &self.own_variant)
            .finish()
    }
}

impl UpgradeContext {
    /// 生产配置：默认预算 + 环境代理 + 预置 DoH。
    ///
    /// `relay_insecure` 必须与 relay 上行客户端同源（`--relay-insecure`）：
    /// 下载侧的 relay 兜底走同一个 relay，TLS 策略不一致会让兜底必然失败。
    pub fn new(
        relay_http: reqwest::Client,
        relay_base: String,
        token: String,
        session_id: String,
        ctrl_tx: Sender<String>,
        target: std::path::PathBuf,
        relay_insecure: bool,
    ) -> Self {
        Self {
            relay_http,
            relay_base,
            token,
            session_id,
            ctrl_tx,
            target,
            own_variant: download::AgentVariant::detect(),
            budget: DownloadBudget {
                connect: Duration::from_secs(10),
                first_byte: DOWNLOAD_FIRST_BYTE_BUDGET,
                chunk: DOWNLOAD_CHUNK_BUDGET,
                total: DOWNLOAD_TOTAL_BUDGET,
                attempts_per_source: 2,
                backoff: Duration::from_millis(800),
            },
            proxy: ProxyConfig::from_env(),
            doh: DohConfig::default(),
            extra_roots: Vec::new(),
            relay_insecure,
        }
    }
}

/// Result of installing the new binary.
struct InstallOutcome {
    /// True when the caller must spawn the new binary and exit. False only on
    /// Windows where the swap was deferred to a helper `.bat` (already
    /// spawned) — the caller should just exit so the bat can finish the swap.
    restart_now: bool,
}

/// 进度上报：把下载器的 [`DownloadProgress`] 转成 `agent:upgrade_progress` 帧。
fn progress_reporter(ctx: &UpgradeContext, version: String) -> ProgressFn {
    let ctrl = ctx.ctrl_tx.clone();
    let sid = ctx.session_id.clone();
    std::sync::Arc::new(move |p: download::DownloadProgress| {
        let ctrl = ctrl.clone();
        let sid = sid.clone();
        let version = version.clone();
        // 下载器是同步回调：把发送丢给运行时，不让它阻塞字节流。
        tokio::spawn(async move {
            let mut payload = json!({"stage": "downloading", "version": version});
            if let Some(pct) = p.percent {
                payload["percent"] = json!(pct);
            }
            payload["detail"] = json!(p.detail);
            send_frame(&ctrl, &sid, payload).await;
        });
    })
}

async fn send_frame(ctrl_tx: &Sender<String>, session_id: &str, payload: serde_json::Value) {
    let msg = Message {
        msg_type: "agent:upgrade_progress".to_string(),
        session_id: session_id.to_string(),
        payload,
    };
    if let Ok(s) = serde_json::to_string(&msg) {
        let _ = ctrl_tx.send(s).await;
    }
}

/// 推一个 `agent:upgrade_progress` 帧（admin 面板据此显示阶段/百分比）。
async fn send_progress(
    ctrl_tx: &Sender<String>,
    session_id: &str,
    stage: &str,
    percent: Option<u64>,
    detail: Option<String>,
    version: &str,
) {
    let mut payload = json!({ "stage": stage, "version": version });
    if let Some(p) = percent {
        payload["percent"] = json!(p);
    }
    if let Some(d) = detail {
        payload["detail"] = json!(d);
    }
    send_frame(ctrl_tx, session_id, payload).await;
}

/// Resolve the real path of the running executable (following symlinks, so a
/// `/usr/local/bin/shell-remote` symlink keeps pointing at the replaced file).
/// Production code resolves this once at startup instead of calling this here —
/// kept for the unit test and as the canonicalization reference.
#[allow(dead_code)]
fn current_exe_real() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe()?.canonicalize()
}

/// Hex-encoded SHA-256 of a file, streamed to bound memory use.
pub use crate::agent::download::sha256_hex_file;

/// Constant-time digest comparison (case-insensitive hex).
pub use crate::agent::download::verify_sha256;

/// Unix: run the artifact once (`--version`) to ensure it actually executes on
/// this platform before replacing the running binary. A wrong-arch or truncated
/// artifact fails here and the old agent stays untouched.
#[cfg(not(windows))]
async fn smoke_test(path: &std::path::Path) -> bool {
    let status = tokio::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    matches!(status, Ok(s) if s.success())
}

#[cfg(windows)]
async fn smoke_test(_path: &std::path::Path) -> bool {
    // Running the payload on Windows would briefly start a second agent; skip
    // the smoke test there — integrity is already covered by SHA-256.
    true
}

/// Atomically replace `target` with `tmp` (same directory).
#[cfg(not(windows))]
fn install_binary(
    tmp: &std::path::Path,
    target: &std::path::Path,
) -> std::io::Result<InstallOutcome> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(tmp, target)?;
    Ok(InstallOutcome { restart_now: true })
}

/// Windows: try a direct replace; if the running `.exe` blocks it (access
/// denied), defer the swap to a helper `.bat` that waits for this process to
/// exit, then moves the file into place and starts the new binary.
#[cfg(windows)]
fn install_binary(
    tmp: &std::path::Path,
    target: &std::path::Path,
) -> std::io::Result<InstallOutcome> {
    match std::fs::rename(tmp, target) {
        Ok(()) => Ok(InstallOutcome { restart_now: true }),
        Err(e)
            if e.kind() == std::io::ErrorKind::PermissionDenied
                || e.kind() == std::io::ErrorKind::Other =>
        {
            let exe_name = target
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("shell-remote.exe");
            let bat = target.with_extension("update.bat");
            let args = std::env::args()
                .skip(1)
                .map(|a| format!("\"{}\"", a))
                .collect::<Vec<_>>()
                .join(" ");
            let script = format!(
                "@echo off\r\n\
                 :wait\r\n\
                 tasklist /FI \"IMAGENAME eq {exe}\" | findstr /I \"{exe}\" >nul 2>nul\r\n\
                 if not errorlevel 1 (\r\n\
                 timeout /t 1 /nobreak >nul\r\n\
                 goto wait\r\n\
                 )\r\n\
                 move /y \"{tmp}\" \"{target}\" >nul\r\n\
                 del \"%~f0\"\r\n\
                 start \"\" \"{target}\" {args}\r\n",
                exe = exe_name,
                tmp = tmp.display(),
                target = target.display(),
                args = args,
            );
            std::fs::write(&bat, script)?;
            let _ = std::process::Command::new("cmd")
                .arg("/c")
                .arg(&bat)
                .spawn();
            Ok(InstallOutcome { restart_now: false })
        }
        Err(e) => Err(e),
    }
}

/// Spawn the (already-replaced) binary with the original argv, detached. On
/// unix the child gets its own process group so the parent's terminal logout
/// (SIGHUP to the old foreground group) cannot kill the upgraded agent.
fn restart_process(target: &std::path::Path) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(target);
    cmd.args(std::env::args().skip(1))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map(|_| ())
}

/// 建 relay 兜底下载用的客户端：**禁止重定向**，TLS 策略与 relay 上行一致。
///
/// 两个约束必须同时满足，缺一个都会在真实现场出问题：
///
/// - 禁重定向：这个客户端带 rw Token，跟一个 302 就等于把 Token 送到别的主机
///   （relay 的制品端点正常永远是 200/206，没有合法跨源跳转）。
/// - TLS 策略跟随 `--relay-insecure`：自签证书 relay 部署下，上行能连上而下载
///   兜底握手失败，等于把「CDN 全都不可用 → 回源 relay」这条最需要兜底的路径
///   整条掐掉，而常规 CDN 测试全绿也发现不了。
///
/// 不复用 `ctx.relay_http`：reqwest 的 `Client` 无法在已有实例上叠加重定向策略，
/// 也不暴露其 TLS 配置，所以这里按同一份语义重建，而不是只建一个裸 builder
/// （那样会丢掉 TLS 配置——正是本函数存在的原因）。
fn build_relay_download_client(ctx: &UpgradeContext) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // 与上行客户端同值：连不上的 relay 不该把整个下载预算耗在建连上。
        .connect_timeout(ctx.budget.connect)
        .tcp_keepalive(Duration::from_secs(30));
    if ctx.relay_insecure {
        crate::tlsutil::install_rustls_provider();
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("cannot build the relay download client: {e}"))
}

/// 构造下载器：外部客户端（无任何凭据）+ relay 客户端（禁重定向、TLS 同源的
/// 独立客户端）。
pub fn build_downloader(ctx: &UpgradeContext, task: &UpgradeTask) -> anyhow::Result<Downloader> {
    // 解析链：只允许本次任务的 CDN 主机；relay 代解析作为第三级。
    let resolver = if task.allows_cdn() {
        let relay = RelayResolver {
            http: ctx.relay_http.clone(),
            base: ctx.relay_base.clone(),
            token: ctx.token.clone(),
            timeout: crate::agent::doh::RELAY_RESOLVE_TIMEOUT,
        };
        let mut doh_cfg = ctx.doh.clone();
        doh_cfg.proxy = ctx.proxy.clone();
        doh_cfg.extra_roots = ctx.extra_roots.clone();
        Some(download::chain_resolver(
            task.cdn_hosts(),
            doh_cfg,
            Some(relay),
        ))
    } else {
        None
    };
    let external =
        Downloader::build_external_client(&ctx.proxy, resolver, &ctx.budget, task.cdn_hosts())?;
    let relay = match task.relay_url.as_ref() {
        // 建不出来就**整体失败**，绝不回退到会跟随重定向的客户端——那等于让
        // Token 处于可被 302 带走的状态。
        Some(_) => Some(RelaySource {
            http: build_relay_download_client(ctx)?,
            base: ctx.relay_base.clone(),
            token: ctx.token.clone(),
        }),
        None => None,
    };
    Ok(Downloader::new(
        external,
        relay,
        ctx.budget.clone(),
        progress_reporter(ctx, task.target_version.clone()),
    ))
}

/// Orchestrate one upgrade. Sends progress frames through `ctrl_tx`; on
/// success the process is replaced and this function never returns. On failure
/// it reports `failed` and returns so the agent keeps running unchanged.
pub async fn perform_upgrade(ctx: UpgradeContext, task: UpgradeTask) {
    let version = task.target_version.clone();
    send_progress(
        &ctx.ctrl_tx,
        &ctx.session_id,
        "checking",
        None,
        Some(format!(
            "{} ({} source(s))",
            task.filename,
            task.sources().len()
        )),
        &version,
    )
    .await;

    // 0. 制品身份：形态/架构/OS 必须就是本机这一份。这一步在联网前做，任务本身
    //    错了就没必要浪费流量，更不能把 UI 包写到 CLI 设备上。
    if let Err(e) = download::verify_artifact_identity(&task, ctx.own_variant) {
        report_failed(&ctx, &version, e).await;
        return;
    }

    let Some(dir) = ctx.target.parent().map(|d| d.to_path_buf()) else {
        report_failed(&ctx, &version, "cannot locate executable directory".into()).await;
        return;
    };
    // `.part` 与目标同目录：同一文件系统，最终 rename 才能原子。
    //
    // 文件名按**目标制品的 SHA-256 前缀**区分（而不是 pid）：断点续传要跨
    // 「这次下载失败 → admin 再点一次升级」也能接上，pid 命名会让每次重试都
    // 从头下几十 MB。hash 不同则是另一个制品，不能复用旧字节。
    let part = dir.join(format!(
        ".shell-remote-upgrade-{}.part",
        &task.sha256[..task.sha256.len().min(16)]
    ));
    // 清理历史命名遗留（旧版本用 pid 命名的临时文件），避免堆积。
    cleanup_stale_parts(&dir).await;

    // 1. 下载（多源 + 续传 + 预算，进度在下载器内部上报）。
    let downloaded = match build_downloader(&ctx, &task) {
        Ok(dl) => dl.fetch(&task, &part).await,
        Err(e) => Err(format!("cannot prepare the downloader: {e}")),
    };
    let downloaded = match downloaded {
        Ok(d) => d,
        Err(e) => {
            // 保留 `.part`：下次 admin 重试（或自动重试）能接着下。下载器自己
            // 在校验失败/脏数据时删它；这里留下的是"没下完"的部分字节。
            report_failed(&ctx, &version, format!("download failed: {e}")).await;
            return;
        }
    };

    // 2. 完整性已由下载器校验（长度 + SHA-256），这里只做一次落地复核，避免
    //    「校验后又被动过」的窗口。
    send_progress(
        &ctx.ctrl_tx,
        &ctx.session_id,
        "verifying",
        Some(100),
        Some(format!(
            "{} bytes from {} (sha256 ok)",
            downloaded.bytes, downloaded.source
        )),
        &version,
    )
    .await;
    let digest = match sha256_hex_file(&part) {
        Ok(d) => d,
        Err(e) => {
            report_failed_cleanup(
                &ctx,
                &version,
                &part,
                format!("cannot hash downloaded file: {e}"),
            )
            .await;
            return;
        }
    };
    if !verify_sha256(&digest, &task.sha256) {
        report_failed_cleanup(
            &ctx,
            &version,
            &part,
            format!(
                "SHA-256 mismatch after download: got {digest}, expected {}",
                task.sha256
            ),
        )
        .await;
        return;
    }
    if task.size > 0 {
        match tokio::fs::metadata(&part).await {
            Ok(m) if m.len() != task.size => {
                report_failed_cleanup(
                    &ctx,
                    &version,
                    &part,
                    format!("downloaded {} bytes, expected {}", m.len(), task.size),
                )
                .await;
                return;
            }
            Ok(_) => {}
            Err(e) => {
                report_failed_cleanup(&ctx, &version, &part, format!("cannot stat download: {e}"))
                    .await;
                return;
            }
        }
    }

    // 3. 确认这个 payload 能在这台机器上跑起来，再动正在运行的二进制。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)) {
            report_failed_cleanup(
                &ctx,
                &version,
                &part,
                format!("cannot mark artifact executable: {e}"),
            )
            .await;
            return;
        }
    }
    if !smoke_test(&part).await {
        report_failed_cleanup(
            &ctx,
            &version,
            &part,
            "new binary does not execute on this platform".into(),
        )
        .await;
        return;
    }

    // 4. 原子替换。
    let outcome = match install_binary(&part, &ctx.target) {
        Ok(o) => o,
        Err(e) => {
            report_failed_cleanup(
                &ctx,
                &version,
                &part,
                format!("cannot replace the running binary: {e}"),
            )
            .await;
            return;
        }
    };
    let _ = std::fs::remove_file(&part);

    // 5. 重启。给终结帧一点时间到 relay，再让这个进程（和它的发送循环）消失。
    send_progress(
        &ctx.ctrl_tx,
        &ctx.session_id,
        "installing",
        None,
        Some(format!("installed {} from {}", version, downloaded.source)),
        &version,
    )
    .await;
    tokio::time::sleep(EXIT_FLUSH_GRACE).await;
    if outcome.restart_now {
        if let Err(e) = restart_process(&ctx.target) {
            report_failed(
                &ctx,
                &version,
                format!("binary replaced but restart failed: {e}"),
            )
            .await;
            return;
        }
    }
    std::process::exit(0);
}

async fn report_failed_cleanup(
    ctx: &UpgradeContext,
    version: &str,
    tmp: &std::path::Path,
    error: String,
) {
    let _ = tokio::fs::remove_file(tmp).await;
    report_failed(ctx, version, error).await;
}

/// 清理可执行文件目录里过期的升级临时文件。
///
/// `.part` 是可复用的续传数据（按目标 hash 命名），但**别的** hash 的残片、
/// 以及旧版本用 pid 命名的残片都是垃圾：它们永远不会再被用上，留着只是占磁盘。
/// 只扫这一层目录、只删我们自己的命名前缀，不动用户文件。
async fn cleanup_stale_parts(dir: &std::path::Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(".shell-remote-upgrade-") || !name.ends_with(".part") {
            continue;
        }
        // 一天之前的残片视为无主：升级不会跨天等待，留着只占空间。
        let stale = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .map(|age| age > Duration::from_secs(24 * 60 * 60))
            .unwrap_or(false);
        if stale {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

async fn report_failed(ctx: &UpgradeContext, version: &str, error: String) {
    // 失败原因进日志与 admin 面板；旧 agent 继续在线，不做任何退出动作。
    tracing::warn!(version, error = %error, "agent self-upgrade failed; staying on the current binary");
    let mut payload = json!({ "stage": "failed", "version": version });
    payload["error"] = json!(error);
    send_frame(&ctx.ctrl_tx, &ctx.session_id, payload).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── relay 兜底的可达性（`--relay-insecure`）─────────────────────
    //
    // 这组用例针对一次真实回归：为满足「relay 下载禁止重定向」，兜底客户端
    // 曾改成裸 `Client::builder()...build()`，把上行客户端上的
    // `danger_accept_invalid_certs` 一并丢了。自签证书 relay 部署下 CDN 全挂
    // 之后回源必然握手失败——正是最需要兜底的场景，而常规 CDN 用例全绿也发现
    // 不了。两个用例成对存在：一个证明 TLS 放行真的生效，一个证明修 TLS 时
    // 没有把禁重定向改回去。

    /// 一个自签证书的 HTTPS relay mock，记录收到的请求（路径 + 是否带 Token）。
    struct MockRelay {
        port: u16,
        hits: std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>,
        _srv: tokio::task::JoinHandle<()>,
    }

    impl MockRelay {
        /// `redirect_to` 非空时，所有请求都回 302 到该地址（模拟被劫持的 relay）。
        async fn start(bytes: Vec<u8>, redirect_to: Option<String>) -> Self {
            use axum::response::{IntoResponse, Response};
            let pki = crate::agent::test_tls::make_pki("localhost");
            let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = hits.clone();
            let (port, _srv) = crate::agent::test_tls::spawn_tls_server(
                &pki,
                move |req: axum::extract::Request| {
                    let seen = seen.clone();
                    let bytes = bytes.clone();
                    let redirect_to = redirect_to.clone();
                    async move {
                        let token = req
                            .headers()
                            .get("x-sr-token")
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());
                        seen.lock()
                            .unwrap()
                            .push((req.uri().path().to_string(), token));
                        if let Some(target) = redirect_to {
                            return Response::builder()
                                .status(axum::http::StatusCode::FOUND)
                                .header("location", target)
                                .header("content-length", "0")
                                .body(axum::body::Body::empty())
                                .unwrap()
                                .into_response();
                        }
                        Response::builder()
                            .status(axum::http::StatusCode::OK)
                            .header("content-length", bytes.len().to_string())
                            .body(axum::body::Body::from(bytes))
                            .unwrap()
                            .into_response()
                    }
                },
            )
            .await;
            Self { port, hits, _srv }
        }

        fn base(&self) -> String {
            format!("https://localhost:{}", self.port)
        }

        fn hits(&self) -> Vec<(String, Option<String>)> {
            self.hits.lock().unwrap().clone()
        }
    }

    /// 制品名必须与测试机身份匹配，否则 `verify_artifact_identity` 先拦下来。
    fn local_artifact_name() -> String {
        format!(
            "shell-remote-{}{}",
            download::own_arch_key(),
            if cfg!(windows) { ".exe" } else { "" }
        )
    }

    /// 只有 relay 源的升级任务（CDN 全部不可用后的兜底形态）。
    fn relay_only_task(relay_path: &str, bytes: &[u8]) -> UpgradeTask {
        use sha2::Digest;
        UpgradeTask {
            upgrade_id: "up-relay".into(),
            target_version: "9.9.9".into(),
            filename: local_artifact_name(),
            size: bytes.len() as u64,
            sha256: hex::encode(sha2::Sha256::digest(bytes)),
            official_path: None,
            cdn_base_urls: Vec::new(),
            relay_url: Some(relay_path.to_string()),
            relay_fallback: true,
            source: "local".into(),
        }
    }

    fn relay_ctx(relay_base: String, insecure: bool) -> UpgradeContext {
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);
        let mut ctx = UpgradeContext::new(
            reqwest::Client::new(),
            relay_base,
            "rw-secret".into(),
            "sess".into(),
            tx,
            std::path::PathBuf::from("/nonexistent/shell-remote"),
            insecure,
        );
        ctx.budget = DownloadBudget {
            connect: Duration::from_secs(5),
            first_byte: Duration::from_secs(5),
            chunk: Duration::from_secs(5),
            total: Duration::from_secs(20),
            attempts_per_source: 1,
            backoff: Duration::from_millis(10),
        };
        // 自签 relay 的 CA 不注入信任库：`--relay-insecure` 是唯一的放行手段，
        // 否则这条用例会因为「恰好被 CA 信任」而假通过。
        ctx.proxy = ProxyConfig::empty();
        ctx
    }

    fn tmp_download(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sr-upg-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("artifact.part")
    }

    const RELAY_ARTIFACT: &[u8] = b"#!fake-binary served by a self-signed relay\n0123456789";

    /// `--relay-insecure` 下，自签证书 relay 的兜底下载必须成功；同一份上下文
    /// 去掉 insecure 后必须因证书校验失败而失败——后半段证明前者的成功确实来自
    /// TLS 放行，而不是「本来就信任这个证书」。
    #[tokio::test]
    async fn test_relay_fallback_downloads_from_self_signed_relay_when_insecure() {
        let relay = MockRelay::start(RELAY_ARTIFACT.to_vec(), None).await;
        let name = local_artifact_name();
        let task = relay_only_task(&format!("/agent/upgrade/blob/{name}"), RELAY_ARTIFACT);

        // insecure：必须成功。
        let ctx = relay_ctx(relay.base(), true);
        let part = tmp_download("relay-insecure");
        let got = build_downloader(&ctx, &task)
            .expect("insecure relay client must be constructible")
            .fetch(&task, &part)
            .await
            .unwrap_or_else(|e| panic!("--relay-insecure 下兜底下载必须成功: {e}"));
        assert_eq!(got.source, "relay");
        assert_eq!(std::fs::read(&part).unwrap(), RELAY_ARTIFACT);
        // 兜底路径仍是带 Token 的 relay 请求（Token 只在 relay 源出现）。
        let hits = relay.hits();
        assert_eq!(hits.len(), 1, "只应请求一次 relay: {hits:?}");
        assert_eq!(hits[0].0, format!("/agent/upgrade/blob/{name}"));
        assert_eq!(hits[0].1.as_deref(), Some("rw-secret"));
        let _ = std::fs::remove_dir_all(part.parent().unwrap());

        // 同一 mock、同一任务，仅关掉 insecure：必须因证书失败。
        let ctx = relay_ctx(relay.base(), false);
        let part = tmp_download("relay-secure");
        let err = build_downloader(&ctx, &task)
            .unwrap()
            .fetch(&task, &part)
            .await
            .expect_err("未信任的自签证书必须被拒绝");
        let lower = err.to_lowercase();
        assert!(
            lower.contains("certificate")
                || lower.contains("tls")
                || lower.contains("invalid peer")
                || lower.contains("handshake"),
            "失败原因必须是证书/握手，而不是别的问题: {err}"
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    /// 修 TLS 不能把「relay 请求禁止重定向」改回去：即使 insecure 放行了证书，
    /// 302 也不得被跟随（否则 rw Token 会被送到跳转目标）。
    #[tokio::test]
    async fn test_relay_download_never_follows_redirect_even_when_insecure() {
        // 跳转目标：一个明文 HTTP 收集器，记录收到的请求头。
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = hits.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(format!(
                    "{} token={:?}",
                    req.uri().path(),
                    req.headers()
                        .get("x-sr-token")
                        .and_then(|v| v.to_str().ok())
                ));
                axum::response::Response::new(axum::body::Body::from("stolen"))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sink = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let relay = MockRelay::start(
            RELAY_ARTIFACT.to_vec(),
            Some(format!("{sink}/stolen-artifact")),
        )
        .await;
        let name = local_artifact_name();
        let task = relay_only_task(&format!("/agent/upgrade/blob/{name}"), RELAY_ARTIFACT);
        let ctx = relay_ctx(relay.base(), true);
        let part = tmp_download("relay-noredir");
        let err = build_downloader(&ctx, &task)
            .unwrap()
            .fetch(&task, &part)
            .await
            .expect_err("302 不得被跟随（Token 会外泄）");
        // 核心断言先跑：跳转目标一次都没被请求过，Token 自然也没出去。放在
        // 「错误文案」断言之前，因为跟随了重定向的实现可能在下游以别的形式失败
        // （长度不符、hash 不符），那种失败仍然意味着 Token 已经泄漏了。
        assert!(
            hits.lock().unwrap().is_empty(),
            "重定向目标不得收到任何请求（Token 会随之泄漏）: {:?}",
            hits.lock().unwrap()
        );
        assert!(
            err.contains("302") || err.contains("FOUND") || err.contains("redirect"),
            "失败应是 302 未跟随，而不是别的错误: {err}"
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[test]
    fn test_sha256_hex_file_known_vector() {
        let dir = std::env::temp_dir().join(format!("sr-upg-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("data.bin");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_hex_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_verify_sha256() {
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_sha256(good, good));
        assert!(verify_sha256(&good.to_uppercase(), good)); // hex case-insensitive
        assert!(!verify_sha256(&format!("f{}", &good[1..]), good));
        assert!(!verify_sha256(good, "ba7816bf"));
    }

    #[test]
    fn test_upgrade_request_from_payload() {
        let req = UpgradeRequest::from_payload(&json!({
            "version": "0.19.0",
            "url": "/agent/upgrade/blob/shell-remote-x86_64",
            "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        }))
        .unwrap();
        assert_eq!(req.target_version, "0.19.0");
        assert_eq!(
            req.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(UpgradeRequest::from_payload(&json!({"version": "1"})).is_none());
    }

    #[test]
    fn test_context_never_prints_token() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(1);
        let ctx = UpgradeContext::new(
            reqwest::Client::new(),
            "https://relay.test".into(),
            "rw-super-secret".into(),
            "sess".into(),
            tx,
            std::path::PathBuf::from("/usr/local/bin/shell-remote"),
            false,
        );
        let rendered = format!("{ctx:?}");
        assert!(
            !rendered.contains("rw-super-secret"),
            "Token 不得进入 Debug 输出: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("https://relay.test"));
    }

    #[cfg(unix)]
    #[test]
    fn test_install_binary_atomic_replace() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sr-upg-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("new");
        let target = dir.join("running");
        std::fs::write(&tmp, b"new-content").unwrap();
        std::fs::write(&target, b"old-content").unwrap();
        let outcome = install_binary(&tmp, &target).unwrap();
        assert!(outcome.restart_now);
        assert_eq!(std::fs::read(&target).unwrap(), b"new-content");
        assert!(!tmp.exists());
        // executable bit preserved for the swapped file
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "installed binary must stay executable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_install_binary_missing_tmp_fails() {
        let dir = std::env::temp_dir().join(format!("sr-upg-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("missing");
        let target = dir.join("running");
        std::fs::write(&target, b"old").unwrap();
        assert!(install_binary(&tmp, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_smoke_test_rejects_non_executable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sr-upg-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("fake-bin");
        std::fs::write(&f, b"this is not an executable").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!smoke_test(&f).await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_smoke_test_accepts_working_script() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sr-upg-smoke-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("fake-bin");
        std::fs::write(&f, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(smoke_test(&f).await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_current_exe_real_points_at_something() {
        let p = current_exe_real().unwrap();
        assert!(p.is_absolute());
        assert!(p.exists());
    }

    #[tokio::test]
    async fn test_cleanup_stale_parts_only_touches_our_old_files() {
        let dir = std::env::temp_dir().join(format!("sr-upg-parts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fresh = dir.join(".shell-remote-upgrade-aaa.part");
        let old = dir.join(".shell-remote-upgrade-bbb.part");
        let unrelated = dir.join("shell-remote");
        let other_tmp = dir.join("notes.txt");
        for p in [&fresh, &old, &unrelated, &other_tmp] {
            std::fs::write(p, b"x").unwrap();
        }
        // 把 old 的 mtime 推到 2 天前（= 无主残片）。
        let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
        set_mtime(&old, two_days_ago);

        cleanup_stale_parts(&dir).await;

        assert!(fresh.exists(), "刚写的续传残片必须保留");
        assert!(!old.exists(), "过期残片应被清理");
        assert!(unrelated.exists(), "运行中的二进制绝不能被删");
        assert!(other_tmp.exists(), "无关文件绝不能被删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 把文件 mtime 设为指定时间（`std::fs::File::set_modified` 稳定可用）。
    fn set_mtime(path: &std::path::Path, t: std::time::SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }
}
