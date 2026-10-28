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
            .field("old_pid", &self.old_pid)
            .field("upgrade_id", &self.upgrade_id)
            .field("target_version", &self.target_version)
            .field("target_path", &self.target_path)
            .field("new_binary", &self.new_binary)
            .field("new_sha256", &self.new_sha256)
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

impl RestartContext {
    /// 从旧 agent 的视角构造：`argv` 必须已剥掉隐藏恢复参数（否则新进程
    /// 会去读上一轮的交接文件，身份与 upgrade_id 全错）。
    pub fn new(
        session_id: String,
        tokens: Vec<(String, String)>,
        raw_argv: Vec<OsString>,
        target_path: PathBuf,
        new_binary: PathBuf,
        new_sha256: String,
        upgrade_id: String,
        target_version: String,
    ) -> Self {
        Self {
            session_id,
            tokens,
            argv: strip_restore_args(raw_argv),
            old_pid: std::process::id(),
            old_start: process_start_stamp(std::process::id()),
            upgrade_id,
            target_version,
            target_path,
            new_binary,
            new_sha256,
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
            os_encoding: os_encoding_name().to_string(),
            argv: self.argv.iter().map(|a| encode_os(a)).collect(),
            old_pid: self.old_pid,
            old_start: self.old_start,
            upgrade_id: self.upgrade_id.clone(),
            target_version: self.target_version.clone(),
            target_path: encode_path(&self.target_path),
            new_binary: encode_path(&self.new_binary),
            new_sha256: self.new_sha256.clone(),
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
            argv,
            old_pid: f.old_pid,
            old_start: f.old_start,
            upgrade_id: f.upgrade_id,
            target_version: f.target_version,
            target_path,
            new_binary,
            new_sha256: f.new_sha256.to_ascii_lowercase(),
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
pub fn restore_handoff_path(flag: Option<&str>) -> Option<PathBuf> {
    flag.map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os(RESTORE_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
}

/// 一次性身份恢复的完整输入：交给 `connect_with_retry` 的初始 cached_tokens
/// 与注册回执，以及"成功之后要删交接文件"这个义务。
pub struct RestoredIdentity {
    pub restore: UpgradeRestore,
    pub handoff: PathBuf,
    /// 是否已经完成（交接文件已删、助手已回执）。`start()` 用它在首次会话
    /// 结束后停止传递恢复语义。
    consumed: std::sync::atomic::AtomicBool,
}

impl RestoredIdentity {
    /// 读交接文件。文件不存在/不可读/权限不符 → `None`：宁可回到正常启动
    /// 流程（固定 key 仍可恢复），也不要因为一个坏文件卡死这台设备。
    pub fn load(path: &Path) -> Option<Self> {
        // 回滚原因一次性消费：不能让后续子进程继承成"我们也回滚了"。
        let rollback = std::env::var(ROLLBACK_ENV).ok().filter(|s| !s.is_empty());
        std::env::remove_var(ROLLBACK_ENV);
        Self::load_with_rollback(path, rollback)
    }

    /// [`load`] 的显式传参版本。测试用它避免争抢进程级环境变量（并行跑时
    /// 两个用例会互相覆盖 `SR_UPGRADE_ROLLBACK`）。
    pub fn load_with_rollback(path: &Path, rollback: Option<String>) -> Option<Self> {
        let ctx = match RestartContext::read_handoff(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    handoff = %path.display(),
                    error = %e,
                    "ignoring the upgrade handoff file (cannot read it safely)"
                );
                return None;
            }
        };
        Some(Self {
            restore: UpgradeRestore {
                tokens: ctx.tokens.clone(),
                session_id: ctx.session_id.clone(),
                upgrade_id: ctx.upgrade_id.clone(),
                target_version: ctx.target_version.clone(),
                rollback,
            },
            handoff: path.to_path_buf(),
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
    }

    /// 回滚后的重启要把失败原因送回 relay——否则管理界面只会看到一个
    /// "升级了但版本没变"的谜。`None` = 正常升级后的首次注册，无需上报。
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

/// 助手的结局。`run()` 把它映射成退出码；测试直接断言它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperOutcome {
    /// 新进程注册成功，磁盘上已经是新版。
    Registered { version: String },
    /// 换机失败，已回滚旧文件并用原身份拉起旧版。
    RolledBack { reason: String },
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
    fn rolled_back(reason: impl Into<String>) -> Self {
        Self::RolledBack {
            reason: reason.into(),
        }
    }
}

/// 隐藏子命令 `update-apply` 的入口。返回值即进程退出码。
///
/// 退出码语义固定（便于运维脚本判定）：0 成功、2 未动磁盘即放弃、3 已回滚。
pub fn run(handoff: &Path) -> i32 {
    match helper_main_with(handoff, true, HelperTimeouts::default()) {
        HelperOutcome::Registered { .. } | HelperOutcome::Delegated => EXIT_REGISTERED,
        HelperOutcome::RolledBack { .. } => EXIT_ROLLED_BACK,
        HelperOutcome::Aborted { .. } => EXIT_ABORTED,
    }
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
            return rollback(&ctx, handoff, &backup, &mut log, msg, None);
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
            rollback(&ctx, handoff, &backup, &mut log, reason, Some(child.id()))
        }
    }
}

/// 回滚：恢复旧文件，用**原身份**拉起旧版，并留下可读日志。
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
        return HelperOutcome::rolled_back(msg);
    }
    log.line("previous binary restored");
    fsync_dir(ctx.target_path.parent().unwrap_or(Path::new(".")));

    match spawn_agent(ctx, handoff, Some(&reason)) {
        Ok(child) => log.line(&format!(
            "previous agent restarted with its original identity (pid {})",
            child.id()
        )),
        Err(e) => log.line(&format!(
            "previous binary restored but cannot be restarted: {e} — start it manually"
        )),
    }
    HelperOutcome::rolled_back(reason)
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
pub fn launch_helper_and_wait_ready(
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
