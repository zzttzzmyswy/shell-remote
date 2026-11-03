//! 目标目录不可写时的 overlay 自升级（MYS-2574）。
//!
//! ## 为什么需要这个模块
//!
//! 最常见的生产形态是「root 所有的 `/usr/local/bin` + `User=<普通用户>` 的
//! systemd unit」。既有三条换机路径（`ManagedPlan::{Direct, Coordinated,
//! SystemdRestart}`）都要**改写目标二进制所在目录**，在这种部署下必然失败
//! （`Permission denied (os error 13)`）。
//!
//! 这里换一个思路：**root 所有的 base 二进制一个字节都不动**，新版本装进用户
//! 可写的 overlay 目录，每次启动由 base 做完安全检查后把执行权交给它。
//!
//! ```text
//! /usr/local/bin/shell-remote             ← base（root 所有，永不改写）
//! ~/.local/share/shell-remote/overlay/
//!   current.json                          ← 指针：装的是哪一版、健康与否、试了几次
//!   bin/shell-remote-0.59.13-1a2b3c4d     ← 新版本二进制（0700）
//! ```
//!
//! 启动时序（`agent` 子命令，注册之前）：
//!
//! ```text
//! base 启动 → 读 current.json → 版本/属主/权限/sha256 全过 → exec(overlay)
//!                            └→ 任一不过 → warn + 照常用 base 跑
//! overlay 注册成功 → current.json 写 healthy=true, attempts=0
//! overlay 起不来（连续 3 次） → current.json 记 rejected=true，回落到 base
//! ```
//!
//! ## 不变式
//!
//! 1. **只在目标目录不可写时才启用**。可写时完全沿用既有换机路径，行为不变。
//! 2. **overlay 二进制的可信度由指针 + 文件系统权限 + sha256 三者共同担保**：
//!    任何一项不满足就不交接，绝不"尽力而为"地执行一个可能被别人换过的文件。
//! 3. **回滚不需要备份**：base 一直在盘上，且永远不被改动，`rejected` 一置位就
//!    等于回到 base 运行。
//!
//! Windows 不走这条路：目标目录不可写时由 SCM/ACL 那套语义处理，本模块只在
//! `cfg(unix)` 下编译。

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ── 常量 ────────────────────────────────────────────────────────────

/// 能力声明：本 agent 支持 overlay 自升级（relay 可据此放宽"目录不可写"的判定）。
pub const CAPABILITY: &str = "upgrade:overlay-v1";

/// 交接标记（环境变量）。base `exec` overlay 时设置，值为 base 的绝对路径。
///
/// 它同时是**防循环**的凭据：设置了它的进程本身就来自 overlay，绝不再交接一次。
pub const ENV_FROM: &str = "SR_OVERLAY_FROM";

/// 指针格式版本。不认识就拒绝交接（宁可跑 base，也不猜别的格式）。
pub const SCHEMA: u32 = 1;

/// 连续这么多次交接后仍未 `healthy`，判定新版本起不来，自动回落到 base。
pub const MAX_ATTEMPTS: u32 = 3;

/// 指针文件名。
pub const POINTER_FILE: &str = "current.json";
/// 版本二进制所在子目录。
pub const BIN_DIR: &str = "bin";
/// 版本二进制文件名前缀（后缀是 `<版本>-<sha256 前 8 位>`）。
const EXE_PREFIX: &str = "shell-remote-";

// ── 目录布局 ────────────────────────────────────────────────────────

