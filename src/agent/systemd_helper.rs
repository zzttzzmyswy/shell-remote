//! systemd 托管下的换机助手启动（MYS-2425-D）。
//!
//! ## 问题
//!
//! 旧 agent 用 `process_group(0)` 起换机助手，只改变了**进程组**，没有改变
//! **cgroup**。`shell-remote-agent.service` 用默认的 `KillMode=control-group`：
//! 主进程一退出，systemd 就把整个 service cgroup 里的进程一起杀掉——助手正好
//! 在里面。现场表现是助手日志停在 `ready announced; waiting for the old
//! process to exit`，磁盘始终是旧版，relay 收不到终结帧，管理页停在 `swapping`。
//!
//! `setsid` / `nohup` / `process_group(0)` 都救不了：它们与 cgroup 无关。
//!
//! ## 修法
//!
//! 通过 `systemd-run` 把同一个隐藏子命令 `update-apply` 起成一个**独立的
//! transient unit**，它有自己的一份 cgroup，因此不受旧 service 的 `KillMode`
//! 影响。与直接助手相比，差别只在**启动方式与进程生命周期管理**：交接文件路径
//! 之外的任何信息（Token、`--key`、原始 argv）一律不进 argv、不进 unit 属性、
//! 不进日志。
//!
//! 三条硬约束：
//!
//! 1. **不回退**。`systemd-run` 不存在 / 无权限 / 起不来 / 没等到 `ready` 时，
//!    必须回到「旧进程继续运行 + 磁盘不动 + 上报 `failed`」，绝不退回会被同
//!    cgroup 清掉的直接助手——那正是本 bug 本身。
//! 2. **不扩大影响面**。全程不 stop/restart 旧 service：现场那种按 PID cgroup
//!    清过子进程的 service 一旦被重启，会把它管着的用户 shell 一起换掉。本模块
//!    只操作**自己创建**的 unit（名字恒以 [`UNIT_PREFIX`] 开头）。
//! 3. **有界等待**。ready 等待与直接助手同源。
//!
//! ## 检测
//!
//! 只在**确实**跑在 systemd unit 里时才走这条路径：必须同时看到 `INVOCATION_ID`
//! （systemd 只给 unit 内进程注入）与 `/proc/self/cgroup` 里以 `.service` /
//! `.scope` 结尾的 unit 路径。普通 shell 里 `INVOCATION_ID` 可能被继承下来
//! （在服务的终端里手工跑 agent 就是这种情况），单看它不够。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::update_apply;

/// 起 transient unit 用的可执行文件名。只按名字查 PATH——不接受外部路径，
/// 避免"选哪个 systemd-run"变成可注入项。
const SYSTEMD_RUN: &str = "systemd-run";

/// transient helper 的 unit 名前缀。`stop_unit` 只认这个前缀的 unit，
/// 保证清理动作不可能落到别人的 unit（含旧 service）上。
pub const UNIT_PREFIX: &str = "shell-remote-upgrade-";

/// `systemd-run` 失败时保留的 stderr 上限。helper 整个升级周期的 stderr 都可能
/// 接到这条管道上（新 agent 的日志也会经过），不设上限会随升级时长无界增长。
const STDERR_KEEP_BYTES: usize = 8 * 1024;

/// 本次 agent 是否由 systemd unit 托管（即：主进程退出会带走同 cgroup 的一切）。
///
/// 两个条件必须同时成立，理由见模块文档「检测」。`cgroup` 由调用方传入
/// `/proc/self/cgroup` 的内容，便于单测覆盖各种取值。
pub fn is_managed_unit(invocation_id: Option<&str>, cgroup: Option<&str>) -> bool {
    let has_invocation = invocation_id.is_some_and(|v| !v.trim().is_empty());
    has_invocation && cgroup.is_some_and(cgroup_is_unit)
}

/// `/proc/self/cgroup` 的任一行是否指向一个 systemd **service** unit。
///
/// 只认 `.service`，不认 `.scope`，理由是机制而不是口味：只有 service 有
/// "主进程"，`KillMode=control-group` 的清理是由**主进程退出**触发的；scope
/// 没有主进程，某个成员退出不会导致 systemd 拆掉整个 scope。把登录会话
/// （`session-N.scope`）判成危险环境，只会在根本不存在本 bug 的场合把一个
/// 本来能成功的升级推去依赖 `systemd-run`。
///
/// 容器里 cgroup v2 常见取值 `0::/`、`0::/docker/<id>` 不以 unit 结尾，
/// 同样**不**判定为托管——容器里也没有可用的 `systemd-run` 目标，走直接助手
/// 才是对的。
fn cgroup_is_unit(raw: &str) -> bool {
    raw.lines().any(|line| {
        // 形如 `0::/system.slice/shell-remote-agent.service`，取最后一个 `:` 之后。
        let path = line.rsplit_once(':').map(|(_, p)| p).unwrap_or(line).trim();
        let last = path.rsplit('/').next().unwrap_or("");
        last.ends_with(".service")
    })
}

/// 读本进程的 systemd 托管情况。
pub fn current_is_managed() -> bool {
    let inv = std::env::var("INVOCATION_ID").ok();
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok();
    is_managed_unit(inv.as_deref(), cg.as_deref())
}

/// 本进程的 cgroup 路径（`/proc/self/cgroup` 的 v2 `0::` 行），只用于日志诊断。
pub fn current_cgroup_path() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    raw.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
}

