//! 统一制品库与升级任务清单（MYS-2425 阶段 1）。
//!
//! relay 侧只有**一份**制品存储：`--download-dir`（`--agent-upgrade-dir` 作为
//! 兼容别名指向同一目录）。同一批字节有两个读取入口，权限边界不合并：
//!
//! | 入口 | 用途 | 鉴权 |
//! |---|---|---|
//! | `/download/<filename>` | 安装脚本公开下载 | 无 |
//! | `/agent/upgrade/blob/<filename>` | 已注册 agent 自升级 | rw Token |
//!
//! 升级任务的可信哈希只有一个来源：**官方 GitHub release 的 asset digest**。
//! 本模块负责：
//!
//! 1. 读取官方 release 清单（有限体积 / 超时 / 重定向），解析 `digest: sha256:…`；
//! 2. 把清单原子缓存在制品库的 `.metadata/manifests/`（绝不通过 `/download` 暴露）；
//! 3. 计算本地镜像 hash，并按「官方 digest 交叉校验 / 管理员批准的自定义构建」
//!    决定任务来源标记；
//! 4. 用 semver 比较 relay 与 agent 版本，决定是否需要升级；
//! 5. 生成不可变的升级任务（版本 + 制品身份 + 字节长度 + SHA-256 + CDN 列表）。
//!
//! 信任边界：CDN、DoH、relay 代解析都只解决**可达性**；版本与字节由 relay 依据
//! 官方 digest 决定，agent 端必须自行完整校验 SHA-256。本模块不宣称能抵御官方
//! 仓库或 release 资产被恶意替换——那需要发布签名，不在本期范围。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 官方发布仓库（固定，不接受外部输入）。
pub const OFFICIAL_REPO: &str = "zzttzzmyswy/shell-remote";
/// GitHub REST API 根（测试可注入本地 mock）。
pub const OFFICIAL_API_BASE: &str = "https://api.github.com";
/// 官方下载根：`OFFICIAL_DOWNLOAD_BASE + official_path` 即 `browser_download_url`。
pub const OFFICIAL_DOWNLOAD_BASE: &str = "https://github.com";

/// release 清单响应的硬上限。真实清单 ~10KB，1MiB 已足够并挡住异常/恶意大响应。
pub const MANIFEST_MAX_BYTES: usize = 1024 * 1024;
/// 取清单的总超时。
pub const MANIFEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 取清单的连接超时。
pub const MANIFEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 取清单跟随的重定向上限。
pub const MANIFEST_MAX_REDIRECTS: usize = 3;

/// 首期预置 CDN 入口（`cdn_base_urls` 的默认值，relay 可用 `--upgrade-cdn` 覆盖）。
///
/// 每项都是「前缀式镜像」：`base + official_path` 即为该源的完整 URL。第一项是
/// 官方 origin，其后是 gh-proxy 系镜像（与 `web/install.sh` 的镜像列表同源，
/// 便于运维只维护一套认知）。条目顺序是 relay 的建议优先级；agent 侧可以按
/// 健康状态与地域调整，但不得改变制品身份。
pub const DEFAULT_CDN_BASE_URLS: &[&str] = &[
    "https://github.com",
    "https://edgeone.gh-proxy.com",
    "https://gh-proxy.com",
    "https://gh.llkk.cc",
];

/// 制品形态：CLI（lean，仅终端转发）或 UI（桌面 agent + 窗口面板）。
///
/// 两者是同一次 release 的不同产物，**升级时必须按当前运行的可执行文件选**，
/// 否则桌面能力会被换成 CLI 而静默丢失。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    Cli,
    Ui,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Ui => "ui",
        }
    }

    /// 制品文件名前缀（与 `tools/build-dist.sh` 的产物命名一致）。
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Cli => "shell-remote",
            Self::Ui => "shell-remote-ui",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cli" => Some(Self::Cli),
            "ui" => Some(Self::Ui),
            _ => None,
        }
    }
}

/// 任务来源。决定 agent 该信任到哪一步、以及能否走外部 CDN。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactSource {
    /// 官方 digest 可用、relay 无本地副本 → agent 从官方路径 / CDN 取字节。
    Official,
    /// relay 本地镜像，且（若有官方清单）已与官方 digest 交叉校验一致。
    Local,
    /// 本地镜像的 hash 与官方 digest 不一致或根本没有官方清单 → 管理员批准
    /// 的自定义构建，**不提供外部 CDN**，只走 relay 镜像。
    Custom,
}

impl ArtifactSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::Local => "local",
            Self::Custom => "custom",
        }
    }

    /// 这份字节是否具有官方 release 来源（custom 恒为 false）。
    pub fn is_official_release(self) -> bool {
        matches!(self, Self::Official | Self::Local)
    }
}

/// 一个具体的制品身份：形态 + 架构 + 是否 Windows。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    pub variant: Variant,
    /// 归一化架构 key（`x86_64` / `aarch64` / `armv7` / …）。
    pub arch: String,
    pub windows: bool,
}

impl ArtifactRef {
    /// 构造并归一化架构。架构名只允许 `[a-z0-9_]`，避免经设备上报/请求体注入
    /// 路径分隔符或 `..`。
    pub fn new(variant: Variant, arch_raw: &str, windows: bool) -> Option<Self> {
        let arch = normalize_arch(arch_raw);
        if arch.is_empty() || !arch.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        Some(Self {
            variant,
            arch,
            windows,
        })
    }

    /// 发布制品文件名，如 `shell-remote-ui-aarch64` / `shell-remote-x86_64.exe`。
    pub fn filename(&self) -> String {
        let ext = if self.windows { ".exe" } else { "" };
        format!("{}-{}{}", self.variant.prefix(), self.arch, ext)
    }

    /// 官方 CDN 上的相对路径（`browser_download_url` 去掉 `https://github.com`）。
    pub fn official_path(&self, version: &str) -> String {
        format!(
            "/{}/releases/download/v{}/{}",
            OFFICIAL_REPO,
            version,
            self.filename()
        )
    }
}

/// `uname -m` / `PROCESSOR_ARCHITECTURE` 的各种写法归一到一个发布产物 key。
///
/// 与 `relay::admin::artifact_arch_key` 同义（后者委托到这里），保留独立函数是
/// 为了让本模块自洽、可单测，不反向依赖 admin。
pub fn normalize_arch(arch: &str) -> String {
    match arch.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => "x86_64".to_string(),
        "aarch64" | "arm64" => "aarch64".to_string(),
        "armv7l" | "armv7" => "armv7".to_string(),
        other => other.to_string(),
    }
}

/// 解析制品文件名回 [`ArtifactRef`]。UI 前缀必须先于 CLI 前缀判断，否则
/// `shell-remote-ui-x86_64` 会被当成架构 `ui-x86_64`。
pub fn parse_artifact_ref(name: &str) -> Option<ArtifactRef> {
    let (variant, rest) = if let Some(r) = name.strip_prefix("shell-remote-ui-") {
        (Variant::Ui, r)
    } else if let Some(r) = name.strip_prefix("shell-remote-") {
        (Variant::Cli, r)
    } else {
        return None;
    };
    let windows = rest.ends_with(".exe");
    let arch = rest.strip_suffix(".exe").unwrap_or(rest);
    if arch.is_empty() {
        return None;
    }
    ArtifactRef::new(variant, arch, windows)
}

/// 官方 release 的一个资产。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfficialAsset {
    /// 资产名（必须与 [`ArtifactRef::filename`] 精确相等才被采用）。
    pub name: String,
    pub size: u64,
    /// 小写 hex SHA-256（来自 GitHub 的 `digest`）。
    pub sha256: String,
    /// GitHub 给出的下载地址（用于推导 `official_path`）。
    pub download_url: String,
}

/// 某个版本的官方清单（relay 内部元数据，不作为公开制品暴露）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfficialManifest {
    pub schema_version: u32,
    pub tag: String,
    pub version: String,
    /// 取到清单的 Unix 时间戳（秒）。
    pub fetched_at: u64,
    pub assets: Vec<OfficialAsset>,
}

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