/// overlay 目录布局。所有路径都从 `root` 派生，测试因此可以指向临时目录。
///
/// 生产默认 `${XDG_DATA_HOME:-$HOME/.local/share}/shell-remote/overlay`：
/// 这是"用户自己的数据目录"，普通账户天然可写，且不随 /tmp 被清理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 生产路径：`$XDG_DATA_HOME` → `$HOME/.local/share`（见 [`Self::default_root`]）。
    ///
    /// 两者都拿不到（无 HOME 的极端服务账号）就返回 `Err`：overlay 必须落在
    /// **用户可写且重启后仍在**的位置，退到 `/tmp` 会在重启后失效，等于假装
    /// 升级成功。
    pub fn from_env() -> Result<Self, String> {
        Ok(Self::new(Self::default_root()?))
    }

    pub fn default_root() -> Result<PathBuf, String> {
        if let Some(data) = non_empty_env("XDG_DATA_HOME") {
            return Ok(PathBuf::from(data).join("shell-remote").join("overlay"));
        }
        if let Some(home) = non_empty_env("HOME") {
            return Ok(PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("shell-remote")
                .join("overlay"));
        }
        Err(
            "cannot locate an overlay directory: neither $XDG_DATA_HOME nor $HOME is set"
                .to_string(),
        )
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join(BIN_DIR)
    }

    pub fn pointer_path(&self) -> PathBuf {
        self.root.join(POINTER_FILE)
    }

    /// 下载用的 `.part` 路径（放在 overlay 里：目标目录不可写，别处放不下）。
    ///
    /// 按目标制品的 sha256 前缀命名，与 `upgrade.rs` 的既有约定一致——断点续传
    /// 要能跨"这次失败 → 再点一次升级"接上，同时不同制品绝不复用字节。
    pub fn part_path(&self, sha256: &str) -> PathBuf {
        let head: String = sha256.chars().take(16).collect();
        self.root
            .join(format!(".shell-remote-upgrade-{head}.part"))
    }

    /// 某个版本二进制的落位路径：`bin/shell-remote-<版本>-<sha256 前 8 位>`。
    pub fn version_file(&self, version: &str, sha256: &str) -> PathBuf {
        let head: String = sha256.chars().take(8).collect();
        self.bin_dir()
            .join(format!("{EXE_PREFIX}{}-{}", sanitize(version), head))
    }

    /// 建好目录并复核权限：`root` 与 `bin` 都是 `0700`、属主是当前 euid。
    ///
    /// 用 `DirBuilder::mode` 在创建时就定权限，不存在"先建 0755 再 chmod"的
    /// 可抢占窗口；已存在的目录只校验、不修补——被别人改成 0777 是现场问题，
    /// 静默改回来会掩盖它。
    pub fn ensure(&self) -> Result<(), String> {
        for dir in [self.root.clone(), self.bin_dir()] {
            if !dir.exists() {
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder
                    .create(&dir)
                    .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            }
            let facts = dir_facts(&dir);
            if !facts.owner_ok {
                return Err(format!(
                    "the overlay directory {} is not owned by the current user",
                    dir.display()
                ));
            }
            if !facts.not_group_writable {
                return Err(format!(
                    "the overlay directory {} is writable by group/other (expected 0700)",
                    dir.display()
                ));
            }
        }
        Ok(())
    }

    /// 本机是否**已经在用** overlay（指针存在）。仅用于诊断/日志。
    pub fn is_active(&self) -> bool {
        self.pointer_path().is_file()
    }
}

fn non_empty_env(key: &str) -> Option<OsString> {
    std::env::var_os(key).filter(|v| !v.is_empty())
}

/// 把版本号收进文件名：只保留 `[A-Za-z0-9._-]`，其余替换成 `_`。
fn sanitize(raw: &str) -> String {
    let out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
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

// ── 指针（current.json）─────────────────────────────────────────────

/// `current.json` 的内容。
///
/// 前 8 个字段是设计定下的契约形状；`rejected` / `rejected_reported` /
/// `upgrade_id` / `previous` 是为"自动回滚"和"回滚后如实上报"追加的可选字段
/// （老文件没有它们也能读，缺省即 `false`/`None`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pointer {
    pub schema: u32,
    pub version: String,
    pub sha256: String,
    /// 已安装的版本二进制绝对路径。
    pub path: PathBuf,
    /// 装它的时候正在运行的 base 二进制（诊断用：管理员后来换过 base 时能看出来）。
    #[serde(default)]
    pub base_exe: Option<PathBuf>,
    /// 安装时刻（Unix 秒）。
    #[serde(default)]
    pub installed_at: u64,
    /// 这一版是否已经成功注册过。`true` 之后不再受 `attempts` 上限约束。
    #[serde(default)]
    pub healthy: bool,
    /// 已经交接出去的次数（每次 exec 前 +1）。
    #[serde(default)]
    pub attempts: u32,
    /// 新版本起不来的结论。置位后不再交接，等于回落到 base。
    #[serde(default)]
    pub rejected: bool,
    /// 本轮升级的 `upgrade_id`：回滚后要靠它把失败原因对账到 relay 的那次任务上。
    #[serde(default)]
    pub upgrade_id: Option<String>,
    /// `rejected` 是否已经上报过（避免每次重启都重复推一条失败帧）。
    #[serde(default)]
    pub rejected_reported: bool,
    /// 上一代版本文件（保留一份，便于人工回退/排查）。
    #[serde(default)]
    pub previous: Vec<PathBuf>,
}