/// transient helper 的 unit 名。
///
/// 带 `upgrade_id` 前缀是为了让同机并发的换机任务互不撞名——同时升级两台设备
/// （或一台设备连续两次升级）时，固定名字会让第二个 `systemd-run` 直接失败。
/// `upgrade_id` 是 relay 下发的随机串，这里只取字母数字并截断，保证 unit 名
/// 合法；它本身不是凭据。
pub fn helper_unit_name(upgrade_id: &str) -> String {
    let mut tag: String = upgrade_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect();
    if tag.is_empty() {
        // 极端情况（upgrade_id 全是非常规字符）：用 pid 保证唯一。
        tag = format!("pid{}", std::process::id());
    }
    format!("{UNIT_PREFIX}{tag}")
}

/// 起 transient helper 的方式：unit 名与命令行参数（不含任何秘密）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransientLaunch {
    pub unit: String,
    pub args: Vec<String>,
}

impl TransientLaunch {
    /// 构造 `systemd-run` 参数。
    ///
    /// 关键属性：
    ///
    /// - `--user`：走**当前用户**的用户级 manager。agent 常以普通账户运行，
    ///   系统级 `systemd-run` 要 polkit 授权（非交互环境直接 `Access denied`）；
    ///   用户级 manager 起自己的 unit 不需要授权。也因此 helper 天然保持原
    ///   agent 的运行身份与文件访问权限。
    /// - `--collect`：helper 结束后连 unit 一起收掉，不留 failed 残留。
    /// - `--property=KillMode=process`：helper 退出时**只**收掉它自己，不连带
    ///   清理它拉起的**新 agent**。默认的 `control-group` 会把新 agent 一起杀掉
    ///   ——那等于把本 bug 下移一层。
    /// - `--quiet`：不把 `Running as unit ...` 之类的提示混进 stdout；stdout 是
    ///   `ready` 的握手通道。
    ///
    /// **只有**交接文件路径进 argv。Token、`--key`、原始 agent 参数全留在那个
    /// `0600` 文件里；unit 属性（`systemctl show` 可读）一个字都不带。
    /// 构造参数。`user_scope = true` 走用户级 manager（普通账户的常态），
    /// `false` 走系统级（root / 没有用户会话的账户：`/run/user/<uid>` 不存在时
    /// `systemd-run --user` 会直接报 `Failed to connect to user scope bus`）。
    pub fn new(helper_exe: &Path, handoff: &Path, upgrade_id: &str, user_scope: bool) -> Self {
        let unit = helper_unit_name(upgrade_id);
        let mut args: Vec<String> = Vec::new();
        if user_scope {
            args.push("--user".to_string());
        }
        args.extend([
            format!("--unit={unit}"),
            "--collect".to_string(),
            "--quiet".to_string(),
            "--property=KillMode=process".to_string(),
            "--pipe".to_string(),
            helper_exe.display().to_string(),
            "update-apply".to_string(),
            handoff.display().to_string(),
        ]);
        Self { args, unit }
    }

    /// 供日志/测试断言用的命令行表示。生产路径不需要它，但把它做成函数是为了
    /// 让「没有秘密进 argv」这条纪律有一个可执行、可测试的定义。
    pub fn command_line(&self) -> String {
        format!("{SYSTEMD_RUN} {}", self.args.join(" "))
    }
}

/// 启动结果。
pub enum TransientHelper {
    /// `systemd-run` 已确认 helper 就绪。
    Ready(Child),
    /// 起不来。旧 agent 必须据此上报 `failed` 并继续运行；**不得**回退直接助手。
    Failed(String),
}

/// `systemd-run` 的定位方式：允许测试注入一个假的实现。
#[derive(Debug, Clone)]
pub enum RunTarget {
    /// 按 PATH 找 `systemd-run`（生产路径）。
    Path,
    /// 用指定可执行文件代替 `systemd-run`（测试：模拟不存在 / 失败 / 超时）。
    Explicit(std::path::PathBuf),
}

impl RunTarget {
    fn program(&self) -> std::path::PathBuf {
        match self {
            Self::Path => std::path::PathBuf::from(SYSTEMD_RUN),
            Self::Explicit(p) => p.clone(),
        }
    }
}