impl OfficialManifest {
    /// 从 GitHub Release API 的 JSON 解析，只保留带合法 `sha256:` digest 的资产。
    ///
    /// `expected_version` 必须与响应里的 `tag_name`（`v<version>`）一致，防止
    /// 注入的版本号拿到别的 release。
    pub fn from_github_json(
        expected_version: &str,
        body: &serde_json::Value,
        now_unix: u64,
    ) -> Result<Self, String> {
        let tag = body
            .get("tag_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "release response missing tag_name".to_string())?;
        let want_tag = format!("v{expected_version}");
        if tag != want_tag {
            return Err(format!(
                "release tag mismatch: expected {want_tag}, got {tag}"
            ));
        }
        let assets_json = body
            .get("assets")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "release response missing assets[]".to_string())?;
        let mut assets = Vec::new();
        for a in assets_json {
            let name = match a.get("name").and_then(|v| v.as_str()) {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => continue,
            };
            // 只接受我们认识的制品名——不认识的资产（源码包、签名文件…）
            // 既不进入任务，也不能成为路径注入的来源。
            if parse_artifact_ref(&name).is_none() {
                continue;
            }
            let Some(sha256) = a
                .get("digest")
                .and_then(|v| v.as_str())
                .and_then(parse_digest)
            else {
                // 老 release 或上传未完成可能没有 digest → 无法校验，直接跳过。
                continue;
            };
            let size = a.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            let download_url = a
                .get("browser_download_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            assets.push(OfficialAsset {
                name,
                size,
                sha256,
                download_url,
            });
        }
        if assets.is_empty() {
            return Err(format!(
                "release {tag} has no usable asset with a sha256 digest"
            ));
        }
        Ok(Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            tag: tag.to_string(),
            version: expected_version.to_string(),
            fetched_at: now_unix,
            assets,
        })
    }

    /// 精确匹配资产名。不做模糊/大小写匹配——制品身份必须唯一。
    pub fn asset(&self, artifact: &ArtifactRef) -> Option<&OfficialAsset> {
        let want = artifact.filename();
        self.assets.iter().find(|a| a.name == want)
    }
}

/// 解析 `sha256:<64 hex>` 形式。GitHub 的 `digest` 字段唯一合法形态。
pub fn parse_digest(raw: &str) -> Option<String> {
    let hex = raw.trim().strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex.to_ascii_lowercase())
}

/// 从 `browser_download_url` 推导 `official_path`（相对官方 origin 的路径）。
///
/// 只接受 `https://github.com/...` 且路径中不含 `..`，否则返回 `None`——避免把
/// 上游返回的任意 URL 当成 CDN 相对路径拼出去。
pub fn official_path_from_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix(OFFICIAL_DOWNLOAD_BASE)?;
    if !rest.starts_with('/') || rest.contains("..") || rest.contains('\\') {
        return None;
    }
    Some(rest.to_string())
}

/// 与官方 origin 拼接出完整下载 URL（agent 侧 `cdn_base_urls[i] + official_path`
/// 的 relay 侧等价实现，用于校验与测试）。
pub fn join_cdn(base: &str, official_path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), official_path)
}

// ── 版本判定 ────────────────────────────────────────────────────────

/// relay 与 agent 版本比较结果。只有 [`Self::Upgrade`] 才自动下发任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionDecision {
    Upgrade,
    UpToDate,
    AgentNewer,
    /// agent 未上报版本（老 agent）或上报为空。
    AgentUnknown,
    InvalidRelayVersion,
    InvalidAgentVersion,
}

impl VersionDecision {
    pub fn needs_upgrade(&self) -> bool {
        matches!(self, Self::Upgrade)
    }

    /// 给管理界面/日志看的原因串（稳定、可断言）。
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Upgrade => "agent version is lower than relay version",
            Self::UpToDate => "agent already runs the relay version",
            Self::AgentNewer => "agent version is newer than the relay; no downgrade",
            Self::AgentUnknown => "agent version unknown; refusing automatic upgrade",
            Self::InvalidRelayVersion => "relay version is not valid semver",
            Self::InvalidAgentVersion => "agent version is not valid semver",
        }
    }
}

/// `semver(relay) > semver(agent)` 才升级；相等不升级，更低不降级。
///
/// 预发布按 semver 规则处理：`0.59.2-beta.1 < 0.59.2`，因此跑预发布的 relay 不会
/// 把稳定版 agent 判成需要升级。非法或缺失版本一律不升级（只显示"版本未知"）。
pub fn version_decision(relay_version: &str, agent_version: Option<&str>) -> VersionDecision {
    let Ok(relay) = semver::Version::parse(relay_version.trim()) else {
        return VersionDecision::InvalidRelayVersion;
    };
    let agent_raw = match agent_version.map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => a,
        None => return VersionDecision::AgentUnknown,
    };
    // 允许 agent 上报带 `v` 前缀的版本（`--version` 输出风格）。
    let agent_trim = agent_raw.strip_prefix('v').unwrap_or(agent_raw);
    let Ok(agent) = semver::Version::parse(agent_trim) else {
        return VersionDecision::InvalidAgentVersion;
    };
    if relay > agent {
        VersionDecision::Upgrade
    } else if relay == agent {
        VersionDecision::UpToDate
    } else {
        VersionDecision::AgentNewer
    }
}

// ── 制品库 ──────────────────────────────────────────────────────────

/// 统一制品库：一份目录，两种读取入口。
///
/// 布局：
/// ```text
/// <root>/shell-remote-aarch64              扁平（安装脚本的既有约定）
/// <root>/shell-remote-aarch64.version      可选的版本标注（admin 展示）
/// <root>/releases/v0.59.2/<filename>       多版本隔离（显式版本路径）
/// <root>/.metadata/manifests/v0.59.2.json  官方清单缓存（不对外暴露）
/// ```
///
/// 读取顺序是「版本目录 → 扁平」：管理员可以并存多个版本且互不覆盖，而老安装
/// 脚本继续用扁平名拿到"当前"版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 内部元数据目录。绝不注册任何指向它的 HTTP 路由。
    pub fn metadata_dir(&self) -> PathBuf {
        self.root.join(".metadata")
    }

    pub fn manifest_path(&self, version: &str) -> PathBuf {
        self.metadata_dir()
            .join("manifests")
            .join(format!("v{}.json", sanitize_version(version)))
    }

    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.root
            .join("releases")
            .join(format!("v{}", sanitize_version(version)))
    }

    /// 定位某个制品文件：先版本目录，再扁平名（兼容存量部署）。
    pub fn locate(&self, version: &str, filename: &str) -> Option<PathBuf> {
        // 文件名必须先通过制品名校验，防止 `..` / 分隔符进入路径拼接。
        if parse_artifact_ref(filename).is_none() {
            return None;
        }
        let versioned = self.version_dir(version).join(filename);
        if versioned.is_file() {
            return Some(versioned);
        }
        let flat = self.root.join(filename);
        flat.is_file().then_some(flat)
    }

    /// 读取 `.version` 伴随文件（admin 展示用的目标版本标注）。
    pub fn declared_version(&self, artifact: &ArtifactRef) -> Option<String> {
        let name = artifact.filename();
        let stem = name.strip_suffix(".exe").unwrap_or(&name);
        let path = self.root.join(format!("{stem}.version"));
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// 枚举库内所有可识别的制品（不含版本目录里的历史版本，避免 admin 列表膨胀）。
    pub fn list(&self) -> Vec<LocalArtifact> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(artifact) = parse_artifact_ref(&name) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            out.push(LocalArtifact {
                artifact,
                path: entry.path(),
                size: meta.len(),
                // 列表用（admin 周期刷新）不算 hash：几十 MB 的文件每次刷新都算
                // 会让面板卡住。真正触发升级时才 hash。
                sha256: None,
            });
        }
        out.sort_by(|a, b| a.artifact.filename().cmp(&b.artifact.filename()));
        out
    }
}

/// 版本串进入文件名前的清洗：只保留 `[0-9A-Za-z.+-]`。
///
/// 版本通常已经过 semver 解析，这里是纵深防御——任何路径元字符都不允许进入
/// 缓存路径或 release 目录名。
pub fn sanitize_version(version: &str) -> String {
    version
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
        .collect()
}

/// 库内的一个本地制品。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalArtifact {
    pub artifact: ArtifactRef,
    pub path: PathBuf,
    pub size: u64,
    /// `None` 表示只做了目录枚举、还没算 hash。
    pub sha256: Option<String>,
}

impl LocalArtifact {
    /// 现场计算 SHA-256（管理员触发升级时才调用）。
    pub fn with_hash(mut self) -> std::io::Result<Self> {
        let (size, digest) = hash_file(&self.path)?;
        self.size = size;
        self.sha256 = Some(digest);
        Ok(self)
    }
}