impl Pointer {
    /// 新装一版时的初始指针：未验证、未尝试。
    pub fn fresh(
        version: &str,
        sha256: &str,
        path: PathBuf,
        base_exe: Option<PathBuf>,
        upgrade_id: Option<String>,
        previous: Vec<PathBuf>,
    ) -> Self {
        Self {
            schema: SCHEMA,
            version: version.to_string(),
            sha256: sha256.to_ascii_lowercase(),
            path,
            base_exe,
            installed_at: now_unix(),
            healthy: false,
            attempts: 0,
            rejected: false,
            upgrade_id,
            rejected_reported: false,
            previous,
        }
    }

    /// 读指针。`Ok(None)` = 没有安装过任何 overlay 版本。
    ///
    /// 读到的东西要能读成"自己的私有文件"：属主必须是当前 euid，且不可被
    /// group/other 写。不满足就报错，**绝不**顺手修权限——那等于替攻击者
    /// 擦掉痕迹。
    pub fn read(layout: &Layout) -> Result<Option<Self>, String> {
        let path = layout.pointer_path();
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let file = file_facts(&path);
        if !file.not_group_writable {
            return Err(format!(
                "{} is writable by group/other; refusing to trust the overlay pointer",
                path.display()
            ));
        }
        if raw.len() > 16 * 1024 {
            return Err(format!("{} is unexpectedly large", path.display()));
        }
        let pointer: Self = serde_json::from_slice(&raw)
            .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        if pointer.schema != SCHEMA {
            return Err(format!(
                "unsupported overlay pointer schema {} (this build understands {SCHEMA})",
                pointer.schema
            ));
        }
        Ok(Some(pointer))
    }

    /// 原子写回：先写同目录临时文件（`0600`）再 `rename`，读端永远看到完整内容。
    pub fn write(layout: &Layout, pointer: &Self) -> Result<(), String> {
        let path = layout.pointer_path();
        let body = serde_json::to_vec_pretty(pointer)
            .map_err(|e| format!("cannot encode the overlay pointer: {e}"))?;
        let tmp = layout
            .root
            .join(format!("{POINTER_FILE}.tmp-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?
        };
        file.write_all(&body)
            .and_then(|_| file.sync_all())
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("cannot write {}: {e}", tmp.display())
            })?;
        drop(file);
        std::fs::rename(&tmp, &path)
            .map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
        fsync_dir(&layout.root);
        Ok(())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fsync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

// ── 文件系统事实 ────────────────────────────────────────────────────

/// 一个文件系统对象的权限事实（目录与普通文件共用同一形状）。
///
/// `owner_ok` 同时涵盖"对象类型对不对"：目录必须真的是目录、文件必须真的是
/// 普通文件，两者的符号链接一律判失败——overlay 里的每一样东西都只能是自己
/// 亲手创建的实体，跟随链接等于让别人把它指到任意位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts2 {
    /// 存在、类型正确（不是符号链接）、属主是当前 euid。
    pub owner_ok: bool,
    /// 不可被 group/other 写。
    pub not_group_writable: bool,
}

/// 目录的权限事实。
pub type DirFacts = Facts2;
/// 文件的权限事实。
pub type FileFacts = Facts2;

/// 读目录事实。符号链接直接判失败：overlay 目录只能是自己建的实体目录，
/// 跟随链接等于让别人把"安全目录"指到任意位置。
pub fn dir_facts(dir: &Path) -> DirFacts {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => DirFacts {
            owner_ok: meta.is_dir() && meta.uid() == unsafe { libc::geteuid() },
            not_group_writable: meta.mode() & 0o022 == 0,
        },
        Err(_) => DirFacts {
            owner_ok: false,
            not_group_writable: false,
        },
    }
}

/// 读文件事实（同样不跟随符号链接）。
pub fn file_facts(path: &Path) -> FileFacts {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => FileFacts {
            owner_ok: meta.file_type().is_file() && meta.uid() == unsafe { libc::geteuid() },
            not_group_writable: meta.mode() & 0o022 == 0,
        },
        Err(_) => FileFacts {
            owner_ok: false,
            not_group_writable: false,
        },
    }
}