/// 当前账户有没有可用的**用户级** systemd manager。
///
/// 判据是 `/run/user/<uid>` 与其中的 bus socket 是否真的存在，而不是"看起来像
/// root"：root 常常没有 `user@0.service`（实测 `systemd-run --user` 报
/// `Failed to connect to user scope bus: No such file or directory`），而普通
/// 登录账户一定有。不可用时回退到**系统级** transient unit（root 天然被授权）。
pub fn user_manager_available() -> bool {
    #[cfg(unix)]
    {
        let uid = unsafe { libc::geteuid() };
        let dir = format!("/run/user/{uid}");
        if !Path::new(&dir).is_dir() {
            return false;
        }
        Path::new(&format!("{dir}/bus")).exists()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// 用户级 manager 的位置由这两个环境变量决定。systemd 给 service 注入的是
/// `INVOCATION_ID` 之类的 unit 变量，**不**包含用户会话的 `XDG_RUNTIME_DIR` /
/// `DBUS_SESSION_BUS_ADDRESS`；缺了它们 `systemd-run --user` 直接报
/// "Failed to connect to user scope bus"。两者都能从 uid 推出，不是秘密，
/// 也不需要复制整个环境（设计约束：必要环境有界传递）。
#[cfg(unix)]
fn apply_user_manager_env(cmd: &mut Command) {
    let uid = unsafe { libc::geteuid() };
    cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
    cmd.env(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path=/run/user/{uid}/bus"),
    );
}

#[cfg(not(unix))]
fn apply_user_manager_env(_cmd: &mut Command) {}

/// 通过 `systemd-run` 起 transient helper，并等到它 `ready`。
///
/// 返回 `Ready(child)` 之后调用方才能退出——与直接助手是同一条不变式：**只有
/// helper 真的接手了，旧进程才离开**。
pub fn launch(
    helper_exe: &Path,
    handoff: &Path,
    upgrade_id: &str,
    timeout: Duration,
) -> TransientHelper {
    launch_with(RunTarget::Path, helper_exe, handoff, upgrade_id, timeout)
}

/// [`launch`] 的可注入版本（测试用）。
pub fn launch_with(
    target: RunTarget,
    helper_exe: &Path,
    handoff: &Path,
    upgrade_id: &str,
    timeout: Duration,
) -> TransientHelper {
    let plan = TransientLaunch::new(helper_exe, handoff, upgrade_id, user_manager_available());
    let mut cmd = Command::new(target.program());
    cmd.args(&plan.args)
        .stdin(Stdio::null())
        // stdout 是握手通道：`--pipe` 把 helper 的 stdout 直接接到这里。
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_user_manager_env(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return TransientHelper::Failed(format!(
                "systemd-run is not available ({e}); a helper started the usual way would be \
                 killed together with this service's cgroup, so the upgrade was not started"
            ))
        }
    };
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return TransientHelper::Failed(
                "systemd-run gave no stdout pipe for the helper handshake".to_string(),
            );
        }
    };

    // stderr 单独排空：`systemd-run` 失败时把原因写在这里，而 stdout 只会是空的
    // ——不读它就只能报一句没用的 "timed out"。保留量有界（见 STDERR_KEEP_BYTES）。
    //
    // **同时转发到本进程的 stderr**：接管前那几行是 `systemd-run` 自己的报错，
    // 接管后则是助手与新 agent 的启动日志（migration 期间也走这条管道——现场
    // 排查全靠它们）。只收不发会让这些日志凭空消失：服务托管下 agent 的 stderr
    // 就是 journal，运维看不到新版本到底起没起来。
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    if let Some(se) = child.stderr.take() {
        let tail = stderr_tail.clone();
        let _ = std::thread::Builder::new()
            .name("sr-systemd-run-stderr".into())
            .spawn(move || {
                use std::io::{BufRead as _, Write as _};
                let mut reader = std::io::BufReader::new(se);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Ok(mut t) = tail.lock() {
                        if t.len() < STDERR_KEEP_BYTES {
                            t.push_str(line.trim_end());
                            t.push('\n');
                        }
                    }
                    // 帮手的 stderr 可能已经断了（旧 agent 被清理）：写失败忽略。
                    let _ = std::io::stderr().write_all(line.as_bytes());
                    line.clear();
                }
            });
    }

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let _ = std::thread::Builder::new()
        .name("sr-transient-ready".into())
        .spawn(move || {
            use std::io::BufRead as _;
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            if reader.read_line(&mut line).ok().is_some_and(|n| n > 0) {
                let _ = tx.send(line.trim_end().to_string());
            }
            // 与直接助手一致：helper 之后不再写 stdout，读线程到此结束。
        });

    match rx.recv_timeout(timeout) {
        Ok(first) if first == update_apply::READY_LINE => TransientHelper::Ready(child),
        Ok(first) => {
            let _ = child.kill();
            let _ = child.wait();
            stop_unit(&plan.unit);
            // 去掉 `error: ` 前缀，与直接助手的错误文案保持同一读法。
            let reason = first
                .strip_prefix(update_apply::ERROR_PREFIX)
                .unwrap_or(&first)
                .trim()
                .to_string();
            TransientHelper::Failed(if reason.is_empty() {
                "systemd-run started a helper that exited without becoming ready".to_string()
            } else {
                reason
            })
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            // 给 stderr 排空线程一点时间把 systemd-run 的报错取回来。
            std::thread::sleep(Duration::from_millis(80));
            let detail = stderr_tail
                .lock()
                .map(|t| t.trim().to_string())
                .unwrap_or_default();
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            // 关键：helper 可能还活着并在等旧 PID。旧 agent 不打算退出了，必须
            // 把**我们自己**这个 unit 停掉，否则它会在别处继续动磁盘。
            stop_unit(&plan.unit);
            TransientHelper::Failed(format!(
                "systemd-run did not report the helper ready within {timeout:?}{detail}"
            ))
        }
    }
}

/// 停掉**我们自己创建**的 transient unit。
///
/// 只对 [`UNIT_PREFIX`] 开头的名字生效，因此绝不可能落到旧 service 或别人的
/// unit 上（设计约束：不扩大影响面）。调用点是失败收尾——此时 helper 还停在
/// "等旧 PID"之前，磁盘没动。
pub fn stop_unit(unit: &str) {
    if !unit.starts_with(UNIT_PREFIX) {
        debug_assert!(false, "refusing to stop a unit we did not create: {unit}");
        return;
    }
    #[cfg(unix)]
    {
        let mut cmd = Command::new("systemctl");
        cmd.args(["--user", "stop", unit])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_user_manager_env(&mut cmd);
        let _ = cmd.status();
    }
    #[cfg(not(unix))]
    let _ = unit;
}

// ── unit 的 Restart= 策略：决定"谁负责拉起新 agent" ────────────────