/// 流式计算文件 SHA-256，返回 `(字节数, 小写 hex)`。
pub fn hash_file(path: &Path) -> std::io::Result<(u64, String)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((total, hex::encode(hasher.finalize())))
}

// ── 清单缓存（原子写 + 冲突拒绝）────────────────────────────────────

/// 读取已缓存的清单。解析失败或 schema 不认识 → `None`（当作缓存未命中，
/// 而不是让一个坏文件永久破坏升级路径）。
pub fn read_cached_manifest(store: &ArtifactStore, version: &str) -> Option<OfficialManifest> {
    let raw = std::fs::read_to_string(store.manifest_path(version)).ok()?;
    let m: OfficialManifest = serde_json::from_str(&raw).ok()?;
    if m.schema_version != MANIFEST_SCHEMA_VERSION || m.version != version {
        return None;
    }
    Some(m)
}

/// 原子写入清单：同目录临时文件 → `fsync` → `rename`。
///
/// 中途崩溃只会留下临时文件（下次写会被覆盖），绝不会让读到半个 JSON。
pub fn write_manifest_atomic(
    store: &ArtifactStore,
    manifest: &OfficialManifest,
) -> std::io::Result<()> {
    use std::io::Write;
    let path = store.manifest_path(&manifest.version);
    let dir = path.parent().expect("manifest path has a parent");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string_pretty(manifest)?.as_bytes())?;
        f.sync_all()?;
    }
    match std::fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 对比已缓存清单与刚取到的新清单。`Some(msg)` 表示同一版本出现了不一致的
/// digest/size/资产集——发布异常，必须停止该版本升级，不能静默换 hash。
pub fn manifest_conflict(cached: &OfficialManifest, incoming: &OfficialManifest) -> Option<String> {
    if cached.version != incoming.version {
        return Some(format!(
            "version mismatch: cached {} vs incoming {}",
            cached.version, incoming.version
        ));
    }
    for a in &cached.assets {
        match incoming.assets.iter().find(|b| b.name == a.name) {
            None => return Some(format!("asset {} disappeared from the release", a.name)),
            Some(b) if b.sha256 != a.sha256 => {
                return Some(format!(
                    "asset {} digest changed: cached {} vs incoming {}",
                    a.name, a.sha256, b.sha256
                ))
            }
            Some(b) if b.size != a.size => {
                return Some(format!(
                    "asset {} size changed: cached {} vs incoming {}",
                    a.name, a.size, b.size
                ))
            }
            Some(_) => {}
        }
    }
    None
}

// ── 官方清单抓取 ────────────────────────────────────────────────────

/// 清单抓取的边界参数。测试注入较小的超时与本地 `api_base`。
#[derive(Debug, Clone)]
pub struct ManifestFetchConfig {
    pub api_base: String,
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_bytes: usize,
    pub max_redirects: usize,
}

impl Default for ManifestFetchConfig {
    fn default() -> Self {
        Self {
            api_base: OFFICIAL_API_BASE.to_string(),
            timeout: MANIFEST_TIMEOUT,
            connect_timeout: MANIFEST_CONNECT_TIMEOUT,
            max_bytes: MANIFEST_MAX_BYTES,
            max_redirects: MANIFEST_MAX_REDIRECTS,
        }
    }
}

/// 拉取并解析某个版本的官方 release 清单。
///
/// 所有失败都以 `Err(String)` 返回（relay 据此回落缓存/本地/custom 规则），
/// 不 panic、不无限等待：连接、总时限、重定向次数、响应体积都有上限。
pub async fn fetch_official_manifest(
    cfg: &ManifestFetchConfig,
    version: &str,
) -> Result<OfficialManifest, String> {
    // 版本先过 semver：既保证是合法 tag，也杜绝任何路径/查询串注入。
    let parsed = semver::Version::parse(version.trim())
        .map_err(|e| format!("invalid version {version:?}: {e}"))?;
    let version = parsed.to_string();

    let client = reqwest::Client::builder()
        .connect_timeout(cfg.connect_timeout)
        .timeout(cfg.timeout)
        .redirect(reqwest::redirect::Policy::limited(cfg.max_redirects))
        .user_agent(concat!("shell-remote-relay/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("cannot build HTTP client: {e}"))?;

    let url = format!(
        "{}/repos/{}/releases/tags/v{}",
        cfg.api_base.trim_end_matches('/'),
        OFFICIAL_REPO,
        version
    );
    let resp = client
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("release request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("release request returned HTTP {status}"));
    }
    if let Some(len) = resp.content_length() {
        if len as usize > cfg.max_bytes {
            return Err(format!(
                "release response too large: {len} bytes (limit {})",
                cfg.max_bytes
            ));
        }
    }
    let mut body = Vec::new();
    let mut resp = resp;
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| format!("release body read failed: {e}"))?;
        let Some(chunk) = chunk else { break };
        if body.len() + chunk.len() > cfg.max_bytes {
            return Err(format!("release response exceeded {} bytes", cfg.max_bytes));
        }
        body.extend_from_slice(&chunk);
    }
    let json: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("release response is not JSON: {e}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    OfficialManifest::from_github_json(&version, &json, now)
}

// ── 升级任务 ────────────────────────────────────────────────────────

/// 不可变的升级任务。下发给 agent 的 `agent:upgrade.payload` 就是它的序列化。
///
/// **向后兼容**：存量 agent（≤ v0.59.2）用
/// `UpgradeRequest::from_payload` 读 `version` / `url` / `sha256` 三个键。
/// 序列化时因此额外附带这三个别名字段（由 [`UpgradeTask::wire_payload`]
/// 生成），让老 agent 能继续升级到支持完整协议的版本；新 agent 只读新字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeTask {
    pub upgrade_id: String,
    pub target_version: String,
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    /// 官方 CDN 上的相对路径；custom 制品没有官方路径 → `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub official_path: Option<String>,
    /// 外部 CDN 入口（每项与 `official_path` 拼接）。custom 为空。
    pub cdn_base_urls: Vec<String>,
    /// relay 自身镜像的下载路径；库内没有对应制品时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    /// `relay_url.is_some()` 的显式副本——agent 不必推断就能决定是否回退。
    pub relay_fallback: bool,
    pub source: ArtifactSource,
    /// 这份字节是否来自官方 release（custom 恒 false）。
    pub official_release: bool,
    pub variant: Variant,
    pub arch: String,
    pub windows: bool,
}

impl UpgradeTask {
    /// 生成下发给 agent 的 `payload`，附带存量 agent 需要的别名字段。
    ///
    /// - `version` = `target_version`：老 agent 用它做日志/上报。
    /// - `url` = `relay_url`：老 agent 只会从 relay 下载，没有 CDN 概念。因此
    ///   **只有存在 relay 副本时**才给 `url`——否则老 agent 会去请求一个不存在的
    ///   端点。这种情况下 relay 无法服务它，交给管理员先镜像制品（新 agent 才
    ///   具备 CDN 能力）。
    /// - `sha256` 两边同名，无需别名。
    pub fn wire_payload(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "version".to_string(),
                serde_json::Value::String(self.target_version.clone()),
            );
            if let Some(url) = &self.relay_url {
                obj.insert("url".to_string(), serde_json::Value::String(url.clone()));
            }
        }
        v
    }

    /// 老 agent（只认 `version`/`url`/`sha256`）能否消费这个任务。
    ///
    /// 条件就是"有 relay 副本可下载"。admin 触发时用它决定是否给出明确提示，
    /// 而不是让老 agent 拿着缺失的 `url` 静默失败。
    pub fn legacy_compatible(&self) -> bool {
        self.relay_url.is_some()
    }
}

/// 拿不到可信任务时的明确原因（不回退到"随便猜一个 hash"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRefusal {
    /// 库内与官方清单都没有该制品。
    NoArtifact,
    /// 本地镜像的 hash/size 与官方 digest 不一致——发布异常，拒绝下发。
    HashConflict { local: String, official: String },
    /// 本地制品没有官方来源，且管理员未批准自定义构建。
    CustomNotAllowed { filename: String },
}