/// 目录是否可写：在**该目录里**真的建一个临时文件再删掉。
///
/// 不能用 `access(W_OK)` 之类的判定：它不看 ACL、不看只读挂载之外的
/// 各种细节，而且真正的失败点是"建临时文件"本身——既有 `update_apply` 的
/// 助手用的也是同一判据（真建一个文件），这里保持一致。
pub fn dir_is_writable(dir: &Path) -> bool {
    use rand::Rng as _;
    let probe = dir.join(format!(
        ".shell-remote-overlay-probe-{:016x}",
        rand::thread_rng().gen::<u64>()
    ));
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe);
    match created {
        Ok(mut f) => {
            let ok = f.write_all(b"ok").is_ok();
            drop(f);
            let _ = std::fs::remove_file(&probe);
            ok
        }
        Err(_) => false,
    }
}

// ── 交接判定 ────────────────────────────────────────────────────────

/// 判定所需的全部事实。运行路径由 [`probe`] 采集；单测直接构造，从而把每个
/// 分支都穷举到（真实文件系统里"属主不对""group-writable"很难稳定复现）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// `$SR_OVERLAY_FROM`：非空表示本进程就是被 base 交接过来的 overlay 版本。
    pub from_env: Option<PathBuf>,
    /// 本进程的可执行文件（已解析符号链接）。
    pub self_exe: PathBuf,
    /// 本进程自报版本（`CARGO_PKG_VERSION`）。
    pub self_version: String,
    pub pointer: Pointer,
    pub root: DirFacts,
    pub bin: DirFacts,
    /// 指针里那个版本二进制的权限事实。
    pub binary: FileFacts,
    /// 指针文件本身的权限事实。
    pub pointer_file: FileFacts,
    /// 指针里的版本二进制是否**直接位于** overlay 的 `bin/` 目录下。`bin/` 是
    /// 已核验过属主与权限的 0700 目录；放行它之外的路径，等于把"文件在
    /// sha256 校验与 exec 之间不被替换"寄托在一个没检查过的父目录上。
    pub binary_in_bin_dir: bool,
    /// 版本二进制实测 sha256（读不到为 `None`）。
    pub binary_sha256: Option<String>,
}

/// 一次启动的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 条件全部满足：exec 到这个路径。
    Exec { path: PathBuf },
    /// 照常用 base 跑，原因进日志。
    Skip { reason: String },
    /// 自身版本已高于 overlay（管理员手工换过更新的 base）：忽略 overlay，
    /// 并删除比自身旧的版本文件。
    PruneOlder { reason: String },
    /// 交接次数用尽仍未健康：记 `rejected`，此后不再交接（等价自动回滚）。
    Reject { reason: String },
}

impl Verdict {
    pub fn reason(&self) -> &str {
        match self {
            Self::Exec { .. } => "handing the exec over to the overlay binary",
            Self::Skip { reason } | Self::PruneOlder { reason } | Self::Reject { reason } => reason,
        }
    }
}