/// 读 unit 当前的 `MainPID`（读不到 / 已无主进程返回 `None`）。
///
/// **只有旧 agent 仍在运行时读才有意义**：换机窗口内 systemd 会把它置 `0`。
/// 本方案的检查因此全部安排在磁盘改动之前。
pub fn unit_main_pid(unit: &str) -> Option<u32> {
    #[cfg(unix)]
    {
        let mut cmd = Command::new("systemctl");
        cmd.args(["show", "-p", "MainPID", "--value", unit])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        apply_user_manager_env(&mut cmd);
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        let v = String::from_utf8_lossy(&out.stdout);
        match v.trim().parse::<u32>() {
            Ok(pid) if pid > 0 => Some(pid),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = unit;
        None
    }
}

/// 读某个 unit 的 `Restart=` 策略（`always` / `on-failure` / `no` / …）。
///
/// 非 root 也能读（`systemctl show` 的属性读权限不受 polkit 限制，实测
/// `systemctl show -p Restart --value <unit>` 可直接拿到 `always`）。
///
/// 读不到时返回 `None`——调用方必须把它当作"**不知道**"，而不是"不会重启"：
/// 知道不清楚时宁可自己拉起新 agent（设备不失联优先），也不要赌 systemd 会补。
pub fn service_restart_policy(unit: &str) -> Option<String> {
    #[cfg(unix)]
    {
        let mut cmd = Command::new("systemctl");
        cmd.args(["show", "-p", "Restart", "--value", unit])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        apply_user_manager_env(&mut cmd);
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = unit;
        None
    }
}

/// `Restart=` 是否会让 systemd 在**正常退出**后补起一个实例。
///
/// 只有 `always` 会。`on-failure` / `on-abnormal` / `no` 都不会——它们只对
/// 异常退出生效，而换机交接是 `exit 0`。
pub fn restart_policy_respawns_on_success(policy: &str) -> bool {
    policy.trim().eq_ignore_ascii_case("always")
}

/// 本进程所在 service cgroup 里，除了自己还有没有别的 agent。
///
/// 判据与 [`is_service_side_duplicate`] 同源（同 `exe`、同 cgroup），但不要求
/// "无恢复意图"：这里只问"是不是还有同伴"。读不到 `/proc` 时返回 `false`
/// （当作"只有自己"）。
pub fn alone_in_service_cgroup(service_cgroup: &str) -> bool {
    find_agent_in_cgroup(service_cgroup, None).is_none()
}

/// 在给定 cgroup 里找 agent 进程（`exclude` 可排除某个 pid）。
fn find_agent_in_cgroup(service_cgroup: &str, exclude: Option<u32>) -> Option<u32> {
    let me = std::process::id();
    let my_exe = std::fs::read_link("/proc/self/exe").ok()?;
    let entries = std::fs::read_dir("/proc").ok()?;
    entries.flatten().find_map(|entry| {
        let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
        if pid == me || Some(pid) == exclude {
            return None;
        }
        if std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .as_deref()
            != Some(my_exe.as_path())
        {
            return None;
        }
        (cgroup_path_of_pid(pid).as_deref() == Some(service_cgroup)).then_some(pid)
    })
}

// ── 换机前的协调能力检查（MYS-2425-D：fail-closed）────────────────

/// 换机前对 systemd 托管环境的判定结果。
///
/// ## 为什么必须在**换机前**判定
///
/// 旧版尝试过"先让旧进程退出，再指望 systemd 自动补起新版"：这条路径要求
/// 旧 PID 已经消失才能判定，而 systemd 在 `RestartSec` 窗口内把 `MainPID` 置
/// `0`，判定永远拿不到答案；自动补起也无法可靠继承一次性身份恢复上下文，
/// 注册失败时的回滚边界不清。设计决定因此改为：**在磁盘动之前**把"能不能
/// 协调"问清楚，能就做，不能就当场失败。
///
/// ## 三条结论
///
/// - [`ManagedPlan::Direct`]：不是 systemd service 托管，走既有直接助手路径。
/// - [`ManagedPlan::Coordinated`]：归属明确的单 agent service，且当前身份**已**
///   被授权管理该 unit 的生命周期 ⇒ 由助手执行 stop → swap → start。
/// - [`ManagedPlan::Refuse`]：其它一切情况。**在旧 agent 退出前**上报 `failed`
///   并给出可操作的解决方法，磁盘一点不动，也不产生第二个实例。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedPlan {
    /// 非 systemd service 托管：既有路径，行为不变。
    Direct,
    /// 可以协调：助手负责 stop/swap/start，新 agent 回到唯一受管 MainPID。
    Coordinated { unit: String },
    /// 拒绝协调。`reason` 是给运维看的可读原因，直接进 Web。
    Refuse {
        unit: Option<String>,
        reason: String,
    },
}

/// 判定所需的外部事实。全部由 [`probe`] 采集，测试可直接构造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// 自身是否由 systemd service 托管（`INVOCATION_ID` + cgroup 落在 `*.service`）。
    pub managed: bool,
    /// 当前 cgroup 对应的 unit 名（读不到时为 `None`）。
    pub unit: Option<String>,
    /// `systemctl show -p MainPID` 的值。`None` = 读不到。
    ///
    /// 注意：**只有在旧 agent 仍在运行时读才有意义**，这正是本检查放在换机
    /// 之前的原因；换机窗口内 systemd 会把它置 0。
    pub main_pid: Option<u32>,
    /// 当前 agent 自己的 PID。
    pub self_pid: u32,
    /// unit 的 `Restart=` 策略（读不到时为 `None`）。
    pub restart_policy: Option<String>,
    /// unit 的 `ExecStart` 首个可执行文件（`systemctl show -p ExecStart`）。
    pub exec_start_exe: Option<String>,
    /// 本进程的可执行文件（`/proc/self/exe`）。
    pub self_exe: Option<String>,
    /// 同一 cgroup 里是否还有**别的** agent（同 exe）。
    pub shares_cgroup_with_other_agent: bool,
    /// 当前身份是否**已**被授权管理该 unit 的生命周期。
    ///
    /// 由 `pkcheck --action-id org.freedesktop.systemd1.manage-units` 判定：
    /// `polkit.result=yes` 才是已授权；`auth_admin*` 表示需要交互认证（即
    /// 非交互的 agent 做不了）。**不探测、不提权、不改 sudoers。**
    pub lifecycle_authorized: bool,
}