impl TaskRefusal {
    pub fn message(&self) -> String {
        match self {
            Self::NoArtifact => {
                "目标档案不存在：官方清单无该制品，relay 制品库也没有对应文件".to_string()
            }
            Self::HashConflict { local, official } => {
                format!("本地镜像与官方 digest 不一致（本地 {local} / 官方 {official}），拒绝下发")
            }
            Self::CustomNotAllowed { filename } => format!(
                "{filename} 没有官方 digest 来源，需要 --allow-custom-upgrade 显式批准后才能下发"
            ),
        }
    }
}

/// 依据官方清单 / 本地镜像构造升级任务。
///
/// 决策表（`asset` = 官方清单里的对应资产，`local` = 库内实测 hash 的副本）：
///
/// | asset | local | 结果 |
/// |---|---|---|
/// | 有 | hash/size 一致 | `Local` 源 + relay 镜像 + CDN 双通道 |
/// | 有 | hash/size 不一致 | [`TaskRefusal::HashConflict`] |
/// | 有 | 无 | `Official` 源，仅 CDN |
/// | 无 | 有且允许 custom | `Custom` 源，仅 relay 镜像 |
/// | 无 | 有但未批准 | [`TaskRefusal::CustomNotAllowed`] |
/// | 无 | 无 | [`TaskRefusal::NoArtifact`] |
///
/// `target_version` 是**正在比较的 relay 版本**（触发升级时由 `env!(
/// CARGO_PKG_VERSION)` 提供）。没有官方清单时任务也要带非空版本——custom 分支
/// 曾经回落到 `""`，agent 侧日志/上报会显示空版本，且"升级到哪个版本"这件事
/// 本身仍然成立：目标就是与 agent 版本比较过的那个 relay 版本。
pub fn build_task(
    upgrade_id: String,
    target_version: &str,
    artifact: &ArtifactRef,
    manifest: Option<&OfficialManifest>,
    local: Option<&LocalArtifact>,
    cdn_base_urls: &[String],
    allow_custom: bool,
) -> Result<UpgradeTask, TaskRefusal> {
    let asset = manifest.and_then(|m| m.asset(artifact));
    // 清单里的版本是权威的（它就是为了这个版本抓的）；没有清单时用目标版本，
    // 绝不留空。
    let version = manifest
        .map(|m| m.version.clone())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| target_version.to_string());

    let (source, size, sha256, official_path) = match (asset, local) {
        (Some(a), Some(l)) => {
            let local_hash = l.sha256.clone().unwrap_or_default();
            if local_hash != a.sha256 || (a.size != 0 && l.size != a.size) {
                return Err(TaskRefusal::HashConflict {
                    local: format!("{} bytes / {}", l.size, local_hash),
                    official: format!("{} bytes / {}", a.size, a.sha256),
                });
            }
            (
                ArtifactSource::Local,
                a.size.max(l.size),
                a.sha256.clone(),
                official_path_from_url(&a.download_url)
                    .or_else(|| Some(artifact.official_path(&version))),
            )
        }
        (Some(a), None) => (
            ArtifactSource::Official,
            a.size,
            a.sha256.clone(),
            official_path_from_url(&a.download_url)
                .or_else(|| Some(artifact.official_path(&version))),
        ),
        (None, Some(l)) => {
            if !allow_custom {
                return Err(TaskRefusal::CustomNotAllowed {
                    filename: artifact.filename(),
                });
            }
            (
                ArtifactSource::Custom,
                l.size,
                l.sha256.clone().unwrap_or_default(),
                None,
            )
        }
        (None, None) => return Err(TaskRefusal::NoArtifact),
    };

    // custom 制品不在官方 CDN 上——给出去只会让 agent 下载到同版本的另一个文件。
    let cdn = if source == ArtifactSource::Custom {
        Vec::new()
    } else {
        cdn_base_urls.to_vec()
    };
    let relay_url = local.map(|_| format!("/agent/upgrade/blob/{}", artifact.filename()));

    Ok(UpgradeTask {
        upgrade_id,
        target_version: version,
        filename: artifact.filename(),
        size,
        sha256,
        official_path,
        cdn_base_urls: cdn,
        relay_fallback: relay_url.is_some(),
        relay_url,
        source,
        official_release: source.is_official_release(),
        variant: artifact.variant,
        arch: artifact.arch.clone(),
        windows: artifact.windows,
    })
}

// ── HTTP Range ──────────────────────────────────────────────────────

/// 单区间 Range 解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeRequest {
    /// 无 Range 头、单位不是 bytes、或多区间 → 整文件 200。
    Full,
    /// 满足的闭区间 `[start, end]` → 206。
    Partial { start: u64, end: u64 },
    /// 语法合法但不可满足（`start >= len`、后缀长度 0、`start > end`）→ 416。
    Unsatisfiable,
}

/// 解析 `Range: bytes=…`（RFC 7233 单区间）。
///
/// 规则（与 agent 的 `.part` 续传直接相关）：
/// - 无头 / 单位非 bytes / 多区间 / 语法不可解析 → [`RangeRequest::Full`]
///   （RFC 允许服务端忽略 Range；多区间服务端不支持时忽略比猜更安全）。
/// - `bytes=a-b`（`a<=b`，`a<len`）、`bytes=a-`、`bytes=-n`（`n>0`）→
///   [`RangeRequest::Partial`]，`end` 截到 `len-1`。
/// - `a>=len`、`bytes=-0`、`a>b` → [`RangeRequest::Unsatisfiable`]，回 416。
pub fn parse_byte_range(header: Option<&str>, len: u64) -> RangeRequest {
    let Some(raw) = header else {
        return RangeRequest::Full;
    };
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return RangeRequest::Full;
    };
    if spec.contains(',') {
        return RangeRequest::Full;
    }
    let Some((start_s, end_s)) = spec.trim().split_once('-') else {
        return RangeRequest::Full;
    };
    let start_s = start_s.trim();
    let end_s = end_s.trim();

    if start_s.is_empty() {
        // 后缀区间 `bytes=-n`：最后 n 个字节。
        let Ok(n) = end_s.parse::<u64>() else {
            return RangeRequest::Full;
        };
        if n == 0 || len == 0 {
            return RangeRequest::Unsatisfiable;
        }
        let n = n.min(len);
        return RangeRequest::Partial {
            start: len - n,
            end: len - 1,
        };
    }
    let Ok(start) = start_s.parse::<u64>() else {
        return RangeRequest::Full;
    };
    let end = if end_s.is_empty() {
        None
    } else {
        match end_s.parse::<u64>() {
            Ok(e) => Some(e),
            Err(_) => return RangeRequest::Full,
        }
    };
    if let Some(e) = end {
        if start > e {
            return RangeRequest::Unsatisfiable;
        }
    }
    if len == 0 || start >= len {
        return RangeRequest::Unsatisfiable;
    }
    let end = end.map(|e| e.min(len - 1)).unwrap_or(len - 1);
    RangeRequest::Partial { start, end }
}

/// 206 响应的 `Content-Range` 头值。
pub fn content_range_value(start: u64, end: u64, len: u64) -> String {
    format!("bytes {start}-{end}/{len}")
}