/// 交接判定（纯函数，所有输入都在 [`Facts`] 里）。
///
/// 判定顺序就是设计的检查清单，每一步都能单独测到：
///
/// 1. 防循环：`SR_OVERLAY_FROM` 已设置 / 自身就是指针指向的那个文件；
/// 2. 版本：必须**严格高于**自身（相等=同一版无需换，更低=管理员换过更新的
///    base，此时忽略 overlay 并清理更旧的版本文件）；
/// 3. 安全：目录/子目录/二进制/指针的属主都是 euid，且都不可被 group/other 写；
/// 4. 完整性：版本二进制的 sha256 与指针一致；
/// 5. 健康：`healthy` 为真，或尝试次数未到上限；到了上限就 `Reject`。
pub fn decide(facts: &Facts) -> Verdict {
    // 1. 防循环。
    if facts.from_env.is_some() {
        return Verdict::Skip {
            reason: format!(
                "already running from the overlay (${ENV_FROM} is set); not handing off again"
            ),
        };
    }
    if paths_equal(&facts.self_exe, &facts.pointer.path) {
        return Verdict::Skip {
            reason: "this process already is the overlay binary recorded in the pointer".to_string(),
        };
    }

    // 2. 版本。
    let (Some(self_ver), Some(over_ver)) = (
        parse_version(&facts.self_version),
        parse_version(&facts.pointer.version),
    ) else {
        return Verdict::Skip {
            reason: format!(
                "cannot compare versions (running {}, overlay {})",
                facts.self_version, facts.pointer.version
            ),
        };
    };
    if over_ver == self_ver {
        return Verdict::Skip {
            reason: format!("the overlay holds the same version ({over_ver}); nothing to hand off"),
        };
    }
    if over_ver < self_ver {
        return Verdict::PruneOlder {
            reason: format!(
                "the running binary ({self_ver}) is newer than the overlay ({over_ver}) — the base \
                 binary was replaced by hand; ignoring the overlay"
            ),
        };
    }

    // 3. 安全：版本二进制必须就在 overlay 的 bin/ 里；四样东西都必须是当前
    //    用户所有、且不可被 group/other 写。
    if !facts.binary_in_bin_dir {
        return Verdict::Skip {
            reason: format!(
                "the overlay binary {} is not inside the overlay bin directory",
                facts.pointer.path.display()
            ),
        };
    }
    for (what, facts_ok) in [
        ("the overlay directory", facts.root),
        ("the overlay bin directory", facts.bin),
        ("the overlay binary", facts.binary),
        ("the overlay pointer", facts.pointer_file),
    ] {
        if !facts_ok.owner_ok {
            return Verdict::Skip {
                reason: format!("{what} is not owned by the current user (or is not a plain file/dir)"),
            };
        }
        if !facts_ok.not_group_writable {
            return Verdict::Skip {
                reason: format!("{what} is writable by group/other"),
            };
        }
    }

    // 4. 完整性。
    match facts.binary_sha256.as_deref() {
        Some(actual) if crate::agent::download::verify_sha256(actual, &facts.pointer.sha256) => {}
        Some(actual) => {
            return Verdict::Skip {
                reason: format!(
                    "the overlay binary does not match the pointer digest (got {actual}, expected {})",
                    facts.pointer.sha256
                ),
            }
        }
        None => {
            return Verdict::Skip {
                reason: format!(
                    "cannot hash the overlay binary {}",
                    facts.pointer.path.display()
                ),
            }
        }
    }

    // 5. 健康 / 次数。
    if facts.pointer.rejected {
        return Verdict::Skip {
            reason: format!(
                "overlay {} was already rejected after {MAX_ATTEMPTS} failed starts; staying on the \
                 base binary",
                facts.pointer.version
            ),
        };
    }
    if !facts.pointer.healthy && facts.pointer.attempts >= MAX_ATTEMPTS {
        return Verdict::Reject {
            reason: format!(
                "overlay {} did not register after {} attempts; marking it rejected and staying on \
                 the base binary",
                facts.pointer.version, facts.pointer.attempts
            ),
        };
    }
    Verdict::Exec {
        path: facts.pointer.path.clone(),
    }
}