/// 纯判定：把 [`Probe`] 变成 [`ManagedPlan`]。不做任何 IO，便于穷举测试。
pub fn decide(probe: &Probe) -> ManagedPlan {
    // 非托管：既有直接助手路径，行为与本改动之前完全一致。
    if !probe.managed {
        return ManagedPlan::Direct;
    }
    let Some(unit) = probe.unit.as_deref().filter(|u| u.ends_with(".service")) else {
        return ManagedPlan::Refuse {
            unit: None,
            reason: REFUSE_CAUSE_UNIT_UNKNOWN.to_string(),
        };
    };

    // 归属必须明确：MainPID 要等于自己。不匹配说明这个 unit 的主进程不是本
    // agent（多 agent 共享 daemon、或 unit 里套了 wrapper），systemd 的重启
    // 语义对不上这台设备，不能猜。
    match probe.main_pid {
        Some(pid) if pid == probe.self_pid => {}
        Some(pid) => {
            return ManagedPlan::Refuse {
                unit: Some(unit.to_string()),
                reason: format!(
                    "{REFUSE_CAUSE_NOT_MAIN_PID} (unit {unit} has MainPID {pid}, this agent is \
                     pid {}). This usually means several agents share one service.",
                    probe.self_pid
                ),
            }
        }
        None => {
            return ManagedPlan::Refuse {
                unit: Some(unit.to_string()),
                reason: format!("{REFUSE_CAUSE_MAIN_PID_UNREADABLE} (unit {unit})"),
            }
        }
    }

    // 单 agent：同 cgroup 里还有别的 agent 时，stop 会连带影响它们。
    if probe.shares_cgroup_with_other_agent {
        return ManagedPlan::Refuse {
            unit: Some(unit.to_string()),
            reason: format!(
                "{REFUSE_CAUSE_SHARED_CGROUP} (unit {unit}). Stopping it would also stop the other \
                 agents it hosts."
            ),
        };
    }

    // 形态相符：ExecStart 指的就是这份二进制。
    //
    // 只接受"直接 exec 本二进制"。**不**接受 wrapper 脚本（`run.sh` 再 exec
    // agent）：那种形态下 stop/start 能不能生效取决于脚本内容，我们读不到，
    // 不能猜——猜错就是"换了文件却起不来"或"起了别的东西"。
    match (&probe.exec_start_exe, &probe.self_exe) {
        (Some(exec), Some(me)) if same_path(exec, me) => {}
        (Some(exec), Some(me)) => {
            return ManagedPlan::Refuse {
                unit: Some(unit.to_string()),
                reason: format!(
                    "{REFUSE_CAUSE_EXECSTART_MISMATCH} (unit {unit} starts `{exec}`, this agent                      runs `{me}`). Coordinated upgrades need `ExecStart` to exec this binary                      directly."
                ),
            }
        }
        _ => {
            return ManagedPlan::Refuse {
                unit: Some(unit.to_string()),
                reason: format!("{REFUSE_CAUSE_EXECSTART_UNREADABLE} (unit {unit})"),
            }
        }
    }

    // Restart 策略决定"能不能直接走既有路径"：
    //
    // - `always`：旧 agent 为换机正常退出后 systemd 会补起一个，必须由助手接管
    //   服务生命周期（stop 取消待执行重启 → 换盘 → start 拿回唯一 MainPID）。
    // - **其它策略**（`on-failure` / `on-abnormal` / `no` / …）：systemd **不会**
    //   因正常退出补起，既有直接助手路径天然只有一个实例 ⇒ 不需要协调，直接
    //   走 Direct。把它判成"拒绝"是错的：那条路径本来就能升级。
    // - 读不到：无法确认"停掉之后会不会有别的东西复活"，也不确认"不协调是否
    //   安全" ⇒ 拒绝（fail-closed）。
    match probe.restart_policy.as_deref() {
        Some(p) if restart_policy_respawns_on_success(p) => {}
        Some(_) => return ManagedPlan::Direct,
        None => {
            return ManagedPlan::Refuse {
                unit: Some(unit.to_string()),
                reason: format!("{REFUSE_CAUSE_RESTART_POLICY_UNREADABLE} (unit {unit})"),
            }
        }
    }

    // 授权：只在**已经**授权时协调。需要交互认证 = 做不到，不偷偷提权。
    if !probe.lifecycle_authorized {
        return ManagedPlan::Refuse {
            unit: Some(unit.to_string()),
            reason: format!("{REFUSE_CAUSE_NOT_AUTHORIZED} (unit {unit})"),
        };
    }

    ManagedPlan::Coordinated {
        unit: unit.to_string(),
    }
}

