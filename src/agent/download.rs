//! Agent 端制品下载器：CDN 优先、多源切换、断点续传、有界预算（MYS-2425 阶段2）。
//!
//! 获取顺序（父 issue 契约 1/3）：
//!
//! 1. 任务里的每个 `cdn_base_urls[i] + official_path`，按 relay 给出的顺序；
//! 2. 仅当任务带 `relay_url` 时最后走 relay 镜像（沿用 rw Token）。
//!
//! 外部 CDN 请求**绝不**携带 relay Token——它们是第三方主机，Token 泄露等于
//! 把整个会话交出去。两类请求因此用两个独立客户端：外部客户端只配置代理/解析，
//! relay 客户端才附加 `X-SR-Token`。
//!
//! 每个源都有自己的预算：连接、首字节、分块静默、总时间；一个源失败就切下一个，
//! 全程有总时间上限与指数退避，不会无限阻塞升级状态机。
//!
//! 断点续传：字节写进 `.part`，重试与换源都带 `Range: bytes=<已有长度>-`。服务端
//! 回 206 时校验 `Content-Range` 起点与总长；回 200 表示它不支持 Range，必须**从头
//! 覆盖**而不是往后追加（否则拼出来的文件必然校验失败）。416 说明 `.part` 已不
//! 小于目标，丢弃重来。
//!
//! 最终只有「长度 + SHA-256 + 制品身份（variant/arch/OS）」同时通过才算下载成功；
//! 任何一项不符即失败，旧 agent 不动。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt;

use crate::agent::doh::url_host;
use crate::relay::artifact_store::parse_artifact_ref;

// ── 注册能力（relay 据此决定能否下发 CDN-only 任务 / 该发哪个形态的制品）──

/// 声明本 agent 理解 CDN 优先的升级任务协议（契约 1 全字段）。
pub const CAPABILITY_UPGRADE_V2: &str = "upgrade:v2";
/// 声明本二进制的形态：lean CLI。
pub const CAPABILITY_VARIANT_CLI: &str = "variant:cli";
/// 声明本二进制的形态：桌面 UI。
pub const CAPABILITY_VARIANT_UI: &str = "variant:ui";

/// 本进程的制品形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentVariant {
    Cli,
    Ui,
}

impl AgentVariant {
    pub fn capability(self) -> &'static str {
        match self {
            Self::Cli => CAPABILITY_VARIANT_CLI,
            Self::Ui => CAPABILITY_VARIANT_UI,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Ui => "ui",
        }
    }

    /// 从可执行文件名判定形态（发布产物就是 `shell-remote-<arch>` /
    /// `shell-remote-ui-<arch>`，名字即身份）。
    ///
    /// 为什么不用 `cfg!(feature = "desktop")`：默认 feature 包含 desktop，用
    /// 默认 feature 构建的 CLI 二进制会被误判成 UI——那正是「升级时把 CLI 设备
    /// 换成 30MB 桌面包」的事故路径。lib 里也拿不到 `CARGO_BIN_NAME`（只有 bin
    /// crate 可见），而 `src/main.rs` / `src/bin/shell-remote-ui.rs` 在本阶段禁改。
    /// 因此以文件名为准；名字不认识时**不猜**：退回编译期 feature 只在能明确
    /// 排除桌面代码时用（lean 构建），否则返回 `None`（不声明 `variant:*`，让
    /// relay 按其契约 409 拒绝，而不是让 agent 冒充某个形态）。
    pub fn from_exe_name(name: &str) -> Option<Self> {
        let stem = name.trim_end_matches(".exe").to_ascii_lowercase();
        // UI 名必须先判：`shell-remote-ui-*` 也以 `shell-remote` 开头。
        if stem.contains("shell-remote-ui") {
            return Some(Self::Ui);
        }
        // CLI 只认**确切**的发布名（`shell-remote` 或 `shell-remote-<arch>`）。
        // 放宽成前缀匹配会把「被改名的 UI 二进制」判成 CLI，进而被换成 lean
        // 包——那是静默的能力丢失，比拒绝升级糟得多。
        if stem == "shell-remote" {
            return Some(Self::Cli);
        }
        if let Some(arch) = stem.strip_prefix("shell-remote-") {
            if matches!(arch, "x86_64" | "aarch64" | "armv7" | "amd64" | "arm64") {
                return Some(Self::Cli);
            }
        }
        // 名字被改过：只有确定不含桌面代码时才敢说自己是 CLI（lean 构建根本
        // 产不出 UI 形态）。含桌面代码的未知名字无法区分两个产物 → 不声明，
        // 让 relay 按其契约 409，由运维显式配置。
        if cfg!(not(feature = "desktop")) {
            return Some(Self::Cli);
        }
        None
    }

    /// 当前进程的形态；`None` = 无法确定（不声明 `variant:*`）。
    pub fn detect() -> Option<Self> {
        let exe = std::env::current_exe().ok();
        let name = exe
            .as_deref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        match name {
            Some(n) => Self::from_exe_name(&n),
            None => Self::from_exe_name(""),
        }
    }
}

/// 本机发布架构 key（与 relay 的制品命名一致）。
pub const fn own_arch_key() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "x86_64"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "aarch64"
    }
    #[cfg(target_arch = "arm")]
    {
        "armv7"
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "arm")))]
    {
        "unknown"
    }
}

// ── 代理配置 ────────────────────────────────────────────────────────

/// 显式代理配置：从标准环境变量读取，可注入（测试）。
///
/// reqwest 默认就会读这些变量（`auto_sys_proxy`），但那依赖 reqwest/hyper-util
/// 的内部实现与 feature 集；这里把语义显式化，让「relay 内网地址按 NO_PROXY
/// 直连」有可测的边界。配置为空时保持 reqwest 的默认环境探测不变。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub no_proxy: Option<String>,
}