/// `path` 的父目录是否就是 `dir`（两侧都按 canonicalize 比较；任一侧解析
/// 失败一律判否）。
pub fn is_directly_inside(path: &Path, dir: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    match (parent.canonicalize(), dir.canonicalize()) {
        (Ok(p), Ok(d)) => p == d,
        _ => false,
    }
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    // 任一侧解析失败时只认字面比较：解析不了就别猜。
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn parse_version(raw: &str) -> Option<semver::Version> {
    semver::Version::parse(raw.trim()).ok()
}

/// 从事实里采集判定输入（生产路径）。
///
/// 指针里那个路径要**重新算** sha256：指针只是一个文件，真正被执行的是磁盘上
/// 的字节，两者必须对得上。
pub fn probe(layout: &Layout, self_exe: &Path, self_version: &str) -> Option<Facts> {
    let pointer = Pointer::read(layout).ok().flatten()?;
    Some(Facts {
        from_env: non_empty_env(ENV_FROM).map(PathBuf::from),
        self_exe: self_exe.to_path_buf(),
        self_version: self_version.to_string(),
        root: dir_facts(layout.root()),
        bin: dir_facts(&layout.bin_dir()),
        binary: file_facts(&pointer.path),
        pointer_file: file_facts(&layout.pointer_path()),
        binary_in_bin_dir: is_directly_inside(&pointer.path, &layout.bin_dir()),
        binary_sha256: crate::agent::download::sha256_hex_file(&pointer.path)
            .ok()
            .map(|s| s.to_ascii_lowercase()),
        pointer,
    })
}

// ── 启动交接（生产入口）─────────────────────────────────────────────

/// `agent` 子命令启动时调用（注册之前）。满足条件就 **exec** 到 overlay 版本，
/// 本函数不再返回；否则照常用 base 运行。
///
/// 只在 unix 上有意义；调用点已按 `cfg(unix)` 门控。
pub fn handoff_on_startup() {
    // 已经是被交接过来的进程：什么都不做（避免 exec 环）。
    if non_empty_env(ENV_FROM).is_some() {
        return;
    }
    let self_exe = match std::env::current_exe().and_then(|p| p.canonicalize()) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "cannot resolve the running executable; skipping the overlay handoff");
            return;
        }
    };
    let layout = match Layout::from_env() {
        Ok(l) => l,
        Err(e) => {
            tracing::debug!(error = %e, "no overlay directory; running the base binary");
            return;
        }
    };
    let Some(facts) = probe(&layout, &self_exe, env!("CARGO_PKG_VERSION")) else {
        return;
    };
    match decide(&facts) {
        Verdict::Exec { path } => {
            // 先记一笔再交接：进程被 exec 替换后没有机会再写盘。这样"交接了但没
            // 起来"的每一次失败都能被下一次启动看到，三次之后自动回落。
            let mut pointer = facts.pointer.clone();
            pointer.attempts = pointer.attempts.saturating_add(1);
            if let Err(e) = Pointer::write(&layout, &pointer) {
                // 写不进尝试次数就**不交接**：否则一个起不来的版本会被无限拉起
                // （systemd 每次重启都重新数到 0），再也没有回滚的机会。
                tracing::warn!(error = %e, "cannot record the overlay attempt; staying on the base binary");
                return;
            }
            tracing::info!(
                overlay = %path.display(),
                version = %pointer.version,
                attempt = pointer.attempts,
                base = %self_exe.display(),
                "handing the exec over to the overlay binary"
            );
            if let Err(e) = exec_overlay(&path, &self_exe) {
                // exec 只在失败时返回：继续用 base 跑，并把刚加的次数退回去，
                // 免得一个"永远 exec 不成功"的环境把自己数到 rejected。
                pointer.attempts = pointer.attempts.saturating_sub(1);
                let _ = Pointer::write(&layout, &pointer);
                tracing::warn!(
                    overlay = %path.display(),
                    error = %e,
                    "cannot exec the overlay binary; staying on the base binary"
                );
            }
        }
        Verdict::Skip { reason } => {
            tracing::warn!(reason = %reason, "not handing off to the overlay binary");
        }
        Verdict::PruneOlder { reason } => {
            tracing::warn!(reason = %reason, "ignoring the overlay binary");
            let self_ver = parse_version(&facts.self_version);
            prune_older_than(&layout, self_ver.as_ref());
        }
        Verdict::Reject { reason } => {
            tracing::error!(
                reason = %reason,
                version = %facts.pointer.version,
                "the overlay version never became healthy; falling back to the base binary"
            );
            let mut pointer = facts.pointer.clone();
            pointer.rejected = true;
            pointer.healthy = false;
            if let Err(e) = Pointer::write(&layout, &pointer) {
                tracing::warn!(error = %e, "cannot persist the overlay rejection");
            }
        }
    }
}

/// 用 overlay 二进制替换当前进程：argv 与 env 原样传递，另加 [`ENV_FROM`]。
///
/// `exec` 成功后本函数不返回；返回 `Err` 只在替换失败时发生（调用方继续用
/// base 运行，绝不 `exit`）。
fn exec_overlay(overlay: &Path, base: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let argv: Vec<OsString> = std::env::args_os().collect();
    let mut cmd = std::process::Command::new(overlay);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    // 环境原样继承（含 `SR_UPGRADE_RESTORE` —— 身份恢复就靠它），只多一个标记。
    cmd.env(ENV_FROM, base);
    // `exec` 成功时**不返回**；返回的类型本身就是"失败原因"。
    Err(cmd.exec())
}

// ── "我是谁"：base 还是 overlay ─────────────────────────────────────

/// 本进程是不是 overlay 版本（`$SR_OVERLAY_FROM` 已设置）。
///
/// 只看环境变量，不看文件名：这个标记由 base 在 exec 前打上，是**唯一**可信的
/// 判据（文件名可以被手工改，环境变量只有交接那条路径会设）。
pub fn running_from_overlay() -> bool {
    non_empty_env(ENV_FROM).is_some()
}