/// 416 响应的 `Content-Range` 头值。
pub fn unsatisfied_range_value(len: u64) -> String {
    format!("bytes */{len}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 文件名 / 架构映射 ───────────────────────────────────────────

    #[test]
    fn test_cli_ui_artifact_mapping() {
        let cli = ArtifactRef::new(Variant::Cli, "x86_64", false).unwrap();
        assert_eq!(cli.filename(), "shell-remote-x86_64");
        let ui = ArtifactRef::new(Variant::Ui, "aarch64", false).unwrap();
        assert_eq!(ui.filename(), "shell-remote-ui-aarch64");
        let ui_win = ArtifactRef::new(Variant::Ui, "x86_64", true).unwrap();
        assert_eq!(ui_win.filename(), "shell-remote-ui-x86_64.exe");
        let cli_win = ArtifactRef::new(Variant::Cli, "x86_64", true).unwrap();
        assert_eq!(cli_win.filename(), "shell-remote-x86_64.exe");

        // 官方路径严格对应 build-dist.sh 的发布布局。
        assert_eq!(
            ui.official_path("0.59.2"),
            "/zzttzzmyswy/shell-remote/releases/download/v0.59.2/shell-remote-ui-aarch64"
        );
    }

    #[test]
    fn test_parse_artifact_ref_distinguishes_ui_from_cli() {
        let ui = parse_artifact_ref("shell-remote-ui-aarch64").unwrap();
        assert_eq!(ui.variant, Variant::Ui);
        assert_eq!(ui.arch, "aarch64");
        assert!(!ui.windows);

        let ui_win = parse_artifact_ref("shell-remote-ui-x86_64.exe").unwrap();
        assert_eq!(ui_win.variant, Variant::Ui);
        assert!(ui_win.windows);

        let cli = parse_artifact_ref("shell-remote-armv7").unwrap();
        assert_eq!(cli.variant, Variant::Cli);
        assert_eq!(cli.arch, "armv7");

        assert!(parse_artifact_ref("shell-remote-").is_none());
        assert!(parse_artifact_ref("shell-remote-ui-").is_none());
        assert!(parse_artifact_ref("other.bin").is_none());
        assert!(parse_artifact_ref("../../etc/passwd").is_none());
    }

    #[test]
    fn test_arch_normalization_and_injection_rejection() {
        assert_eq!(normalize_arch("AMD64"), "x86_64");
        assert_eq!(normalize_arch("arm64"), "aarch64");
        assert_eq!(normalize_arch("armv7l"), "armv7");
        // 路径元字符一律拒绝。
        assert!(ArtifactRef::new(Variant::Cli, "../x", false).is_none());
        assert!(ArtifactRef::new(Variant::Cli, "a/b", false).is_none());
        assert!(ArtifactRef::new(Variant::Cli, "", false).is_none());
        assert!(ArtifactRef::new(Variant::Cli, "x86 64", false).is_none());
    }

    #[test]
    fn test_variant_parse() {
        assert_eq!(Variant::parse("cli"), Some(Variant::Cli));
        assert_eq!(Variant::parse("UI"), Some(Variant::Ui));
        assert_eq!(Variant::parse("desktop"), None);
    }

    // ── 官方 digest 解析 ────────────────────────────────────────────

    #[test]
    fn test_parse_digest() {
        let hex = "e8233ca1b7dcd7ed18a2bc46c5107211e60128fa42bf8c1d75ba00203a22d851";
        assert_eq!(
            parse_digest(&format!("sha256:{hex}")),
            Some(hex.to_string())
        );
        // 大写归一
        assert_eq!(
            parse_digest(&format!("sha256:{}", hex.to_uppercase())),
            Some(hex.to_string())
        );
        assert!(parse_digest(hex).is_none(), "missing algorithm prefix");
        assert!(parse_digest("sha512:abc").is_none(), "wrong algorithm");
        assert!(parse_digest("sha256:abc").is_none(), "short digest");
        assert!(
            parse_digest(&format!("sha256:{hex}0")).is_none(),
            "long digest"
        );
        assert!(parse_digest(&format!("sha256:{}", "z".repeat(64))).is_none());
    }

    fn release_json(tag: &str, assets: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "tag_name": tag, "assets": assets })
    }

    const DIGEST_AARCH64: &str = "7eabddeeb9d4b8d2366e45c25c579d1226b2768bdbea365d0634ace9628469ce";

    fn sample_assets() -> serde_json::Value {
        serde_json::json!([
            {
                "name": "shell-remote-aarch64",
                "size": 6596656,
                "digest": format!("sha256:{DIGEST_AARCH64}"),
                "browser_download_url": "https://github.com/zzttzzmyswy/shell-remote/releases/download/v0.59.2/shell-remote-aarch64"
            },
            {
                "name": "shell-remote-ui-aarch64",
                "size": 16956344,
                "digest": "sha256:e8233ca1b7dcd7ed18a2bc46c5107211e60128fa42bf8c1d75ba00203a22d851",
                "browser_download_url": "https://github.com/zzttzzmyswy/shell-remote/releases/download/v0.59.2/shell-remote-ui-aarch64"
            },
            {
                "name": "Source code (zip)",
                "size": 1234,
                "digest": "sha256:e8233ca1b7dcd7ed18a2bc46c5107211e60128fa42bf8c1d75ba00203a22d851",
                "browser_download_url": "https://github.com/zzttzzmyswy/shell-remote/archive/v0.59.2.zip"
            }
        ])
    }

    #[test]
    fn test_manifest_from_github_json_filters_and_parses() {
        let body = release_json("v0.59.2", sample_assets());
        let m = OfficialManifest::from_github_json("0.59.2", &body, 1700).unwrap();
        assert_eq!(m.version, "0.59.2");
        assert_eq!(m.tag, "v0.59.2");
        assert_eq!(m.fetched_at, 1700);
        // 源码包不是制品 → 被过滤。
        assert_eq!(m.assets.len(), 2);

        let cli = m
            .asset(&ArtifactRef::new(Variant::Cli, "aarch64", false).unwrap())
            .expect("cli asset");
        assert_eq!(cli.size, 6_596_656);
        assert_eq!(cli.sha256, DIGEST_AARCH64);
        assert_eq!(
            official_path_from_url(&cli.download_url).unwrap(),
            "/zzttzzmyswy/shell-remote/releases/download/v0.59.2/shell-remote-aarch64"
        );

        let ui = m
            .asset(&ArtifactRef::new(Variant::Ui, "aarch64", false).unwrap())
            .expect("ui asset");
        assert_eq!(ui.size, 16_956_344);
    }

    #[test]
    fn test_manifest_from_github_json_rejects_wrong_tag() {
        let body = release_json("v0.58.0", sample_assets());
        let err = OfficialManifest::from_github_json("0.59.2", &body, 0).unwrap_err();
        assert!(err.contains("tag mismatch"), "{err}");
    }

    #[test]
    fn test_manifest_from_github_json_rejects_assets_without_digest() {
        let body = release_json(
            "v0.59.2",
            serde_json::json!([{
                "name": "shell-remote-x86_64",
                "size": 100,
                "browser_download_url": "https://github.com/x/y"
            }]),
        );
        let err = OfficialManifest::from_github_json("0.59.2", &body, 0).unwrap_err();
        assert!(err.contains("no usable asset"), "{err}");
    }

    #[test]
    fn test_official_path_from_url_rejects_foreign_host_and_traversal() {
        assert!(official_path_from_url("https://evil.example/x").is_none());
        assert!(
            official_path_from_url("https://github.com/../../etc/passwd").is_none(),
            "traversal must be rejected"
        );
        assert!(official_path_from_url("https://github.com\\x").is_none());
        assert_eq!(
            official_path_from_url("https://github.com/a/b").unwrap(),
            "/a/b"
        );
    }

    #[test]
    fn test_join_cdn() {
        assert_eq!(
            join_cdn("https://gh-proxy.com/", "/a/b"),
            "https://gh-proxy.com/a/b"
        );
        assert_eq!(
            join_cdn("https://github.com", "/a/b"),
            "https://github.com/a/b"
        );
    }

    // ── 版本比较与预发布 ────────────────────────────────────────────

    #[test]
    fn test_version_decision_basic() {
        assert_eq!(
            version_decision("0.59.2", Some("0.59.1")),
            VersionDecision::Upgrade
        );
        assert_eq!(
            version_decision("0.59.2", Some("0.59.2")),
            VersionDecision::UpToDate
        );
        assert_eq!(
            version_decision("0.59.2", Some("0.60.0")),
            VersionDecision::AgentNewer
        );
        // `v` 前缀容忍
        assert_eq!(
            version_decision("0.59.2", Some("v0.59.1")),
            VersionDecision::Upgrade
        );
        // 缺失/空 → 版本未知，不升级
        assert_eq!(
            version_decision("0.59.2", None),
            VersionDecision::AgentUnknown
        );
        assert_eq!(
            version_decision("0.59.2", Some("  ")),
            VersionDecision::AgentUnknown
        );
        assert_eq!(
            version_decision("0.59.2", Some("garbage")),
            VersionDecision::InvalidAgentVersion
        );
        assert_eq!(
            version_decision("not-a-version", Some("0.1.0")),
            VersionDecision::InvalidRelayVersion
        );
        assert!(!VersionDecision::AgentNewer.needs_upgrade());
        assert!(!VersionDecision::AgentUnknown.needs_upgrade());
    }

    #[test]
    fn test_version_decision_prerelease() {
        // 稳定版 relay 覆盖预发布 agent。
        assert_eq!(
            version_decision("0.59.2", Some("0.59.2-beta.1")),
            VersionDecision::Upgrade
        );
        // 预发布只在 major.minor.patch 相同时才排在稳定版之前：0.59.2-beta.1
        // 仍高于 0.59.1，所以那是升级而不是降级。
        assert_eq!(
            version_decision("0.59.2-beta.1", Some("0.59.1")),
            VersionDecision::Upgrade
        );
        // 同版本时预发布 relay 不会把稳定版 agent 判成需升级（不降级）。
        assert_eq!(
            version_decision("0.59.2-beta.1", Some("0.59.2")),
            VersionDecision::AgentNewer
        );
        // 同预发布串相等 → 不升级
        assert_eq!(
            version_decision("0.59.2-beta.1", Some("0.59.2-beta.1")),
            VersionDecision::UpToDate
        );
        // 预发布序：beta.1 < beta.2
        assert_eq!(
            version_decision("0.59.2-beta.2", Some("0.59.2-beta.1")),
            VersionDecision::Upgrade
        );
        // 数字序按 semver，不是字符串序：0.9.0 < 0.59.2
        assert_eq!(
            version_decision("0.59.2", Some("0.9.0")),
            VersionDecision::Upgrade
        );
    }

    // ── 制品库：目录兼容 / 原子写 / 冲突拒绝 ─────────────────────────

    fn temp_store(tag: &str) -> ArtifactStore {
        let dir = std::env::temp_dir().join(format!(
            "sr-store-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        ArtifactStore::new(dir)
    }

    fn sample_manifest(version: &str) -> OfficialManifest {
        let body = release_json(&format!("v{version}"), sample_assets());
        OfficialManifest::from_github_json(version, &body, 100).unwrap()
    }

    #[test]
    fn test_store_download_dir_flat_compat_and_versioned_precedence() {
        let store = temp_store("compat");
        // 扁平布局（存量部署 + 安装脚本既有约定）
        std::fs::write(store.root().join("shell-remote-x86_64"), b"flat").unwrap();
        let flat = store.locate("0.59.2", "shell-remote-x86_64").unwrap();
        assert_eq!(std::fs::read(&flat).unwrap(), b"flat");

        // 版本目录优先于扁平
        let vdir = store.version_dir("0.59.2");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("shell-remote-x86_64"), b"versioned").unwrap();
        let v = store.locate("0.59.2", "shell-remote-x86_64").unwrap();
        assert_eq!(std::fs::read(&v).unwrap(), b"versioned");

        // 另一个版本回落到扁平
        let v2 = store.locate("0.60.0", "shell-remote-x86_64").unwrap();
        assert_eq!(std::fs::read(&v2).unwrap(), b"flat");

        // 非法文件名（穿越）即使文件存在也不返回
        std::fs::write(store.root().join("..secret"), b"x").unwrap();
        assert!(store.locate("0.59.2", "../../etc/passwd").is_none());

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn test_store_lists_artifacts_and_reads_version_companion() {
        let store = temp_store("list");
        std::fs::write(store.root().join("shell-remote-x86_64"), b"a").unwrap();
        std::fs::write(store.root().join("shell-remote-ui-aarch64"), b"bb").unwrap();
        std::fs::write(store.root().join("shell-remote-x86_64.version"), "0.59.2").unwrap();
        std::fs::write(store.root().join("README.txt"), b"noise").unwrap();

        let list = store.list();
        let names: Vec<String> = list.iter().map(|l| l.artifact.filename()).collect();
        assert_eq!(
            names,
            vec!["shell-remote-ui-aarch64", "shell-remote-x86_64"]
        );
        assert_eq!(list[0].size, 2, "UI artifact size");
        assert!(list[0].sha256.is_none(), "listing must not hash");

        let cli = ArtifactRef::new(Variant::Cli, "x86_64", false).unwrap();
        assert_eq!(store.declared_version(&cli).as_deref(), Some("0.59.2"));

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn test_manifest_cache_roundtrip_and_miss_cases() {
        let store = temp_store("cache");
        assert!(
            read_cached_manifest(&store, "0.59.2").is_none(),
            "empty cache"
        );

        let m = sample_manifest("0.59.2");
        write_manifest_atomic(&store, &m).unwrap();
        let got = read_cached_manifest(&store, "0.59.2").expect("cache hit");
        assert_eq!(got, m);
        // 清单落在 .metadata 下（不会被 /download 的制品名校验放行）
        let p = store.manifest_path("0.59.2");
        assert!(p.starts_with(store.metadata_dir()), "{}", p.display());
        assert!(parse_artifact_ref("v0.59.2.json").is_none());

        // 损坏的缓存当作未命中，而不是永久卡死
        std::fs::write(&p, b"{ not json").unwrap();
        assert!(read_cached_manifest(&store, "0.59.2").is_none());

        // schema 不认识 → 未命中
        let mut other = m.clone();
        other.schema_version = 999;
        std::fs::write(&p, serde_json::to_string(&other).unwrap()).unwrap();
        assert!(read_cached_manifest(&store, "0.59.2").is_none());

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn test_manifest_atomic_write_leaves_no_temp_and_overwrites() {
        let store = temp_store("atomic");
        let m1 = sample_manifest("0.59.2");
        write_manifest_atomic(&store, &m1).unwrap();
        // 二次写（同内容）不残留临时文件
        write_manifest_atomic(&store, &m1).unwrap();

        let dir = store
            .manifest_path("0.59.2")
            .parent()
            .unwrap()
            .to_path_buf();
        let strays: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".tmp-"))
            .collect();
        assert!(
            strays.is_empty(),
            "temp files must be renamed away: {strays:?}"
        );
        assert_eq!(read_cached_manifest(&store, "0.59.2").unwrap(), m1);

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn test_manifest_conflict_detects_digest_and_size_changes() {
        let m = sample_manifest("0.59.2");
        assert!(manifest_conflict(&m, &m).is_none(), "identical is fine");

        let mut tampered = m.clone();
        tampered.assets[0].sha256 = "0".repeat(64);
        let msg = manifest_conflict(&m, &tampered).expect("digest change must conflict");
        assert!(msg.contains("digest changed"), "{msg}");

        let mut resized = m.clone();
        resized.assets[0].size += 1;
        assert!(manifest_conflict(&m, &resized)
            .expect("size change must conflict")
            .contains("size changed"));

        let mut removed = m.clone();
        removed.assets.remove(0);
        assert!(manifest_conflict(&m, &removed)
            .expect("missing asset must conflict")
            .contains("disappeared"));

        let mut other_version = m.clone();
        other_version.version = "0.60.0".to_string();
        assert!(manifest_conflict(&m, &other_version).is_some());
    }

    // ── 任务构造：官方 / 本地 / custom ──────────────────────────────

    fn cdns() -> Vec<String> {
        DEFAULT_CDN_BASE_URLS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn test_build_task_official_only_has_cdn_and_no_relay_url() {
        let m = sample_manifest("0.59.2");
        let art = ArtifactRef::new(Variant::Cli, "aarch64", false).unwrap();
        let t = build_task("u1".into(), "0.59.2", &art, Some(&m), None, &cdns(), false).unwrap();
        assert_eq!(t.source, ArtifactSource::Official);
        assert_eq!(t.target_version, "0.59.2");
        assert_eq!(t.filename, "shell-remote-aarch64");
        assert_eq!(t.size, 6_596_656);
        assert_eq!(t.sha256, DIGEST_AARCH64);
        assert_eq!(
            t.official_path.as_deref(),
            Some("/zzttzzmyswy/shell-remote/releases/download/v0.59.2/shell-remote-aarch64")
        );
        assert_eq!(t.cdn_base_urls.len(), DEFAULT_CDN_BASE_URLS.len());
        assert!(t.cdn_base_urls.len() >= 2, "首期至少两个独立入口");
        assert!(t.relay_url.is_none(), "库内没有副本就不能给 relay URL");
        assert!(!t.relay_fallback);
        assert!(t.official_release);
        assert_eq!(t.upgrade_id, "u1");
    }

    #[test]
    fn test_build_task_verified_local_mirror_is_local_source_with_both_paths() {
        let store = temp_store("task-local");
        let art = ArtifactRef::new(Variant::Ui, "aarch64", false).unwrap();
        let path = store.root().join(art.filename());
        // 内容随意，但 hash 必须与清单一致才能通过交叉校验。
        std::fs::write(&path, b"winner-bytes").unwrap();
        let (size, digest) = hash_file(&path).unwrap();

        let mut m = sample_manifest("0.59.2");
        let asset = m
            .assets
            .iter_mut()
            .find(|a| a.name == "shell-remote-ui-aarch64")
            .unwrap();
        asset.size = size;
        asset.sha256 = digest.clone();

        let local = LocalArtifact {
            artifact: art.clone(),
            path,
            size,
            sha256: Some(digest.clone()),
        };
        let t = build_task(
            "u2".into(),
            "0.59.2",
            &art,
            Some(&m),
            Some(&local),
            &cdns(),
            false,
        )
        .unwrap();
        assert_eq!(t.source, ArtifactSource::Local);
        assert_eq!(t.sha256, digest);
        assert_eq!(t.size, size);
        assert_eq!(
            t.relay_url.as_deref(),
            Some("/agent/upgrade/blob/shell-remote-ui-aarch64")
        );
        assert!(t.relay_fallback);
        assert!(!t.cdn_base_urls.is_empty(), "CDN 仍然优先");
        assert!(t.official_release);

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn test_build_task_rejects_hash_conflict() {
        let art = ArtifactRef::new(Variant::Cli, "aarch64", false).unwrap();
        let m = sample_manifest("0.59.2");
        // 本地字节与官方 digest 不一致 → 必须拒绝，而不是挑一个 hash 下发。
        let local = LocalArtifact {
            artifact: art.clone(),
            path: PathBuf::from("/nonexistent"),
            size: 6_596_656,
            sha256: Some("1".repeat(64)),
        };
        let err = build_task(
            "u3".into(),
            "0.59.2",
            &art,
            Some(&m),
            Some(&local),
            &cdns(),
            false,
        )
        .unwrap_err();
        match err {
            TaskRefusal::HashConflict { local, official } => {
                assert!(local.contains(&"1".repeat(64)), "{local}");
                assert!(official.contains(DIGEST_AARCH64), "{official}");
            }
            other => panic!("expected HashConflict, got {other:?}"),
        }
    }

    #[test]
    fn test_build_task_custom_requires_approval_and_has_no_cdn() {
        let art = ArtifactRef::new(Variant::Cli, "x86_64", false).unwrap();
        let local = LocalArtifact {
            artifact: art.clone(),
            path: PathBuf::from("/nonexistent"),
            size: 42,
            sha256: Some("2".repeat(64)),
        };

        // 未批准 → 明确拒绝
        let err = build_task(
            "u4".into(),
            "0.59.2",
            &art,
            None,
            Some(&local),
            &cdns(),
            false,
        )
        .unwrap_err();
        assert!(matches!(err, TaskRefusal::CustomNotAllowed { .. }));
        assert!(err.message().contains("--allow-custom-upgrade"));

        // 批准 → custom 明确标记，且不提供官方 CDN
        let t = build_task(
            "u4".into(),
            "0.59.2",
            &art,
            None,
            Some(&local),
            &cdns(),
            true,
        )
        .unwrap();
        assert_eq!(t.source, ArtifactSource::Custom);
        assert_eq!(t.source.as_str(), "custom");
        assert!(!t.official_release);
        assert!(t.cdn_base_urls.is_empty());
        assert!(t.official_path.is_none());
        assert_eq!(t.sha256, "2".repeat(64));
        assert_eq!(t.size, 42);
        // 无官方清单时版本必须回落成"正在比较的 relay 目标版本"，不能为空。
        assert_eq!(
            t.target_version, "0.59.2",
            "custom 分支的 target_version 不能是空串"
        );
        assert!(!t.target_version.is_empty());
        // 序列化后 agent 也要看到非空版本
        let wire = t.wire_payload();
        assert_eq!(wire["target_version"], "0.59.2");
        assert_eq!(wire["version"], "0.59.2");
        assert_eq!(
            t.relay_url.as_deref(),
            Some("/agent/upgrade/blob/shell-remote-x86_64")
        );
        assert!(t.relay_fallback);
    }

    #[test]
    fn test_build_task_refuses_when_nothing_available() {
        let art = ArtifactRef::new(Variant::Ui, "armv7", false).unwrap();
        let err = build_task("u5".into(), "0.59.2", &art, None, None, &cdns(), true).unwrap_err();
        assert_eq!(err, TaskRefusal::NoArtifact);
    }

    #[test]
    fn test_build_task_missing_variant_in_manifest_falls_back_to_refusal() {
        // 清单里只有 CLI，UI 请求不应被静默映射到 CLI 制品。
        let m = sample_manifest("0.59.2");
        let art = ArtifactRef::new(Variant::Ui, "x86_64", false).unwrap();
        let err =
            build_task("u6".into(), "0.59.2", &art, Some(&m), None, &cdns(), false).unwrap_err();
        assert_eq!(err, TaskRefusal::NoArtifact);
    }

    #[test]
    fn test_upgrade_task_serializes_expected_wire_fields() {
        let m = sample_manifest("0.59.2");
        let art = ArtifactRef::new(Variant::Cli, "aarch64", false).unwrap();
        let t = build_task("u7".into(), "0.59.2", &art, Some(&m), None, &cdns(), false).unwrap();
        let v = serde_json::to_value(&t).unwrap();
        for k in [
            "upgrade_id",
            "target_version",
            "filename",
            "size",
            "sha256",
            "official_path",
            "cdn_base_urls",
            "source",
            "relay_fallback",
            "official_release",
            "variant",
            "arch",
            "windows",
        ] {
            assert!(v.get(k).is_some(), "missing wire field {k}");
        }
        // relay_url 为 None 时省略（agent 据此判定没有兜底）
        assert!(v.get("relay_url").is_none());
        assert_eq!(v["source"], "official");
    }

    #[test]
    fn test_wire_payload_keeps_legacy_agent_fields() {
        // 存量 agent（≤ v0.59.2）读 version/url/sha256 —— 有 relay 副本时必须全给。
        let art = ArtifactRef::new(Variant::Cli, "aarch64", false).unwrap();
        let local = LocalArtifact {
            artifact: art.clone(),
            path: PathBuf::from("/nonexistent"),
            size: 7,
            sha256: Some("9".repeat(64)),
        };
        let t = build_task("u8".into(), "0.59.2", &art, None, Some(&local), &[], true).unwrap();
        let p = t.wire_payload();
        assert_eq!(p["version"], t.target_version);
        assert_eq!(p["url"], "/agent/upgrade/blob/shell-remote-aarch64");
        assert_eq!(p["sha256"], "9".repeat(64));
        assert_eq!(p["target_version"], p["version"], "两个版本字段必须一致");
        assert!(t.legacy_compatible());

        // 老 agent 真正能解析出的内容（与 agent::upgrade::UpgradeRequest 同契约）。
        assert_eq!(
            (
                p["version"].as_str(),
                p["url"].as_str(),
                p["sha256"].as_str()
            ),
            (
                Some(t.target_version.as_str()),
                Some("/agent/upgrade/blob/shell-remote-aarch64"),
                Some("9999999999999999999999999999999999999999999999999999999999999999"),
            )
        );

        // 只有 CDN、没有 relay 副本 → 不给 `url`，并标记老 agent 不兼容。
        let m = sample_manifest("0.59.2");
        let t2 = build_task("u9".into(), "0.59.2", &art, Some(&m), None, &cdns(), false).unwrap();
        let p2 = t2.wire_payload();
        assert!(p2.get("url").is_none(), "没有 relay 副本就不该给 url");
        assert_eq!(p2["version"], t2.target_version, "version 仍然给");
        assert!(!t2.legacy_compatible());
    }

    // ── Range 解析 ─────────────────────────────────────────────────

    #[test]
    fn test_parse_byte_range_full_and_partial() {
        // 无头 → 200
        assert_eq!(parse_byte_range(None, 100), RangeRequest::Full);
        // 单位不是 bytes → 忽略 Range
        assert_eq!(parse_byte_range(Some("items=0-1"), 100), RangeRequest::Full);
        // 多区间 → 忽略（服务端不支持时按 RFC 允许忽略）
        assert_eq!(
            parse_byte_range(Some("bytes=0-1,5-6"), 100),
            RangeRequest::Full
        );
        // 语法不可解析 → 忽略
        assert_eq!(parse_byte_range(Some("bytes=abc"), 100), RangeRequest::Full);
        assert_eq!(parse_byte_range(Some("bytes="), 100), RangeRequest::Full);

        // 常规区间
        assert_eq!(
            parse_byte_range(Some("bytes=0-9"), 100),
            RangeRequest::Partial { start: 0, end: 9 }
        );
        // 开区间到末尾
        assert_eq!(
            parse_byte_range(Some("bytes=90-"), 100),
            RangeRequest::Partial { start: 90, end: 99 }
        );
        // 尾部超出 → 截到 len-1
        assert_eq!(
            parse_byte_range(Some("bytes=90-1000"), 100),
            RangeRequest::Partial { start: 90, end: 99 }
        );
        // 后缀区间
        assert_eq!(
            parse_byte_range(Some("bytes=-10"), 100),
            RangeRequest::Partial { start: 90, end: 99 }
        );
        // 后缀比文件大 → 整文件区间的 206
        assert_eq!(
            parse_byte_range(Some("bytes=-1000"), 100),
            RangeRequest::Partial { start: 0, end: 99 }
        );
        // 空白容忍
        assert_eq!(
            parse_byte_range(Some(" bytes = 0-1 "), 100),
            RangeRequest::Full,
            "leading space before unit is not a valid bytes unit per our contract"
        );
    }

    #[test]
    fn test_parse_byte_range_unsatisfiable() {
        // start 越界 → 416
        assert_eq!(
            parse_byte_range(Some("bytes=100-"), 100),
            RangeRequest::Unsatisfiable
        );
        assert_eq!(
            parse_byte_range(Some("bytes=200-300"), 100),
            RangeRequest::Unsatisfiable
        );
        // start > end → 416
        assert_eq!(
            parse_byte_range(Some("bytes=50-10"), 100),
            RangeRequest::Unsatisfiable
        );
        // 后缀 0 → 416
        assert_eq!(
            parse_byte_range(Some("bytes=-0"), 100),
            RangeRequest::Unsatisfiable
        );
        // 空文件任何区间都不可满足
        assert_eq!(
            parse_byte_range(Some("bytes=0-"), 0),
            RangeRequest::Unsatisfiable
        );
    }

    #[test]
    fn test_range_response_headers() {
        assert_eq!(content_range_value(0, 99, 100), "bytes 0-99/100");
        assert_eq!(content_range_value(90, 99, 100), "bytes 90-99/100");
        assert_eq!(unsatisfied_range_value(100), "bytes */100");
    }

    #[test]
    fn test_sanitize_version_blocks_path_metacharacters() {
        assert_eq!(sanitize_version("0.59.2"), "0.59.2");
        assert_eq!(
            sanitize_version("0.59.2-beta.1+build"),
            "0.59.2-beta.1+build"
        );
        assert_eq!(sanitize_version("../../etc"), "....etc");
        assert_eq!(sanitize_version("1/2"), "12");
    }

    // ── 官方清单抓取（本地 HTTP mock，不依赖外网）────────────────────

    /// 起一个只服务 `GET /repos/<owner>/<repo>/releases/tags/<tag>` 的本地 mock。
    /// `only_tag` 之外一律 404，用来断言 relay 对非目标 tag 的处理。
    async fn spawn_release_mock(
        body: serde_json::Value,
        only_tag: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::routing::get;
        use axum::Router;

        let app = Router::new().route(
            "/repos/:owner/:repo/releases/tags/:tag",
            get(
                move |axum::extract::Path((_owner, _repo, tag)): axum::extract::Path<(
                    String,
                    String,
                    String,
                )>| {
                    let body = body.clone();
                    async move {
                        if tag == only_tag {
                            (axum::http::StatusCode::OK, axum::Json(body))
                        } else {
                            (
                                axum::http::StatusCode::NOT_FOUND,
                                axum::Json(serde_json::json!({"message": "Not Found"})),
                            )
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    fn mock_cfg(api_base: String) -> ManifestFetchConfig {
        ManifestFetchConfig {
            api_base,
            timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(2),
            max_bytes: 1024 * 1024,
            max_redirects: 3,
        }
    }

    #[tokio::test]
    async fn test_fetch_official_manifest_against_local_mock() {
        let body = release_json("v0.59.2", sample_assets());
        let (base, _h) = spawn_release_mock(body, "v0.59.2").await;
        let m = fetch_official_manifest(&mock_cfg(base), "0.59.2")
            .await
            .expect("mock fetch");
        assert_eq!(m.version, "0.59.2");
        assert_eq!(m.assets.len(), 2);
        let cli = m
            .asset(&ArtifactRef::new(Variant::Cli, "aarch64", false).unwrap())
            .unwrap();
        assert_eq!(cli.sha256, DIGEST_AARCH64);
    }

    #[tokio::test]
    async fn test_fetch_official_manifest_reports_http_error() {
        let body = release_json("v0.59.2", sample_assets());
        let (base, _h) = spawn_release_mock(body, "v0.59.2").await;
        // mock 只认 v0.59.2，其它 tag 返回 404
        let err = fetch_official_manifest(&mock_cfg(base), "0.58.0")
            .await
            .unwrap_err();
        assert!(err.contains("404"), "{err}");
    }

    #[tokio::test]
    async fn test_fetch_official_manifest_rejects_invalid_version() {
        let err = fetch_official_manifest(&mock_cfg("http://127.0.0.1:1".into()), "../../x")
            .await
            .unwrap_err();
        assert!(err.contains("invalid version"), "{err}");
    }

    #[tokio::test]
    async fn test_fetch_official_manifest_enforces_size_limit() {
        // 响应体超过 max_bytes → 明确失败而不是无界读取。
        let big = serde_json::json!({
            "tag_name": "v0.59.2",
            "assets": sample_assets(),
            "padding": "x".repeat(4096),
        });
        let (base, _h) = spawn_release_mock(big, "v0.59.2").await;
        let mut cfg = mock_cfg(base);
        cfg.max_bytes = 512;
        let err = fetch_official_manifest(&cfg, "0.59.2").await.unwrap_err();
        assert!(err.contains("512"), "{err}");
    }

    #[tokio::test]
    async fn test_fetch_official_manifest_unreachable_reports_error() {
        // 指向一个必然拒绝连接的端口：必须快速失败而不是挂住。
        let cfg = ManifestFetchConfig {
            api_base: "http://127.0.0.1:1".into(),
            timeout: Duration::from_millis(500),
            connect_timeout: Duration::from_millis(300),
            ..ManifestFetchConfig::default()
        };
        let err = fetch_official_manifest(&cfg, "0.59.2").await.unwrap_err();
        assert!(err.contains("release request failed"), "{err}");
    }

    /// 真实 GitHub API 冒烟（默认 ignored：CI 无外网时不该失败）。
    ///
    /// 手工验证清单解析在真实响应上成立：`cargo test -- --ignored live_release`。
    /// 断言的是结构不变量，不锁死具体版本号。
    #[tokio::test]
    #[ignore = "requires network access to api.github.com"]
    async fn live_release_manifest_parses_real_github_response() {
        let cfg = ManifestFetchConfig::default();
        // 用 v0.59.2（本轮已核实存在且 8 个资产都带 digest）。
        let m = fetch_official_manifest(&cfg, "0.59.2")
            .await
            .expect("live GitHub release fetch must succeed");
        assert_eq!(m.tag, "v0.59.2");
        assert_eq!(m.version, "0.59.2");
        assert!(m.assets.len() >= 4, "assets: {:?}", m.assets.len());

        // 四种形态都必须能从真实清单里定位到，且 digest/size 合法。
        for art in [
            ArtifactRef::new(Variant::Cli, "aarch64", false).unwrap(),
            ArtifactRef::new(Variant::Cli, "x86_64", true).unwrap(),
            ArtifactRef::new(Variant::Ui, "aarch64", false).unwrap(),
            ArtifactRef::new(Variant::Ui, "x86_64", false).unwrap(),
        ] {
            let a = m
                .asset(&art)
                .unwrap_or_else(|| panic!("missing live asset {}", art.filename()));
            assert_eq!(a.sha256.len(), 64, "{}: digest must be 64 hex", a.name);
            assert!(a.size > 0, "{}: size must be > 0", a.name);
            let path = official_path_from_url(&a.download_url)
                .unwrap_or_else(|| panic!("{}: unfetchable download_url", a.name));
            assert_eq!(path, art.official_path("0.59.2"), "{}", a.name);
        }

        // 真实 digest 必须与已知值一致（防止解析到别的 release/资产）。
        let cli = m
            .asset(&ArtifactRef::new(Variant::Cli, "aarch64", false).unwrap())
            .unwrap();
        assert_eq!(
            cli.sha256,
            "7eabddeeb9d4b8d2366e45c25c579d1226b2768bdbea365d0634ace9628469ce"
        );
    }
}