fn env_nonempty(keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Ok(v) = std::env::var(k) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

impl ProxyConfig {
    /// 不配置任何代理，也**不**读环境（测试与「强制直连」用）。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 读标准环境变量（`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`，
    /// 同时接受小写）。
    pub fn from_env() -> Self {
        Self {
            http_proxy: env_nonempty(&["HTTP_PROXY", "http_proxy"]),
            https_proxy: env_nonempty(&["HTTPS_PROXY", "https_proxy"]),
            all_proxy: env_nonempty(&["ALL_PROXY", "all_proxy"]),
            no_proxy: env_nonempty(&["NO_PROXY", "no_proxy"]),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.http_proxy.is_none() && self.https_proxy.is_none() && self.all_proxy.is_none()
    }

    /// 把配置应用到 reqwest builder。非空时**显式**注册代理（这会关掉 reqwest
    /// 的自动系统代理解析，避免同一份配置被读两遍）；`NO_PROXY` 一律生效，
    /// relay 的内网地址因此可以直连。
    pub fn apply(&self, mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        if self.is_empty() {
            return builder;
        }
        let no_proxy = self
            .no_proxy
            .as_deref()
            .and_then(reqwest::NoProxy::from_string);
        // 顺序有意义：reqwest 取第一个命中的代理规则，具体协议先于 ALL_PROXY。
        for (target, url) in [
            (ProxyTarget::Https, self.https_proxy.as_deref()),
            (ProxyTarget::Http, self.http_proxy.as_deref()),
            (ProxyTarget::All, self.all_proxy.as_deref()),
        ] {
            let Some(url) = url.filter(|u| !u.trim().is_empty()) else {
                continue;
            };
            let proxy = match target {
                ProxyTarget::Https => reqwest::Proxy::https(url),
                ProxyTarget::Http => reqwest::Proxy::http(url),
                ProxyTarget::All => reqwest::Proxy::all(url),
            };
            match proxy {
                Ok(p) => builder = builder.proxy(p.no_proxy(no_proxy.clone())),
                Err(e) => {
                    // 配置错误不能静默变成直连（那会让"以为走了代理"的部署暴露直连）。
                    tracing::warn!(proxy = %url, error = %e, "invalid proxy URL ignored");
                }
            }
        }
        builder
    }

    /// 某个主机是否应直连（命中 `NO_PROXY`）。
    ///
    /// reqwest 的 `NoProxy` 不公开匹配方法，这里按同一套语义自实现一份可测的
    /// 判定：`*` 全匹配，`a.example` / `.a.example` 匹配自身及子域，IPv4/主机名
    /// 精确匹配。只用于日志与测试断言，真正的代理绕行仍由 reqwest 执行。
    pub fn bypasses(&self, host: &str) -> bool {
        let Some(no_proxy) = self.no_proxy.as_deref() else {
            return false;
        };
        let host = host
            .trim()
            .trim_matches('[')
            .trim_matches(']')
            .to_ascii_lowercase();
        no_proxy.split(',').any(|entry| {
            let entry = entry.trim().to_ascii_lowercase();
            if entry.is_empty() {
                return false;
            }
            if entry == "*" {
                return true;
            }
            // 去掉可选端口（`host:port` 写法）。
            let entry = entry.split(':').next().unwrap_or("").to_string();
            match entry.strip_prefix('.') {
                Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
                None => host == entry || host.ends_with(&format!(".{entry}")),
            }
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum ProxyTarget {
    Http,
    Https,
    All,
}

// ── 下载预算 ────────────────────────────────────────────────────────

/// 每个源的下载预算。全部有界：宁可升级失败，也不能让状态机卡在 downloading。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadBudget {
    /// 建连超时。
    pub connect: Duration,
    /// 首字节（含 TLS 握手 + 响应头）超时。
    pub first_byte: Duration,
    /// 分块之间的静默上限——弱网断流必须尽快失败并换源/续传。
    pub chunk: Duration,
    /// 整个 `fetch` 的总时间上限（跨全部源与重试）。
    pub total: Duration,
    /// 单个源的最大尝试次数（含首次）。
    pub attempts_per_source: usize,
    /// 首次重试前的退避，之后按 2 倍增长。
    pub backoff: Duration,
}

impl Default for DownloadBudget {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            first_byte: Duration::from_secs(15),
            chunk: Duration::from_secs(30),
            total: Duration::from_secs(600),
            attempts_per_source: 2,
            backoff: Duration::from_millis(500),
        }
    }
}

/// 制品大小硬上限（防止恶意/错配镜像把磁盘写满）。
pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

// ── 任务（relay 下发，契约 1）──────────────────────────────────────

/// relay 下发的升级任务。字段与 `relay::artifact_store::UpgradeTask` 对应，
/// 另兼容存量 relay 的三个老键（`version` / `url` / `sha256`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeTask {
    pub upgrade_id: String,
    pub target_version: String,
    pub filename: String,
    /// 期望字节数；0 表示 relay 未提供（只有 SHA-256 可校验）。
    pub size: u64,
    /// 小写 hex SHA-256。
    pub sha256: String,
    /// 官方 CDN 上的相对路径（custom 制品没有）。
    pub official_path: Option<String>,
    pub cdn_base_urls: Vec<String>,
    /// relay 镜像路径（存在才能回退）。
    pub relay_url: Option<String>,
    pub relay_fallback: bool,
    /// `official` / `local` / `custom`；老 relay 无此字段 → 按 official 处理
    /// （只影响是否允许走 CDN，见 [`UpgradeTask::sources`]）。
    pub source: String,
}

impl UpgradeTask {
    /// 解析 `agent:upgrade.payload`。缺失关键字段 → `None`（调用方回错误帧）。
    ///
    /// 兼容存量 relay：只有 `version` / `url` / `sha256` 时也能升级，此时
    /// `relay_url` 取 `url`、没有 CDN 源——老 relay 的语义就是"只从 relay 下载"。
    pub fn from_payload(p: &serde_json::Value) -> Option<Self> {
        let sha256 = p.get("sha256")?.as_str()?.trim().to_ascii_lowercase();
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let target_version = p
            .get("target_version")
            .or_else(|| p.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if target_version.is_empty() {
            return None;
        }
        let relay_url = p
            .get("relay_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| p.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()));
        // relay 镜像路径**必须同源且是唯一允许的端点**：它会被拼到 relay base 上
        // 并附带 rw Token，任何能让它指向别的 host 的写法（`//evil`、绝对 URL、
        // `..`、反斜杠、query/fragment）都等于把 Token 送给第三方。
        //
        // 老 relay 的 `url` 走同一条校验：兼容不等于放宽。
        let relay_url = match relay_url {
            Some(raw) => match validate_relay_path(&raw) {
                Ok(path) => Some(path),
                Err(e) => {
                    tracing::warn!(relay_url = %raw, error = %e, "rejecting unsafe relay download path");
                    return None;
                }
            },
            None => None,
        };
        let cdn_base_urls: Vec<String> = p
            .get("cdn_base_urls")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    // 不 trim：带空白的 base 是畸形输入，交给 `normalize_cdn_base`
                    // 拒绝，而不是静默修剪成"看起来合法"。
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let official_path = p
            .get("official_path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let filename = p
            .get("filename")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                // 老 relay 不带 filename：从 relay_url 的最后一段推断
                // （`/agent/upgrade/blob/shell-remote-x86_64`）。
                relay_url
                    .as_deref()
                    .and_then(|u| u.rsplit('/').find(|s| !s.is_empty()))
                    .map(|s| s.to_string())
            })?;
        // 官方路径必须结构与「本任务制品」都对得上：CDN 会照着它取字节，配错
        // 就是把另一个制品（甚至任意路径）拉到这台机器上。校验失败即整条任务
        // 拒绝——不静默降级成「只用 relay」，那会让管理员以为 CDN 在生效。
        let official_path = match official_path {
            Some(raw) => match validate_official_path(&raw, &filename) {
                Ok(path) => Some(path),
                Err(e) => {
                    tracing::warn!(official_path = %raw, error = %e, "rejecting unsafe official_path");
                    return None;
                }
            },
            None => None,
        };
        let size = p.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let source = p
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("official")
            .to_ascii_lowercase();
        Some(Self {
            upgrade_id: p
                .get("upgrade_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            target_version,
            filename,
            size,
            sha256,
            official_path,
            cdn_base_urls,
            relay_fallback: p
                .get("relay_fallback")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            relay_url,
            source,
        })
    }

    /// 该走外部 CDN 吗？只有官方来源（official/local）才有官方路径；custom
    /// 制品按契约只能走 relay 镜像，绝不接受 CDN。
    pub fn allows_cdn(&self) -> bool {
        self.source != "custom" && self.official_path.is_some() && !self.cdn_base_urls.is_empty()
    }

    /// 这次下载允许解析的主机白名单（本任务**合法** CDN base 的主机）。
    ///
    /// 只收通过 [`normalize_cdn_base`] 的 base：非法条目本来就不会被下载，让它
    /// 进入 DNS 白名单只会平白扩大可解析面。
    pub fn cdn_hosts(&self) -> Vec<String> {
        if !self.allows_cdn() {
            return Vec::new();
        }
        let mut hosts: Vec<String> = Vec::new();
        for base in &self.cdn_base_urls {
            let Ok(parsed) = normalize_cdn_base(base) else {
                continue;
            };
            if let Some(host) = parsed.host_str() {
                let host = host.to_ascii_lowercase();
                if !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
        }
        hosts
    }

    /// 按契约排出下载源：CDN 依次，relay 最后。
    pub fn sources(&self) -> Vec<Source> {
        let mut out: Vec<Source> = Vec::new();
        if self.allows_cdn() {
            let path = self.official_path.as_deref().unwrap_or("");
            for base in &self.cdn_base_urls {
                match join_cdn_url(base, path) {
                    Some(url) => out.push(Source {
                        kind: SourceKind::Cdn,
                        label: format!("cdn:{}", url_host(&url).unwrap_or_else(|| base.clone())),
                        url,
                    }),
                    None => tracing::warn!(base = %base, "skipping malformed CDN base url"),
                }
            }
        }
        // relay 镜像只在任务明确带路径时启用（`relay_url` 为空 = relay 没有这份
        // 制品，硬拼一个 URL 只会 404）。路径已由 `from_payload` 按
        // [`validate_relay_path`] 校验过；这里再校验一次，因为 `UpgradeTask` 也
        // 可能被测试或别的调用方直接构造。
        if let Some(relay_url) = &self.relay_url {
            match validate_relay_path(relay_url) {
                Ok(path) => out.push(Source {
                    kind: SourceKind::Relay,
                    label: "relay".to_string(),
                    url: path,
                }),
                Err(e) => {
                    tracing::warn!(relay_url = %relay_url, error = %e, "refusing unsafe relay download path");
                }
            }
        }
        out
    }
}

/// 制品身份校验：任务里的文件名必须就是「本机 × 本形态」的发布产物。
///
/// 这是「UI/CLI 制品不串包」的最后一道闸：换错形态等于原地把设备能力换掉，
/// 长度与 hash 都对也可能装错。
///
/// **本机形态未知时必须拒绝，不能放行。** 「不知道自己是 CLI 还是 UI」不等于
/// 「任何形态都行」：升级是原地替换二进制，把 UI 设备换成 lean CLI 会静默丢掉
/// 桌面能力。这与阶段 1 relay 侧「形态未知回 409、不猜」是同一条原则——两端都不
/// 猜，才不会有任何一端替另一端做决定。此时给出可操作的提示（显式配置或人工
/// 处理），而不是默默继续。
pub fn verify_artifact_identity(
    task: &UpgradeTask,
    own_variant: Option<AgentVariant>,
) -> Result<(), String> {
    let Some(parsed) = parse_artifact_ref(&task.filename) else {
        return Err(format!(
            "upgrade task filename {:?} is not a shell-remote release artifact",
            task.filename
        ));
    };
    let own_arch = own_arch_key();
    if parsed.arch != own_arch {
        return Err(format!(
            "upgrade artifact {:?} targets arch {}, this agent is {own_arch}",
            task.filename, parsed.arch
        ));
    }
    let own_windows = cfg!(windows);
    if parsed.windows != own_windows {
        return Err(format!(
            "upgrade artifact {:?} targets {} but this agent runs on {}",
            task.filename,
            if parsed.windows { "windows" } else { "unix" },
            if own_windows { "windows" } else { "unix" }
        ));
    }
    let Some(own) = own_variant else {
        // 名字被改过 / 无法判定形态。拒绝并说明怎么继续：把它换回发布名重启
        // （最常见情形），或人工升级。绝不"按保守值猜一个"。
        return Err(format!(
            "cannot determine this agent's own artifact variant (CLI or UI) from the executable \
             name {:?}, and the task offers {:?}; refusing to install it — a wrong guess would \
             swap a desktop agent for the lean CLI build (or vice versa). Run this agent under \
             its release filename (shell-remote-<arch> for CLI, shell-remote-ui-<arch> for UI) \
             and retry, or upgrade this host manually.",
            std::env::current_exe()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "<unknown>".to_string()),
            task.filename
        ));
    };
    let want = own.as_str();
    let got = match parsed.variant {
        crate::relay::artifact_store::Variant::Cli => "cli",
        crate::relay::artifact_store::Variant::Ui => "ui",
    };
    if got != want {
        return Err(format!(
            "upgrade artifact {:?} is the {got} build but this agent is {want}",
            task.filename
        ));
    }
    Ok(())
}

/// 一个下载源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub kind: SourceKind,
    /// 外部源：完整绝对 URL；relay 源：relay 相对路径。
    pub url: String,
    /// 日志/进度里用的名字（不含 Token）。
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Cdn,
    Relay,
}

/// relay 镜像路径的固定前缀。只有 `relay` 自己的鉴权下载端点能带 Token。
pub const RELAY_BLOB_PREFIX: &str = "/agent/upgrade/blob/";

/// 校验 relay 下发的镜像路径，返回规范化后的同源相对路径。
///
/// **必须同源**：这个路径会被拼到 relay base 上，并附带 `X-SR-Token`。任何能
/// 让它指向别的 host 的写法都等于把 rw Token 送给第三方：
///
/// - `//evil.example/x` —— 协议相对 URL，拼成 `https://relay + //evil…` 后
///   `Url::join` 语义会把它变成 `https://evil.example/x`（即使字符串拼接只是
///   变成 `https://relay//evil.example/x`，路径规范化也可能改写 origin）；
/// - `https://evil.example/x`、`http://…` —— 绝对 URL；
/// - `/agent/upgrade/blob/../../etc/passwd`、反斜杠、`%2e%2e` 编码；
/// - `?query` / `#fragment` —— 改变实际请求目标；
/// - 任何非 `/agent/upgrade/blob/<合法制品名>` 的路径。
///
/// 允许的**唯一**形态：以单个 `/` 开头，紧跟 `RELAY_BLOB_PREFIX`，其后是一个
/// 合法发布制品名（`shell-remote[-ui]-<arch>[.exe]`）。返回原字符串（已确认无
/// 需改写）。
pub fn validate_relay_path(raw: &str) -> Result<String, String> {
    // 不 trim：带空白/控制字符的路径不是"合法路径的别名"，静默修剪只会让畸形
    // 输入看起来通过。
    let path = raw;
    if path.is_empty() {
        return Err("relay download path is empty".to_string());
    }
    if path.starts_with("//") {
        return Err(format!(
            "relay download path {path:?} is protocol-relative (//) and would leave the relay origin"
        ));
    }
    if !path.starts_with('/') {
        return Err(format!(
            "relay download path {path:?} must be an absolute path on the relay (starting with a single '/')"
        ));
    }
    // 绝对 URL 走私（`/https://evil` 不会命中，但 `\` 与空格会改变解析）。
    if path.contains('\\') || path.contains(' ') || path.chars().any(|c| c.is_control()) {
        return Err(format!(
            "relay download path {path:?} contains whitespace, backslashes or control characters"
        ));
    }
    if path.contains('?') || path.contains('#') {
        return Err(format!(
            "relay download path {path:?} must not carry a query or fragment"
        ));
    }
    // `..` 与百分号编码都能让路径语义偏离字面（`%2e%2e`、`%2f`）。relay 生成的
    // 路径里不需要任何转义，出现即拒绝。
    if path.contains("..") {
        return Err(format!(
            "relay download path {path:?} must not contain '..'"
        ));
    }
    if path.contains('%') {
        return Err(format!(
            "relay download path {path:?} must not use percent-encoding"
        ));
    }
    let Some(name) = path.strip_prefix(RELAY_BLOB_PREFIX) else {
        return Err(format!(
            "relay download path {path:?} is not {RELAY_BLOB_PREFIX}<artifact>"
        ));
    };
    if name.is_empty() || name.contains('/') {
        return Err(format!(
            "relay download path {path:?} must name a single artifact directly under {RELAY_BLOB_PREFIX}"
        ));
    }
    if parse_artifact_ref(name).is_none() {
        return Err(format!(
            "relay download path {path:?} does not end in a shell-remote release artifact name"
        ));
    }
    Ok(path.to_string())
}

/// CDN base 的规范化形态：`scheme://host[:port]`，无 userinfo / path / query /
/// fragment。
///
/// 用标准 URL parser 而不是字符串前缀判断——后者放行
/// `https://user:pass@cdn.test/?x#y` 这类会让请求落到奇怪目标、并把凭据写进日志
/// 的写法。
pub fn normalize_cdn_base(raw: &str) -> Result<url::Url, String> {
    // 不 trim：`url::Url::parse` 会静默吃掉前后空白，让畸形条目"看起来合法"。
    // 带空白的 base 是配置错误，明确报出来比悄悄修好更安全。
    if raw.trim() != raw {
        return Err(format!("CDN base {raw:?} has leading/trailing whitespace"));
    }
    let parsed =
        url::Url::parse(raw).map_err(|e| format!("CDN base {raw:?} is not a valid URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "CDN base {raw:?} uses scheme {other:?}; only http/https are accepted"
            ))
        }
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "CDN base {raw:?} must not carry userinfo (credentials in a mirror URL)"
        ));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(format!("CDN base {raw:?} has no host"));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(format!(
            "CDN base {raw:?} must not carry a query or fragment (it is an origin prefix)"
        ));
    }
    // 只接受裸 origin：`/` 是唯一可接受的 path。镜像约定是 base + official_path。
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return Err(format!(
            "CDN base {raw:?} must be a bare origin without a path (got {:?})",
            parsed.path()
        ));
    }
    Ok(parsed)
}

/// 校验 `official_path`：必须就是官方 release 的下载路径。
///
/// 形态固定为 `/<owner>/<repo>/releases/download/v<version>/<filename>`（stage 1
/// 的 `ArtifactRef::official_path` 就长这样）。只做结构校验 + 与任务文件名交叉
/// 核对，不接受 query/fragment/用户信息/穿越，也不接受百分号编码。
pub fn validate_official_path(raw: &str, filename: &str) -> Result<String, String> {
    // 不 trim：见 `validate_relay_path`。
    let path = raw;
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(format!(
            "official_path {raw:?} must be an absolute path with a single leading '/'"
        ));
    }
    if path.contains('\\') || path.contains("..") || path.contains('%') {
        return Err(format!(
            "official_path {raw:?} must not contain backslashes, '..' or percent-encoding"
        ));
    }
    if path.contains('?') || path.contains('#') {
        return Err(format!(
            "official_path {raw:?} must not carry a query or fragment"
        ));
    }
    if path.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!(
            "official_path {raw:?} contains whitespace or control characters"
        ));
    }
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    if segments.len() != 6 || segments[2] != "releases" || segments[3] != "download" {
        return Err(format!(
            "official_path {raw:?} is not <owner>/<repo>/releases/download/v<version>/<file>"
        ));
    }
    if segments[0].is_empty() || segments[1].is_empty() {
        return Err(format!(
            "official_path {raw:?} has an empty owner/repo segment"
        ));
    }
    // 版本段必须带 `v` 前缀，且不含路径元字符（已由上面的检查兜住）。
    if !segments[4].starts_with('v') || segments[4].len() < 2 {
        return Err(format!(
            "official_path {raw:?} version segment {:?} must look like v<version>",
            segments[4]
        ));
    }
    // 最后一段必须就是本任务的制品名——否则 CDN 会拿到另一个制品（例如把 UI
    // 的路径配在 CLI 任务上）。
    if segments[5] != filename {
        return Err(format!(
            "official_path {raw:?} points at {:?} but the task artifact is {filename:?}",
            segments[5]
        ));
    }
    if parse_artifact_ref(segments[5]).is_none() {
        return Err(format!(
            "official_path {raw:?} does not end in a shell-remote release artifact name"
        ));
    }
    Ok(path.to_string())
}

/// `base + official_path`，两段都按 URL 语法校验（不接受 userinfo、query、
/// fragment、`..`、反斜杠、控制字符）。拼不出合法 URL 就跳过该源。
///
/// 返回值仍带 origin 校验：拼好后再确认 host 与 base 一致——万一 `Url::join`
/// 的路径规范化把目标改到别的 host，这里会拒绝。
pub fn join_cdn_url(base: &str, official_path: &str) -> Option<String> {
    let base_url = normalize_cdn_base(base).ok()?;
    // 不 trim：畸形 path 一律拒绝（见 `validate_relay_path`）。
    let path = official_path;
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("..")
        || path.contains('\\')
        || path.contains('%')
        || path.contains('?')
        || path.contains('#')
        || path.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let mut url = base_url;
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    // 再确认一次：host 必须还是 base 的 host，且没有引入 query/fragment。
    if url.host_str().is_none() || url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    Some(url.to_string().trim_end_matches('/').to_string())
}

// ── 进度 ────────────────────────────────────────────────────────────

/// 下载进度（由 `upgrade` 转成 `agent:upgrade_progress` 帧）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadProgress {
    pub percent: Option<u64>,
    /// 当前源 / 失败原因等，给人看的一句话。
    pub detail: String,
}

/// 进度回调。用闭包而不是 channel：下载器不感知 relay 协议，也便于单测。
pub type ProgressFn = Arc<dyn Fn(DownloadProgress) + Send + Sync>;

pub fn no_progress() -> ProgressFn {
    Arc::new(|_| {})
}