/// 这台设备真正的 **base 二进制**——升级与重启都要拉起它，而不是 overlay。
///
/// 三个来源，按可信度排序：
///
/// 1. `$SR_OVERLAY_FROM`：base 交接时亲手写进来的，最准；
/// 2. 指针里的 `base_exe`：进程重启后（由 systemd 拉起 overlay）环境变量已经
///    不在了，只能靠安装时记下的那个路径；
/// 3. 兜底：本进程自己的路径（即"本来就不是 overlay 版本"）。
///
/// 三档都是"路径"而不是"猜测"：拿不到就返回自己的路径，让调用方按普通设备
/// 处理，绝不凭空编一个路径出来。
pub fn base_exe() -> PathBuf {
    if let Some(from) = non_empty_env(ENV_FROM) {
        return PathBuf::from(from);
    }
    if let Ok(layout) = Layout::from_env() {
        if let Ok(Some(pointer)) = Pointer::read(&layout) {
            if let Some(base) = pointer.base_exe {
                return base;
            }
        }
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("shell-remote"))
}

// ── 安装 ────────────────────────────────────────────────────────────

/// 一次成功安装的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// 落位后的版本二进制路径。
    pub path: PathBuf,
    /// 保留的上一代版本文件（若原来已有一版）。
    pub previous: Vec<PathBuf>,
}

/// 把已校验的制品装进 overlay 并写好指针。
///
/// 顺序不可交换：先落位二进制（`rename` 原子就位）→ 再写指针。反过来的话，
/// 指针会短暂指向一个还没写完的文件，而启动交接正是按指针去找文件的。
pub fn install(
    layout: &Layout,
    artifact: &Path,
    version: &str,
    sha256: &str,
    base_exe: Option<&Path>,
    upgrade_id: Option<&str>,
) -> Result<Installed, String> {
    layout.ensure()?;
    let dest = layout.version_file(version, sha256);
    // 上一代：指针里原有的那一版（只留一份，见 `prune`）。
    let previous: Vec<PathBuf> = Pointer::read(layout)?
        .map(|p| vec![p.path])
        .unwrap_or_default()
        .into_iter()
        .filter(|p| *p != dest)
        .collect();

    place_binary(artifact, &dest, sha256)?;
    let pointer = Pointer::fresh(
        version,
        sha256,
        dest.clone(),
        base_exe.map(|p| p.to_path_buf()),
        upgrade_id.map(|s| s.to_string()),
        previous.clone(),
    );
    Pointer::write(layout, &pointer)?;
    prune_versions(layout, &dest, &previous);
    Ok(Installed {
        path: dest,
        previous,
    })
}

/// 把制品搬到目标路径：同目录临时文件 → `0700` → `rename`。
///
/// 直接用 `rename` 会把 `.part` 的名字换掉，断点续传的字节就此消失——成功路径
/// 上这没问题（已经装好了），失败路径上则由调用方决定是否保留 `.part`。
/// 跨文件系统时退化为"复制 + 删源"。
fn place_binary(artifact: &Path, dest: &Path, expected_sha: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    // 已存在同名文件：只要字节与指针一致就直接复用（重试同一版时的常态）。
    if dest.is_file() {
        if let Ok(actual) = crate::agent::download::sha256_hex_file(dest) {
            if crate::agent::download::verify_sha256(&actual, expected_sha) {
                let _ = std::fs::remove_file(artifact);
                return Ok(());
            }
        }
        std::fs::remove_file(dest)
            .map_err(|e| format!("cannot replace {}: {e}", dest.display()))?;
    }
    match std::fs::rename(artifact, dest) {
        Ok(()) => {}
        Err(_) => {
            // 跨文件系统（overlay 根与 .part 同目录时不会发生，但别假设）。
            std::fs::copy(artifact, dest)
                .map_err(|e| format!("cannot copy the artifact into {}: {e}", dest.display()))?;
            let _ = std::fs::remove_file(artifact);
        }
    }
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot mark {} private/executable: {e}", dest.display()))?;
    if let Some(dir) = dest.parent() {
        fsync_dir(dir);
    }
    // 落地复核：搬动之后字节必须仍是任务要的那一份。
    let actual = crate::agent::download::sha256_hex_file(dest)
        .map_err(|e| format!("cannot hash {}: {e}", dest.display()))?;
    if !crate::agent::download::verify_sha256(&actual, expected_sha) {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "the installed overlay binary does not match its digest (got {actual}, expected \
             {expected_sha})"
        ));
    }
    Ok(())
}

