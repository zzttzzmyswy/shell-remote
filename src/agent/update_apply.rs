//! 换机助手（隐藏子命令 `update-apply`）与一次性身份交接。
//!
//! 升级的**装字节**部分在 [`upgrade`](crate::agent::upgrade)：下载、校验、
//! 冒烟测试。本模块负责最后一步——把已经在磁盘上的新字节换成正在运行的
//! 二进制，并在换机前后保住 agent 的身份（`session_id` + tokens）。
//!
//! 为什么必须是独立进程：Windows 无法替换正在运行的 `.exe`，Unix 也无法在
//! 自己退出之后再拉起自己。旧 agent 因此在**下载与校验全部完成之后**启动
//! 本模块的助手进程，把身份与原始参数交接给它，然后退出。助手是短生命周期
//! 进程：不装 systemd/SCM、不留 supervisor、不留长期身份文件。
//!
//! 时序（握手全部走匿名管道，不信任何"成功"文件标记）：
//!
//! ```text
//! 旧 agent --(stdout 匿名管道)--> 助手      助手就绪才回 ready，旧进程收到才退出
//! 助手 --------------------------------------> 按确切 old_pid 等旧进程退出
//! 助手 --------------------------------------> 旧文件改名备份，新文件原子就位
//! 助手 --(stderr 匿名管道)------> 新 agent  注册成功才回 registered 标记
//! 助手 --------------------------------------> 成功删备份；失败回滚并原身份拉起旧版
//! ```
//!
//! 三个不变式：
//!
//! 1. **旧进程只在自己退出这一件事上有主动权**：助手没 ready、目录不可写、
//!    新文件校验不符——任何一步失败都发生在旧进程退出之前，旧 agent 继续在线。
//! 2. **等待按 PID，不按进程名**：同一台机器上跑两个同名 agent 是常态（临时
//!    设备重复注册），按名字等会互相阻塞、按名字杀会误杀。
//! 3. **Token 只出现在 0600/当前用户 ACL 的交接文件里**：不进 argv（可能被
//!    `ps` 看到）、不进日志、不进握手管道。
//!
//! 回滚是有界的：助手只负责一次换机，失败就回滚一次并原样拉起旧版，不做
//! 重试循环——否则新旧进程会互相驱逐，设备彻底失联。

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::agent::client::UpgradeRestore;

// ── 常量 ────────────────────────────────────────────────────────────

/// 交接文件格式版本。旧 agent 写的文件只能被同代的助手读懂——升级永远是
/// 「同一次构建里的助手」，所以版本不符直接拒绝，不做兼容猜测。
pub const HANDOFF_SCHEMA: u32 = 1;

/// 隐藏恢复参数（argv）。只在换机拉起新进程时使用；回滚走环境变量
/// [`RESTORE_ENV`]，因为被回滚的旧版二进制可能还不认识这个参数。
pub const RESTORE_FLAG: &str = "--upgrade-restore";
/// 隐藏恢复参数（环境变量）。**回滚路径唯一可用**的通道：不认识它的旧版
/// 二进制会忽略它而不是像未知 argv 那样直接启动失败。
pub const RESTORE_ENV: &str = "SR_UPGRADE_RESTORE";
/// 回滚原因（环境变量）。助手拉起旧版时设置，旧版注册时转成 relay 的
/// `upgrade_failed`，让管理界面看到"回滚了、为什么"。
pub const ROLLBACK_ENV: &str = "SR_UPGRADE_ROLLBACK";

/// 助手→旧 agent 的就绪信号（stdout 一行）。
pub const READY_LINE: &str = "ready";
/// 助手→旧 agent 的失败信号前缀（stdout 一行）。
pub const ERROR_PREFIX: &str = "error: ";
/// 新 agent→助手的注册成功标记（stderr 一行）。带 upgrade_id 便于对账。
pub const REGISTERED_MARKER: &str = "shell-remote: upgrade-registered";

/// 助手必须在旧进程退出前完成的准备工作上限：解析交接 → 校验新文件 →
/// 探目录可写 → 回 ready。再久说明环境有问题，旧 agent 应当放弃本次升级。
pub const HELPER_READY_TIMEOUT: Duration = Duration::from_secs(20);
/// 等旧进程退出的上限。旧 agent 收到 ready 后立刻退出，这里只是兜底。
pub const OLD_EXIT_TIMEOUT: Duration = Duration::from_secs(45);
/// 等新进程注册成功的上限（契约要求 60–90 秒）。
pub const REGISTER_TIMEOUT: Duration = Duration::from_secs(75);
/// 轮询粒度。
const POLL: Duration = Duration::from_millis(100);
/// 备份文件名后缀（中间插 upgrade_id 前 8 位，避免两次升级互相覆盖）。
const BACKUP_INFIX: &str = ".sr-old-";
/// Windows 自我搬运出来的助手副本前缀（用完即删，启动时顺手清理残留）。
const HELPER_COPY_PREFIX: &str = ".shell-remote-update-apply-";

/// "本次换机待恢复"的指针文件名（`0700` 私有目录，内容两行：
/// `upgrade_id` + 交接文件路径）。
///
/// 只在**受管 service 协调式换机**里写：那时新 agent 由 `systemctl start`
/// 拉起，环境由 unit 决定，拿不到助手设置的 `SR_UPGRADE_RESTORE`，只能靠这个
/// 指针找到交接文件。指针本身不含 Token。
const RESTORE_PENDING_FILE: &str = ".restore-pending";

/// 助手的三段等待上限。
///
/// 抽成值而不是写死常量，是为了让测试能用毫秒级的上限跑**同一条**代码路径
/// ——真实值（45s / 75s）会让回归测试跑上几分钟，而用环境变量注入又会和
/// 并行测试互相覆盖。生产路径永远用 [`HelperTimeouts::default`]。
#[derive(Debug, Clone, Copy)]
pub struct HelperTimeouts {
    /// 等旧进程退出。
    pub old_exit: Duration,
    /// 等新进程注册。
    pub register: Duration,
}

impl Default for HelperTimeouts {
    fn default() -> Self {
        Self {
            old_exit: OLD_EXIT_TIMEOUT,
            register: REGISTER_TIMEOUT,
        }
    }
}

/// 退出码：新进程注册成功。
pub const EXIT_REGISTERED: i32 = 0;
/// 退出码：还没动磁盘就放弃了（旧 agent 仍在运行）。
pub const EXIT_ABORTED: i32 = 2;
/// 退出码：换机失败并已回滚到旧版。
pub const EXIT_ROLLED_BACK: i32 = 3;

// ── 交接文件 ────────────────────────────────────────────────────────

/// 一次换机所需的全部信息。写在私有交接文件里，由助手读回。
///
/// `argv` 是**原始参数**（去掉 argv[0] 和隐藏恢复参数），助手用它原样拉起
/// 新旧两个进程；tokens 只用于新进程的一次性注册。
#[derive(Clone, PartialEq, Eq)]
pub struct RestartContext {
    pub session_id: String,
    pub tokens: Vec<(String, String)>,
    /// relay 根地址。回滚时助手要替旧版把身份绑回去，所以必须知道往哪发。
    pub relay_base: Option<String>,
    /// `--relay-insecure`：自签 relay 上绑定身份不能因为证书校验失败而放弃。
    pub relay_insecure: bool,
    /// 原始 `--key`（固定 key 设备才有）。回滚时**唯一**能把身份拉回来的
    /// 东西：旧版二进制不认识交接文件，但它认识自己的 `--key`。
    pub fixed_key: Option<String>,
    /// 原始 `--token-type`（rw/ro/both）。回滚用 `--key` 重注册时权限必须
    /// 与升级前一模一样，否则 `both` 会悄悄丢掉 ro Token。
    pub token_type: String,
    pub argv: Vec<OsString>,
    pub old_pid: u32,
    /// 旧进程的启动时刻（Unix `/proc/<pid>/stat` 的 starttime，Windows
    /// FILETIME）。只用于防止 PID 复用后误等/误杀一个无关进程。
    pub old_start: Option<u64>,
    pub upgrade_id: String,
    pub target_version: String,
    /// 正在运行的可执行文件路径（规范化后，符号链接已解析）。
    pub target_path: PathBuf,
    /// 已下载并校验通过的新文件（与 target 同目录，同一文件系统）。
    pub new_binary: PathBuf,
    pub new_sha256: String,
    /// 旧 agent 所在的 systemd service cgroup（`/proc/self/cgroup` 的 v2 路径）。
    ///
    /// **只**用于换机后识别 systemd 按 `Restart=always` 补起的重复实例：新进程
    /// 拿它和新旧进程的 cgroup 做比对，认出"服务侧补起的那个"并给出可执行的
    /// 运维命令（只报告，不终止任何进程）。非 systemd 环境恒为 `None`，
    /// 那条路径行为不变。
    ///
    /// 它是路径，不是凭据：不含 Token、不含 argv。
    pub service_cgroup: Option<String>,
    /// **换机前**算好的 systemd 协调方案（MYS-2425-D 设计决定：fail-closed）。
    ///
    /// 必须在旧 agent 退出、磁盘改动之前算出来：`Restart=always` 的
    /// `MainPID` 在重启窗口内是 0，事后无法判定；而且"能不能协调"本就该在
    /// 动手之前问清楚。`None` = 非 systemd 托管（走既有直接助手路径）。
    pub managed_plan: Option<super::systemd_helper::ManagedPlan>,
    /// 旧 agent 所在 unit 的 `Restart=` 策略（`always` / `on-failure` / …）。
    ///
    /// 换机成功后在 `Restart=always` 下的行为与其它策略**根本不同**：
    ///
    /// - `always`：旧主进程一退出，systemd 就用**已替换的新二进制**再起一个，
    ///   得到一个受原 service 托管的唯一 MainPID。此时助手**不能**再自己起
    ///   新进程，否则就是两个无人协调的 agent 互相驱逐。
    /// - 其它（`on-failure` / `no` / 空）：systemd 不会因"正常退出"补起，
    ///   助手必须自己拉起新 agent，否则设备直接失联。
    ///
    /// 由旧 agent 在交接时读一次写进来（非 systemd 环境恒为 `None`）。
    pub service_restart_policy: Option<String>,
}

impl std::fmt::Debug for RestartContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Token 永不进日志：`{:?}` 出现在任何 tracing 字段里都必须安全。
        f.debug_struct("RestartContext")
            .field("session_id", &self.session_id)
            .field(
                "tokens",
                &format!("<{} token(s) redacted>", self.tokens.len()),
            )
            .field("argv", &self.argv)
            // 只报"有没有"：`--key` 本身就是凭据。
            .field("fixed_key", &self.fixed_key.as_ref().map(|_| "<redacted>"))
            .field("token_type", &self.token_type)
            .field("old_pid", &self.old_pid)
            .field("upgrade_id", &self.upgrade_id)
            .field("target_version", &self.target_version)
            .field("target_path", &self.target_path)
            .field("new_binary", &self.new_binary)
            .field("new_sha256", &self.new_sha256)
            .field("service_cgroup", &self.service_cgroup)
            .field("managed_plan", &self.managed_plan)
            .field("service_restart_policy", &self.service_restart_policy)
            .finish()
    }
}

/// 磁盘上的 JSON 形态。路径与参数按平台原生字节 base64 编码：Unix 的
/// `OsStr` 不保证 UTF-8，用 `to_string_lossy` 会让带非 UTF-8 字符的参数
/// （某些 locale 下的目录名）静默变成另一个路径，拉起的是错误的东西。
#[derive(Serialize, Deserialize)]
struct HandoffFile {
    schema: u32,
    session_id: String,
    tokens: Vec<TokenEntry>,
    #[serde(default)]
    fixed_key: Option<String>,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    relay_base: Option<String>,
    #[serde(default)]
    relay_insecure: bool,
    os_encoding: String,
    argv: Vec<String>,
    old_pid: u32,
    #[serde(default)]
    old_start: Option<u64>,
    upgrade_id: String,
    target_version: String,
    target_path: String,
    new_binary: String,
    new_sha256: String,
    /// 旧 agent 的 service cgroup（systemd 托管时才有）。见 [`RestartContext`]。
    #[serde(default)]
    service_cgroup: Option<String>,
    /// 旧 agent 所在 unit 的 `Restart=` 策略（systemd 托管时才有）。
    #[serde(default)]
    service_restart_policy: Option<String>,
    /// 换机前算好的协调方案：`None` / `direct` / `coordinated:<unit>` / `refuse:<原因>`。
    ///
    /// 用字符串而不是枚举序列化，是为了让交接文件对旧版/未来版都保持可读。
    #[serde(default)]
    managed_plan: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct TokenEntry {
    token: String,
    permission: String,
}

/// 本平台的 `OsStr` 字节表示名。跨平台读到自己不认识的形式直接拒绝——
/// 交接文件不跨平台传递，出现这种情况只可能是损坏或伪造。
const fn os_encoding_name() -> &'static str {
    #[cfg(unix)]
    {
        "unix-bytes"
    }
    #[cfg(windows)]
    {
        "windows-wide"
    }
    #[cfg(not(any(unix, windows)))]
    {
        "unsupported"
    }
}