/// 拒绝原因的前半句：稳定的可断言前缀，后面接具体情况。
pub const REFUSE_CAUSE_UNIT_UNKNOWN: &str = "cannot determine which systemd service hosts this \
                                              agent, so the upgrade cannot coordinate it";
pub const REFUSE_CAUSE_NOT_MAIN_PID: &str = "this agent is not the service's main process";
pub const REFUSE_CAUSE_MAIN_PID_UNREADABLE: &str = "cannot read the service's main process";
pub const REFUSE_CAUSE_SHARED_CGROUP: &str = "this service hosts more than one agent";
pub const REFUSE_CAUSE_EXECSTART_MISMATCH: &str =
    "the service starts a different binary than the one running";
pub const REFUSE_CAUSE_EXECSTART_UNREADABLE: &str = "cannot read the service's ExecStart";
pub const REFUSE_CAUSE_RESTART_POLICY: &str = "the service is not configured with Restart=always";
pub const REFUSE_CAUSE_RESTART_POLICY_UNREADABLE: &str = "cannot read the service's Restart policy";
pub const REFUSE_CAUSE_NOT_AUTHORIZED: &str =
    "this agent is not authorized to manage the service lifecycle";

/// 路径比较：解析符号链接后相等即可（同一份二进制换个写法也算相符）。
fn same_path(a: &str, b: &str) -> bool {
    let ra = std::fs::canonicalize(a).unwrap_or_else(|_| PathBuf::from(a));
    let rb = std::fs::canonicalize(b).unwrap_or_else(|_| PathBuf::from(b));
    ra == rb
}

// ── 采集真实事实 & 执行协调 ──────────────────────────────────────

/// 采集判定所需的事实（只读，无副作用）。
///
/// 全部通过 `systemctl show` / `/proc` / `pkcheck` 读取；**不**停启任何 unit、
/// **不**探测性提权。任何一项读不到都如实记为 `None`，交由 [`decide`] 拒绝。
pub fn probe() -> Probe {
    #[cfg(unix)]
    let self_pid = std::process::id();
    #[cfg(not(unix))]
    let self_pid = std::process::id();

    let self_exe = std::fs::read_link("/proc/self/exe")
        .ok()
        .map(|p| p.display().to_string());

    #[cfg(target_os = "linux")]
    let (managed, unit) = {
        let managed = current_is_managed();
        let unit = current_cgroup_path().map(|cg| service_unit_name(&cg));
        // 只有 service 才谈得上"主进程/重启策略"。
        let unit = unit.filter(|u| u.ends_with(".service"));
        (managed, unit)
    };
    #[cfg(not(target_os = "linux"))]
    let (managed, unit) = (false, None);

    let main_pid = unit.as_deref().and_then(unit_main_pid);
    let restart_policy = unit.as_deref().and_then(service_restart_policy);
    let exec_start_exe = unit.as_deref().and_then(service_exec_start_exe);
    let shares_cgroup_with_other_agent = match (managed, unit.as_deref()) {
        (true, Some(u)) => {
            let cg = unit_cgroup_path(u);
            cg.as_deref()
                .map(|c| !alone_in_service_cgroup(c))
                .unwrap_or(false)
        }
        _ => false,
    };
    let lifecycle_authorized = unit.as_deref().is_some_and(lifecycle_authorized);

    Probe {
        managed,
        unit,
        main_pid,
        self_pid,
        restart_policy,
        exec_start_exe,
        self_exe,
        shares_cgroup_with_other_agent,
        lifecycle_authorized,
    }
}

/// unit 的 cgroup 路径（按 unit 名推导，v2 直接在 `/sys/fs/cgroup` 下）。
fn unit_cgroup_path(unit: &str) -> Option<String> {
    // 自身 cgroup 已包含完整路径；优先用它（同一 unit 必然一致）。
    if let Some(cg) = current_cgroup_path() {
        if service_unit_name(&cg) == unit {
            return Some(cg);
        }
    }
    None
}