/// 只保留"当前版本 + 上一版本"，更早的版本文件删掉。
///
/// 为什么要留两代：新版本刚装上还没 `healthy` 时，上一代是唯一能手工回退的
/// 东西（`current.json` 指向哪里就执行哪里，改指针即可）。再早的版本没有任何
/// 用途，只是每次升级在用户目录里多堆几十 MB。
fn prune_versions(layout: &Layout, current: &Path, previous: &[PathBuf]) {
    let keep: Vec<PathBuf> = std::iter::once(current.to_path_buf())
        .chain(previous.iter().cloned())
        .collect();
    let Ok(entries) = std::fs::read_dir(layout.bin_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if keep.iter().any(|k| *k == path) {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(EXE_PREFIX)) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// 删除版本号**低于** `self_ver` 的 overlay 版本文件（含指针指向的那个不再被
/// 使用时）。用于"管理员手工把 base 换成了更新的版本"的场景：此时 overlay 里
/// 全是过时货，留着只会占空间。
pub fn prune_older_than(layout: &Layout, self_ver: Option<&semver::Version>) {
    let Some(self_ver) = self_ver else { return };
    let Ok(entries) = std::fs::read_dir(layout.bin_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(ver) = version_of_file_name(name) else {
            continue;
        };
        if ver < *self_ver {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// 从 `shell-remote-<版本>-<sha8>` 里取出并解析版本号。
fn version_of_file_name(name: &str) -> Option<semver::Version> {
    let rest = name.strip_prefix(EXE_PREFIX)?;
    // 后缀是 `-<8 位 hex>`：从右侧切掉最后一段。
    let (version, sha) = rest.rsplit_once('-')?;
    if sha.len() != 8 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    parse_version(version)
}

// ── 注册成功之后：标记健康 / 上报被拒 ───────────────────────────────

/// 注册成功之后调用（`run_session` 里紧接身份恢复回执的位置）。
///
/// 两种情况，都只做一件事：
///
/// - 本进程是 overlay 版本（`$SR_OVERLAY_FROM` 已设置）→ 指针写
///   `healthy=true, attempts=0`：这一版能起来、能注册，交给它的决定是对的。
///   之后它再也不受 `attempts` 上限约束。
/// - 本进程是 base，而指针已被 `rejected` → 返回一条 `agent:upgrade_progress`
///   帧，把"新版本没起来、已回落到 base"如实告诉 relay / 管理面板。只返回一次
///   （写 `rejected_reported`），否则设备每次重启都会重复刷同一条失败。
///
/// 返回的帧由调用方用既有的 `control_tx` 发出去：这里不持有发送端，避免把
/// 一次纯文件操作写成异步、也避免在注册成功的关键路径上 await 网络。
pub fn after_registration(session_id: &str) -> Option<String> {
    let layout = Layout::from_env().ok()?;
    let mut pointer = Pointer::read(&layout).ok().flatten()?;

    if non_empty_env(ENV_FROM).is_some() {
        // 只有指针指向的正是自己时才标健康：手工 `exec` 一个别的 overlay 文件
        // 不该把指针里的那一版洗白。
        let self_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok());
        if !self_exe.as_deref().is_some_and(|p| paths_equal(p, &pointer.path)) {
            return None;
        }
        pointer.healthy = true;
        pointer.attempts = 0;
        pointer.rejected = false;
        pointer.rejected_reported = false;
        match Pointer::write(&layout, &pointer) {
            Ok(()) => tracing::info!(
                version = %pointer.version,
                "the overlay version registered; marked healthy"
            ),
            Err(e) => tracing::warn!(error = %e, "cannot mark the overlay version healthy"),
        }
        return None;
    }

    if !pointer.rejected || pointer.rejected_reported {
        return None;
    }
    let version = pointer.version.clone();
    let payload = serde_json::json!({
        "stage": "failed",
        "version": version,
        "upgrade_id": pointer.upgrade_id,
        "error": format!(
            "the overlay version {version} did not register after {MAX_ATTEMPTS} attempts; this \
             device is back on the base binary"
        ),
        "detail": format!(
            "mode: overlay; overlay {version} was rejected, running the base binary"
        ),
    });
    let msg = crate::proto::Message {
        msg_type: "agent:upgrade_progress".to_string(),
        session_id: session_id.to_string(),
        payload,
    };
    pointer.rejected_reported = true;
    if let Err(e) = Pointer::write(&layout, &pointer) {
        tracing::warn!(error = %e, "cannot record that the overlay rejection was reported");
    }
    serde_json::to_string(&msg).ok()
}

#[cfg(test)]
mod tests;