#[cfg(unix)]
fn os_to_bytes(s: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

#[cfg(windows)]
fn os_to_bytes(s: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut out = Vec::new();
    for unit in s.encode_wide() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

#[cfg(not(any(unix, windows)))]
fn os_to_bytes(s: &OsStr) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn bytes_to_os(bytes: Vec<u8>) -> Option<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Some(OsString::from_vec(bytes))
}

#[cfg(windows)]
fn bytes_to_os(bytes: Vec<u8>) -> Option<OsString> {
    use std::os::windows::ffi::OsStringExt;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(OsString::from_wide(&units))
}

#[cfg(not(any(unix, windows)))]
fn bytes_to_os(bytes: Vec<u8>) -> Option<OsString> {
    String::from_utf8(bytes).ok().map(OsString::from)
}

fn encode_os(s: &OsStr) -> String {
    base64::engine::general_purpose::STANDARD.encode(os_to_bytes(s))
}

fn decode_os(s: &str) -> Option<OsString> {
    let raw = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    bytes_to_os(raw)
}

fn decode_path(s: &str) -> Option<PathBuf> {
    decode_os(s).map(PathBuf::from)
}

fn encode_path(p: &Path) -> String {
    encode_os(p.as_os_str())
}

/// 测试入口：把协调方案编码（供 systemd_helper 的测试复用同一份编码逻辑）。
#[cfg(test)]
pub(crate) fn encode_plan_for_test(plan: &super::systemd_helper::ManagedPlan) -> String {
    encode_plan(plan)
}

/// 测试入口：解码。
#[cfg(test)]
pub(crate) fn decode_plan_for_test(raw: &str) -> Option<super::systemd_helper::ManagedPlan> {
    decode_plan(raw)
}

/// 把协调方案编码成交接文件里的一行（不含任何凭据）。
fn encode_plan(plan: &super::systemd_helper::ManagedPlan) -> String {
    use super::systemd_helper::ManagedPlan;
    match plan {
        ManagedPlan::Direct => "direct".to_string(),
        ManagedPlan::Coordinated { unit } => format!("coordinated:{unit}"),
        ManagedPlan::Refuse { unit, reason } => match unit {
            Some(u) => format!("refuse:{u}:{reason}"),
            None => format!("refuse::{reason}"),
        },
    }
}

/// [`encode_plan`] 的逆过程。无法解析时返回 `None`（当作非托管，交给既有路径）。
fn decode_plan(raw: &str) -> Option<super::systemd_helper::ManagedPlan> {
    use super::systemd_helper::ManagedPlan;
    if raw == "direct" {
        return Some(ManagedPlan::Direct);
    }
    if let Some(rest) = raw.strip_prefix("coordinated:") {
        return Some(ManagedPlan::Coordinated {
            unit: rest.to_string(),
        });
    }
    if let Some(rest) = raw.strip_prefix("refuse:") {
        let (unit, reason) = rest.split_once(':').unwrap_or(("", rest));
        return Some(ManagedPlan::Refuse {
            unit: if unit.is_empty() {
                None
            } else {
                Some(unit.to_string())
            },
            reason: reason.to_string(),
        });
    }
    None
}

impl RestartContext {
    /// 从旧 agent 的视角构造：`argv` 必须已剥掉隐藏恢复参数（否则新进程
    /// 会去读上一轮的交接文件，身份与 upgrade_id 全错）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        tokens: Vec<(String, String)>,
        fixed_key: Option<String>,
        token_type: String,
        relay_base: Option<String>,
        relay_insecure: bool,
        raw_argv: Vec<OsString>,
        target_path: PathBuf,
        new_binary: PathBuf,
        new_sha256: String,
        upgrade_id: String,
        target_version: String,
        service_cgroup: Option<String>,
        service_restart_policy: Option<String>,
        managed_plan: Option<super::systemd_helper::ManagedPlan>,
    ) -> Self {
        Self {
            session_id,
            tokens,
            fixed_key,
            token_type,
            relay_base,
            relay_insecure,
            argv: strip_restore_args(raw_argv),
            old_pid: std::process::id(),
            old_start: process_start_stamp(std::process::id()),
            upgrade_id,
            target_version,
            target_path,
            new_binary,
            new_sha256,
            service_cgroup,
            service_restart_policy,
            managed_plan,
        }
    }

    fn to_file(&self) -> HandoffFile {
        HandoffFile {
            schema: HANDOFF_SCHEMA,
            session_id: self.session_id.clone(),
            tokens: self
                .tokens
                .iter()
                .map(|(t, p)| TokenEntry {
                    token: t.clone(),
                    permission: p.clone(),
                })
                .collect(),
            fixed_key: self.fixed_key.clone(),
            token_type: self.token_type.clone(),
            relay_base: self.relay_base.clone(),
            relay_insecure: self.relay_insecure,
            os_encoding: os_encoding_name().to_string(),
            argv: self.argv.iter().map(|a| encode_os(a)).collect(),
            old_pid: self.old_pid,
            old_start: self.old_start,
            upgrade_id: self.upgrade_id.clone(),
            target_version: self.target_version.clone(),
            target_path: encode_path(&self.target_path),
            new_binary: encode_path(&self.new_binary),
            new_sha256: self.new_sha256.clone(),
            service_cgroup: self.service_cgroup.clone(),
            service_restart_policy: self.service_restart_policy.clone(),
            managed_plan: self.managed_plan.as_ref().map(encode_plan),
        }
    }

    fn from_file(f: HandoffFile) -> Result<Self, String> {
        if f.schema != HANDOFF_SCHEMA {
            return Err(format!(
                "handoff schema {} is not supported (expected {})",
                f.schema, HANDOFF_SCHEMA
            ));
        }
        if f.os_encoding != os_encoding_name() {
            return Err(format!(
                "handoff was written with `{}` argument encoding, this build uses `{}`",
                f.os_encoding,
                os_encoding_name()
            ));
        }
        let mut argv = Vec::with_capacity(f.argv.len());
        for (i, a) in f.argv.iter().enumerate() {
            argv.push(decode_os(a).ok_or_else(|| format!("cannot decode argv[{i}]"))?);
        }
        let target_path =
            decode_path(&f.target_path).ok_or_else(|| "cannot decode target_path".to_string())?;
        let new_binary =
            decode_path(&f.new_binary).ok_or_else(|| "cannot decode new_binary".to_string())?;
        if f.new_sha256.len() != 64 || !f.new_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("handoff carries a malformed sha256".to_string());
        }
        if f.tokens.is_empty() {
            return Err("handoff carries no tokens".to_string());
        }
        Ok(Self {
            session_id: f.session_id,
            tokens: f
                .tokens
                .into_iter()
                .map(|t| (t.token, t.permission))
                .collect(),
            fixed_key: f.fixed_key,
            relay_base: f.relay_base,
            relay_insecure: f.relay_insecure,
            token_type: if f.token_type.is_empty() {
                "rw".to_string()
            } else {
                f.token_type
            },
            argv,
            old_pid: f.old_pid,
            old_start: f.old_start,
            upgrade_id: f.upgrade_id,
            target_version: f.target_version,
            target_path,
            new_binary,
            new_sha256: f.new_sha256.to_ascii_lowercase(),
            service_cgroup: f.service_cgroup,
            service_restart_policy: f.service_restart_policy,
            managed_plan: f.managed_plan.as_deref().and_then(decode_plan),
        })
    }

    /// 交接文件所在的私有目录。
    ///
    /// 三个候选都要求"只有当前用户可进"：`HOME/.shell-remote/upgrade`（常态）、
    /// `XDG_RUNTIME_DIR`（无 HOME 的服务账号）、带 uid 的临时目录（最后兜底）。
    /// 目录本身用 `0700` 创建（`DirBuilder::mode` 在创建时就生效，不存在
    /// "先建后改权限"的可抢占窗口）。
    pub fn private_dir() -> std::io::Result<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            candidates.push(PathBuf::from(home).join(".shell-remote").join("upgrade"));
        }
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").filter(|h| !h.is_empty()) {
            candidates.push(PathBuf::from(rt).join("shell-remote-upgrade"));
        }
        #[cfg(windows)]
        {
            if let Some(profile) = std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()) {
                candidates.insert(
                    0,
                    PathBuf::from(profile).join(".shell-remote").join("upgrade"),
                );
            }
        }
        #[cfg(unix)]
        {
            candidates.push(
                std::env::temp_dir().join(format!("shell-remote-upgrade-{}", unsafe {
                    libc::geteuid()
                })),
            );
        }
        #[cfg(not(unix))]
        {
            candidates.push(std::env::temp_dir().join("shell-remote-upgrade"));
        }

        let mut last_err = None;
        for dir in candidates {
            match ensure_private_dir(&dir) {
                Ok(()) => return Ok(dir),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no private directory available",
            )
        }))
    }

    /// 把交接文件写进私有目录，返回路径。
    ///
    /// 三重防护：随机文件名（不可预测）、`create_new`（不可抢占/不覆盖）、
    /// 缩到当前用户的权限（Unix `0600`，Windows 当前用户 DACL）。
    pub fn write_handoff(&self) -> std::io::Result<PathBuf> {
        self.write_handoff_into(&Self::private_dir()?)
    }

    /// 写进指定目录（测试用；调用方负责保证该目录只有当前用户可访问）。
    ///
    /// 抽出来是为了让测试用隔离的临时目录，而不是所有人的 `~/.shell-remote/`
    /// ——测试并行跑，共用真实 HOME 下同一个目录会互相踩，也会污染开发机。
    /// 校验逻辑与生产路径完全相同（`read_handoff` 会复核权限与属主）。
    pub fn write_handoff_into(&self, dir: &Path) -> std::io::Result<PathBuf> {
        use rand::Rng as _;
        ensure_private_dir(dir)?;
        let mut rng = rand::thread_rng();
        let mut last_err = None;
        for _ in 0..8 {
            let name = format!(".handoff-{:016x}.json", rng.gen::<u64>());
            let path = dir.join(name);
            match write_private_new(&path, &self.encode()?) {
                Ok(()) => return Ok(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "cannot allocate a handoff path",
            )
        }))
    }

    fn encode(&self) -> std::io::Result<Vec<u8>> {
        serde_json::to_vec(&self.to_file())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// 读交接文件并校验它确实是我们自己的私有文件（防"别的用户放一个
    /// 文件诱导助手替换二进制"）。权限/属主不符即拒绝，不尝试修复。
    pub fn read_handoff(path: &Path) -> std::io::Result<Self> {
        verify_private_file(path)?;
        let raw = std::fs::read(path)?;
        if raw.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "handoff file is unexpectedly large",
            ));
        }
        let file: HandoffFile = serde_json::from_slice(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Self::from_file(file).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// 创建（或确认）只有当前用户可访问的目录。
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return verify_private_dir(dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // `recursive(true)` 只对最后一段保证 mode，父目录可能是 0755
        //（`~/.shell-remote`）——那是可接受的，秘密只在最后一层。
        verify_private_dir(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)?;
        Ok(())
    }
}

#[cfg(unix)]
fn verify_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "handoff directory is not a directory",
        ));
    }
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "handoff directory is owned by another user",
        ));
    }
    if meta.mode() & 0o077 != 0 {
        // 目录里放的是 Token：别人能列目录就已经是不能接受的状态。
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "handoff directory is accessible by other users (expected 0700)",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_private_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// 以 `0600` / 当前用户 ACL 创建一个**全新**文件（不覆盖已存在的路径）。
fn write_private_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    #[cfg(windows)]
    restrict_to_current_user(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn verify_private_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "handoff path is not a regular file",
        ));
    }
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "handoff file is owned by another user",
        ));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "handoff file is readable by other users (expected 0600)",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_private_file(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "handoff path is not a regular file",
        ));
    }
    Ok(())
}