// ── 下载器 ──────────────────────────────────────────────────────────

/// relay 镜像下载所需的凭据与地址（仅此客户端带 Token）。
#[derive(Clone)]
pub struct RelaySource {
    pub http: reqwest::Client,
    /// relay 根地址，如 `https://mu.example:3000`。
    pub base: String,
    /// rw Token。
    pub token: String,
}

impl std::fmt::Debug for RelaySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelaySource")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// 外部下载的重定向策略：**只允许跳到任务里显式列出的 host**。
///
/// CDN 镜像会把 `github.com/...` 302 到自己的对象存储（`objects.githubusercontent.com`
/// 之类），所以完全禁止跳转会打断正常镜像。但「跟随任意 host」又让一个被劫持的
/// CDN 把请求引到内网地址或攻击者服务器（SSRF / 流量劫持）。折中：跳转目标
/// 必须仍在本次任务的 CDN 主机白名单内，否则请求直接失败。
///
/// 白名单为空 → 一个跳转都不跟随（调用方无法给出可信集合时的保守默认）。
pub fn redirect_policy(allowed_hosts: Vec<String>) -> reqwest::redirect::Policy {
    let allowed: Vec<String> = allowed_hosts
        .into_iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects while downloading the upgrade artifact");
        }
        let next = attempt.url().clone();
        let host = next.host_str().unwrap_or("").to_ascii_lowercase();
        let scheme_ok = matches!(next.scheme(), "http" | "https");
        // 降级到 http 会把「已校验的 TLS 通道」换成明文，直接拒绝。
        let downgrade = next.scheme() == "http"
            && attempt
                .previous()
                .last()
                .is_some_and(|p| p.scheme() == "https");
        if scheme_ok && !downgrade && allowed.iter().any(|a| a == &host) {
            attempt.follow()
        } else {
            attempt.error(format!(
                "refusing redirect to {next} — host {host:?} is not one of this upgrade task's \
                 CDN hosts {allowed:?} (or the redirect downgrades https to http)"
            ))
        }
    })
}

/// 下载成功的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    /// 实际服务的源（`cdn:...` / `relay`）。
    pub source: String,
    pub bytes: u64,
    /// 实测小写 hex SHA-256（已与任务声明比对通过）。
    pub sha256: String,
    /// 这几轮一共发起了几次 HTTP 请求（诊断用）。
    pub attempts: usize,
    /// 是否用了 Range 续传。
    pub resumed_from: u64,
}

/// 一次源级失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFailure {
    pub source: String,
    pub error: String,
}

/// 下载器。外部与 relay 两个客户端分开，Token 不可能泄漏到 CDN。
pub struct Downloader {
    external: reqwest::Client,
    relay: Option<RelaySource>,
    budget: DownloadBudget,
    progress: ProgressFn,
}

impl Downloader {
    pub fn new(
        external: reqwest::Client,
        relay: Option<RelaySource>,
        budget: DownloadBudget,
        progress: ProgressFn,
    ) -> Self {
        Self {
            external,
            relay,
            budget,
            progress,
        }
    }

    /// 建外部客户端：只带代理配置与可选的解析链，**没有任何鉴权头**。
    ///
    /// `allowed_hosts` 是本次任务显式允许的 CDN 主机集合，重定向只允许落在其中
    /// （见 [`redirect_policy`]）。传空集合意味着「不允许任何重定向」——调用方
    /// 拿不到白名单时应当保守。
    pub fn build_external_client(
        proxy: &ProxyConfig,
        resolver: Option<Arc<dyn reqwest::dns::Resolve>>,
        budget: &DownloadBudget,
        allowed_hosts: Vec<String>,
    ) -> anyhow::Result<reqwest::Client> {
        crate::tlsutil::install_rustls_provider();
        let mut builder = reqwest::Client::builder()
            .connect_timeout(budget.connect)
            .tcp_keepalive(Duration::from_secs(30))
            // 不让「首字节很慢」把连接拖死：总时间由 fetch 自己管。
            .redirect(redirect_policy(allowed_hosts));
        if let Some(r) = resolver {
            // `dns_resolver` 要求具体的 `R: Sized`；`Arc<dyn Resolve>` 走 `dns_resolver2`
            // （它实现 `IntoResolve`），这样调用方不必知道具体解析器类型。
            builder = builder.dns_resolver2(r);
        }
        builder = proxy.apply(builder);
        builder
            .build()
            .map_err(|e| anyhow::anyhow!("cannot build download client: {e}"))
    }

    /// 按 `task.sources()` 顺序下载到 `part` 并校验。
    ///
    /// `part` 是**跨源共享**的续传文件路径；实际写入的是每个源自己的命名空间
    /// （见 [`Downloader::part_path_for`]），成功后再原子落到 `part`。
    ///
    /// 为什么必须分源：两个 CDN 对同一 URL 可能返回不同字节（镜像滞后、被污染、
    /// 劫持）。共享一个 `.part` 时，A 源断流后的前缀会被 B 源当作已验证数据接着
    /// 写，拼出「A 的头 + B 的尾」——最终 hash 只能说明拼接结果不是任务要的，
    /// 却无法在任何单点前阻止这种拼装。分源命名让每个源的字节只与自己续传。
    pub async fn fetch(&self, task: &UpgradeTask, part: &Path) -> Result<Downloaded, String> {
        let sources = task.sources();
        if sources.is_empty() {
            return Err(
                "upgrade task has no download source (no usable CDN url and no relay mirror)"
                    .to_string(),
            );
        }
        if task.size > MAX_ARTIFACT_BYTES {
            return Err(format!(
                "upgrade artifact is {} bytes, above the {} byte limit",
                task.size, MAX_ARTIFACT_BYTES
            ));
        }
        let deadline = Instant::now() + self.budget.total;
        let mut failures: Vec<SourceFailure> = Vec::new();
        let mut attempts_total = 0usize;

        for (idx, source) in sources.iter().enumerate() {
            if Instant::now() >= deadline {
                failures.push(SourceFailure {
                    source: source.label.clone(),
                    error: format!(
                        "skipped: total download budget of {:?} exhausted",
                        self.budget.total
                    ),
                });
                continue;
            }
            (self.progress)(DownloadProgress {
                percent: None,
                detail: format!("source {}/{} ({})", idx + 1, sources.len(), source.label),
            });
            // 这个源专属的续传文件：命名里同时含源身份与目标 hash，因此
            // 「换源」不可能读到另一个源的字节，「换版本」也不可能。
            let part_for_source = match part_path_for(part, source) {
                Ok(p) => p,
                Err(e) => {
                    failures.push(SourceFailure {
                        source: source.label.clone(),
                        error: e,
                    });
                    continue;
                }
            };
            let mut backoff = self.budget.backoff;
            for attempt in 1..=self.budget.attempts_per_source.max(1) {
                if Instant::now() >= deadline {
                    break;
                }
                attempts_total += 1;
                match self
                    .download_from(source, task, &part_for_source, deadline)
                    .await
                {
                    Ok(start_offset) => {
                        // 长度 + hash 校验；不通过就换下一个源（脏字节绝不落地）。
                        match self.verify(task, &part_for_source).await {
                            Ok((sha, size)) => {
                                // 校验通过才落到调用方给的路径：那是唯一会被
                                // 安装逻辑消费的文件。
                                tokio::fs::rename(&part_for_source, part)
                                    .await
                                    .map_err(|e| {
                                        format!("cannot move the verified download into place: {e}")
                                    })?;
                                // 其它源留下的残片已经没有价值（每个都可能是几十
                                // MB），成功即清掉，避免目录被历史残片占满。
                                cleanup_source_parts(part, &part_for_source).await;
                                return Ok(Downloaded {
                                    source: source.label.clone(),
                                    bytes: size,
                                    sha256: sha,
                                    attempts: attempts_total,
                                    resumed_from: start_offset,
                                });
                            }
                            Err(e) => {
                                failures.push(SourceFailure {
                                    source: source.label.clone(),
                                    error: e.clone(),
                                });
                                let _ = tokio::fs::remove_file(&part_for_source).await;
                                break; // 内容不对，重试同一个源没有意义
                            }
                        }
                    }
                    Err(outcome) => {
                        let (error, retry, discard) =
                            (outcome.error, outcome.retry, outcome.discard_part);
                        if discard {
                            // 坏字节不能留给同源的下一次续传。
                            let _ = tokio::fs::remove_file(&part_for_source).await;
                        }
                        failures.push(SourceFailure {
                            source: source.label.clone(),
                            error: error.clone(),
                        });
                        if !retry {
                            break;
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        let wait = backoff.min(remaining);
                        if wait.is_zero() {
                            break;
                        }
                        tracing::warn!(
                            source = %source.label,
                            attempt,
                            error = %error,
                            "upgrade download attempt failed; backing off"
                        );
                        tokio::time::sleep(wait).await;
                        backoff = (backoff * 2).min(Duration::from_secs(5));
                    }
                }
            }
        }

        Err(format!(
            "all {} upgrade source(s) failed: {}",
            sources.len(),
            failures
                .iter()
                .map(|f| format!("{}: {}", f.source, f.error))
                .collect::<Vec<_>>()
                .join(" | ")
        ))
    }

    /// 从单个源把字节追加到 `part`（带 Range 续传）。
    ///
    /// 返回本次尝试的起始偏移（>0 表示用了 Range 续传），供最终报告区分
    /// 「全量下载」与「续传完成」。
    async fn download_from(
        &self,
        source: &Source,
        task: &UpgradeTask,
        part: &Path,
        deadline: Instant,
    ) -> Result<u64, Outcome> {
        // 续传文件必须是普通文件：符号链接 / FIFO / 设备一律拒绝（共享可写目录
        // 里跟随链接会覆盖任意文件）。
        if let Err(e) = ensure_regular_part_file(part) {
            return Err(Outcome::fatal(e));
        }
        let existing = tokio::fs::metadata(part)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let (client, url) = match source.kind {
            SourceKind::Cdn => (&self.external, source.url.clone()),
            SourceKind::Relay => {
                let Some(relay) = &self.relay else {
                    return Err(Outcome::fatal(
                        "relay mirror requested but no relay credentials are available".to_string(),
                    ));
                };
                (
                    &relay.http,
                    format!(
                        "{}/{}",
                        relay.base.trim_end_matches('/'),
                        source.url.trim_start_matches('/')
                    ),
                )
            }
        };
        // 已有字节不可能是完整的合法制品（否则早就校验成功了），但也不能超过
        // 目标大小——那种 `.part` 是上一次失败留下的脏数据。
        let start = if task.size > 0 && existing > task.size {
            0
        } else {
            existing
        };
        let mut req = client.get(&url);
        if source.kind == SourceKind::Relay {
            if let Some(relay) = &self.relay {
                // 只有 relay 客户端带 Token；CDN 分支根本没有这个头。
                req = req.header("X-SR-Token", &relay.token);
            }
        }
        if start > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={start}-"));
        }

        let first_byte_budget = self
            .budget
            .first_byte
            .min(deadline.saturating_duration_since(Instant::now()));
        let resp = tokio::time::timeout(first_byte_budget, req.send())
            .await
            .map_err(|_| {
                Outcome::retryable(format!(
                    "no response within {first_byte_budget:?} (connect/first byte budget)"
                ))
            })?
            // 带上底层原因（TLS/证书/连接）：现场只有这一条日志可用。
            .map_err(|e| {
                Outcome::retryable(format!(
                    "request failed: {}",
                    crate::agent::doh::error_chain(&e)
                ))
            })?;

        let status = resp.status();
        if status.as_u16() == 416 {
            // `.part` 已经不小于目标区间：丢掉脏数据，下次从头来。
            return Err(Outcome::fatal_retryable(
                "HTTP 416: partial file is not resumable; discarding it".to_string(),
            ));
        }
        if !status.is_success() {
            // 4xx 视为该源不可用（换源）；5xx 允许同源重试一次。两种情况都
            // 没有写入新字节，`.part` 保留给后续续传。
            return Err(Outcome {
                error: format!("HTTP {status}"),
                retry: status.is_server_error(),
                discard_part: false,
            });
        }

        // 206 必须真的从我们请求的偏移开始；200 表示服务端忽略 Range，必须
        // 从头覆盖（追加会拼出坏文件）。
        let mut written = match status.as_u16() {
            206 => {
                let cr = resp
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                match parse_content_range(cr) {
                    Some((s, _e, total)) => {
                        if start > 0 && s != start {
                            return Err(Outcome::fatal(format!(
                                "server resumed at byte {s} but {start} was requested"
                            )));
                        }
                        if total > MAX_ARTIFACT_BYTES {
                            return Err(Outcome::fatal(format!(
                                "server reports a {total} byte artifact, above the limit"
                            )));
                        }
                        if task.size > 0 && total != task.size {
                            return Err(Outcome::fatal(format!(
                                "server reports {total} bytes but the task declares {}",
                                task.size
                            )));
                        }
                        s
                    }
                    None => {
                        return Err(Outcome::fatal(format!(
                            "206 response without a usable Content-Range ({cr:?})"
                        )))
                    }
                }
            }
            200 => 0,
            other => {
                return Err(Outcome::fatal(format!(
                    "unexpected HTTP {other} for a download request"
                )))
            }
        };
        let restart = status.as_u16() == 200 && start > 0;
        // 真正复用了多少已有字节：只有「请求了偏移且服务端接受」才算续传。
        let resumed = if status.as_u16() == 206 && start > 0 {
            start
        } else {
            0
        };
        if let Some(len) = resp.content_length() {
            if len > MAX_ARTIFACT_BYTES {
                return Err(Outcome::fatal(format!(
                    "content length {len} exceeds the {} byte limit",
                    MAX_ARTIFACT_BYTES
                )));
            }
            if task.size > 0 && status.as_u16() == 200 && len != task.size {
                return Err(Outcome::fatal(format!(
                    "content length {len} does not match the task size {}",
                    task.size
                )));
            }
        }

        let mut file = if restart || written == 0 {
            tokio::fs::File::create(part)
                .await
                .map_err(|e| Outcome::fatal(format!("cannot create {}: {e}", part.display())))?
        } else {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(part)
                .await
                .map_err(|e| Outcome::fatal(format!("cannot resume {}: {e}", part.display())))?
        };

        let total_hint = if task.size > 0 { task.size } else { 0 };
        let mut last_pct: Option<u64> = None;
        let mut stream = resp.bytes_stream();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = file.flush().await;
                return Err(Outcome::retryable(format!(
                    "total download budget of {:?} exhausted",
                    self.budget.total
                )));
            }
            let chunk_budget = self.budget.chunk.min(remaining);
            let next = tokio::time::timeout(chunk_budget, stream.next())
                .await
                .map_err(|_| {
                    Outcome::retryable(format!("stream stalled: no data for {chunk_budget:?}"))
                })?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|e| {
                Outcome::retryable(format!("stream error after {written} bytes: {e}"))
            })?;
            file.write_all(&chunk)
                .await
                .map_err(|e| Outcome::fatal(format!("write error: {e}")))?;
            written += chunk.len() as u64;
            if written > MAX_ARTIFACT_BYTES {
                let _ = file.flush().await;
                return Err(Outcome::fatal(format!(
                    "download exceeded the {} byte limit",
                    MAX_ARTIFACT_BYTES
                )));
            }
            if total_hint > 0 {
                let pct = (written.saturating_mul(100) / total_hint).min(100);
                if last_pct != Some(pct) {
                    last_pct = Some(pct);
                    (self.progress)(DownloadProgress {
                        percent: Some(pct),
                        detail: source.label.clone(),
                    });
                }
            }
        }
        file.flush()
            .await
            .map_err(|e| Outcome::fatal(format!("flush error: {e}")))?;
        let _ = file.sync_all().await;
        Ok(resumed)
    }

    /// 长度 + SHA-256。返回（实测 hash，实测字节数）。
    async fn verify(&self, task: &UpgradeTask, part: &Path) -> Result<(String, u64), String> {
        let meta = tokio::fs::metadata(part)
            .await
            .map_err(|e| format!("cannot stat downloaded file: {e}"))?;
        if task.size > 0 && meta.len() != task.size {
            return Err(format!(
                "downloaded {} bytes but the task declares {}",
                meta.len(),
                task.size
            ));
        }
        let digest = sha256_hex_file_async(part.to_path_buf())
            .await
            .map_err(|e| format!("cannot hash downloaded file: {e}"))?;
        if !digest.eq_ignore_ascii_case(&task.sha256) {
            return Err(format!(
                "SHA-256 mismatch: got {digest}, task declared {}",
                task.sha256
            ));
        }
        Ok((digest, meta.len()))
    }
}