/// 读 unit 的 `ExecStart` 首个可执行文件路径。
///
/// `systemctl show -p ExecStart` 的输出形如：
/// `{ path=/usr/local/bin/shell-remote ; argv[]=/usr/local/bin/shell-remote agent ... }`
fn service_exec_start_exe(unit: &str) -> Option<String> {
    #[cfg(unix)]
    {
        let mut cmd = Command::new("systemctl");
        cmd.args(["show", "-p", "ExecStart", "--value", unit])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        apply_user_manager_env(&mut cmd);
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let at = text.find("path=")?;
        let rest = &text[at + "path=".len()..];
        let end = rest.find(" ;").or_else(|| rest.find(';'))?;
        let p = rest[..end].trim();
        if p.is_empty() {
            None
        } else {
            Some(p.to_string())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = unit;
        None
    }
}

/// 当前身份是否**已经**被授权管理 unit 生命周期（`systemd1.manage-units`）。
///
/// 用 polkit 官方探针 `pkcheck`：**只查询、不尝试提权**。判定规则：
///
/// - `polkit.result=yes` ⇒ 已授权（例如管理员配了 polkit 规则或 agent 以 root 跑）；
/// - `auth_admin` / `auth_admin_keep` / `auth_self*` ⇒ 需要交互认证，非交互的
///   agent 做不到 ⇒ 未授权；
/// - `no` ⇒ 明确拒绝；
/// - `pkcheck` 不存在或输出无法解析 ⇒ 未授权（fail-closed）。
///
/// 这与设计决定一致：授权只走系统既有机制，不提权、不改 sudoers/unit。
pub fn lifecycle_authorized(unit: &str) -> bool {
    let _ = unit; // 结论与具体 unit 无关：polkit 按 action + 调用者身份判定。
    #[cfg(unix)]
    {
        let mut cmd = Command::new("pkcheck");
        cmd.args([
            "--action-id",
            "org.freedesktop.systemd1.manage-units",
            "--process",
            &std::process::id().to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let Ok(out) = cmd.output() else { return false };
        // 输出形如 `polkit\56result=yes`（`\56` 是 `.` 的转义），也可能直接是
        // `authorized`。两种都认。
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        if out.status.success() {
            // pkcheck 退出码 0 = 已授权。
            return true;
        }
        text.contains("result=yes") || text.contains("authorized")
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// 对一个已确认可协调的 unit 执行受控 stop。
///
/// 前置条件（由 [`decide`] 保证）：单 agent、自己是 MainPID、ExecStart 相符、
/// `Restart=always`、**已授权**。语义：
///
/// - `systemctl stop` 会取消 `Restart=always` 的待执行重启（这是选它而不是
///   "杀进程等补起"的原因）；
/// - 必须在**旧 agent 退出之前**调用，且 helper 自身**不在**该 service 的
///   cgroup 里（helper 跑在独立 transient unit，见 [`launch`] 的
///   `KillMode=process`），否则 stop 会把 helper 一起清掉。
pub fn stop_unit_checked(unit: &str) -> Result<(), String> {
    if !unit.starts_with(UNIT_PREFIX) && !unit.ends_with(".service") {
        return Err(format!("refusing to stop a non-service unit: {unit}"));
    }
    run_systemctl(&["stop", unit]).map_err(|e| format!("cannot stop {unit}: {e}"))
}

/// 对一个已确认可协调的 unit 执行 start（把新 agent 拉回唯一受管 MainPID）。
pub fn start_unit_checked(unit: &str) -> Result<(), String> {
    if !unit.starts_with(UNIT_PREFIX) && !unit.ends_with(".service") {
        return Err(format!("refusing to start a non-service unit: {unit}"));
    }
    run_systemctl(&["start", unit]).map_err(|e| format!("cannot start {unit}: {e}"))
}

/// 跑一条 `systemctl` 子命令，失败时把 stderr 带回来。
fn run_systemctl(args: &[&str]) -> Result<(), String> {
    #[cfg(unix)]
    {
        let mut cmd = Command::new("systemctl");
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_user_manager_env(&mut cmd);
        let out = cmd.output().map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("systemctl {} failed", args.join(" "))
        } else {
            err
        })
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err("systemctl is not available".to_string())
    }
}

/// 等 unit 的 cgroup 里不再有进程（stop 之后确认残留已退出）。
///
/// 有界等待：`systemctl stop` 正常是同步的，但 cgroup 排空可能滞后几毫秒。
pub fn wait_unit_cgroup_empty(unit: &str, timeout: Duration) -> bool {
    let Some(cg) = unit_cgroup_path(unit) else {
        return true; // 拿不到 cgroup 路径时不阻塞流程
    };
    let path = format!("/sys/fs/cgroup{cg}/cgroup.procs");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                if text.split_whitespace().next().is_none() {
                    return true;
                }
            }
            // 路径不存在 = cgroup 已经没了，同样算空。
            Err(_) => return true,
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── 重复进程识别（MYS-2425-D）──────────────────────────────────────

/// 判定**另一个**进程是否是 systemd 按 `Restart=always` 补起的重复实例。
///
/// 判据（全部是可验证的系统事实，不靠进程名）：
///
/// 1. 与换机前的旧 agent 同处一个 service cgroup —— cgroup 归属由 systemd
///    决定，进程自己改不了；
/// 2. **没有**换机恢复意图（`SR_UPGRADE_RESTORE` / `--upgrade-restore`）——
///    只有换机助手会设它，普通启动没有；
/// 3. 与本进程是**同一份二进制**（`/proc/<pid>/exe` 指向同一路径）。
///
/// 助手拉起的新进程在 helper 的 transient unit 里（不是 service）且带恢复
/// 意图，因此永远不会被判成重复实例——这正是它必须活下来的那个。
///
/// 读不到进程信息（跨用户 / 权限收紧 / 已退出）时一律**不**认定。
pub fn is_service_side_duplicate(pid: u32, service_cgroup: &str) -> bool {
    if pid == std::process::id() {
        return false;
    }
    let Ok(my_exe) = std::fs::read_link("/proc/self/exe") else {
        return false;
    };
    if std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .as_deref()
        != Some(my_exe.as_path())
    {
        return false;
    }
    if cgroup_path_of_pid(pid).as_deref() != Some(service_cgroup) {
        return false;
    }
    // 读不到 environ 时不认定：宁可漏报，也不把助手拉起的新进程说成重复。
    let Some(env) = std::fs::read(format!("/proc/{pid}/environ")).ok() else {
        return false;
    };
    !env.split(|b| *b == 0).any(|kv| {
        kv.starts_with(update_apply::RESTORE_ENV.as_bytes())
            && kv.len() > update_apply::RESTORE_ENV.len()
    })
}

/// 解析 `/proc/<pid>/cgroup` 里的 v2 路径（`0::/...`）。
pub fn cgroup_path_of_pid(pid: u32) -> Option<String> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    raw.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
}

/// 在本 service 的 cgroup 里找"另一个 agent 进程"（systemd 补起的那个）。
///
/// 判据全部委托给 [`is_service_side_duplicate`]——生产路径与单测走**同一份**
/// 判定逻辑，不给"测过的和实际跑的不是一回事"留缝。
pub fn find_service_side_agent(service_cgroup: &str) -> Option<u32> {
    let me = std::process::id();
    let entries = std::fs::read_dir("/proc").ok()?;
    entries.flatten().find_map(|entry| {
        let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
        if pid == me {
            return None;
        }
        is_service_side_duplicate(pid, service_cgroup).then_some(pid)
    })
}

/// 读进程启动时刻（`/proc/<pid>/stat` 的 starttime），用于防止 PID 复用后
/// 误杀一个无关进程。读不到返回 `None`（调用方据此放弃终止）。
pub fn pid_start_stamp(pid: u32) -> Option<u64> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm 里可能有空格和括号：从最后一个 ')' 之后开始切字段。
    let rest = raw.rsplit_once(')')?.1;
    // 第 22 个字段是 starttime；`)` 之后的第一项是第 3 个字段，所以取第 20 项。
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// 交接后识别 systemd 按 `Restart=always` 补起的重复实例，并给出运维可执行的结论。
///
/// ## 为什么需要
///
/// 旧 agent 因为"要换机"而退出，systemd 看到主进程没了就按 `Restart=always`
/// 再拉起一个——它跑的是刚换上的**新**版本，于是和新进程并存：两个 agent、
/// 同一个 `session_id`。这是设计里点名的"避免新旧无限互相驱逐"要防的形状。
///
/// ## 判据
///
/// 见 [`is_service_side_duplicate`]：**同 service cgroup + 无恢复意图**。
/// 这两个条件都由系统保证、进程自己伪造不了——助手拉起的新进程在 helper 的
/// transient unit 里，且带 `SR_UPGRADE_RESTORE`。
///
/// ## 为什么只识别不终止
///
/// 实测过两种自动处置，都不可靠：
///
/// - **SIGTERM 那个 pid**：`Restart=always` 会立刻再补一个，形成我们主动发起的
///   杀掉—重启循环。实测 `NRestarts` 因此从 0 涨到 3，等于亲手把设备推进
///   "反复重启"的状态。
/// - **`systemctl stop <unit>` 取消后续重启**：语义是对的（stop 会清掉待执行的
///   重启），但 agent 现场是**非 root 运行**的（unit 里 `User=sr-agent`），
///   实测被 polkit 拒绝（`Access denied ... interactive authentication`）。
///
/// 而设计明确要求"不扩大影响面、不做无限驱逐"。所以这里只做**有界观察 +
/// 精确报告**：把确切的 unit 名与可执行的修复命令交给运维，不擅自对别人的
/// unit 动手。运维侧的根治是让 service 不再在 agent 主动退出时重启
/// （`Restart=on-failure`，或换机后 `systemctl stop`），见 docs/upgrade.md。
pub fn settle_service_duplicate(state: &Arc<SettleState>) -> SettleOutcome {
    settle_service_duplicate_with(state, SYSTEMD_RESTART_KILL_WINDOW)
}

/// [`settle_service_duplicate`] 的可注入窗口版本（测试用，避免真等 20s）。
pub fn settle_service_duplicate_with(state: &Arc<SettleState>, window: Duration) -> SettleOutcome {
    let Some(service_cg) = state.service_cgroup.as_deref() else {
        return SettleOutcome::NotApplicable;
    };
    let deadline = std::time::Instant::now() + window;
    // 观察整个窗口：systemd 是隔 `RestartSec` 才补起的，交接完成那一刻它往往
    // 还不存在——只看一眼等于什么都没看到（实测就是这样漏掉的）。
    loop {
        if find_service_side_agent(service_cg).is_some() {
            return SettleOutcome::DuplicatePresent {
                unit: service_unit_name(service_cg),
            };
        }
        if std::time::Instant::now() >= deadline {
            return SettleOutcome::Settled;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// 从 service 的 cgroup 路径取 unit 名（`/system.slice/<name>.service` → `<name>.service`）。
///
/// 只用来拼一条给运维看的命令；取不出来时返回整个路径，不做猜测。
pub fn service_unit_name(cgroup_path: &str) -> String {
    cgroup_path
        .trim()
        .rsplit('/')
        .next()
        .filter(|s| s.ends_with(".service"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| cgroup_path.trim().to_string())
}

/// 观察 systemd 补起新实例的窗口。
///
/// 必须**覆盖若干个** `RestartSec`（模板默认 5s、现场 10s）：交接完成的那一刻
/// systemd 通常还没补起，只看一眼等于什么都没看到（实测正是如此）。
pub const SYSTEMD_RESTART_KILL_WINDOW: Duration = Duration::from_secs(20);

/// [`settle_service_duplicate`] 的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleOutcome {
    /// 窗口内没有出现重复实例——这是正常结果。
    Settled,
    /// 窗口内出现了 systemd 按 `Restart=always` 补起的重复实例。
    ///
    /// `unit` 是那个 service 的 unit 名，调用方据此给出可执行的运维命令。
    DuplicatePresent { unit: String },
    /// 不是 systemd 托管场景，或没有可供比对 cgroup：什么都不做。
    NotApplicable,
}

/// 观察动作需要的输入。
pub struct SettleState {
    /// 旧 agent 所在的 service cgroup 路径（从交接上下文继承，不是现读的）。
    pub service_cgroup: Option<String>,
}

impl SettleState {
    pub fn new(service_cgroup: Option<String>) -> Arc<Self> {
        Arc::new(Self { service_cgroup })
    }
}

#[cfg(test)]
mod tests;