/// Windows：把文件 DACL 收紧到"只有当前用户"，并断开继承。
///
/// 不用 `icacls`（外部进程、命令行插值、输出解析都不受控）。直接构一个
/// 单 ACE 的 DACL：`AddAccessAllowedAce(GENERIC_ALL, 当前用户 SID)` +
/// `PROTECTED_DACL_SECURITY_INFORMATION`（父目录的 ACE 不再继承进来）。
#[cfg(windows)]
fn restrict_to_current_user(path: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::GENERIC_ALL;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::{
        AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
        InitializeSecurityDescriptor, SetFileSecurityW, SetSecurityDescriptorDacl, TokenUser, ACL,
        ACL_REVISION, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn last_os_error(what: &str) -> std::io::Error {
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("{what} failed (win32 error {code})"),
        )
    }

    // 对齐到 8 字节：TOKEN_USER / SECURITY_DESCRIPTOR / ACL 都要求指针对齐。
    #[repr(C, align(8))]
    struct SecDesc([u8; 32]);

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(last_os_error("OpenProcessToken"));
        }
        let mut needed: u32 = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            CloseHandle(token);
            return Err(last_os_error("GetTokenInformation(size)"));
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut _,
            (buf.len() * 8) as u32,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(last_os_error("GetTokenInformation"));
        }
        let token_user = std::ptr::read_unaligned(buf.as_ptr() as *const TOKEN_USER);
        let sid = token_user.User.Sid;
        if sid.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "current user SID is unavailable",
            ));
        }
        let sid_len = GetLengthSid(sid) as usize;

        // ACL 头 8 字节 + ACCESS_ALLOWED_ACE 头 8 字节 + SID，再留 8 字节余量。
        let acl_size: usize = 8 + 8 + sid_len + 8;
        let mut acl_backing = vec![0u64; acl_size.div_ceil(8)];
        let acl_ptr = acl_backing.as_mut_ptr() as *mut ACL;
        if InitializeAcl(acl_ptr, (acl_backing.len() * 8) as u32, ACL_REVISION) == 0 {
            return Err(last_os_error("InitializeAcl"));
        }
        if AddAccessAllowedAce(acl_ptr, ACL_REVISION, GENERIC_ALL, sid) == 0 {
            return Err(last_os_error("AddAccessAllowedAce"));
        }

        let mut sd_backing = SecDesc([0u8; 32]);
        let sd_ptr = sd_backing.0.as_mut_ptr() as *mut core::ffi::c_void;
        if InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION) == 0 {
            return Err(last_os_error("InitializeSecurityDescriptor"));
        }
        if SetSecurityDescriptorDacl(sd_ptr, 1, acl_ptr, 0) == 0 {
            return Err(last_os_error("SetSecurityDescriptorDacl"));
        }
        let wide: Vec<u16> = {
            use std::os::windows::ffi::OsStrExt;
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        };
        let info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
        if SetFileSecurityW(wide.as_ptr(), info, sd_ptr) == 0 {
            return Err(last_os_error("SetFileSecurityW"));
        }
        let _ = LocalFree(std::ptr::null_mut());
    }
    Ok(())
}

// ── 恢复参数（新 agent 侧）─────────────────────────────────────────

/// 从 argv 剥离隐藏恢复参数（`--upgrade-restore X` / `--upgrade-restore=X`）。
///
/// 换机拉起的新进程带着这个参数；它**再**升级时必须把上一轮剥掉，否则会把
/// 上一轮的 session/tokens/upgrade_id 当成这一轮的。
pub fn strip_restore_args(argv: Vec<OsString>) -> Vec<OsString> {
    let mut out = Vec::with_capacity(argv.len());
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        if arg == OsStr::new(RESTORE_FLAG) {
            // 连同它的值一起丢掉。
            let _ = it.next();
            continue;
        }
        if let Some(s) = arg.to_str() {
            let prefix = format!("{RESTORE_FLAG}=");
            if s.starts_with(&prefix) {
                continue;
            }
        }
        out.push(arg);
    }
    out
}

/// 本次进程是否由换机助手拉起。`flag` 是 CLI 显式给出的隐藏参数；没有就
/// 看环境变量（回滚路径只走环境变量）。
///
/// 三种结果要分清（不能用一个 `Option` 折叠掉）：
///
/// - `Ok(None)`：普通启动，没有任何恢复意图 —— 照常注册。
/// - `Ok(Some(path))`：换机助手把我拉起来的，必须按交接文件恢复身份。
/// - `Err(reason)`：**明确要求了恢复**但拿不到可用的路径。此时必须
///   fail-closed：如果照常走全新注册，临时设备会拿到新 session_id/Token，
///   而助手还在等 75 秒的 `registered`，最后把"已经变成另一台设备"当成
///   升级成功。宁可让本进程启动失败（助手据此回滚），也不能静默换身份。
pub fn restore_handoff_path(flag: Option<&str>) -> Result<Option<PathBuf>, String> {
    let from_flag = flag
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty());
    let from_env = std::env::var_os(RESTORE_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    // 显式给了参数但为空（`--upgrade-restore ""`）= 调用方本来就要恢复，
    // 只是没传出路径：同样不能当作普通启动。
    if flag.is_some() && from_flag.is_none() {
        return Err("the upgrade restore path is empty".to_string());
    }
    match from_flag.or(from_env) {
        Some(path) => Ok(Some(path)),
        None => Ok(None),
    }
}

/// 读取并**消费**"本次换机待恢复"指针。
///
/// 只认 `0700` 私有目录里、`0600`、属主为自己的指针（复用私有文件校验），
/// 指向的交接文件必须真实存在；读到后立刻删除指针，保证只恢复一次。
///
/// 返回 `None` = 没有待恢复的换机（普通启动）。
pub fn take_restore_pending() -> Option<PathBuf> {
    let dir = RestartContext::private_dir().ok()?;
    take_restore_pending_in(&dir)
}

/// [`take_restore_pending`] 的显式目录版本（测试用，校验逻辑完全相同）。
pub fn take_restore_pending_in(dir: &Path) -> Option<PathBuf> {
    let ptr = dir.join(RESTORE_PENDING_FILE);
    if !ptr.exists() {
        return None;
    }
    verify_private_file(&ptr).ok()?;
    let body = std::fs::read_to_string(&ptr).ok()?;
    let handoff = body.lines().nth(1).map(PathBuf::from)?;
    // 交接文件必须在同一私有目录里（不给"指向任意路径"留口子）。
    if handoff.parent() != Some(dir) || !handoff.is_file() {
        let _ = std::fs::remove_file(&ptr);
        return None;
    }
    let _ = std::fs::remove_file(&ptr);
    Some(handoff)
}

/// 解析并加载一次性恢复信息（CLI/UI 共用的入口）。
///
/// fail-closed：**明确要求了恢复**（隐藏参数或环境变量）但读不出来时返回
/// `Err`，调用方必须让进程启动失败。这样助手会看到新进程提前退出并回滚，
/// 而不是让临时设备静默变成另一台设备。
pub fn resolve_restore(flag: Option<&str>) -> Result<Option<RestoredIdentity>, String> {
    match restore_handoff_path(flag)? {
        Some(path) => RestoredIdentity::load(&path).map(Some),
        // 没有显式恢复通道 → 看是否有"受管 service 协调式换机"留下的指针。
        //
        // 这条只在 `systemctl start` 拉起的新进程上命中（它拿不到
        // `SR_UPGRADE_RESTORE`）。指针由**助手**在换机前写入、启动成功后删除，
        // 因此不存在"自动补起的进程误认领别人的交接"：整个过程只有助手安排，
        // 没有 systemd 自动补起参与（设计决定 MYS-2425-D）。
        None => match take_restore_pending() {
            Some(path) => RestoredIdentity::load(&path).map(Some),
            None => Ok(None),
        },
    }
}

/// 一次性身份恢复的完整输入：交给 `connect_with_retry` 的初始 cached_tokens
/// 与注册回执，以及"成功之后要删交接文件"这个义务。
pub struct RestoredIdentity {
    pub restore: UpgradeRestore,
    pub handoff: PathBuf,
    /// 旧 agent 的 service cgroup（systemd 托管时才有）。注册成功后用它识别
    /// systemd 按 `Restart=always` 补起的重复实例；非 systemd 环境恒为 `None`。
    pub service_cgroup: Option<String>,
    /// 是否已经完成（交接文件已删、助手已回执）。`start()` 用它在首次会话
    /// 结束后停止传递恢复语义。
    consumed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for RestoredIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 与 RestartContext 同一条纪律：凭据永不进 Debug 输出。
        f.debug_struct("RestoredIdentity")
            .field("session_id", &self.restore.session_id)
            .field(
                "tokens",
                &format!("<{} redacted>", self.restore.tokens.len()),
            )
            .field("upgrade_id", &self.restore.upgrade_id)
            .field("handoff", &self.handoff)
            .field("consumed", &self.consumed())
            .finish()
    }
}

impl RestoredIdentity {
    /// 读交接文件。**显式恢复模式下不吞错误**（fail-closed）。
    ///
    /// 之前这里返回 `Option`：文件读不出来就当普通启动，于是临时设备会拿到
    /// 全新的 session_id/Token —— 设备变成另一台，而助手还在等 `registered`，
    /// 最终把这次"静默换身份"记成升级成功。现在把失败原因交回调用方，由它让
    /// 进程启动失败，助手据此回滚并保留备份。
    pub fn load(path: &Path) -> Result<Self, String> {
        // 回滚原因一次性消费：不能让后续子进程继承成"我们也回滚了"。
        let rollback = std::env::var(ROLLBACK_ENV).ok().filter(|s| !s.is_empty());
        std::env::remove_var(ROLLBACK_ENV);
        Self::load_with_rollback(path, rollback)
    }