/// 每个下载源专属的续传文件路径。
///
/// 命名规则：`<调用方给的 part 路径>.<源命名空间>.part`
///
/// 命名空间由「源 + 目标 hash」派生：
/// - CDN 源：`cdn-<host>_<port>-<hash 前 16 位>`；
/// - relay 源：`relay-<hash 前 16 位>`。
///
/// 于是「同源重试」能接着续传（同一命名空间），而「换源」拿到的永远是一个全新
/// 的路径——A 源断流留下的前缀不可能被 B 源当成自己的字节接着写。目标 hash 进
/// 命名则把「同源但换了目标版本」也隔开。
///
/// 主机名里的 `.` `:` 等字符会被替换成 `_`，且整段只保留 `[A-Za-z0-9_-]`，
/// 因此命名空间不会引入路径分隔符或穿越。
pub fn part_path_for(part: &Path, source: &Source) -> Result<PathBuf, String> {
    let namespace = source_namespace(source)?;
    let file_name = part
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("download path {} has no usable file name", part.display()))?;
    let dir = part.parent().unwrap_or_else(|| Path::new("."));
    Ok(dir.join(format!(".{file_name}.{namespace}.part")))
}

/// 源命名空间（同时用于续传文件名与诊断日志，不含 Token / query）。
fn source_namespace(source: &Source) -> Result<String, String> {
    match source.kind {
        SourceKind::Cdn => {
            let parsed = url::Url::parse(&source.url)
                .map_err(|e| format!("download url {:?} is not a valid URL: {e}", source.url))?;
            let host = parsed
                .host_str()
                .ok_or_else(|| format!("download url {:?} has no host", source.url))?;
            let port = parsed.port_or_known_default().unwrap_or(0);
            let hash = source_target_hash(&source.url);
            Ok(format!(
                "cdn-{}-{port}-{hash}",
                sanitize_namespace_segment(host)
            ))
        }
        SourceKind::Relay => Ok(format!("relay-{}", source_target_hash(&source.url))),
    }
}

/// 源路径的稳定短标识（`download url` / relay 路径的 FNV-1a 64 位）。
///
/// 这里只需要「不同目标不撞名」，不需要密码学强度；调用方另有一层目标 hash
/// 参与命名。
fn source_target_hash(url: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 把任意字符串收成安全的文件名片段：只保留 `[A-Za-z0-9_-]`，其余替换成 `_`。
fn sanitize_namespace_segment(raw: &str) -> String {
    let out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "_".to_string()
    } else {
        out
    }
}

/// 确认 `path` 处的续传文件可以安全续写：必须是**普通文件**（或不存在）。
///
/// 可执行文件目录在部分部署里是共享可写的（`/opt/...`、`/usr/local/bin`、用户
/// 目录）。如果那里被人预先放了符号链接或 FIFO，续写等于替攻击者覆盖它指向的
/// 任意文件（或阻塞在管道上）。因此：
/// - 符号链接 → 拒绝（不跟随，也不删除后重建，避免 TOCTOU 里再被换成链接）；
/// - 设备/管道/目录 → 拒绝；
/// - 普通文件 → 允许（可能是自己上一次的续传数据）。
pub fn ensure_regular_part_file(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                return Err(format!(
                    "refusing to resume into {}: it is a symbolic link (a shared-writable \
                     directory must not let a link redirect the download)",
                    path.display()
                ));
            }
            if !ft.is_file() {
                return Err(format!(
                    "refusing to resume into {}: it is not a regular file",
                    path.display()
                ));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot inspect {}: {e}", path.display())),
    }
}

/// 删除同一个目标下**其它源**的续传残片（`keep` 之外全部清掉）。
///
/// 只动 `<part 文件名>.<namespace>.part` 这一族，不碰调用方给的 `part` 本身，
/// 也不碰目录里任何无关文件。
async fn cleanup_source_parts(part: &Path, keep: &Path) {
    let Some(base) = part.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let Some(dir) = part.parent() else { return };
    let prefix = format!(".{base}.");
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(".part") {
            continue;
        }
        if entry.path() == keep {
            continue;
        }
        let _ = tokio::fs::remove_file(entry.path()).await;
    }
}

/// 一个源的失败结果。
///
/// - `retry`：同源是否值得再试（网络类故障值得，404/协议异常不值得）；
/// - `discard_part`：`.part` 里的字节是否已被证明不可信。**内容类**失败
///   （服务端声明的长度/区间与任务不符、HTML 错误页）必须丢弃——否则下一个源
///   会从这些坏字节之后续传，拼出来的文件注定校验失败，白白浪费一个源；
///   而网络中断/分块静默恰恰是续传要救的场景，那些字节是好的，必须保留。
struct Outcome {
    error: String,
    retry: bool,
    discard_part: bool,
}

impl Outcome {
    /// 网络类失败：值得重试，`.part` 保留（下次续传接着下）。
    fn retryable(error: String) -> Self {
        Self {
            error,
            retry: true,
            discard_part: false,
        }
    }
    /// 内容/协议类失败：换源，且丢弃已下载字节。
    fn fatal(error: String) -> Self {
        Self {
            error,
            retry: false,
            discard_part: true,
        }
    }
    /// 协议类失败但已自行清理（如 416 已删文件）。
    fn fatal_retryable(error: String) -> Self {
        Self {
            error,
            retry: true,
            discard_part: true,
        }
    }
}

/// 解析 `Content-Range: bytes <start>-<end>/<total>`（`total` 可为 `*`）。
fn parse_content_range(raw: &str) -> Option<(u64, u64, u64)> {
    let rest = raw.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    let total: u64 = total.trim().parse().ok()?;
    if end < start || total == 0 {
        return None;
    }
    Some((start, end, total))
}

/// 流式计算文件 SHA-256（不把制品读进内存）。
pub async fn sha256_hex_file_async(path: std::path::PathBuf) -> std::io::Result<String> {
    tokio::task::spawn_blocking(move || sha256_hex_file(&path))
        .await
        .map_err(|e| std::io::Error::other(format!("hash task failed: {e}")))?
}

/// 同步版本（`upgrade` 里沿用）。
pub fn sha256_hex_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 常量时间比较两个 hex digest（大小写不敏感）。
pub fn verify_sha256(actual: &str, expected: &str) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    actual
        .as_bytes()
        .iter()
        .zip(expected.as_bytes().iter())
        .fold(0u8, |acc, (x, y)| {
            acc | x.to_ascii_lowercase() ^ y.to_ascii_lowercase()
        })
        == 0
}

/// 把解析链包装成 reqwest 的 DNS 解析器：白名单内的主机走三级链路，其它
/// 主机（如 HTTP 代理自己的地址）走系统解析——否则配了代理就没法建连。
pub struct ChainResolver {
    pub chain: Arc<crate::agent::doh::ResolveChain>,
}

impl reqwest::dns::Resolve for ChainResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_ascii_lowercase();
        let chain = self.chain.clone();
        Box::pin(async move {
            let ips = match chain.resolve(&host).await {
                Ok(r) => r.addrs,
                Err(e) => {
                    // 非白名单主机（代理节点等）：退回系统解析。
                    if e.contains("not part of this upgrade task") {
                        let addrs = tokio::net::lookup_host((host.as_str(), 0)).await.map_err(
                            |e| -> Box<dyn std::error::Error + Send + Sync> {
                                format!("system lookup for {host} failed: {e}").into()
                            },
                        )?;
                        addrs.map(|a| a.ip()).collect::<Vec<_>>()
                    } else {
                        return Err(format!("DNS chain failed for {host}: {e}").into());
                    }
                }
            };
            let out: reqwest::dns::Addrs =
                Box::new(ips.into_iter().map(|ip| std::net::SocketAddr::new(ip, 0)));
            Ok(out)
        })
    }
}