    /// [`load`] 的显式传参版本。测试用它避免争抢进程级环境变量（并行跑时
    /// 两个用例会互相覆盖 `SR_UPGRADE_ROLLBACK`）。
    ///
    /// 错误信息只包含路径与 IO/格式原因，**不含任何 Token**——它会被写进
    /// stderr，进而被助手转存到日志。
    pub fn load_with_rollback(path: &Path, rollback: Option<String>) -> Result<Self, String> {
        let ctx = RestartContext::read_handoff(path)
            .map_err(|e| format!("cannot read the upgrade handoff {}: {e}", path.display()))?;
        Ok(Self {
            restore: UpgradeRestore {
                tokens: ctx.tokens.clone(),
                session_id: ctx.session_id.clone(),
                fixed_key: ctx.fixed_key.clone(),
                upgrade_id: ctx.upgrade_id.clone(),
                target_version: ctx.target_version.clone(),
                rollback,
            },
            handoff: path.to_path_buf(),
            service_cgroup: ctx.service_cgroup.clone(),
            consumed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// 已经消费过（交接文件删了、助手已收到回执）。
    pub fn consumed(&self) -> bool {
        self.consumed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 注册成功之后调用：删交接文件（身份已进内存，文件再无用途），向助手
    /// 报 `registered`。标记写 stderr——助手的匿名管道读的就是这条流。
    ///
    /// 幂等：重连后的第二次注册不会再删/再报一次。
    pub fn complete(&self) {
        if self
            .consumed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        match std::fs::remove_file(&self.handoff) {
            Ok(()) => tracing::info!(
                handoff = %self.handoff.display(),
                "upgrade handoff consumed and removed"
            ),
            Err(e) => tracing::warn!(
                handoff = %self.handoff.display(),
                error = %e,
                "cannot remove the consumed upgrade handoff file"
            ),
        }
        announce_registered(
            &self.restore.upgrade_id,
            &self.restore.target_version,
            &self.restore.session_id,
        );
        // 重复进程的收尾**不在这里做**：它要占用一个观察窗口（systemd 是隔
        // `RestartSec` 才补起的），而 `complete()` 紧跟在注册成功之后、排在
        // 任何可能失败的初始化之前——在这里等窗口会平白推迟会话建立。由
        // 收尾动作（重复实例的识别）已随 fail-closed 方案移除。
    }

    /// 回滚后的重启要把失败原因送回 relay——否则管理界面只会看到一个
    /// "升级了但版本没变"的谜。`None` = 正常升级后的首次注册，无需上报。
    /// 换机成功后的终结帧。
    ///
    /// relay 在下发任务时先写一条 `started`，agent 再推 downloading/swapping。
    /// 如果新进程不上报 `registered`，面板会永远停在 `swapping`——尤其同版本
    /// 重装（`force`）时版本号没变，界面上完全看不出已经成功。所以由新进程
    /// 自己确认"我上线了，这就是结论"。
    pub fn registered_frame(&self, session_id: &str) -> Option<String> {
        if self.restore.rollback.is_some() {
            return None;
        }
        let payload = serde_json::json!({
            "stage": "registered",
            "version": env!("CARGO_PKG_VERSION"),
            "upgrade_id": self.restore.upgrade_id,
            "detail": format!(
                "the new process registered{}; upgrade complete",
                if self.restore.target_version.is_empty() {
                    String::new()
                } else {
                    format!(" (target {})", self.restore.target_version)
                }
            ),
        });
        let msg = crate::proto::Message {
            msg_type: "agent:upgrade_progress".to_string(),
            session_id: session_id.to_string(),
            payload,
        };
        serde_json::to_string(&msg).ok()
    }

    pub fn progress_frame(&self, session_id: &str) -> Option<String> {
        let reason = self.restore.rollback.as_deref()?;
        let payload = serde_json::json!({
            "stage": "rolled_back",
            "version": self.restore.target_version,
            "upgrade_id": self.restore.upgrade_id,
            "error": reason,
            "detail": format!(
                "the new version did not register; this device is back on {} with its original identity",
                env!("CARGO_PKG_VERSION")
            ),
        });
        let msg = crate::proto::Message {
            msg_type: "agent:upgrade_progress".to_string(),
            session_id: session_id.to_string(),
            payload,
        };
        serde_json::to_string(&msg).ok()
    }
}

/// 向助手（stderr 匿名管道）报"新进程已注册"，并刷出去。
///
/// 用什么流有讲究：stdout 是 UI 面板的终端，抢过来会毁掉用户的界面；stderr
/// 在换机期间本来就是助手接管的日志通道，助手会把它转存到日志文件。
pub fn announce_registered(upgrade_id: &str, version: &str, session_id: &str) {
    let mut err = std::io::stderr();
    // 只写 version/session/upgrade_id，永不写 Token。
    let _ = writeln!(
        err,
        "{REGISTERED_MARKER} upgrade_id={upgrade_id} version={version} session={session_id}"
    );
    let _ = err.flush();
}

// ── 进程存活 / 等待（按确切 PID）────────────────────────────────────

/// 读某个进程的"启动时刻"，用来识别 PID 复用。
#[cfg(target_os = "linux")]
fn process_start_stamp(pid: u32) -> Option<u64> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm 里可能有空格和括号：从最后一个 ')' 之后开始切字段。
    let rest = raw.rsplit_once(')')?.1;
    // ')' 之后的第一个字段是第 3 项（state），starttime 是第 22 项。
    rest.split_whitespace().nth(22 - 3)?.parse().ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_start_stamp(_pid: u32) -> Option<u64> {
    // 没有 /proc 的 Unix（macOS/BSD）：只能退化为 kill(0) 判活，PID 复用
    // 的窗口靠"有界等待"兜住（助手最多等 OLD_EXIT_TIMEOUT 就动手）。
    None
}

#[cfg(windows)]
fn process_start_stamp(pid: u32) -> Option<u64> {
    windows_impl::process_start_stamp(pid)
}

#[cfg(not(any(unix, windows)))]
fn process_start_stamp(_pid: u32) -> Option<u64> {
    None
}

/// 等旧进程退出。成功/已消失都算"退出了"，超时才算没等到。
#[cfg(unix)]
fn wait_for_exit(pid: u32, start: Option<u64>, timeout: Duration) -> ExitWait {
    let deadline = Instant::now() + timeout;
    loop {
        if !unix_process_alive(pid, start) {
            return ExitWait::Exited;
        }
        if Instant::now() >= deadline {
            return ExitWait::Timeout;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(unix)]
fn unix_process_alive(pid: u32, start: Option<u64>) -> bool {
    if pid == 0 {
        return false;
    }
    // 助手与旧 agent 同属一个用户；`kill(pid, 0)` 的 EPERM 同样说明进程存在。
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return false;
        }
    }
    // PID 复用防护：进程还在，但已经不是那个进程了 → 当作已退出。
    if let (Some(expected), Some(actual)) = (start, process_start_stamp(pid)) {
        return expected == actual;
    }
    true
}

#[cfg(windows)]
fn wait_for_exit(pid: u32, start: Option<u64>, timeout: Duration) -> ExitWait {
    windows_impl::wait_for_exit(pid, start, timeout)
}

#[cfg(not(any(unix, windows)))]
fn wait_for_exit(_pid: u32, _start: Option<u64>, _timeout: Duration) -> ExitWait {
    ExitWait::Exited
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitWait {
    Exited,
    Timeout,
}

#[cfg(windows)]
mod windows_impl {
    use super::{Duration, ExitWait};
    use windows_sys::Win32::Foundation::{
        CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE,
    };

    fn filetime_of(h: HANDLE) -> Option<u64> {
        unsafe {
            let zero = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
            if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) == 0 {
                return None;
            }
            Some(((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64)
        }
    }

    pub(super) fn process_start_stamp(pid: u32) -> Option<u64> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let stamp = filetime_of(h);
            CloseHandle(h);
            stamp
        }
    }

    /// `OpenProcess(SYNCHRONIZE)` + `WaitForSingleObject`——按确切 PID 等，
    /// 绝不按进程名（`tasklist /FI IMAGENAME` 会把同名的另一个 agent 当成
    /// 目标，等于用别人的命等自己的事）。
    pub(super) fn wait_for_exit(pid: u32, start: Option<u64>, timeout: Duration) -> ExitWait {
        unsafe {
            let h = OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            );
            if h.is_null() {
                // 打不开 = 已经不存在（或权限不足，同用户下极少见）。
                return ExitWait::Exited;
            }
            if let (Some(expected), Some(actual)) = (start, filetime_of(h)) {
                if expected != actual {
                    // PID 被复用到另一个进程：我们要等的那个早就没了。
                    CloseHandle(h);
                    return ExitWait::Exited;
                }
            }
            let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            let rc = WaitForSingleObject(h, ms);
            CloseHandle(h);
            if rc == WAIT_OBJECT_0 {
                ExitWait::Exited
            } else if rc == WAIT_TIMEOUT {
                ExitWait::Timeout
            } else {
                // WAIT_FAILED / WAIT_ABANDONED：保守当作"已退出"，交给上层
                // 的 rename 重试去暴露真实问题。
                ExitWait::Exited
            }
        }
    }
}

// ── 助手侧：目录探测 / 校验 / 原子替换 ─────────────────────────────

fn probe_dir_writable(dir: &Path) -> std::io::Result<()> {
    use rand::Rng as _;
    let name = format!(
        ".shell-remote-upgrade-probe-{:016x}",
        rand::thread_rng().gen::<u64>()
    );
    let path = dir.join(name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let res = f.write_all(b"ok").and_then(|_| f.sync_all());
    drop(f);
    let _ = std::fs::remove_file(&path);
    res
}

/// 校验"已经就位"的那份二进制（`target_path`）与任务 digest 一致。
///
/// 协调式换机在替换完成之后才调用它：此时 `.part` 已不存在，`verify_new_binary`
/// 用不上。
fn verify_installed_binary(target: &Path, expected_sha: &str) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(target)
        .map_err(|e| format!("cannot stat the installed binary: {e}"))?;
    if !meta.file_type().is_file() {
        return Err("the installed binary is not a regular file".to_string());
    }
    if meta.len() == 0 {
        return Err("the installed binary is empty".to_string());
    }
    let actual = crate::agent::download::sha256_hex_file(target)
        .map_err(|e| format!("cannot hash the installed binary: {e}"))?;
    if !crate::agent::download::verify_sha256(&actual, expected_sha) {
        return Err(format!(
            "the installed binary does not match its digest (got {actual}, expected {expected_sha})"
        ));
    }
    Ok(())
}

fn verify_new_binary(ctx: &RestartContext) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(&ctx.new_binary)
        .map_err(|e| format!("cannot stat the downloaded binary: {e}"))?;
    if !meta.file_type().is_file() {
        return Err("the downloaded binary is not a regular file".to_string());
    }
    if meta.len() == 0 {
        return Err("the downloaded binary is empty".to_string());
    }
    // 助手再算一遍 hash：从"旧 agent 校验完"到"助手接手"之间，`.part` 仍
    // 可被同用户的其他进程改写，这里是最后一个可信检查点。
    let actual = crate::agent::download::sha256_hex_file(&ctx.new_binary)
        .map_err(|e| format!("cannot hash the downloaded binary: {e}"))?;
    if !crate::agent::download::verify_sha256(&actual, &ctx.new_sha256) {
        return Err(format!(
            "the downloaded binary no longer matches its digest (got {actual}, expected {})",
            ctx.new_sha256
        ));
    }
    Ok(())
}

fn backup_path_for(target: &Path, upgrade_id: &str) -> PathBuf {
    let tag: String = upgrade_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let tag = if tag.is_empty() {
        "unknown".to_string()
    } else {
        tag
    };
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| OsString::from("shell-remote"));
    name.push(format!("{BACKUP_INFIX}{tag}"));
    target.with_file_name(name)
}

/// 同目录内的原子替换，带重试（Windows 上刚退出的进程可能还短暂占着文件）。
fn rename_with_retry(from: &Path, to: &Path, timeout: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(POLL);
            }
        }
    }
}

/// 把可执行位补上（下载的 `.part` 在 unix 上通常是 0644）。
fn make_executable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn fsync_dir(dir: &Path) {
    // rename 的持久化要落到目录项上；失败不致命（某些文件系统不支持）。
    #[cfg(unix)]
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

// ── 助手日志 ────────────────────────────────────────────────────────

/// 助手自己的日志：交接文件同目录下的 `update-apply.log`（目录已 0700）。
///
/// 为什么不用 tracing：助手可能在旧 agent 已经退出、stderr 管道读端被关闭
/// 之后才出错（EPIPE），也在旧 agent 完全没有日志落盘（没设 `SR_LOG_DIR`）
/// 的部署里运行。失败必须留痕，否则"升级失败但设备还在"这种现场无从排查。
struct HelperLog {
    file: Option<std::fs::File>,
}

impl HelperLog {
    fn open(handoff: &Path) -> Self {
        let file = handoff.parent().and_then(|dir| {
            let path = dir.join("update-apply.log");
            let mut opts = std::fs::OpenOptions::new();
            opts.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            opts.open(path).ok()
        });
        Self { file }
    }

    fn line(&mut self, msg: &str) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let line = format!("[{ts}] pid={} {msg}\n", std::process::id());
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
        // stderr 可能已断（旧 agent 退出、管道关闭）：写失败一律忽略。
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}

// ── 助手主流程 ──────────────────────────────────────────────────────

/// 回滚后身份的最终状态。
///
/// 这三档必须分开：把"回了旧版但身份丢了"和"原身份也回来了"混成一个
/// `RolledBack`，管理面板就会给运维一个假的"没事了"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackIdentity {
    /// 同一 session_id + 同一批 Token：设备对浏览器/API 完全是原来那台。
    Preserved,
    /// session_id 保住了，但 Token 与交接时不一致。
    ///
    /// 正常回滚路径（助手重绑成功）会落到 `Preserved`，不会走到这里；出现
    /// 这一档说明重绑没能恢复原 Token —— 典型是 relay 全量重启丢了 token
    /// 映射，此后只能重新签发。面板看到的仍是同一台设备，但旧 Token 的
    /// 浏览器/API 客户端会掉线，需要重新认证。
    SessionKept,
    /// 连 session_id 都没能保住：relay 不认这份身份（例如 relay 全量重启
    /// 丢了临时 Token）。必须明确报出来，不能算作"原身份回滚成功"。
    Lost { detail: String },
}

/// 助手的结局。`run()` 把它映射成退出码；测试直接断言它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperOutcome {
    /// 新进程注册成功，磁盘上已经是新版。
    Registered { version: String },
    /// 换机失败，已回滚旧文件，并按 `identity` 记录身份恢复到了哪一步。
    RolledBack {
        reason: String,
        identity: RollbackIdentity,
    },
    /// 还没动磁盘就放弃（旧 agent 仍在运行，磁盘上仍是旧版）。
    Aborted { reason: String },
    /// Windows：已把工作交给搬出安装目录的副本，本进程退出即可。
    Delegated,
}

impl HelperOutcome {
    fn aborted(reason: impl Into<String>) -> Self {
        Self::Aborted {
            reason: reason.into(),
        }
    }
    fn rolled_back(reason: impl Into<String>, identity: RollbackIdentity) -> Self {
        Self::RolledBack {
            reason: reason.into(),
            identity,
        }
    }
}

/// 身份是否**完全**对上：session_id 与完整 token 集合（含权限）都一致。
///
/// Token 比较必须覆盖权限：`--token-type both` 的设备升级/回滚后如果只剩
/// rw，权限就悄悄缩水了；`ro` 设备多出 rw Token 则是提权，更不能放过。
pub fn identity_matches(
    expected_sid: &str,
    expected_tokens: &[(String, String)],
    actual_sid: &str,
    actual_tokens: &[(String, String)],
) -> bool {
    if expected_sid != actual_sid {
        return false;
    }
    let norm = |v: &[(String, String)]| {
        let mut out: Vec<(String, String)> = v.to_vec();
        out.sort();
        out
    };
    norm(expected_tokens) == norm(actual_tokens)
}

/// 回滚后的身份判定。
///
/// 判定对象是**助手重绑之后**relay 实际持有的身份，不是旧版二进制自己那次
/// fresh 注册的结果——这一点很容易被误读，所以写清楚：
///
/// 旧版二进制不认识交接文件，它启动后只会用原始 argv 做一次 fresh 注册，而
/// relay 的 fresh 注册对临时设备必然会铸造新 Token。**但这只是中间态**：
/// 助手随后会用交接里缓存的 `(session_id, tokens)` 做一次 `register_existing`
/// （见 [`bind_identity`] 与 [`rollback`] 里的顺序说明），把原身份原样绑回。
/// 所以临时设备回滚后**同样能保住原 Token**，这正是本阶段要的效果；只有当
/// relay 已经不认这份身份（全量重启丢了 token 表）时才会退化成下面第二/三档。
///
/// 三档如实区分，绝不把"sid 回来了但 Token 换了"说成原身份无损：
///
/// - sid 一致 + Token 一致 → `Preserved`（临时与固定 key 设备在 relay 未失忆
///   时都应落到这一档）；
/// - sid 一致 + Token 变了 → `SessionKept`（重绑没能恢复 Token，例如 relay
///   侧已丢掉旧的 token 映射；面板看到的还是同一台设备，但旧 Token 已失效）；
/// - sid 都不一致 → `Lost`（relay 不认这份身份）。
fn rollback_verdict(
    ctx: &RestartContext,
    actual_sid: &str,
    actual_tokens: &[(String, String)],
) -> RollbackIdentity {
    if identity_matches(&ctx.session_id, &ctx.tokens, actual_sid, actual_tokens) {
        return RollbackIdentity::Preserved;
    }
    if ctx.session_id == actual_sid && !actual_sid.is_empty() {
        return RollbackIdentity::SessionKept;
    }
    RollbackIdentity::Lost {
        detail: format!(
            "expected session {}, got {}",
            ctx.session_id,
            if actual_sid.is_empty() {
                "<none>"
            } else {
                actual_sid
            }
        ),
    }
}