/// 供 `upgrade` 使用：解析链 → reqwest resolver。
pub fn chain_resolver(
    allowed_hosts: Vec<String>,
    doh_cfg: crate::agent::doh::DohConfig,
    relay: Option<crate::agent::doh::RelayResolver>,
) -> Arc<dyn reqwest::dns::Resolve> {
    Arc::new(ChainResolver {
        chain: Arc::new(crate::agent::doh::ResolveChain::production(
            allowed_hosts,
            doh_cfg,
            relay,
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// 环境变量是进程级的：动 env 的测试必须串行。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn task_with(
        name: &str,
        bytes: &[u8],
        cdn_bases: Vec<String>,
        relay: Option<String>,
    ) -> UpgradeTask {
        UpgradeTask {
            upgrade_id: "up-1".into(),
            target_version: "9.9.9".into(),
            filename: name.into(),
            size: bytes.len() as u64,
            sha256: {
                use sha2::Digest;
                hex::encode(sha2::Sha256::digest(bytes))
            },
            official_path: Some(format!(
                "/zzttzzmyswy/shell-remote/releases/download/v9.9.9/{name}"
            )),
            cdn_base_urls: cdn_bases,
            relay_url: relay,
            relay_fallback: false,
            source: "official".into(),
        }
    }

    /// 制品名必须与测试机身份匹配（`verify_artifact_identity` 会拦）。
    fn local_filename() -> String {
        let variant = if cfg!(feature = "gui") && cfg!(feature = "desktop") && cfg!(feature = "tui")
        {
            "shell-remote-ui"
        } else {
            "shell-remote"
        };
        format!(
            "{}-{}{}",
            variant,
            own_arch_key(),
            if cfg!(windows) { ".exe" } else { "" }
        )
    }

    // ── mock CDN ─────────────────────────────────────────────────

    #[derive(Clone, Default)]
    struct CdnBehaviour {
        /// 提供的字节。
        bytes: Arc<Vec<u8>>,
        /// 是否支持 Range（false → 一律 200 全量）。
        supports_range: bool,
        /// 强制返回该状态码（404 等）。
        force_status: Option<u16>,
        /// 200 但内容是 HTML 错误页。
        html_error_page: bool,
        /// 响应头发完就挂住（首字节后无数据）。
        stall_after_headers: bool,
        /// 只发前 N 字节就断流（模拟弱网）。
        truncate_after: Option<usize>,
    }

    struct CdnState {
        behaviour: CdnBehaviour,
        requests: AtomicUsize,
        ranges: Mutex<Vec<Option<String>>>,
        headers: Mutex<Vec<Vec<(String, String)>>>,
    }

    async fn cdn_handler(State(state): State<Arc<CdnState>>, headers: HeaderMap) -> Response {
        state.requests.fetch_add(1, Ordering::SeqCst);
        let range = headers
            .get(axum::http::header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        {
            let mut seen = state.ranges.lock().unwrap();
            seen.push(range.clone());
            let mut hdrs = state.headers.lock().unwrap();
            hdrs.push(
                headers
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_ascii_lowercase(),
                            v.to_str().unwrap_or("<binary>").to_string(),
                        )
                    })
                    .collect(),
            );
        }
        let b = &state.behaviour;
        if let Some(code) = b.force_status {
            return Response::builder()
                .status(StatusCode::from_u16(code).unwrap())
                .body(Body::from("not found"))
                .unwrap();
        }
        if b.html_error_page {
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/html")
                .body(Body::from(
                    "<html><body>404 Not Found (mirror)</body></html>",
                ))
                .unwrap();
        }
        if b.stall_after_headers {
            // 声明长度但永不发 body：客户端应因分块静默超时失败。
            // 声明长度但 body 永不产生任何字节：客户端必须靠分块静默预算失败。
            let stream = futures_util::stream::pending::<Result<Vec<u8>, std::io::Error>>();
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-length", b.bytes.len().to_string())
                .body(Body::from_stream(stream))
                .unwrap();
        }
        let total = b.bytes.len();
        let start = if b.supports_range {
            range
                .as_deref()
                .and_then(|r| r.trim().strip_prefix("bytes="))
                .and_then(|r| r.split('-').next())
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(0)
        } else {
            0
        };
        let body_bytes = if start > 0 && b.supports_range {
            if start >= total {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header("content-range", format!("bytes */{total}"))
                    .body(Body::empty())
                    .unwrap();
            }
            b.bytes[start..].to_vec()
        } else {
            b.bytes.to_vec()
        };
        let status = if start > 0 && b.supports_range {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        let declared = body_bytes.len();
        let mut builder = Response::builder()
            .status(status)
            .header("content-length", declared.to_string());
        if status == StatusCode::PARTIAL_CONTENT {
            builder = builder.header(
                "content-range",
                format!("bytes {}-{}/{}", start, total - 1, total),
            );
        }
        // 截断场景：**声明完整长度**但只发前 N 字节然后断流。必须走 body stream
        // ——直接给一个更短的 `Body::from(Vec)` 会让 hyper 自己发现长度不符并
        // panic，那不是真实网络的形态（真实对端是连接被掐断）。
        let body = match b.truncate_after {
            Some(n) if n < declared => {
                // 先发一段，然后**静默**（连接不断但没数据）：弱网断流的真实
                // 形态之一，也让客户端确定已经把前 n 字节落到磁盘上。
                let head = body_bytes[..n].to_vec();
                let tail = futures_util::stream::pending::<Result<Vec<u8>, std::io::Error>>();
                Body::from_stream(
                    futures_util::stream::iter(vec![Ok::<_, std::io::Error>(head)]).chain(tail),
                )
            }
            _ => Body::from(body_bytes),
        };
        builder.body(body).unwrap()
    }

    async fn spawn_cdn(behaviour: CdnBehaviour) -> (String, Arc<CdnState>) {
        let state = Arc::new(CdnState {
            behaviour,
            requests: AtomicUsize::new(0),
            ranges: Mutex::new(Vec::new()),
            headers: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .fallback(get(cdn_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), state)
    }

    /// 一个必然连不上的地址（端口已释放）——用于「前一个源超时/失败」场景。
    fn dead_base() -> String {
        // 0.0.0.0:1 在本机不监听；连接会被立即拒绝或超时。
        "http://127.0.0.1:1".to_string()
    }

    fn budget() -> DownloadBudget {
        DownloadBudget {
            connect: Duration::from_secs(2),
            first_byte: Duration::from_secs(2),
            chunk: Duration::from_millis(800),
            total: Duration::from_secs(20),
            attempts_per_source: 2,
            backoff: Duration::from_millis(20),
        }
    }

    /// 测试用下载器。mock CDN 全部监听 127.0.0.1，因此重定向白名单默认放行它
    /// —— 需要验证「跨域跳转被拒」的用例用 [`downloader_with_hosts`]。
    fn downloader(relay: Option<RelaySource>, b: DownloadBudget) -> Downloader {
        downloader_with_hosts(relay, b, vec!["127.0.0.1".to_string()])
    }

    fn downloader_with_hosts(
        relay: Option<RelaySource>,
        b: DownloadBudget,
        hosts: Vec<String>,
    ) -> Downloader {
        let external =
            Downloader::build_external_client(&ProxyConfig::empty(), None, &b, hosts).unwrap();
        Downloader::new(external, relay, b, no_progress())
    }

    /// 某个源会使用的 `.part` 路径（与下载器内部命名规则一致）。
    fn source_part(
        task: &UpgradeTask,
        part: &std::path::Path,
        source_idx: usize,
    ) -> std::path::PathBuf {
        let sources = task.sources();
        part_path_for(part, &sources[source_idx]).unwrap()
    }

    fn tmp_part(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sr-dl-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("artifact.part")
    }

    const ARTIFACT: &[u8] = b"#!fake-binary payload for downgrade tests\n0123456789";

    // ── 多 CDN 顺序 ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_tries_cdns_in_order_and_stops_at_first_good() {
        let (bad, bad_state) = spawn_cdn(CdnBehaviour {
            force_status: Some(404),
            ..Default::default()
        })
        .await;
        let (good, good_state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let third = dead_base();
        let task = task_with(&local_filename(), ARTIFACT, vec![bad, good, third], None);
        let part = tmp_part("order");
        let got = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        assert!(got.source.starts_with("cdn:"), "{}", got.source);
        assert_eq!(got.bytes, task.size);
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        assert_eq!(
            bad_state.requests.load(Ordering::SeqCst),
            1,
            "404 不重试同源"
        );
        assert_eq!(good_state.requests.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_404_and_timeout_switch_to_next_source() {
        let (missing, _) = spawn_cdn(CdnBehaviour {
            force_status: Some(404),
            ..Default::default()
        })
        .await;
        let (stalling, stall_state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            stall_after_headers: true,
            ..Default::default()
        })
        .await;
        let (good, _) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(
            &local_filename(),
            ARTIFACT,
            vec![missing, stalling, good],
            None,
        );
        let part = tmp_part("timeout");
        let got = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        assert!(got.source.starts_with("cdn:"));
        assert!(stall_state.requests.load(Ordering::SeqCst) >= 1);
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    // ── HTML 错误页 / hash 不匹配 ────────────────────────────────

    #[tokio::test]
    async fn test_html_error_page_is_rejected_by_hash() {
        let (mirror, _) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            html_error_page: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![mirror], None);
        let part = tmp_part("html");
        let err = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap_err();
        // 200 + HTML 页：长度/hash 不符，必须拒绝而不是把网页当二进制装上。
        assert!(
            err.contains("does not match the task size") || err.contains("SHA-256 mismatch"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_bad_source_does_not_poison_the_next_source_resume() {
        // 第一个源在一个被截断的脏 `.part` 上返回「长度与任务不符」——它写下的
        // 字节不可信。第二个源必须从 0 开始，而不是从脏字节之后续传。
        let (liar, liar_state) = spawn_cdn(CdnBehaviour {
            // 长度与 task.size 不符：内容类失败 → 必须丢弃已写字节。
            bytes: Arc::new(b"short".to_vec()),
            supports_range: false,
            ..Default::default()
        })
        .await;
        let (good, good_state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![liar, good], None);
        let part = tmp_part("poison");
        // 预置一段脏的前缀（模拟上一次中断留下的、不可信的部分字节），写进
        // **第一个源**的命名空间。
        std::fs::write(source_part(&task, &part, 0), b"garbage-prefix").unwrap();

        let got = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        assert!(got.source.starts_with("cdn:"));
        assert_eq!(
            std::fs::read(&part).unwrap(),
            ARTIFACT,
            "最终文件必须是完整制品"
        );
        assert_eq!(got.resumed_from, 0, "内容类失败后不得复用脏字节");

        assert_eq!(liar_state.requests.load(Ordering::SeqCst), 1);
        let good_ranges = good_state.ranges.lock().unwrap().clone();
        // 好源必须被从头请求（脏 `.part` 已被丢弃）。
        assert_eq!(
            good_ranges.first().and_then(|r| r.clone()),
            None,
            "脏字节必须被丢弃，好源从 0 开始：{good_ranges:?}"
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_network_failure_keeps_part_for_resume() {
        // 与上一条相反：纯网络失败写下的字节是好的，必须保留给下一次续传。
        let (flaky, _) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            truncate_after: Some(10),
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![flaky], None);
        let part = tmp_part("keep");
        let mut b = budget();
        b.attempts_per_source = 3;
        b.chunk = Duration::from_millis(500);
        let err = downloader(None, b).fetch(&task, &part).await.unwrap_err();
        assert!(
            err.contains("stalled") || err.contains("stream error") || err.contains("budget"),
            "{err}"
        );
        assert!(
            source_part(&task, &part, 0).exists(),
            "网络断流留下的字节必须保留，供下次续传（否则几十 MB 白下）"
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_hash_mismatch_fails_without_installing() {
        let (mirror, _) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(b"tampered bytes".to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        // 任务声明的是别的字节（模拟被篡改的镜像）。
        let task = task_with(&local_filename(), ARTIFACT, vec![mirror], None);
        let part = tmp_part("hash");
        let err = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap_err();
        assert!(
            err.contains("SHA-256 mismatch") || err.contains("does not match"),
            "{err}"
        );
        // 校验失败必须删掉脏文件，不能留在原地被当成"下载完成"。
        assert!(!part.exists(), "脏字节必须被清理");
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    // ── Range 206 / 200 / 断流续传 ───────────────────────────────

    #[tokio::test]
    async fn test_range_resume_sends_offset_and_accepts_206() {
        let (cdn, state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn], None);
        let part = tmp_part("resume");
        // 预先写入前半段，模拟断点（写进该源自己的命名空间——这是唯一会被续传
        // 读取的位置）。
        let half = ARTIFACT.len() / 2;
        std::fs::write(source_part(&task, &part, 0), &ARTIFACT[..half]).unwrap();
        let got = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        assert_eq!(
            state.ranges.lock().unwrap()[0].as_deref(),
            Some(format!("bytes={half}-").as_str())
        );
        assert_eq!(got.bytes, task.size);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_200_response_restarts_from_zero_instead_of_appending() {
        let (cdn, state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: false, // 忽略 Range，一律 200 全量
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn], None);
        let part = tmp_part("restart");
        std::fs::write(source_part(&task, &part, 0), &ARTIFACT[..10]).unwrap();
        downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        // 200 必须从头覆盖：文件等于完整制品，不是「前10字节+全量」。
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        assert_eq!(
            state.ranges.lock().unwrap()[0].as_deref(),
            Some("bytes=10-")
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_stalled_stream_resumes_on_retry() {
        // 第一次只发一部分就断流；重试（带 Range）拿剩下的。
        #[derive(Clone)]
        struct Flaky {
            sends: Arc<AtomicUsize>,
            bytes: Arc<Vec<u8>>,
        }
        async fn flaky_handler(State(s): State<Flaky>, headers: HeaderMap) -> Response {
            let n = s.sends.fetch_add(1, Ordering::SeqCst);
            let start = headers
                .get(axum::http::header::RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|r| r.strip_prefix("bytes=").map(|s| s.to_string()))
                .and_then(|r| r.split('-').next().map(|s| s.to_string()))
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let total = s.bytes.len();
            if n == 0 {
                // 声明完整长度，只发前 12 字节然后断流。
                let partial = s.bytes[..12].to_vec();
                let stream = futures_util::stream::iter(vec![
                    Ok::<_, std::io::Error>(partial),
                    Err(std::io::Error::other("connection reset by peer")),
                ]);
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-length", total.to_string())
                    .body(Body::from_stream(stream))
                    .unwrap();
            }
            let body = s.bytes[start..].to_vec();
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header("content-length", body.len().to_string())
                .header(
                    "content-range",
                    format!("bytes {}-{}/{}", start, total - 1, total),
                )
                .body(Body::from(body))
                .unwrap()
        }
        let flaky = Flaky {
            sends: Arc::new(AtomicUsize::new(0)),
            bytes: Arc::new(ARTIFACT.to_vec()),
        };
        let app = Router::new()
            .fallback(get(flaky_handler))
            .with_state(flaky.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let task = task_with(
            &local_filename(),
            ARTIFACT,
            vec![format!("http://{addr}")],
            None,
        );
        let part = tmp_part("flaky");
        let got = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        assert_eq!(flaky.sends.load(Ordering::SeqCst), 2, "断流后应重试一次");
        assert!(got.attempts >= 2);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_bad_part_is_discarded_when_larger_than_target() {
        let (cdn, _) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn], None);
        let part = tmp_part("dirty");
        // 脏 `.part` 比目标还大：必须忽略它并从头下载。
        std::fs::write(&part, vec![0xAAu8; ARTIFACT.len() + 50]).unwrap();
        downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_resume_continues_across_separate_download_attempts() {
        // 模拟「第一次升级下载中断 → admin 再点一次」：新的 Downloader/进程复用
        // 同一个 `.part`，必须只请求缺的那一段（而不是重下整个制品）。
        let (cdn, state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn], None);
        let part = tmp_part("cross-attempt");
        let half = ARTIFACT.len() / 2;

        // 第一次：只写了前半段就"进程没了"（同一源 → 同一命名空间）。
        std::fs::write(source_part(&task, &part, 0), &ARTIFACT[..half]).unwrap();

        // 第二次：全新下载器（模拟重启后的 agent）接着下。
        let dl = downloader(None, budget());
        let got = dl.fetch(&task, &part).await.unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        assert_eq!(got.resumed_from, half as u64, "必须报告续传起点");
        assert_eq!(
            state.ranges.lock().unwrap()[0].as_deref(),
            Some(format!("bytes={half}-").as_str()),
            "跨进程重试也必须带 Range"
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    /// 200 响应覆盖已有 `.part` 时，`resumed_from` 必须是 0（没有复用任何字节）。
    #[tokio::test]
    async fn test_restart_reports_zero_resumed_bytes() {
        let (cdn, _) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: false,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn], None);
        let part = tmp_part("restart-report");
        std::fs::write(source_part(&task, &part, 0), &ARTIFACT[..10]).unwrap();
        let got = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        assert_eq!(got.resumed_from, 0);
        assert_eq!(got.bytes, ARTIFACT.len() as u64);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    // ── relay 兜底 + Token 不泄漏 ────────────────────────────────

    #[tokio::test]
    async fn test_relay_is_used_last_and_only_with_token() {
        let (cdn, cdn_state) = spawn_cdn(CdnBehaviour {
            force_status: Some(404),
            ..Default::default()
        })
        .await;
        let (relay, relay_state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(
            &local_filename(),
            ARTIFACT,
            vec![cdn],
            Some(format!("/agent/upgrade/blob/{}", local_filename())),
        );
        let relay_src = RelaySource {
            http: reqwest::Client::new(),
            base: relay,
            token: "rw-secret-token".to_string(),
        };
        let part = tmp_part("relay");
        let got = downloader(Some(relay_src), budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        assert_eq!(got.source, "relay");
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);

        // relay 请求带 Token……
        let relay_headers = relay_state.headers.lock().unwrap();
        assert!(
            relay_headers[0]
                .iter()
                .any(|(k, v)| k == "x-sr-token" && v == "rw-secret-token"),
            "relay 镜像请求必须带 rw Token"
        );
        // ……外部 CDN 请求绝不带（任何形式）。
        let cdn_headers = cdn_state.headers.lock().unwrap();
        for hdrs in cdn_headers.iter() {
            for (k, v) in hdrs {
                assert_ne!(k, "x-sr-token", "Token 泄漏到外部 CDN");
                assert_ne!(k, "authorization", "外部请求不得携带鉴权头");
                assert!(!v.contains("rw-secret-token"), "Token 出现在外部请求里");
            }
        }
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_no_relay_mirror_means_no_relay_attempt() {
        // relay_url 为空时不能硬拼 relay 端点（契约：仅在配置镜像时启用）。
        let (cdn, _) = spawn_cdn(CdnBehaviour {
            force_status: Some(404),
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn], None);
        assert_eq!(task.sources().len(), 1);
        assert_eq!(task.sources()[0].kind, SourceKind::Cdn);
        let part = tmp_part("norelay");
        let err = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap_err();
        assert!(err.contains("404"), "{err}");
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_all_sources_failing_reports_each_one() {
        let (a, _) = spawn_cdn(CdnBehaviour {
            force_status: Some(404),
            ..Default::default()
        })
        .await;
        let (b, _) = spawn_cdn(CdnBehaviour {
            force_status: Some(500),
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![a, b], None);
        let part = tmp_part("allfail");
        let err = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap_err();
        assert!(err.contains("all 2 upgrade source(s) failed"), "{err}");
        assert!(err.contains("HTTP 404"), "{err}");
        assert!(err.contains("HTTP 500"), "{err}");
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    // ── 任务解析 / 源构造 / 身份校验 ─────────────────────────────

    #[test]
    fn test_task_from_payload_new_protocol() {
        let task = UpgradeTask::from_payload(&serde_json::json!({
            "schema": 1,
            "upgrade_id": "abc",
            "target_version": "0.60.0",
            "filename": "shell-remote-x86_64",
            "size": 1234,
            "sha256": "ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "official_path": "/zzttzzmyswy/shell-remote/releases/download/v0.60.0/shell-remote-x86_64",
            "cdn_base_urls": ["https://cdn-a.test", "https://cdn-b.test"],
            "relay_url": "/agent/upgrade/blob/shell-remote-x86_64",
            "relay_fallback": true,
            "source": "official",
            // 老键并存：新 agent 只读新字段
            "version": "0.60.0",
            "url": "/agent/upgrade/blob/shell-remote-x86_64",
        }))
        .unwrap();
        assert_eq!(task.target_version, "0.60.0");
        assert_eq!(task.sha256.len(), 64);
        assert_eq!(
            task.sha256,
            task.sha256.to_ascii_lowercase(),
            "hash 统一小写"
        );
        assert_eq!(task.cdn_base_urls.len(), 2);
        assert!(task.allows_cdn());
        let sources = task.sources();
        assert_eq!(sources.len(), 3);
        assert_eq!(
            sources[0].url,
            "https://cdn-a.test/zzttzzmyswy/shell-remote/releases/download/v0.60.0/shell-remote-x86_64"
        );
        assert_eq!(sources[2].kind, SourceKind::Relay);
    }

    #[test]
    fn test_task_from_payload_legacy_relay() {
        // 存量 relay：只有 version/url/sha256（没有 filename/size/CDN）。
        let task = UpgradeTask::from_payload(&serde_json::json!({
            "version": "0.19.0",
            "url": "/agent/upgrade/blob/shell-remote-x86_64",
            "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        }))
        .unwrap();
        assert_eq!(task.target_version, "0.19.0");
        assert_eq!(
            task.filename, "shell-remote-x86_64",
            "从 url 尾段推断文件名"
        );
        assert_eq!(task.size, 0, "老 relay 不提供 size");
        assert!(!task.allows_cdn(), "老 relay 没有 CDN 概念");
        let sources = task.sources();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind, SourceKind::Relay);
    }

    #[test]
    fn test_task_from_payload_rejects_incomplete() {
        // 缺 sha256 / 缺版本 / hash 形态非法 → None（调用方回错误帧）。
        assert!(UpgradeTask::from_payload(&serde_json::json!({
            "version": "1", "url": "/x", "sha256": "abc"
        }))
        .is_none());
        assert!(UpgradeTask::from_payload(&serde_json::json!({
            "url": "/x", "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        }))
        .is_none());
        assert!(UpgradeTask::from_payload(&serde_json::json!({})).is_none());
    }

    /// 畸形 CDN base 不会让整条任务解析失败（relay 可能给多个源，坏一个不该
    /// 拦住全部），但**不能**进白名单、也不能被拼成请求。
    #[test]
    fn test_payload_with_malformed_cdn_base_is_excluded_not_normalized() {
        let hash = {
            use sha2::Digest;
            hex::encode(sha2::Sha256::digest(ARTIFACT))
        };
        let name = local_filename();
        let task = UpgradeTask::from_payload(&serde_json::json!({
            "target_version": "9.9.9",
            "filename": name,
            "size": ARTIFACT.len(),
            "sha256": hash,
            "official_path": format!("/zzttzzmyswy/shell-remote/releases/download/v9.9.9/{name}"),
            "cdn_base_urls": [
                "https://user:pass@evil.test",       // userinfo
                "https://cdn.test/?x=1",             // query
                " https://cdn.test",                 // 前后空白
                "https://cdn.test/mirror",           // 带路径
                "https://good.test",                 // 唯一合法的
            ],
        }))
        .expect("任务本身仍可解析");
        // 白名单里只剩真正合法的那个 host。
        assert_eq!(task.cdn_hosts(), vec!["good.test".to_string()]);
        let sources = task.sources();
        assert_eq!(sources.len(), 1, "只有合法 base 会变成下载源: {sources:?}");
        assert!(sources[0].url.starts_with("https://good.test/"));
        // 任何拼出来的 URL 都不带着凭据/查询。
        for s in &sources {
            assert!(!s.url.contains("user:pass"));
            assert!(!s.url.contains('?'));
        }
    }

    /// official_path 与任务制品名不符时整条任务拒绝（不能把 UI 路径配给 CLI）。
    #[test]
    fn test_payload_with_mismatched_official_path_is_rejected() {
        let hash = {
            use sha2::Digest;
            hex::encode(sha2::Sha256::digest(ARTIFACT))
        };
        let task = UpgradeTask::from_payload(&serde_json::json!({
            "target_version": "9.9.9",
            "filename": local_filename(),
            "size": ARTIFACT.len(),
            "sha256": hash,
            // 指向另一个制品
            "official_path": "/zzttzzmyswy/shell-remote/releases/download/v9.9.9/shell-remote-ui-x86_64",
            "cdn_base_urls": ["https://cdn.test"],
            "relay_url": format!("/agent/upgrade/blob/{}", local_filename()),
        }));
        assert!(
            task.is_none(),
            "official_path 与 filename 不符必须整条拒绝，而不是静默只走 relay"
        );
    }

    #[test]
    fn test_custom_source_never_uses_cdn() {
        let mut task = task_with(
            &local_filename(),
            ARTIFACT,
            vec!["https://cdn.test".into()],
            Some(format!("/agent/upgrade/blob/{}", local_filename())),
        );
        task.source = "custom".into();
        let sources = task.sources();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind, SourceKind::Relay);
        assert!(task.cdn_hosts().is_empty());
    }

    #[test]
    fn test_join_cdn_url_rejects_unsafe_shapes() {
        assert!(join_cdn_url("https://cdn.test", "/a/b").is_some());
        assert!(join_cdn_url("https://cdn.test/", "/a/b").is_some());
        // 非 http(s)
        assert!(join_cdn_url("ftp://cdn.test", "/a").is_none());
        // 路径穿越 / 反斜杠 / 控制字符
        assert!(join_cdn_url("https://cdn.test", "/../etc/passwd").is_none());
        assert!(join_cdn_url("https://cdn.test", "a/b").is_none());
        assert!(join_cdn_url("https://cdn.test", "/a\\b").is_none());
        assert!(join_cdn_url("https://cdn.test", "/a\nb").is_none());
        assert!(join_cdn_url("", "/a").is_none());
        assert!(join_cdn_url("https://", "/a").is_none());
    }

    #[test]
    fn test_cdn_hosts_are_the_allowlist() {
        let task = task_with(
            &local_filename(),
            ARTIFACT,
            vec![
                "https://cdn-a.test".into(),
                "https://cdn-b.test:8443".into(),
            ],
            Some("/agent/upgrade/blob/x".into()),
        );
        assert_eq!(task.cdn_hosts(), vec!["cdn-a.test", "cdn-b.test"]);
    }

    #[test]
    fn test_artifact_identity_rejects_wrong_variant_and_arch() {
        let arch = own_arch_key();
        let ext = if cfg!(windows) { ".exe" } else { "" };
        let cli = format!("shell-remote-{arch}{ext}");
        let ui = format!("shell-remote-ui-{arch}{ext}");
        let other_arch = format!("shell-remote-otherarch{ext}");

        // 自己形态匹配：通过
        assert!(verify_artifact_identity(
            &task_with(&cli, ARTIFACT, vec![], None),
            Some(AgentVariant::Cli)
        )
        .is_ok());
        assert!(verify_artifact_identity(
            &task_with(&ui, ARTIFACT, vec![], None),
            Some(AgentVariant::Ui)
        )
        .is_ok());

        // 形态串包：拒绝
        let err = verify_artifact_identity(
            &task_with(&ui, ARTIFACT, vec![], None),
            Some(AgentVariant::Cli),
        )
        .unwrap_err();
        assert!(
            err.contains("is the ui build but this agent is cli"),
            "{err}"
        );

        // 架构不符：拒绝
        let err = verify_artifact_identity(
            &task_with(&other_arch, ARTIFACT, vec![], None),
            Some(AgentVariant::Cli),
        )
        .unwrap_err();
        assert!(err.contains("targets arch otherarch"), "{err}");

        // 不是发布制品名：拒绝
        let err =
            verify_artifact_identity(&task_with("evil-payload", ARTIFACT, vec![], None), None)
                .unwrap_err();
        assert!(err.contains("not a shell-remote release artifact"), "{err}");

        // 未知自身形态：**必须拒绝**，不能因为"不知道"就放行——放行等于允许把
        // UI 设备换成 lean CLI（或反之），静默丢能力。
        let err =
            verify_artifact_identity(&task_with(&ui, ARTIFACT, vec![], None), None).unwrap_err();
        assert!(
            err.contains("cannot determine this agent's own artifact variant"),
            "{err}"
        );
        assert!(err.contains("refusing to install it"), "{err}");
        // 提示里给可操作的下一步，而不是只说"失败"。
        assert!(err.contains("release filename"), "{err}");
        assert!(err.contains("manually"), "{err}");
        // CLI 制品在形态未知时同样拒绝（两端都不猜）。
        assert!(verify_artifact_identity(&task_with(&cli, ARTIFACT, vec![], None), None).is_err());
    }

    #[test]
    fn test_variant_detection_by_exe_name() {
        assert_eq!(
            AgentVariant::from_exe_name("shell-remote-ui-x86_64"),
            Some(AgentVariant::Ui)
        );
        assert_eq!(
            AgentVariant::from_exe_name("shell-remote-ui.exe"),
            Some(AgentVariant::Ui)
        );
        assert_eq!(
            AgentVariant::from_exe_name("shell-remote-aarch64"),
            Some(AgentVariant::Cli)
        );
        assert_eq!(
            AgentVariant::from_exe_name("shell-remote.exe"),
            Some(AgentVariant::Cli)
        );
        // UI 名必须先于 CLI 前缀判定（`shell-remote-ui-*` 也以 `shell-remote` 开头）
        assert_eq!(
            AgentVariant::from_exe_name("shell-remote-ui-armv7"),
            Some(AgentVariant::Ui)
        );
        // 改名/未知名字：含桌面代码时**不声明**（宁可被 relay 拒绝，也不冒充）。
        #[cfg(feature = "desktop")]
        assert_eq!(AgentVariant::from_exe_name("my-renamed-agent"), None);
        // 近似但不确切的名字不能被当成 CLI 发布产物。
        #[cfg(feature = "desktop")]
        assert_eq!(
            AgentVariant::from_exe_name("shell-remote-custom-build"),
            None
        );
        #[cfg(feature = "desktop")]
        assert_eq!(AgentVariant::from_exe_name("not-shell-remote-x86_64"), None);
        assert_eq!(CAPABILITY_VARIANT_CLI, "variant:cli");
        assert_eq!(CAPABILITY_VARIANT_UI, "variant:ui");
        assert_eq!(AgentVariant::Cli.capability(), "variant:cli");
        assert_eq!(AgentVariant::Ui.as_str(), "ui");
    }

    // ── 代理 ────────────────────────────────────────────────────

    #[test]
    fn test_proxy_config_env_precedence_and_bypass() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(&str, Option<String>)> = [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ]
        .iter()
        .map(|k| (*k, std::env::var(k).ok()))
        .collect();
        for k in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ] {
            std::env::remove_var(k);
        }

        assert_eq!(ProxyConfig::from_env(), ProxyConfig::empty());
        assert!(ProxyConfig::from_env().is_empty());

        std::env::set_var("HTTPS_PROXY", "http://proxy.corp:3128");
        std::env::set_var("NO_PROXY", "localhost,127.0.0.1,.internal");
        let cfg = ProxyConfig::from_env();
        assert_eq!(cfg.https_proxy.as_deref(), Some("http://proxy.corp:3128"));
        assert!(cfg.http_proxy.is_none());
        assert!(cfg.bypasses("127.0.0.1"), "NO_PROXY 内的地址必须直连");
        assert!(cfg.bypasses("relay.internal"));
        assert!(!cfg.bypasses("cdn.test"));

        // 空字符串视为未设置（很多环境会导出空变量）。
        std::env::set_var("ALL_PROXY", "   ");
        assert!(ProxyConfig::from_env().all_proxy.is_none());

        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    /// 显式 HTTP_PROXY 必须真的生效：请求打到代理而不是直连 CDN。
    #[tokio::test]
    async fn test_explicit_http_proxy_is_used() {
        // 一个"代理"：收到绝对 URI 请求就返回制品字节，并记录它看到的 URL。
        #[derive(Clone)]
        struct ProxyState {
            seen: Arc<Mutex<Vec<String>>>,
            bytes: Arc<Vec<u8>>,
        }
        async fn proxy_handler(
            State(s): State<ProxyState>,
            uri: axum::http::Uri,
            headers: HeaderMap,
        ) -> Response {
            s.seen.lock().unwrap().push(uri.to_string());
            // 代理不应该收到任何 relay 凭据。
            assert!(
                !headers.keys().any(|k| k.as_str() == "x-sr-token"),
                "代理视角看到了 relay Token"
            );
            Response::builder()
                .status(StatusCode::OK)
                .header("content-length", s.bytes.len().to_string())
                .body(Body::from(s.bytes.to_vec()))
                .unwrap()
        }
        let state = ProxyState {
            seen: Arc::new(Mutex::new(Vec::new())),
            bytes: Arc::new(ARTIFACT.to_vec()),
        };
        // 代理视角：`axum::routing::any` + 三个 FromRequestParts 提取器即可
        // （没有 body 消费，不受「最后一个提取器消费 body」的限制）。
        let app = Router::new()
            .fallback(axum::routing::any(proxy_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let proxy = ProxyConfig {
            http_proxy: Some(format!("http://{proxy_addr}")),
            ..Default::default()
        };
        let b = budget();
        let external = Downloader::build_external_client(&proxy, None, &b, Vec::new()).unwrap();
        let dl = Downloader::new(external, None, b, no_progress());
        // CDN 用一个**不存在**的主机：只有走代理才可能成功。
        let task = task_with(
            &local_filename(),
            ARTIFACT,
            vec!["http://cdn-does-not-exist.invalid".to_string()],
            None,
        );
        let part = tmp_part("proxy");
        let got = dl.fetch(&task, &part).await;
        let seen = state.seen.lock().unwrap().clone();
        assert_eq!(
            seen.len(),
            1,
            "代理应收到且仅收到一次请求（实际 {seen:?}，下载结果 {got:?}）"
        );
        assert!(
            seen[0].contains("cdn-does-not-exist.invalid"),
            "代理收到的是绝对 URI: {}",
            seen[0]
        );
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    /// NO_PROXY 命中时必须直连（relay 内网地址不能被送去代理）。
    #[tokio::test]
    async fn test_no_proxy_bypasses_configured_proxy() {
        let (cdn, _) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        // 指向一个死端口：如果代理被使用，下载必然失败。
        let proxy = ProxyConfig {
            http_proxy: Some("http://127.0.0.1:1".to_string()),
            no_proxy: Some("127.0.0.1,localhost".to_string()),
            ..Default::default()
        };
        let b = budget();
        let external = Downloader::build_external_client(&proxy, None, &b, Vec::new()).unwrap();
        let dl = Downloader::new(external, None, b, no_progress());
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn], None);
        let part = tmp_part("noproxy");
        dl.fetch(&task, &part).await.expect("NO_PROXY 命中应直连");
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    // ── 跨源 .part 隔离（review #1）─────────────────────────────

    /// 源 A 断流后，源 B **不得**从 A 的偏移续传：两者返回的字节不同时，
    /// 共享 `.part` 会拼出「A 的头 + B 的尾」。分源命名必须让 B 从 0 开始。
    #[tokio::test]
    async fn test_second_source_never_resumes_from_first_sources_bytes() {
        // A：返回前半段一致、后半段不同的字节，并在发完前半段后静默断开。
        let liar_bytes: Arc<Vec<u8>> = Arc::new(ARTIFACT.to_vec());
        let mut evil = liar_bytes.as_ref().clone();
        let half = evil.len() / 2;
        for b in evil.iter_mut().skip(half) {
            *b = b'X';
        }
        let evil = Arc::new(evil);
        let (source_a, a_state) = spawn_cdn(CdnBehaviour {
            bytes: evil.clone(),
            supports_range: true,
            truncate_after: Some(half),
            ..Default::default()
        })
        .await;
        // B：正确的字节，支持 Range。
        let (source_b, b_state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![source_a, source_b], None);
        let part = tmp_part("cross-source-isolation");
        let mut b = budget();
        b.attempts_per_source = 1; // 让 A 断流后立刻换源
        b.chunk = Duration::from_millis(300);

        let got = downloader(None, b).fetch(&task, &part).await.unwrap();

        // A 确实先被请求过（并且是 0 起）。
        let a_ranges = a_state.ranges.lock().unwrap().clone();
        assert_eq!(
            a_ranges.first().and_then(|r| r.clone()),
            None,
            "A 首次应全量"
        );

        // B 必须从 0 开始——绝不能带 A 的偏移。
        let b_ranges = b_state.ranges.lock().unwrap().clone();
        assert!(
            b_ranges.iter().all(|r| r.is_none()),
            "源 B 不得从源 A 的字节续传（实际请求区间 {b_ranges:?}）"
        );
        assert_eq!(got.source, "cdn:127.0.0.1");
        assert_eq!(
            std::fs::read(&part).unwrap(),
            ARTIFACT,
            "最终必须是被校验过的完整制品"
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    /// 每个源的续传文件互不相同，且同源重复调用得到同一路径（才谈得上续传）。
    #[test]
    fn test_part_namespaces_are_per_source_and_stable() {
        let task = task_with(
            &local_filename(),
            ARTIFACT,
            vec!["https://cdn-a.test".into(), "https://cdn-b.test".into()],
            Some(format!("/agent/upgrade/blob/{}", local_filename())),
        );
        let sources = task.sources();
        assert_eq!(sources.len(), 3);
        let part = tmp_part("ns");
        let paths: Vec<PathBuf> = sources
            .iter()
            .map(|s| part_path_for(&part, s).unwrap())
            .collect();
        // 三个源三个不同的续传文件。
        for i in 0..paths.len() {
            for j in (i + 1)..paths.len() {
                assert_ne!(paths[i], paths[j], "源 {i} 与源 {j} 不能共用续传文件");
            }
        }
        // 同源稳定（这是续传的前提）。
        assert_eq!(part_path_for(&part, &sources[0]).unwrap(), paths[0]);
        // 都落在调用方给的 part 所在目录（同文件系统，最终 rename 原子）。
        for p in &paths {
            assert_eq!(p.parent(), part.parent());
        }
        // 命名里不含路径分隔符（不会因 host 带 `:` 而跑出目录）。
        for p in &paths {
            let name = p.file_name().unwrap().to_str().unwrap();
            assert!(!name.contains('/') && !name.contains('\\'), "{name}");
        }
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    /// 同源重试仍能续传（分源命名不能把续传能力一起砍掉）。
    #[tokio::test]
    async fn test_same_source_still_resumes_across_attempts() {
        let (cdn, state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn], None);
        let part = tmp_part("same-source-resume");
        let half = ARTIFACT.len() / 2;
        std::fs::write(source_part(&task, &part, 0), &ARTIFACT[..half]).unwrap();

        let got = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        assert_eq!(got.resumed_from, half as u64);
        assert_eq!(
            state.ranges.lock().unwrap()[0].as_deref(),
            Some(format!("bytes={half}-").as_str())
        );
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    /// 续传文件是符号链接 / FIFO / 目录时必须拒绝：共享可写目录里跟随链接
    /// 会覆盖链接指向的任意文件。
    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlinked_part_is_refused_not_followed() {
        let (cdn, _state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn], None);
        let part = tmp_part("symlink");
        let victim = part.parent().unwrap().join("victim.txt");
        std::fs::write(&victim, b"must-not-be-overwritten").unwrap();
        // 把该源的续传路径做成指向受害文件的符号链接。
        let link = source_part(&task, &part, 0);
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        let err = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap_err();
        assert!(err.contains("symbolic link"), "{err}");
        // 受害文件内容不变（没有被跟着写）。
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"must-not-be-overwritten",
            "绝不能跟随符号链接覆盖目标文件"
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_non_regular_part_is_refused() {
        let (cdn, _state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn], None);
        let part = tmp_part("fifo");
        let target = source_part(&task, &part, 0);
        // 目录也算"非普通文件"，且不需要 mkfifo 权限。
        std::fs::create_dir_all(&target).unwrap();

        let err = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[test]
    fn test_ensure_regular_part_file_accepts_missing_and_regular() {
        let dir = std::env::temp_dir().join(format!("sr-dl-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing.part");
        assert!(
            ensure_regular_part_file(&missing).is_ok(),
            "不存在是可写前提"
        );
        let regular = dir.join("regular.part");
        std::fs::write(&regular, b"x").unwrap();
        assert!(ensure_regular_part_file(&regular).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 成功后其它源的残片被清掉（每个都可能几十 MB），保留的那个已改名。
    #[tokio::test]
    async fn test_sibling_parts_are_cleaned_after_success() {
        let (cdn, _) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![cdn.clone(), cdn], None);
        let part = tmp_part("sibling-cleanup");
        // 两个源各留一段残片。
        for i in 0..2 {
            std::fs::write(source_part(&task, &part, i), b"stale").unwrap();
        }
        let dir = part.parent().unwrap().to_path_buf();
        downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap();
        // 目录里只剩最终的 part（残片都清了）。
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".part"))
            .collect();
        assert_eq!(leftovers, vec!["artifact.part".to_string()], "残片应被清理");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── relay 路径同源校验（review #2）───────────────────────────

    #[test]
    fn test_relay_path_rejects_offsite_and_malformed_forms() {
        let name = local_filename();
        let good = format!("/agent/upgrade/blob/{name}");
        assert_eq!(validate_relay_path(&good).unwrap(), good);

        // 每一条都能让带 Token 的请求离开 relay origin，或落到别的端点。
        for bad in [
            "//evil.example/path".to_string(),
            "/agent//evil.example/x".to_string(),
            "https://evil.example/agent/upgrade/blob/shell-remote-x86_64".to_string(),
            "http://evil.example/agent/upgrade/blob/shell-remote-x86_64".to_string(),
            "agent/upgrade/blob/shell-remote-x86_64".to_string(),
            format!("/agent/upgrade/blob/../../etc/passwd"),
            format!("/agent/upgrade/blob/{name}\\\\..\\\\x"),
            format!("/agent/upgrade/blob/{name}?x=1"),
            format!("/agent/upgrade/blob/{name}#frag"),
            format!("/agent/upgrade/blob/%2e%2e/{name}"),
            "/agent/upgrade/blob/".to_string(),
            "/agent/upgrade/blob/sub/dir".to_string(),
            "/download/shell-remote-x86_64".to_string(),
            "/agent/upgrade/blob/not-an-artifact".to_string(),
            "/agent/upgrade/blob/".to_string() + &name + " ",
            String::new(),
        ] {
            assert!(
                validate_relay_path(&bad).is_err(),
                "必须拒绝不安全/不合规的 relay 路径: {bad:?}"
            );
        }
    }

    /// 老 relay 的 `url` 走**同一条**校验：兼容不等于放宽。
    #[test]
    fn test_legacy_url_is_validated_like_relay_url() {
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        // 合法老 payload：能解析。
        let ok = UpgradeTask::from_payload(&serde_json::json!({
            "version": "0.19.0",
            "url": "/agent/upgrade/blob/shell-remote-x86_64",
            "sha256": hash,
        }));
        assert!(ok.is_some());

        // 把 url 换成协议相对 / 绝对 URL：整条任务必须被拒绝。
        for bad in [
            "//evil.example/agent/upgrade/blob/shell-remote-x86_64",
            "https://evil.example/agent/upgrade/blob/shell-remote-x86_64",
            "/download/shell-remote-x86_64",
        ] {
            let task = UpgradeTask::from_payload(&serde_json::json!({
                "version": "0.19.0",
                "url": bad,
                "sha256": hash,
            }));
            assert!(task.is_none(), "老 url 也必须校验，实际接受了 {bad:?}");
        }
    }

    /// 带 Token 的请求绝不能因为路径走私而发到外部主机（端到端回归）。
    #[tokio::test]
    async fn test_token_never_leaves_relay_on_offsite_path() {
        let (external, external_state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let hash = {
            use sha2::Digest;
            hex::encode(sha2::Sha256::digest(ARTIFACT))
        };
        // 攻击形态：任务声称 relay_url 指向外部主机。
        let payload = serde_json::json!({
            "target_version": "9.9.9",
            "filename": local_filename(),
            "size": ARTIFACT.len(),
            "sha256": hash,
            // 协议相对：字符串拼接后会把请求带出 relay origin。
            "relay_url": format!("//{}/agent/upgrade/blob/{}",
                external.trim_start_matches("http://"), local_filename()),
        });
        let task = UpgradeTask::from_payload(&payload);
        assert!(task.is_none(), "协议相对 relay_url 必须在解析期就被拒绝");

        // 直接构造的任务（绕过 from_payload）也必须不会把 Token 发出去。
        let mut manual = task_with(
            &local_filename(),
            ARTIFACT,
            vec![],
            Some(format!(
                "//{}/agent/upgrade/blob/{}",
                external.trim_start_matches("http://"),
                local_filename()
            )),
        );
        manual.official_path = None;
        assert!(
            manual.sources().is_empty(),
            "sources() 必须拒绝不安全的 relay 路径，而不是拼出去"
        );

        // 即便强行调用，外部主机也不会收到任何带 Token 的请求。
        let relay_src = RelaySource {
            http: reqwest::Client::new(),
            base: "http://127.0.0.1:1".to_string(),
            token: "rw-secret".to_string(),
        };
        let part = tmp_part("token-offsite");
        let _ = downloader(Some(relay_src), budget())
            .fetch(&manual, &part)
            .await;
        assert_eq!(
            external_state.requests.load(Ordering::SeqCst),
            0,
            "外部主机不应收到任何请求（更不该带 Token）"
        );
        for hdrs in external_state.headers.lock().unwrap().iter() {
            assert!(
                !hdrs
                    .iter()
                    .any(|(k, v)| k == "x-sr-token" || v.contains("rw-secret")),
                "Token 泄漏到外部主机"
            );
        }
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    /// relay 下载必须**禁止重定向**：带 Token 跟随 302 就等于把 Token 送走。
    #[tokio::test]
    async fn test_relay_client_does_not_follow_redirects() {
        // 一个"跳板"：任何请求都 302 到别处，并记录是否被请求过。
        #[derive(Clone)]
        struct Hop {
            hits: Arc<AtomicUsize>,
            target: String,
        }
        async fn hop_handler(State(s): State<Hop>) -> Response {
            s.hits.fetch_add(1, Ordering::SeqCst);
            Response::builder()
                .status(StatusCode::FOUND)
                .header("location", s.target.clone())
                .header("content-length", "0")
                .body(Body::empty())
                .unwrap()
        }
        let (sink, sink_state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let hop = Hop {
            hits: Arc::new(AtomicUsize::new(0)),
            target: format!("{sink}/stolen"),
        };
        let app = Router::new()
            .fallback(axum::routing::any(hop_handler))
            .with_state(hop.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let task = task_with(
            &local_filename(),
            ARTIFACT,
            vec![],
            Some(format!("/agent/upgrade/blob/{}", local_filename())),
        );
        // 与 upgrade.rs 里生产路径一致：relay 客户端 redirect(none)。
        let relay_src = RelaySource {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            base: format!("http://{addr}"),
            token: "rw-secret".to_string(),
        };
        let part = tmp_part("relay-redirect");
        let err = downloader(Some(relay_src), budget())
            .fetch(&task, &part)
            .await
            .unwrap_err();
        assert!(err.contains("302") || err.contains("HTTP 3"), "{err}");
        assert_eq!(hop.hits.load(Ordering::SeqCst), 1, "跳板应只被请求一次");
        assert_eq!(
            sink_state.requests.load(Ordering::SeqCst),
            0,
            "relay 请求不得跟随重定向到别处"
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    // ── 外部重定向白名单（review #4）─────────────────────────────

    #[tokio::test]
    async fn test_external_redirect_to_unlisted_host_is_refused() {
        // 跳板（在 127.0.0.1，但**不在**任务白名单里）→ 指向另一个 host。
        // 用 localhost 与 127.0.0.1 区分「同 IP 不同 host」，正好测 host 白名单。
        let (sink, sink_state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        #[derive(Clone)]
        struct Hop {
            target: String,
        }
        async fn hop_handler(State(s): State<Hop>) -> Response {
            Response::builder()
                .status(StatusCode::FOUND)
                .header("location", s.target.clone())
                .header("content-length", "0")
                .body(Body::empty())
                .unwrap()
        }
        let app = Router::new()
            .fallback(axum::routing::any(hop_handler))
            .with_state(Hop {
                target: format!("{sink}/elsewhere"),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // 白名单里只放 localhost（不含 127.0.0.1）→ 跳到 127.0.0.1 的 sink 必须被拒。
        let task = task_with(
            &local_filename(),
            ARTIFACT,
            vec![format!("http://{addr}")],
            None,
        );
        let part = tmp_part("redirect-unlisted");
        let err = downloader_with_hosts(None, budget(), vec!["localhost".to_string()])
            .fetch(&task, &part)
            .await
            .unwrap_err();
        assert!(
            err.contains("refusing redirect") && err.contains("not one of this upgrade task"),
            "{err}"
        );
        assert_eq!(
            sink_state.requests.load(Ordering::SeqCst),
            0,
            "白名单外的主机不得收到请求"
        );
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_external_redirect_within_allowed_host_is_followed() {
        // 白名单内的跳转（同一 host，路径不同）必须放行：真实镜像会 302 到
        // 对象存储，完全禁止会打断正常下载。
        let (sink, sink_state) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            supports_range: true,
            ..Default::default()
        })
        .await;
        let sink_host = "127.0.0.1".to_string();
        #[derive(Clone)]
        struct Hop {
            target: String,
        }
        async fn hop_handler(State(s): State<Hop>) -> Response {
            Response::builder()
                .status(StatusCode::FOUND)
                .header("location", s.target.clone())
                .header("content-length", "0")
                .body(Body::empty())
                .unwrap()
        }
        let app = Router::new()
            .fallback(axum::routing::any(hop_handler))
            .with_state(Hop {
                target: format!("{sink}/final"),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let task = task_with(
            &local_filename(),
            ARTIFACT,
            vec![format!("http://{addr}")],
            None,
        );
        let part = tmp_part("redirect-allowed");
        downloader_with_hosts(None, budget(), vec![sink_host])
            .fetch(&task, &part)
            .await
            .expect("白名单内跳转应跟随");
        assert_eq!(std::fs::read(&part).unwrap(), ARTIFACT);
        assert!(sink_state.requests.load(Ordering::SeqCst) >= 1);
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[test]
    fn test_redirect_policy_empty_allowlist_refuses_everything() {
        // 空白名单 = 一个跳转都不跟随（保守默认）。
        let policy = redirect_policy(Vec::new());
        // 直接构造 Attempt 不可行，因此用行为断言：把策略装进客户端，
        // 对一个必然 302 的本地服务发请求，必须失败。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            #[derive(Clone, Default)]
            struct Hop {
                hits: Arc<AtomicUsize>,
            }
            async fn hop_handler(State(s): State<Hop>) -> Response {
                s.hits.fetch_add(1, Ordering::SeqCst);
                Response::builder()
                    .status(StatusCode::FOUND)
                    .header("location", "http://127.0.0.1:1/elsewhere")
                    .header("content-length", "0")
                    .body(Body::empty())
                    .unwrap()
            }
            let hop = Hop::default();
            let app = Router::new()
                .fallback(axum::routing::any(hop_handler))
                .with_state(hop.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            let client = reqwest::Client::builder().redirect(policy).build().unwrap();
            let err = client
                .get(format!("http://{addr}/x"))
                .send()
                .await
                .unwrap_err();
            assert!(
                err.is_redirect() || err.to_string().contains("redirect"),
                "{err}"
            );
        });
    }

    // ── CDN URL 校验（review #5）────────────────────────────────

    #[test]
    fn test_normalize_cdn_base_rejects_malformed_origins() {
        // 合法：裸 origin，可带显式端口（测试用本地 mock）。
        for ok in [
            "https://cdn.test",
            "https://cdn.test:8443",
            "http://127.0.0.1:47812",
            "https://cdn.test/", // 单个结尾斜杠等价
        ] {
            let parsed = normalize_cdn_base(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
            assert!(parsed.host_str().is_some());
        }
        // 非法：userinfo / path / query / fragment / 非 http(s) / 无 host。
        for bad in [
            "https://user:pass@cdn.test",
            "https://user@cdn.test",
            "https://cdn.test/path",
            "https://cdn.test/?x=1",
            "https://cdn.test/#frag",
            "https://cdn.test/a/b",
            "ftp://cdn.test",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "cdn.test",
            "https://",
            "   ",
        ] {
            assert!(
                normalize_cdn_base(bad).is_err(),
                "必须拒绝畸形 CDN base: {bad:?}"
            );
        }
    }

    #[test]
    fn test_validate_official_path_rejects_malformed_and_mismatched() {
        let name = local_filename();
        let good = format!("/zzttzzmyswy/shell-remote/releases/download/v0.59.2/{name}");
        assert_eq!(validate_official_path(&good, &name).unwrap(), good);

        for bad in [
            // 相对 / 协议相对
            format!("zzttzzmyswy/shell-remote/releases/download/v0.59.2/{name}"),
            format!("//zzttzzmyswy/shell-remote/releases/download/v0.59.2/{name}"),
            // query / fragment
            format!("/zzttzzmyswy/shell-remote/releases/download/v0.59.2/{name}?x=1"),
            format!("/zzttzzmyswy/shell-remote/releases/download/v0.59.2/{name}#f"),
            // 穿越 / 编码 / 反斜杠
            "/zzttzzmyswy/shell-remote/releases/download/v0.59.2/../../etc/passwd".to_string(),
            "/zzttzzmyswy/shell-remote/releases/download/v0.59.2/%2e%2e/passwd".to_string(),
            format!("/zzttzzmyswy/shell-remote/releases/download/v0.59.2/{name}\\x"),
            // 结构不对
            format!("/zzttzzmyswy/shell-remote/{name}"),
            format!("/releases/download/v0.59.2/{name}"),
            // 版本段不是 v<ver>
            format!("/zzttzzmyswy/shell-remote/releases/download/0.59.2/{name}"),
            // 空白/控制字符
            format!("/zzttzzmyswy/shell-remote/releases/download/v0.59.2/{name} "),
            "/zzttzzmyswy/shell-remote/releases/download/v0.59.2/\t".to_string(),
        ] {
            assert!(
                validate_official_path(&bad, &name).is_err(),
                "必须拒绝畸形 official_path: {bad:?}"
            );
        }

        // 指向另一个制品（把 UI 路径配到 CLI 任务上）：拒绝。
        let ui =
            format!("/zzttzzmyswy/shell-remote/releases/download/v0.59.2/shell-remote-ui-x86_64");
        assert!(validate_official_path(&ui, "shell-remote-x86_64").is_err());
    }

    /// `join_cdn_url` 拼出来的 URL 必须仍属于 base 的 origin。
    #[test]
    fn test_join_cdn_url_preserves_origin_and_drops_query() {
        let name = local_filename();
        let path = format!("/zzttzzmyswy/shell-remote/releases/download/v9.9.9/{name}");
        let joined = join_cdn_url("https://cdn.test:8443", &path).unwrap();
        let parsed = url::Url::parse(&joined).unwrap();
        assert_eq!(parsed.host_str(), Some("cdn.test"));
        assert_eq!(parsed.port(), Some(8443));
        assert_eq!(parsed.scheme(), "https");
        assert_eq!(parsed.path(), path);
        assert!(parsed.query().is_none());
        assert!(parsed.fragment().is_none());

        // base 带 userinfo / query / path → 整体拒绝（不会静默"修好"）。
        for base in [
            "https://user:pass@cdn.test",
            "https://cdn.test/?x=1",
            "https://cdn.test/#f",
            "https://cdn.test/mirror",
        ] {
            assert!(
                join_cdn_url(base, &path).is_none(),
                "带 userinfo/query/path 的 base 必须被拒: {base:?}"
            );
        }
        // path 带 query/fragment/穿越/编码 → 拒绝。
        for bad_path in [
            format!("{path}?x=1"),
            format!("{path}#f"),
            "/a/../b".to_string(),
            "/a/%2e%2e/b".to_string(),
            "//evil.test/x".to_string(),
            format!("{path} "),
        ] {
            assert!(
                join_cdn_url("https://cdn.test", &bad_path).is_none(),
                "畸形 official path 必须被拒: {bad_path:?}"
            );
        }
    }

    // ── 预算 ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_total_budget_bounds_all_attempts() {
        // 慢源 + 极小总预算：必须在预算内失败，不能挂死。
        let (slow, _) = spawn_cdn(CdnBehaviour {
            bytes: Arc::new(ARTIFACT.to_vec()),
            stall_after_headers: true,
            ..Default::default()
        })
        .await;
        let task = task_with(&local_filename(), ARTIFACT, vec![slow.clone(), slow], None);
        let mut b = budget();
        b.total = Duration::from_millis(600);
        b.chunk = Duration::from_millis(200);
        b.attempts_per_source = 3;
        let part = tmp_part("budget");
        let started = Instant::now();
        let err = downloader(None, b).fetch(&task, &part).await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "总预算必须封顶（实际 {:?}）",
            started.elapsed()
        );
        assert!(err.contains("stalled") || err.contains("budget"), "{err}");
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[test]
    fn test_parse_content_range() {
        assert_eq!(
            parse_content_range("bytes 100-199/1000"),
            Some((100, 199, 1000))
        );
        assert_eq!(parse_content_range("bytes 0-0/1"), Some((0, 0, 1)));
        // 形态不合法一律 None（宁可失败也不猜偏移）
        assert_eq!(parse_content_range("bytes 100-199/*"), None);
        assert_eq!(parse_content_range("bytes 50-10/100"), None);
        assert_eq!(parse_content_range("bytes 0-10/0"), None);
        assert_eq!(parse_content_range("items 0-10/100"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn test_sha256_helpers() {
        let dir = std::env::temp_dir().join(format!("sr-dl-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("d.bin");
        std::fs::write(&f, b"abc").unwrap();
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_hex_file(&f).unwrap(), good);
        assert!(verify_sha256(good, good));
        assert!(verify_sha256(&good.to_uppercase(), good));
        assert!(!verify_sha256(&format!("f{}", &good[1..]), good));
        assert!(!verify_sha256(good, "short"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_oversized_task_size_is_refused_before_network() {
        let mut task = task_with(&local_filename(), ARTIFACT, vec![dead_base()], None);
        task.size = MAX_ARTIFACT_BYTES + 1;
        let part = tmp_part("toobig");
        let err = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap_err();
        assert!(err.contains("above the"), "{err}");
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }

    #[tokio::test]
    async fn test_task_without_sources_is_refused() {
        let mut task = task_with(&local_filename(), ARTIFACT, vec![], None);
        task.official_path = None;
        let part = tmp_part("nosrc");
        let err = downloader(None, budget())
            .fetch(&task, &part)
            .await
            .unwrap_err();
        assert!(err.contains("no download source"), "{err}");
        let _ = std::fs::remove_dir_all(part.parent().unwrap());
    }
}