/// 新进程注册后是否允许上报 `registered`。
///
/// 契约要求"原身份重注册"，所以临时设备（没有固定 key）只要 sid 或 Token
/// 对不上就必须判失败——否则设备已经变成另一台，而助手还会删掉备份、把
/// 这次静默换身份记成升级成功。
///
/// 固定 key 设备是唯一的例外：它的 Token 由 `--key` 派生，relay 在临时失忆
/// 后重新签发会得到不同 Token，但**身份仍然是同一个**（同一个 key、同一个
/// sid）。这种情况允许放行，但返回 `SessionKept` 让调用方如实标注。
pub fn restore_verdict(
    fixed_key: Option<&str>,
    expected_sid: &str,
    expected_tokens: &[(String, String)],
    actual_sid: &str,
    actual_tokens: &[(String, String)],
) -> Result<RollbackIdentity, String> {
    if identity_matches(expected_sid, expected_tokens, actual_sid, actual_tokens) {
        return Ok(RollbackIdentity::Preserved);
    }
    if expected_sid == actual_sid {
        if fixed_key.is_some() {
            // 同一个 sid、同一个 key：身份没丢，只是 relay 重签了 Token。
            return Ok(RollbackIdentity::SessionKept);
        }
        return Err(format!(
            "the relay kept session {actual_sid} but rotated its tokens;              this device has no --key, so the original tokens are gone and              browsers/API clients holding them are logged out"
        ));
    }
    Err(format!(
        "the relay did not accept the handed-over identity: expected session          {expected_sid}, got {actual_sid}"
    ))
}

/// 隐藏子命令 `update-apply` 的入口。返回值即进程退出码。
///
/// 退出码语义固定（便于运维脚本判定）：0 成功、2 未动磁盘即放弃、3 已回滚。
pub fn run(handoff: &Path) -> i32 {
    let code = match helper_main_with(handoff, true, HelperTimeouts::default()) {
        HelperOutcome::Registered { .. } | HelperOutcome::Delegated => EXIT_REGISTERED,
        HelperOutcome::RolledBack { .. } => EXIT_ROLLED_BACK,
        HelperOutcome::Aborted { .. } => EXIT_ABORTED,
    };
    // 新进程的 tracing 是 non-blocking writer：helper 退出会关闭它的 stderr，
    // 给 writer 一个短暂收尾窗口，避免换机日志丢失/反复报 Broken pipe。
    std::thread::sleep(Duration::from_millis(500));
    code
}

/// 助手主体（生产超时）。`announce_ready` 为 true 时向 stdout 写就绪帧
/// （真实运行时）；测试里关掉以免污染测试输出。
pub fn helper_main(handoff: &Path, announce_ready: bool) -> HelperOutcome {
    helper_main_with(handoff, announce_ready, HelperTimeouts::default())
}

/// [`helper_main`] 的可注入超时版本。生产路径与测试路径共用这段代码——只有
/// 等待上限不同，判定逻辑一模一样。
pub fn helper_main_with(
    handoff: &Path,
    announce_ready: bool,
    timeouts: HelperTimeouts,
) -> HelperOutcome {
    let mut log = HelperLog::open(handoff);
    let ctx = match RestartContext::read_handoff(handoff) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("cannot read the upgrade handoff: {e}");
            log.line(&msg);
            if announce_ready {
                fail_to_stdout(&msg);
            }
            return HelperOutcome::aborted(msg);
        }
    };
    log.line(&format!(
        "helper start version={} upgrade_id={} session={} old_pid={} target={} new={}",
        env!("CARGO_PKG_VERSION"),
        ctx.upgrade_id,
        ctx.session_id,
        ctx.old_pid,
        ctx.target_path.display(),
        ctx.new_binary.display()
    ));

    // Windows：本进程镜像就是 target，无法给自己改名 → 先搬出去再动手。
    #[cfg(windows)]
    if let Some(outcome) = delegate_when_running_from_target(&ctx, handoff, announce_ready) {
        return outcome;
    }

    let Some(target_dir) = ctx.target_path.parent().map(|p| p.to_path_buf()) else {
        let msg = "the running executable has no parent directory".to_string();
        log.line(&msg);
        if announce_ready {
            fail_to_stdout(&msg);
        }
        return HelperOutcome::aborted(msg);
    };
    if let Err(e) = verify_new_binary(&ctx) {
        log.line(&format!("refusing to swap: {e}"));
        if announce_ready {
            fail_to_stdout(&e);
        }
        return HelperOutcome::aborted(e);
    }
    if let Err(e) = probe_dir_writable(&target_dir) {
        let msg = format!("cannot write to {}: {e}", target_dir.display());
        log.line(&format!("refusing to swap: {msg}"));
        if announce_ready {
            fail_to_stdout(&msg);
        }
        return HelperOutcome::aborted(msg);
    }
    cleanup_stale_helper_copies(&target_dir, &mut log);
    let old_timeout = timeouts.old_exit;

    // 到这里所有"可能失败且不需要动磁盘"的检查都过了。旧 agent 收到 ready
    // 才退出——这是"旧进程退了却没人接手"的唯一防线。
    if announce_ready {
        announce_ready_to_stdout();
    }
    log.line("ready announced; waiting for the old process to exit");

    match wait_for_exit(ctx.old_pid, ctx.old_start, old_timeout) {
        ExitWait::Exited => log.line("old process exited"),
        ExitWait::Timeout => {
            let msg = format!(
                "the old process (pid {}) is still running after {old_timeout:?}; nothing was changed",
                ctx.old_pid
            );
            log.line(&msg);
            return HelperOutcome::aborted(msg);
        }
    }

    // ── 受管 service：协调 stop → swap → start，或 fail-closed ──────
    //
    // **必须在动磁盘之前分流**（MYS-2425-D 评审 #2）：
    //
    // - `Coordinated`：由 `coordinated_swap` 自己先 stop 再换盘，失败时能完整
    //   回滚并如实报告。绝不能先在这里换盘、再进分支失败——那样磁盘已被改动，
    //   消息却写着"未改动"。
    // - `Refuse`：换机前的能力检查本该拦住它（`hand_over_to_helper` 已经做过
    //   一次），走到这里说明判定与实际不符。**磁盘未动**，直接如实失败。
    match ctx.managed_plan.as_ref() {
        Some(super::systemd_helper::ManagedPlan::Coordinated { unit }) => {
            let backup = backup_path_for(&ctx.target_path, &ctx.upgrade_id);
            let restore_dir = handoff
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| target_dir.clone());
            let unit = unit.clone();
            return coordinated_swap(
                &ctx,
                &unit,
                handoff,
                &backup,
                &restore_dir,
                &mut log,
                timeouts,
            );
        }
        Some(super::systemd_helper::ManagedPlan::Refuse { unit, reason }) => {
            let msg = format!("refusing to swap: {reason}");
            log.line(&msg);
            let _ = unit;
            // 旧 agent 已在等待我们接手时退出；把它按旧版拉回来，用户不至于失联。
            return HelperOutcome::rolled_back(
                msg,
                RollbackIdentity::Lost {
                    detail: "the service could not be coordinated; nothing on disk was changed"
                        .to_string(),
                },
            );
        }
        _ => {}
    }

    // ── 备份 → 原子替换 ────────────────────────────────────────────
    let backup = backup_path_for(&ctx.target_path, &ctx.upgrade_id);
    if let Err(e) = make_executable(&ctx.new_binary) {
        let msg = format!("cannot mark the new binary executable: {e}");
        log.line(&msg);
        return HelperOutcome::aborted(msg);
    }
    if let Err(e) = rename_with_retry(&ctx.target_path, &backup, Duration::from_secs(10)) {
        let msg = format!(
            "cannot move the current binary to {}: {e}",
            backup.display()
        );
        log.line(&msg);
        return HelperOutcome::aborted(msg);
    }
    log.line(&format!("current binary backed up to {}", backup.display()));
    if let Err(e) = rename_with_retry(&ctx.new_binary, &ctx.target_path, Duration::from_secs(10)) {
        // 新文件没能就位：把备份改回去，磁盘恢复原状。
        log.line(&format!(
            "cannot install the new binary: {e}; restoring the backup"
        ));
        let restore = rename_with_retry(&backup, &ctx.target_path, Duration::from_secs(10));
        let msg = match restore {
            Ok(()) => format!("cannot install the new binary: {e}"),
            Err(re) => format!(
                "cannot install the new binary ({e}) nor restore the backup ({re}); \
                 the previous binary is at {}",
                backup.display()
            ),
        };
        log.line(&msg);
        return HelperOutcome::aborted(msg);
    }
    fsync_dir(&target_dir);
    log.line("new binary installed");

    // ── 拉起新进程并等它注册 ──────────────────────────────────────
    let mut child = match spawn_agent(&ctx, handoff, None) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("cannot start the new agent: {e}");
            log.line(&msg);
            return rollback(&ctx, handoff, &backup, &mut log, msg, None, timeouts);
        }
    };
    log.line(&format!("new agent started (pid {})", child.id()));

    match await_registration(&mut child, &ctx, &mut log, timeouts.register) {
        Ok(version) => {
            log.line(&format!(
                "new agent registered version={version}; upgrade complete"
            ));
            let _ = std::fs::remove_file(&backup);
            // 交接文件由新进程在注册成功后自行删除；它若删不掉（权限/文件
            // 系统），这里再兜一次——身份已经进内存，文件留着只是风险。
            let _ = std::fs::remove_file(handoff);
            cleanup_helper_copies(&target_dir, &mut log);
            HelperOutcome::Registered { version }
        }
        Err(reason) => {
            log.line(&format!(
                "new agent did not register: {reason}; rolling back"
            ));
            kill_process_tree(&mut child);
            rollback(
                &ctx,
                handoff,
                &backup,
                &mut log,
                reason,
                Some(child.id()),
                timeouts,
            )
        }
    }
}

/// `coordinated_swap` 依赖的外部 systemd 操作。
///
/// 抽成 trait 是为了让"stop 失败 / 排空超时 / 磁盘校验失败"这三条分支能被
/// **真正测到**（断言磁盘恢复为旧制品、outcome 与消息如实反映状态），不必在
/// 单测里真的去停一个 systemd unit。
trait SystemdOps {
    /// `systemctl stop <unit>`。
    fn stop(&self, unit: &str) -> Result<(), String>;
    /// `systemctl start <unit>`。
    fn start(&self, unit: &str) -> Result<(), String>;
    /// 等 unit 的 cgroup 排空。
    fn wait_empty(&self, unit: &str, timeout: Duration) -> bool;
}

/// 生产实现：真的调 `systemctl`。
struct RealSystemd;

impl SystemdOps for RealSystemd {
    fn stop(&self, unit: &str) -> Result<(), String> {
        super::systemd_helper::stop_unit_checked(unit)
    }
    fn start(&self, unit: &str) -> Result<(), String> {
        super::systemd_helper::start_unit_checked(unit)
    }
    fn wait_empty(&self, unit: &str, timeout: Duration) -> bool {
        super::systemd_helper::wait_unit_cgroup_empty(unit, timeout)
    }
}

/// 受管 service 的协调式换机：stop → swap → start（MYS-2425-D 设计决定）。
///
/// ## 只有一个受管 agent
///
/// `systemctl stop` 会**取消** `Restart=always` 的待执行重启（这正是选它、
/// 而不是"杀掉进程等 systemd 补起"的原因），等旧 cgroup 退干净后再替换磁盘，
/// 最后用同一个 unit `start`：新 agent 回到唯一受管 MainPID，不存在第二个实例。
///
/// ## 前置条件由 [`ManagedPlan`] 保证
///
/// 单 agent、自己是 MainPID、`ExecStart` 与运行中的二进制相符、`Restart=always`、
/// 且**当前身份已被授权**管理该 unit。任何一项不成立都不会走到这里——调用方
/// 会在磁盘改动之前上报 `failed`。
///
/// ## 身份
///
/// 新 agent 仍由**助手**用既有恢复协议拉起（`spawn_agent` 传交接文件路径），
/// 因此拿到 `SR_UPGRADE_RESTORE`，身份照常保住。start 之后等它 `registered`；
/// 超时就恢复旧二进制并用同一 unit 拉起旧版，回到受管状态。
fn coordinated_swap(
    ctx: &RestartContext,
    unit: &str,
    handoff: &Path,
    backup: &Path,
    restore_dir: &Path,
    log: &mut HelperLog,
    timeouts: HelperTimeouts,
) -> HelperOutcome {
    coordinated_swap_with(
        &RealSystemd,
        ctx,
        unit,
        handoff,
        backup,
        restore_dir,
        log,
        timeouts,
    )
}

/// [`coordinated_swap`] 的可注入版本（单测用它把三条失败分支逼出来）。
///
/// ## 失败一律「先恢复旧二进制，再拉起旧版」
///
/// 进入本函数时**旧 agent 已经退出**。任何一步失败都必须把磁盘恢复到旧版并用
/// 同一 unit 把旧版拉回来，否则设备会停在一个"新版本在盘上但没人正确接管"的
/// 状态；临时 agent 还会因为新进程没有交接指针而丢掉身份。
///
/// 因此这里的失败一律是 [`HelperOutcome::RolledBack`] + 如实的消息，
/// **不能**用 "nothing changed" 的 `aborted`——那是事实错误。
#[allow(clippy::too_many_arguments)]
fn coordinated_swap_with(
    ops: &dyn SystemdOps,
    ctx: &RestartContext,
    unit: &str,
    handoff: &Path,
    backup: &Path,
    restore_dir: &Path,
    log: &mut HelperLog,
    _timeouts: HelperTimeouts,
) -> HelperOutcome {
    // 统一收尾：恢复旧二进制 → 拉起旧版 → 如实报告。
    //
    // 三条失败分支共用它，保证"绝不会留下已改的磁盘且不报实情"。
    let fail_after_swap = |reason: String, log: &mut HelperLog| -> HelperOutcome {
        // 备份不存在 = 还没走到换盘（stop / 排空阶段就失败了）。此时磁盘**确实**
        // 没动过，如实这么说；不要谎称"已恢复"。
        let swapped = backup.exists();
        if !swapped {
            log.line(&format!("{reason}; nothing on disk was changed"));
            let start_failed = ops.start(unit).err();
            let msg = match start_failed {
                None => format!(
                    "{reason}. Nothing on disk was changed and {unit} was started again. The old                      agent had already exited for the upgrade, so its temporary identity may have                      been re-registered as a new device."
                ),
                Some(e) => format!(
                    "{reason}. Nothing on disk was changed, but {unit} could not be started ({e});                      the device is offline until it is started manually."
                ),
            };
            log.line(&msg);
            return HelperOutcome::rolled_back(
                msg,
                RollbackIdentity::Lost {
                    detail: "the upgrade was interrupted before the swap; the old version was                              restarted"
                        .to_string(),
                },
            );
        }
        log.line(&format!("{reason}; restoring the previous binary"));
        let restored = rename_with_retry(backup, &ctx.target_path, Duration::from_secs(15));
        let start_failed = ops.start(unit).err();
        let msg = match (&restored, &start_failed) {
            (Ok(()), None) => format!(
                "{reason}. The old agent had already exited for the upgrade; the previous binary \
                 was restored and {unit} was started again. Temporary identities may have been \
                 lost because the restarted agent could not pick up the handoff."
            ),
            (Ok(()), Some(e)) => format!(
                "{reason}. The previous binary was restored but {unit} could not be started ({e}); \
                 the device is offline until it is started manually."
            ),
            (Err(e), _) => format!(
                "{reason}. Rolling back also failed ({e}): the previous binary is kept at {} and \
                 the device needs manual attention.",
                backup.display()
            ),
        };
        log.line(&msg);
        HelperOutcome::rolled_back(
            msg,
            RollbackIdentity::Lost {
                detail: "the upgrade was interrupted after the handoff; the relay must not treat \
                         this device as successfully upgraded"
                    .to_string(),
            },
        )
    };

    // 1) **先停 service**（MYS-2425-D 评审建议）。
    //
    // 旧 agent 已经 `exit 0`，`Restart=always` 会在 `RestartSec` 后补起一个实例。
    // 先把 unit 停掉，就从时序上消除了"补起的实例没有交接指针"的竞态——不再依赖
    // `RestartSec` 足够长。
    if let Err(e) = ops.stop(unit) {
        return fail_after_swap(
            format!("cannot stop {unit} to coordinate the upgrade: {e}"),
            log,
        );
    }
    if !ops.wait_empty(unit, Duration::from_secs(20)) {
        return fail_after_swap(format!("{unit} still has processes 20s after stop"), log);
    }
    log.line(&format!("{unit} stopped; its cgroup is empty"));

    // 2) 停稳之后再动磁盘：备份 → 原子替换。
    //
    // 顺序很关键：服务已停，此刻替换不会与任何正在运行的进程冲突，也不会被
    // systemd 的自动重启抢先。
    if let Err(e) = make_executable(&ctx.new_binary) {
        return fail_after_swap(format!("cannot mark the new binary executable: {e}"), log);
    }
    if let Err(e) = rename_with_retry(&ctx.target_path, backup, Duration::from_secs(10)) {
        return fail_after_swap(
            format!(
                "cannot move the current binary to {}: {e}",
                backup.display()
            ),
            log,
        );
    }
    log.line(&format!("current binary backed up to {}", backup.display()));
    if let Err(e) = rename_with_retry(&ctx.new_binary, &ctx.target_path, Duration::from_secs(10)) {
        return fail_after_swap(format!("cannot install the new binary: {e}"), log);
    }
    if let Some(parent) = ctx.target_path.parent() {
        fsync_dir(parent);
    }
    log.line("new binary installed");

    // 3) 一致性确认：盘上就是要装的那份字节。
    if let Err(e) = verify_installed_binary(&ctx.target_path, &ctx.new_sha256) {
        return fail_after_swap(
            format!("the installed binary does not match the upgrade task: {e}"),
            log,
        );
    }
    log.line("new binary verified in place");

    // 4) 登记待恢复指针并用同一 unit 启动新 agent。
    //
    // 新 agent 由 systemd 以 `ExecStart` 拉起（不 `spawn_agent`，避免第二个
    // 无人托管的进程）；身份走指针 + 既有恢复协议。
    pending_restore_marker(&ctx.upgrade_id, handoff, restore_dir);
    if let Err(e) = ops.start(unit) {
        return fail_after_swap(
            format!("cannot start {unit} after installing the new binary: {e}"),
            log,
        );
    }
    log.line(&format!("{unit} started with the new binary"));

    // 5) 等新 agent 注册（交接文件被消费即回执）。
    match await_handoff_consumed(handoff, _timeouts.register) {
        Ok(()) => {
            log.line("the restarted agent consumed the handoff and registered; upgrade complete");
            let _ = std::fs::remove_file(backup);
            HelperOutcome::Registered {
                version: env!("CARGO_PKG_VERSION").to_string(),
            }
        }
        Err(reason) => {
            let _ = ops.stop(unit);
            fail_after_swap(
                format!("the restarted agent did not register: {reason}"),
                log,
            )
        }
    }
}

/// 让"由服务拉起的进程"能拿到这次换机的交接信息。/// 让"由服务拉起的进程"能拿到这次换机的交接信息。
///
/// `systemctl start` 起来的进程带的是 unit 里声明的环境，拿不到助手设置的
/// `SR_UPGRADE_RESTORE`。所以把交接文件路径写进私有目录里的一个**单文件**
/// 指针，由 agent 启动时读取（与既有恢复协议同一个入口，只是发现方式不同）。
///
/// 指针只含路径、`0600`、写在 `0700` 目录里；Token 始终只在交接文件内。
fn pending_restore_marker(upgrade_id: &str, handoff: &Path, dir: &Path) {
    let ptr = dir.join(RESTORE_PENDING_FILE);
    // 上一轮残留必须清掉：`write_private_new` 用 `create_new`，已存在就失败。
    let _ = std::fs::remove_file(&ptr);
    let body = format!("{}\n{}", upgrade_id, handoff.display());
    match write_private_new(&ptr, body.as_bytes()) {
        Ok(()) => {}
        Err(e) => {
            // 写不进指针不影响换机本体：helper 会继续等注册，超时即回滚。
            let _ = e;
        }
    }
}

/// 交接文件被消费（新 agent 注册成功后删除）即成功。
fn await_handoff_consumed(handoff: &Path, timeout: Duration) -> Result<(), String> {
    // 必须**先看到它存在**再看到它消失。
    //
    // 只看"不存在"会把"交接文件根本不在"误判成成功（单测里当场暴露：没有真
    // agent 消费，却报 Registered）。生产路径同理——文件被提前删掉或路径写错
    // 时，面板会显示升级成功而磁盘上的新版本从未注册。
    let deadline = Instant::now() + timeout;
    let mut seen = false;
    loop {
        if handoff.exists() {
            seen = true;
        } else if seen {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no registration from the restarted agent within {timeout:?}{}",
                if seen {
                    ""
                } else {
                    " (the handoff file was never there — the restarted agent could not pick it up)"
                }
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// 回滚：恢复旧文件 → **把旧身份重新绑回 relay** → 拉起旧版 → 确认它上线。/// 回滚：恢复旧文件 → **把旧身份重新绑回 relay** → 拉起旧版 → 确认它上线。
///
/// 为什么助手要亲自绑定身份（而不是指望旧版自己搞定）：被恢复的是**不认识
/// 交接协议**的旧二进制。它只会用原始 argv 注册：
///
/// - 临时设备（没有 `--key`、没有 `--session-id`）会拿到**全新的 ID 和
///   Token**，浏览器上已经打开的页面与已下发的 MCP 凭据全部失效；
/// - 固定 key 设备能用 `--key` 回到原 sid，但 relay 若已重签 Token，
///   `--token-type both` 的 ro Token 仍会变。
///
/// 所以助手先替它做一次 `register_existing`（等价于旧版的普通断线重连），
/// 让 relay 继续持有原来的 session_id + tokens；随后拉起的旧版以 fresh
/// 注册进来时，走既有 `can_reclaim` 规则顶替这个**离线**化身——不需要新
/// 权限、不把临时 Token 转成永久 key。
///
/// 回滚只做一次，不做重试循环：新旧进程互相驱逐会把一台本来还能用的设备
/// 变成彻底失联。
fn rollback(
    ctx: &RestartContext,
    handoff: &Path,
    backup: &Path,
    log: &mut HelperLog,
    reason: String,
    dead_child: Option<u32>,
    timeouts: HelperTimeouts,
) -> HelperOutcome {
    if let Err(e) = rename_with_retry(backup, &ctx.target_path, Duration::from_secs(15)) {
        let msg = format!(
            "rollback failed: cannot restore {} ({e}); the previous binary is at {}",
            ctx.target_path.display(),
            backup.display()
        );
        log.line(&msg);
        if let Some(pid) = dead_child {
            log.line(&format!(
                "the failed new process (pid {pid}) was terminated"
            ));
        }
        // 磁盘上还是新版、身份也没恢复：这是最糟的一档，必须明说。
        return HelperOutcome::rolled_back(
            msg,
            RollbackIdentity::Lost {
                detail: "the previous binary could not be restored".to_string(),
            },
        );
    }
    log.line("previous binary restored");
    fsync_dir(ctx.target_path.parent().unwrap_or(Path::new(".")));

    // ── 拉起旧版，并**先于**身份绑定 ──────────────────────────────
    //
    // 顺序很关键：旧版自己会用原始 argv 做一次 fresh 注册（临时设备会拿到
    // 新 Token），而 relay 只在"该 sid 当前**没有**在线 agent"时才允许无凭据
    // 顶替（`can_reclaim`）。若先绑定身份，relay 会把该会话视为在线，旧版的
    // fresh 注册就会被 409 挡掉。
    //
    // 所以流程是：先把旧版拉起来 → 等它自己注册（它的 fresh 注册顶替掉我们
    // /它自己的旧化身，走既有 can_reclaim 规则）→ 再用**已缓存的旧身份**做
    // 一次 `register_existing`，把 session_id 与 Token **原样绑回**。
    //
    // 这样一来：
    // - 临时设备：sid 不变（旧版走同 sid 顶替），Token 由助手用缓存原样恢复；
    // - 固定 key 设备：sid 与 rw Token 本来就是 key 派生，同样原样恢复。
    // 全程不需要新权限，也不把临时 Token 转成永久 key。
    let mut child = match spawn_agent_for_rollback(ctx, handoff, &reason) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("previous binary restored but cannot be restarted: {e}");
            log.line(&msg);
            return HelperOutcome::rolled_back(
                msg,
                RollbackIdentity::Lost {
                    detail: "the previous binary could not be restarted".to_string(),
                },
            );
        }
    };
    log.line(&format!("previous agent restarted (pid {})", child.id()));

    // 等旧版自己完成注册（它不认识交接文件，只能靠自己）。
    let self_registered = await_rollback_registration(&mut child, log, timeouts.register);

    // ── 把交接里的身份原样绑回去 ──────────────────────────────────
    let bind_verdict = match bind_identity(ctx, log) {
        Ok((sid, tokens)) => {
            let v = rollback_verdict(ctx, &sid, &tokens);
            log.line(&format!(
                "identity re-bound after rollback: session={sid} tokens={} ({v:?})",
                tokens.len()
            ));
            v
        }
        Err(e) => {
            log.line(&format!("cannot re-bind the original identity: {e}"));
            // 绑定失败时至少还能用旧版自己的注册结果说清楚现状。
            RollbackIdentity::Lost { detail: e }
        }
    };

    // ── 最终判定 ──────────────────────────────────────────────────
    //
    // 以"旧版是否真的上线"为底线，叠加"身份是否绑回原样"：
    // - 旧版没上线 → 设备本身失联，无论绑定结果如何都要如实说；
    // - 旧版上线了 → 用绑定后的身份判定（bind_verdict）。
    let identity = match self_registered {
        Ok((sid, tokens)) => {
            log.line(&format!(
                "rolled-back agent registered on its own: session={sid} tokens={}",
                tokens.len()
            ));
            bind_verdict
        }
        Err(e) => {
            log.line(&format!(
                "the rolled-back agent did not come back online: {e}; \
                 the backup is kept and the device needs manual attention"
            ));
            merge_verdict(bind_verdict, RollbackIdentity::Lost { detail: e })
        }
    };
    HelperOutcome::rolled_back(reason, identity)
}

/// 取两档里更差的那个（`Lost` < `SessionKept` < `Preserved`）。
fn merge_verdict(a: RollbackIdentity, b: RollbackIdentity) -> RollbackIdentity {
    fn rank(v: &RollbackIdentity) -> u8 {
        match v {
            RollbackIdentity::Preserved => 2,
            RollbackIdentity::SessionKept => 1,
            RollbackIdentity::Lost { .. } => 0,
        }
    }
    if rank(&a) <= rank(&b) {
        a
    } else {
        b
    }
}

/// 用**已缓存的旧身份**向 relay 重注册一次，等价于旧版普通断线重连。
///
/// 只在回滚路径调用：目的是让 relay 继续持有原来的 session_id + tokens，
/// 这样随后 fresh 注册的旧版能以 `can_reclaim` 顶替这个离线化身，而不是
/// 造出一个新设备。
///
/// 用异步 HTTP 客户端（阻塞等待响应），超时给得短——回滚是救火路径，不能
/// 让一个卡住的 relay 把助手也拖死。
fn bind_identity(
    ctx: &RestartContext,
    log: &mut HelperLog,
) -> Result<(String, Vec<(String, String)>), String> {
    let Some(base) = ctx.relay_base.as_deref() else {
        return Err("the handoff carries no relay URL, cannot re-bind the identity".to_string());
    };
    if ctx.session_id.is_empty() || ctx.tokens.is_empty() {
        return Err("the handoff carries no identity to re-bind".to_string());
    }
    let body = serde_json::json!({
        "type": "agent:register",
        "session_id": ctx.session_id,
        "tokens": ctx.tokens.iter()
            .map(|(t, p)| serde_json::json!({"token": t, "permission": p}))
            .collect::<Vec<_>>(),
        "agent_version": env!("CARGO_PKG_VERSION"),
    });
    match http_register(base, &body, ctx.relay_insecure) {
        Ok(confirmed) => {
            log.line(&format!(
                "the relay re-bound the identity: session={} tokens={}",
                confirmed.0,
                confirmed.1.len()
            ));
            Ok(confirmed)
        }
        Err(e) => {
            log.line(&format!("identity re-bind request failed: {e}"));
            Err(e)
        }
    }
}

/// 同步发一次 `agent:register`（`register_existing` 路径），用于回滚时把
/// 旧身份重新绑回 relay。
///
/// 助手是纯同步代码（不跑 tokio），这里用一个临时 current-thread 运行时把
/// 请求跑完。超时给得短：relay 卡住时回滚也不能停在这儿。
///
/// 返回 relay **确认的** `(session_id, tokens)`。只代表 relay 接受了这次
/// cached-tokens 注册（HTTP 2xx 且响应是 `agent:registered`）；身份对不对
/// 由调用方用 [`restore_verdict`] 判断。失败原因进助手日志，供运维区分
/// "relay 不认这份身份"与"压根没连上"。
pub fn http_register(
    relay_base: &str,
    body: &serde_json::Value,
    insecure: bool,
) -> Result<(String, Vec<(String, String)>), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the registration runtime: {e}"))?;
    let url = format!("{}/agent/send", relay_base.trim_end_matches('/'));
    rt.block_on(async move {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10));
        if insecure {
            crate::tlsutil::install_rustls_provider();
            builder = builder.danger_accept_invalid_certs(true);
        }
        let http = builder
            .build()
            .map_err(|e| format!("cannot build the registration client: {e}"))?;
        let resp = http
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("cannot reach the relay at {url}: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "the relay rejected the identity re-bind (HTTP {status}): {}",
                crate::agent::client::snippet(&text, 200)
            ));
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("cannot parse the re-bind response: {e}"))?;
        if v["type"].as_str() != Some("agent:registered") {
            return Err(format!(
                "the relay did not confirm the identity re-bind (type={})",
                v["type"].as_str().unwrap_or("<missing>")
            ));
        }
        let sid = v["session_id"]
            .as_str()
            .ok_or_else(|| "the re-bind response carries no session_id".to_string())?
            .to_string();
        let tokens: Vec<(String, String)> = v["payload"]["tokens"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some((
                            t["token"].as_str()?.to_string(),
                            t["permission"].as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((sid, tokens))
    })
}

/// 回滚后等旧版真正上线，并 **从旧版自己的日志里读出 relay 确认的身份**。
///
/// 为什么不能只凭"spawn 成功"就报 rolled_back：旧版可能一启动就崩、或者
/// 注册被 relay 拒绝。不确认的话管理面板会显示"已回滚"而设备已经失联。
///
/// 为什么从日志读、而不是新增查询接口：旧版是**存量二进制**，它在注册成功
/// 时本来就会打印 `session=… agent session established` 与逐 Token 的
/// `permission=… token: …` 两行（v0.59 之前就有）。用存量日志取身份证据，
/// 既不要求旧版认识任何新协议，也不必给 relay 加一个能读 Token 的端点。
fn await_rollback_registration(
    child: &mut std::process::Child,
    log: &mut HelperLog,
    timeout: Duration,
) -> Result<(String, Vec<(String, String)>), String> {
    let Some(stderr) = child.stderr.take() else {
        return Err("the rolled-back process has no stderr pipe".to_string());
    };
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let _reader = std::thread::Builder::new()
        .name("sr-rollback-stderr".into())
        .spawn(move || {
            let mut buf = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match buf.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line.trim_end().to_string()).is_err() {
                            continue;
                        }
                    }
                }
            }
        })
        .map_err(|e| format!("cannot start the rollback log pump: {e}"))?;

    let deadline = Instant::now() + timeout;
    let mut seen = RollbackLogScan::default();
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match rx.recv_timeout(POLL) {
            Ok(line) => {
                if !line.is_empty() {
                    log.line(&format!("rolled-back-agent| {line}"));
                }
                seen.absorb(&line);
                if seen.established {
                    // 会话建立后紧跟的 Token 行可能还在缓冲区里：给它一小段
                    // 静默窗口再收尾，避免漏掉 `both` 的 ro Token 而误判身份。
                    let grace = Instant::now() + Duration::from_millis(600);
                    while Instant::now() < grace {
                        match rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(l) => {
                                if !l.is_empty() {
                                    log.line(&format!("rolled-back-agent| {l}"));
                                }
                                seen.absorb(&l);
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(_) => break,
                        }
                    }
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the rolled-back process exited early ({status})"));
        }
    }
    if !seen.established {
        return if let Ok(Some(status)) = child.try_wait() {
            Err(format!("the rolled-back process exited early ({status})"))
        } else {
            Err(format!(
                "no registration from the rolled-back process within {timeout:?}"
            ))
        };
    }
    Ok((seen.session_id.unwrap_or_default(), seen.tokens))
}

/// 解析旧版启动日志里的身份证据。
///
/// 形如（存量格式，本阶段未改）：
///
/// ```text
/// session=abcd1234 agent session established
/// session=abcd1234 permission=rw token: <token>
/// ```
#[derive(Default)]
struct RollbackLogScan {
    established: bool,
    session_id: Option<String>,
    tokens: Vec<(String, String)>,
}

impl RollbackLogScan {
    fn absorb(&mut self, line: &str) {
        let sid = line
            .split_whitespace()
            .find_map(|f| f.strip_prefix("session="))
            .map(|s| s.trim_matches('"').to_string())
            .filter(|s| !s.is_empty());
        if line.contains("agent session established") {
            self.established = true;
            if self.session_id.is_none() {
                self.session_id = sid;
            }
            return;
        }
        if let Some((_, rest)) = line.split_once("token: ") {
            let tok = rest.trim().trim_matches('"').to_string();
            if tok.is_empty() {
                return;
            }
            let perm = line
                .split_whitespace()
                .find_map(|f| f.strip_prefix("permission="))
                .map(|s| s.trim_matches('"').to_string())
                .unwrap_or_else(|| "rw".to_string());
            if !self.tokens.iter().any(|(t, _)| t == &tok) {
                self.tokens.push((tok, perm));
            }
            if self.session_id.is_none() {
                self.session_id = sid;
            }
        }
    }
}

/// 拉起**旧版**二进制，只注入它原本就认识的东西。
///
/// 与 [`spawn_agent`] 的差别：
///
/// - 不设 `SR_UPGRADE_RESTORE`（旧版不读它，留着只会让日志误导排查）；
/// - 不传 `--upgrade-restore`；
/// - 若交接里没有 `--session-id` 但 relay 上已有同名会话，补上
///   `--session-id <原 id>`——这是存量参数，用来让旧版回到**同一个**会话，
///   否则临时设备每次都会拿到新 ID。补的参数只是旧版本来就该有的身份。
fn spawn_agent_for_rollback(
    ctx: &RestartContext,
    _handoff: &Path,
    rollback_reason: &str,
) -> std::io::Result<std::process::Child> {
    let mut argv = ctx.argv.clone();
    if !ctx.session_id.is_empty() && !argv_has_flag(&argv, "--session-id") {
        argv.push(OsString::from("--session-id"));
        argv.push(OsString::from(&ctx.session_id));
    }
    let mut cmd = std::process::Command::new(&ctx.target_path);
    cmd.args(&argv)
        // 让旧版把"这是回滚重启"记进它自己的日志（未知环境变量无害）。
        .env(ROLLBACK_ENV, rollback_reason)
        .env_remove(RESTORE_ENV)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// argv 里是否已有某个 flag（`--x v` 或 `--x=v` 两种写法）。
fn argv_has_flag(argv: &[OsString], flag: &str) -> bool {
    let with_eq = format!("{flag}=");
    argv.iter().any(|a| {
        a.to_str()
            .map(|s| s == flag || s.starts_with(&with_eq))
            .unwrap_or(false)
    })
}

/// 等价于旧实现的 `restart_process`，但多一条：把一次性身份恢复信息通过
/// **环境变量**交给子进程（回滚路径的旧版二进制可能不认识 argv 参数，
/// 而未知环境变量是无害的）。
fn spawn_agent(
    ctx: &RestartContext,
    handoff: &Path,
    rollback_reason: Option<&str>,
) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(&ctx.target_path);
    cmd.args(&ctx.argv)
        .env(RESTORE_ENV, handoff)
        .env_remove(ROLLBACK_ENV)
        .stdin(std::process::Stdio::null())
        // stdout 留给用户的界面/TUI/窗口，助手不碰。
        .stdout(std::process::Stdio::inherit())
        // stderr 是注册回执通道：助手边读边转存日志。
        .stderr(std::process::Stdio::piped());
    if let Some(reason) = rollback_reason {
        cmd.env(ROLLBACK_ENV, reason);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 独立进程组：旧会话的终端注销（SIGHUP 发给前台进程组）杀不掉它。
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// spawn 时对 ETXTBSY 做有界重试。
///
/// 只重试这一个 errno：其它错误（ENOENT/EACCES/ENOEXEC）重试也没用，只会
/// 拖慢失败上报。
fn spawn_with_etxtbsy_retry(
    cmd: &mut std::process::Command,
    budget: Duration,
) -> std::io::Result<std::process::Child> {
    let deadline = Instant::now() + budget;
    loop {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e) => {
                let busy = e.raw_os_error() == Some(libc_etxtbsy());
                if !busy || Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(unix)]
const fn libc_etxtbsy() -> i32 {
    26
}

#[cfg(not(unix))]
const fn libc_etxtbsy() -> i32 {
    // Windows 没有这个 errno：给一个不可能命中的值，重试逻辑自然不生效。
    -1
}

/// 读新进程的 stderr，直到看到注册标记、进程退出或超时。
///
/// 返回 `Ok(version)` 只在真的收到 `registered` 时——"进程起来了"不等于
/// "身份恢复了"：注册失败的进程可能还在重试，此时回滚才是对的。
fn await_registration(
    child: &mut std::process::Child,
    ctx: &RestartContext,
    log: &mut HelperLog,
    timeout: Duration,
) -> Result<String, String> {
    use std::sync::mpsc;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "the new process has no stderr pipe".to_string())?;
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::Builder::new()
        .name("sr-upgrade-stderr".into())
        .spawn(move || {
            let mut buf = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match buf.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end().to_string();
                        if tx.send(trimmed).is_err() {
                            // 助手不再关心（已经拿到标记或已超时）：继续排空
                            // 到 EOF，避免子进程因为管道写满而阻塞。
                            continue;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| format!("cannot start the log pump: {e}"))?;

    let deadline = Instant::now() + timeout;
    let mut result: Result<String, String> = Err(format!("no registration within {timeout:?}"));
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match rx.recv_timeout(POLL) {
            Ok(line) => {
                if let Some(version) = parse_registered_marker(&line, &ctx.upgrade_id) {
                    log.line(&format!("registration marker: {line}"));
                    result = Ok(version);
                    break;
                }
                if !line.is_empty() {
                    // 新进程的日志转存：升级现场在旧 agent 退出后只剩这一份。
                    log.line(&format!("new-agent| {line}"));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // 管道 EOF：进程退出（或关掉了 stderr）。等 try_wait 给出结论。
                match child.try_wait() {
                    Ok(Some(status)) => {
                        result = Err(format!("the new process exited early ({status})"));
                        break;
                    }
                    _ => {
                        std::thread::sleep(POLL);
                    }
                }
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            result = Err(format!(
                "the new process exited early ({status}) before registering"
            ));
            break;
        }
    }
    // 读线程自行读到 EOF 结束；助手退出时它随进程一起消失。
    drop(reader);
    result
}

/// 解析注册标记行：`shell-remote: upgrade-registered upgrade_id=… version=…`。
///
/// 必须核对 upgrade_id——同一台设备可能连续升级，一个来自上一轮残留进程
/// 的标记绝不能被当成本轮的注册回执。
fn parse_registered_marker(line: &str, expected_upgrade_id: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(REGISTERED_MARKER)?;
    let mut upgrade_id = None;
    let mut version = None;
    for field in rest.split_whitespace() {
        if let Some(v) = field.strip_prefix("upgrade_id=") {
            upgrade_id = Some(v);
        } else if let Some(v) = field.strip_prefix("version=") {
            version = Some(v);
        }
    }
    let got_id = upgrade_id?;
    if !expected_upgrade_id.is_empty() && got_id != expected_upgrade_id {
        return None;
    }
    version.map(|v| v.to_string())
}

/// 结束新进程及其子进程（unix 上它自成进程组，`kill(-pid)` 才能连根拔）。
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        unsafe {
            // 先整组，再兜单个（进程组可能因为 spawn 时序还没建立）。
            libc::kill(-pid, libc::SIGKILL);
            libc::kill(pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn announce_ready_to_stdout() {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{READY_LINE}");
    let _ = out.flush();
}

fn fail_to_stdout(msg: &str) {
    let mut out = std::io::stdout();
    // 单行：读端只读一行就够判定。
    let one_line = msg.replace(['\n', '\r'], " ");
    let _ = writeln!(out, "{ERROR_PREFIX}{one_line}");
    let _ = out.flush();
}

/// 清理安装目录里上一次留下的助手副本 / 过期备份。
///
/// 只碰我们自己命名的文件，并且只清"够旧"的，避免把正在进行的另一轮升级
/// 的中间产物删掉。
fn cleanup_stale_helper_copies(dir: &Path, log: &mut HelperLog) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let ours = name.starts_with(HELPER_COPY_PREFIX) || name.contains(BACKUP_INFIX);
        if !ours {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .map(|age| age > Duration::from_secs(24 * 60 * 60))
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
            log.line(&format!("removed stale upgrade leftover {name}"));
        }
    }
}

/// 只清本次运行产生的助手副本（Windows 上删不掉自己，交由下次启动清理）。
fn cleanup_helper_copies(dir: &Path, log: &mut HelperLog) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let me = std::env::current_exe().ok();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(HELPER_COPY_PREFIX) {
            continue;
        }
        if me.as_ref() == Some(&entry.path()) {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            log.line(&format!("removed helper copy {name}"));
        }
    }
}

/// Windows 专用：本进程镜像正在 target 上 → 复制一份到同目录并委派。
///
/// Windows 不允许给正在运行的 `.exe` 改名，而助手做的第一件事就是把 target
/// 改名备份。所以先从"不在 target 上"的副本执行真正的换机。返回 `None`
/// 表示"本进程不在 target 上，可以自己动手"。
#[cfg(windows)]
fn delegate_when_running_from_target(
    ctx: &RestartContext,
    handoff: &Path,
    announce_ready: bool,
) -> Option<HelperOutcome> {
    let me = std::env::current_exe().ok()?.canonicalize().ok()?;
    let target = ctx
        .target_path
        .canonicalize()
        .unwrap_or_else(|_| ctx.target_path.clone());
    if me != target {
        return None;
    }
    let mut log = HelperLog::open(handoff);
    let dir = target.parent()?;
    use rand::Rng as _;
    let name = format!(
        "{HELPER_COPY_PREFIX}{:016x}.exe",
        rand::thread_rng().gen::<u64>()
    );
    let copy = dir.join(name);
    if let Err(e) = std::fs::copy(&me, &copy) {
        let msg = format!("cannot copy the update helper out of the install directory: {e}");
        log.line(&msg);
        if announce_ready {
            fail_to_stdout(&msg);
        }
        return Some(HelperOutcome::aborted(msg));
    }
    let mut cmd = std::process::Command::new(&copy);
    cmd.arg("update-apply")
        .arg(handoff)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    match cmd.spawn() {
        Ok(child) => {
            log.line(&format!(
                "delegated the swap to {} (pid {})",
                copy.display(),
                child.id()
            ));
            if announce_ready {
                announce_ready_to_stdout();
            }
            Some(HelperOutcome::Delegated)
        }
        Err(e) => {
            let msg = format!("cannot start the relocated update helper: {e}");
            log.line(&msg);
            let _ = std::fs::remove_file(&copy);
            if announce_ready {
                fail_to_stdout(&msg);
            }
            Some(HelperOutcome::aborted(msg))
        }
    }
}

// ── 旧 agent 侧：启动助手并等 ready ────────────────────────────────

/// 启动换机助手并等到它 `ready`。
///
/// 只有返回 `Ok` 才能让旧进程退出。`Err` 覆盖三种情况：助手起不来（spawn
/// 失败）、助手自己报了错（目录不可写、新文件校验不符）、超时没有任何回应。
/// 三者都发生在磁盘被改动之前，旧 agent 照常服务。
///
/// **受 systemd 托管时必须走 transient unit**（MYS-2425-D）：本函数用
/// `process_group(0)` 起的助手仍在旧 service 的 cgroup 里，主进程一退出就会被
/// `KillMode=control-group` 一起清掉——现场就是这么卡住的。判据与失败处理见
/// [`launch_helper_managed`]。
pub fn launch_helper_and_wait_ready(
    helper_exe: &Path,
    handoff: &Path,
    upgrade_id: &str,
    timeout: Duration,
) -> Result<std::process::Child, String> {
    launch_helper_via(
        managed_helper_route(upgrade_id),
        helper_exe,
        handoff,
        upgrade_id,
        timeout,
    )
}

/// [`launch_helper_and_wait_ready`] 的显式路由版本。
///
/// 抽出来有两个用处：让"非 systemd 流程不回归"可以被**确定地**测到（测试机
/// 自己往往就跑在 systemd unit 里，靠环境判定会让用例时灵时不灵），以及让
/// 两条路径的差异只体现在一个枚举上。
pub fn launch_helper_via(
    route: ManagedRoute,
    helper_exe: &Path,
    handoff: &Path,
    upgrade_id: &str,
    timeout: Duration,
) -> Result<std::process::Child, String> {
    match route {
        ManagedRoute::Direct => launch_helper_direct(helper_exe, handoff, timeout),
        ManagedRoute::Transient => launch_helper_managed(helper_exe, handoff, upgrade_id, timeout),
    }
}

/// 换机助手该怎么起。
///
/// 单独抽出来是为了让"什么时候必须走 transient、什么时候可以直连"成为一条
/// 可单测的判定，而不是埋在 `launch_*` 里的一个 `if`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedRoute {
    /// 非 systemd 环境：沿用既有直接助手流程（行为与 v0.59.5 完全一致）。
    Direct,
    /// systemd 托管：必须放到独立 transient unit，否则会被同 cgroup 清理。
    Transient,
}

/// 判定本次该走哪条路。`managed` 由 [`managed_detection`] 给出，便于测试注入。
pub fn managed_route(managed: bool) -> ManagedRoute {
    if managed {
        ManagedRoute::Transient
    } else {
        ManagedRoute::Direct
    }
}

/// 生产路径的托管判定，Linus/macOS/Windows 的差异都在这里收敛。
///
/// 非 Linux 平台恒为 `false`：Windows 有 SCM/Job 那套语义，本修复不涉及，
/// 行为必须与本改动之前逐字节一致。
pub fn managed_detection() -> bool {
    #[cfg(target_os = "linux")]
    {
        super::systemd_helper::current_is_managed()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// [`managed_route`] 的生产入口：读环境后给结论。
fn managed_helper_route(upgrade_id: &str) -> ManagedRoute {
    let route = managed_route(managed_detection());
    if route == ManagedRoute::Transient {
        tracing::info!(
            upgrade_id = %upgrade_id,
            cgroup = ?super::systemd_helper::current_cgroup_path(),
            "this agent runs under a systemd unit — starting the update helper in a transient \
             unit so it outlives this service's cgroup"
        );
    }
    route
}

/// 既有流程：直接 `spawn` 助手，自成进程组。
///
/// 只在**非** systemd 环境使用。systemd 下这里的 `process_group(0)` 不改变
/// cgroup，主进程退出会把助手一起清掉——这正是 MYS-2425-D 的根因，所以托管
/// 环境绝不走这里（见 [`launch_helper_managed`]）。
fn launch_helper_direct(
    helper_exe: &Path,
    handoff: &Path,
    timeout: Duration,
) -> Result<std::process::Child, String> {
    let mut cmd = std::process::Command::new(helper_exe);
    cmd.arg("update-apply")
        .arg(handoff)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 助手要活过旧进程的退出（甚至终端注销）：自成进程组。
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| {
        format!(
            "cannot start the update helper {}: {e}",
            helper_exe.display()
        )
    })?;
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the update helper has no stdout pipe".to_string());
        }
    };
    await_ready_line(stdout, child, timeout)
}

/// systemd 托管下的启动：把同一个隐藏子命令放进独立 transient unit。
///
/// 失败语义与直接助手一致——旧进程继续运行、磁盘不动——但**不回退**到直接
/// 助手：那样只是把同一个 bug 再演一遍，还会让管理界面以为升级在进行中。
fn launch_helper_managed(
    helper_exe: &Path,
    handoff: &Path,
    upgrade_id: &str,
    timeout: Duration,
) -> Result<std::process::Child, String> {
    #[cfg(target_os = "linux")]
    {
        match super::systemd_helper::launch(helper_exe, handoff, upgrade_id, timeout) {
            super::systemd_helper::TransientHelper::Ready(child) => Ok(child),
            super::systemd_helper::TransientHelper::Failed(reason) => Err(format!(
                "cannot start the update helper as a systemd transient unit: {reason}"
            )),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // 不可达：非 Linux 的 managed_detection() 恒为 false。保留分支是为了
        // 让类型对上，且显式说明"这里不该发生"。
        let _ = (helper_exe, handoff, upgrade_id, timeout);
        Err("transient update helpers are only supported on Linux".to_string())
    }
}

/// 从助手的 stdout 读首行，判定就绪与否。
///
/// 直接助手与 transient 助手共用这一段（transient 侧在
/// [`super::systemd_helper::launch`] 里内联了等价逻辑，因为要先读懂 `--pipe`
/// 的握手再决定是否收掉 unit）。这里保留一份，保证两条路径的**判据**一致。
fn await_ready_line(
    stdout: impl std::io::Read + Send + 'static,
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<std::process::Child, String> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("sr-upgrade-ready".into())
        .spawn(move || {
            let mut buf = BufReader::new(stdout);
            let mut line = String::new();
            if buf.read_line(&mut line).is_ok() && !line.is_empty() {
                let _ = tx.send(line.trim_end().to_string());
            }
            // 读线程到此结束：助手之后不再写 stdout。
        })
        .map_err(|e| format!("cannot start the ready pump: {e}"))?;

    let first = match rx.recv_timeout(timeout) {
        Ok(line) => line,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "the update helper did not become ready within {timeout:?}"
            ));
        }
    };
    if first == READY_LINE {
        // 读线程只持有 `stdout`（它读到 EOF 自行结束），child 的所有权仍在本
        // 栈上——直接返回即可。
        return Ok(child);
    }
    let _ = child.kill();
    let _ = child.wait();
    let reason = first
        .strip_prefix(ERROR_PREFIX)
        .unwrap_or(&first)
        .to_string();
    Err(if reason.is_empty() {
        "the update helper exited without becoming ready".to_string()
    } else {
        reason
    })
}

#[cfg(test)]
mod tests;
