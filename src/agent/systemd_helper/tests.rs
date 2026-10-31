//! systemd 换机启动的测试（MYS-2425-D）。
//!
//! 分两层：
//!
//! 1. **纯逻辑**：托管判定、unit 命名、`systemd-run` 参数。不许出现任何凭据。
//! 2. **进程级**：用假 `systemd-run` 覆盖「不存在」「启动失败」「不报 ready」
//!    三条失败路径，断言失败原因可读且**不**回退到直接助手。

use super::*;
use std::path::PathBuf;

// ── 1. 托管判定 ─────────────────────────────────────────────────────

/// 现场真实取值：systemd service 里的 agent。
#[test]
fn test_service_cgroup_with_invocation_id_is_managed() {
    assert!(is_managed_unit(
        Some("0f5c1a2b3d4e5f60718293a4b5c6d7e8"),
        Some("0::/system.slice/shell-remote-agent.service\n")
    ));
}

/// 普通 shell 里手工跑 agent：`INVOCATION_ID` 什么都没有。
#[test]
fn test_plain_shell_is_not_managed() {
    assert!(!is_managed_unit(
        None,
        Some("0::/user.slice/user-1000.slice/session-3.scope\n")
    ));
}

/// **关键回归**：在服务的终端里手工跑 agent 会继承 `INVOCATION_ID`，但自己在
/// session scope 里——不能因为看到变量就走 transient 路径（会平白要求一个
/// 用户级 manager，反而更容易失败）。
#[test]
fn test_inherited_invocation_id_without_unit_cgroup_is_not_managed() {
    assert!(!is_managed_unit(
        Some("0f5c1a2b3d4e5f60718293a4b5c6d7e8"),
        Some("0::/user.slice/user-1000.slice/session-3.scope\n")
    ));
}

/// 容器里的 cgroup v2 形态（`0::/`、`0::/docker/<id>`）：没有 unit，不算托管。
#[test]
fn test_container_cgroup_is_not_managed() {
    for cg in [
        "0::/\n",
        "0::/docker/abc123def456\n",
        "0::/kubepods/besteffort/pod1/x\n",
    ] {
        assert!(
            !is_managed_unit(Some("inv"), Some(cg)),
            "{cg:?} 不是 systemd unit，不能判成托管"
        );
    }
}

/// 空的 `INVOCATION_ID` 等于没有（systemd 不会给空值，但被清空的环境会）。
#[test]
fn test_blank_invocation_id_is_not_managed() {
    assert!(!is_managed_unit(
        Some("   "),
        Some("0::/system.slice/shell-remote-agent.service\n")
    ));
    assert!(!is_managed_unit(
        Some(""),
        Some("0::/system.slice/shell-remote-agent.service\n")
    ));
}

/// cgroup v1 形态（多行，每行 `<id>:<controllers>:<path>`）也要能认出来。
#[test]
fn test_cgroup_v1_multiline_is_managed() {
    let cg = "12:pids:/system.slice/shell-remote-agent.service\n\
              1:name=systemd:/system.slice/shell-remote-agent.service\n\
              0::/system.slice/shell-remote-agent.service\n";
    assert!(is_managed_unit(Some("inv"), Some(cg)));
}

/// scope **不**算托管：scope 没有主进程，成员退出不会触发 systemd 拆 cgroup，
/// 因此本 bug 在 `systemd-run` 跑起来的 scope 里并不存在。把登录会话
/// （`session-N.scope`）判成危险环境只会让本来能成功的升级去依赖 `systemd-run`。
#[test]
fn test_scope_is_not_managed() {
    assert!(!is_managed_unit(
        Some("inv"),
        Some("0::/user.slice/user-1000.slice/user@1000.service/app.slice/run-u12.scope\n")
    ));
    assert!(!is_managed_unit(
        Some("inv"),
        Some("0::/user.slice/user-1000.slice/session-3.scope\n")
    ));
}

// ── 2. unit 名与参数 ────────────────────────────────────────────────

#[test]
fn test_unit_name_is_prefixed_and_unique_per_upgrade() {
    let a = helper_unit_name("7c9a6795-5a1c-4b39-a6f4-f72a0a66ab23");
    let b = helper_unit_name("13bef94f-18c7-48f0-9a1b-241015ac55fb");
    assert_eq!(a, "shell-remote-upgrade-7c9a67955a1c4b39");
    assert_ne!(a, b, "并发升级不得撞名");
    assert!(a.starts_with(UNIT_PREFIX));
    // unit 名只能是合法字符：不能有 `-` 之外的分隔、不能有路径分隔符。
    assert!(!a.contains('/'));
    assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
}

/// `upgrade_id` 全是非常规字符时仍要造出唯一、合法的名字（不空转）。
#[test]
fn test_unit_name_survives_hostile_upgrade_id() {
    let name = helper_unit_name("../../etc/passwd");
    assert!(name.starts_with(UNIT_PREFIX));
    assert!(!name.contains('/'), "unit 名不得携带路径分隔符: {name}");
    assert!(!name.contains(".."));
    // `etcpasswd` 取自合法字符，路径走私被剔除。
    assert_eq!(name, "shell-remote-upgrade-etcpasswd");
}

/// **核心安全断言**：`systemd-run` 的 argv 里只能有可执行文件与交接文件路径。
#[test]
fn test_transient_args_carry_no_secrets() {
    let handoff = Path::new("/home/u/.shell-remote/upgrade/.handoff-0123456789abcdef.json");
    let plan = TransientLaunch::new(
        Path::new("/usr/local/bin/shell-remote"),
        handoff,
        "7c9a6795-5a1c-4b39-a6f4-f72a0a66ab23",
    );
    let line = plan.command_line();
    // 交接文件路径本身允许（它只是个路径，内容在 0600 文件里）。
    assert!(line.contains(&handoff.display().to_string()));
    // 该有的属性都在。
    assert!(line.contains("--user"), "{line}");
    assert!(line.contains("--collect"), "{line}");
    assert!(line.contains("--quiet"), "{line}");
    assert!(line.contains("--property=KillMode=process"), "{line}");
    assert!(line.contains("update-apply"), "{line}");
    // 绝不允许出现的东西：环境变量赋值、内联 JSON、token 字段名。
    for forbidden in ["token", "Token", "--key", "SR_UPGRADE", "--env", "{", "\""] {
        assert!(
            !line.contains(forbidden),
            "systemd-run 参数里不得出现 {forbidden:?}: {line}"
        );
    }
    // 参数个数固定：任何"顺手多塞一个参数"都会改变这个数，逼着人重新审一遍。
    assert_eq!(
        plan.args.len(),
        9,
        "参数列表变了就要重新确认没有秘密进 argv: {line}"
    );
}

/// 交接文件的临时目录名本身不该泄漏到 unit 属性里（`Description=` 等）。
#[test]
fn test_no_unit_property_carries_the_handoff() {
    let plan = TransientLaunch::new(
        Path::new("/usr/local/bin/shell-remote"),
        Path::new("/home/u/.shell-remote/upgrade/.handoff-0123456789abcdef.json"),
        "up1",
    );
    for arg in &plan.args {
        if arg.starts_with("--property=") {
            assert!(
                !arg.contains("handoff") && !arg.contains("/home"),
                "unit 属性不得携带交接文件路径（systemctl show 可读）: {arg}"
            );
        }
    }
}

// ── 3. 失败路径（进程级）────────────────────────────────────────────

/// 造一个假的 `systemd-run` 可执行脚本。
struct FakeRun(PathBuf, TmpDir);

impl FakeRun {
    /// `body` 是脚本主体，可用 `$@` 读参数。
    fn new(tag: &str, body: &str) -> Self {
        let dir = TmpDir::new(tag);
        let path = dir.join("systemd-run");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self(path, dir)
    }
}

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        use rand::Rng as _;
        let dir = std::env::temp_dir().join(format!(
            "sr-sysd-{tag}-{}-{:08x}",
            std::process::id(),
            rand::thread_rng().gen::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn join(&self, n: &str) -> PathBuf {
        self.0.join(n)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `systemd-run` 不存在（PATH 上没有，或被显式指到一个不存在的路径）。
#[test]
fn test_missing_systemd_run_fails_without_starting_anything() {
    let dir = TmpDir::new("missing");
    let handoff = dir.join("handoff.json");
    let target = RunTarget::Explicit(dir.join("definitely-not-here"));
    match launch_with(
        target,
        Path::new("/usr/local/bin/shell-remote"),
        &handoff,
        "up1",
        Duration::from_millis(500),
    ) {
        TransientHelper::Failed(msg) => {
            assert!(
                msg.contains("systemd-run is not available"),
                "原因必须点明是 systemd-run 不可用: {msg}"
            );
        }
        TransientHelper::Ready(_) => panic!("没有 systemd-run 却报告 ready"),
    }
}

/// `systemd-run` 存在但拒绝启动（无权限 / 没有用户级 manager）：把它的 stderr
/// 原样带回，运维才能区分"没装"和"没权限"。
#[test]
fn test_systemd_run_start_failure_surfaces_its_stderr() {
    let fake = FakeRun::new(
        "denied",
        "echo 'Failed to connect to user scope bus via local transport' >&2\nexit 1",
    );
    let dir = TmpDir::new("denied-h");
    match launch_with(
        RunTarget::Explicit(fake.0.clone()),
        Path::new("/usr/local/bin/shell-remote"),
        &dir.join("handoff.json"),
        "up2",
        // 只作安全网：这个假实现立刻退出，超时不该成为断言的一部分。
        // 5s 在并行跑满 860 个用例的机器上会偶发假失败。
        Duration::from_secs(60),
    ) {
        TransientHelper::Failed(msg) => {
            assert!(
                msg.contains("did not report the helper ready"),
                "要说明是没等到 ready: {msg}"
            );
            assert!(
                msg.contains("user scope bus"),
                "必须把 systemd-run 的报错带回来（否则运维无法判断是无权限）: {msg}"
            );
        }
        TransientHelper::Ready(_) => panic!("失败的 systemd-run 却报告 ready"),
    }
}

/// `systemd-run` 成功启动但 helper 没在超时内报 ready：必须有界失败，
/// 且把 `systemd-run` 客户端收掉（不留在后台空转）。
#[test]
fn test_ready_timeout_is_bounded() {
    let fake = FakeRun::new("silent", "sleep 30");
    let dir = TmpDir::new("silent-h");
    let started = std::time::Instant::now();
    match launch_with(
        RunTarget::Explicit(fake.0.clone()),
        Path::new("/usr/local/bin/shell-remote"),
        &dir.join("handoff.json"),
        "up3",
        Duration::from_millis(400),
    ) {
        TransientHelper::Failed(msg) => {
            assert!(msg.contains("did not report the helper ready"), "{msg}");
        }
        TransientHelper::Ready(_) => panic!("没报 ready 却返回 Ready"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "ready 超时必须是有界失败，实际耗时 {:?}",
        started.elapsed()
    );
}

/// helper 自己在首行报错（例如目录不可写）：旧 agent 必须拿到那条原因，
/// 而不是一句泛泛的超时。
#[test]
fn test_helper_reported_error_is_passed_through() {
    let fake = FakeRun::new(
        "reporterr",
        r#"echo "error: cannot write to /usr/local/bin: Permission denied""#,
    );
    let dir = TmpDir::new("reporterr-h");
    match launch_with(
        RunTarget::Explicit(fake.0.clone()),
        Path::new("/usr/local/bin/shell-remote"),
        &dir.join("handoff.json"),
        "up4",
        Duration::from_secs(60), // 安全网，见上
    ) {
        TransientHelper::Failed(msg) => {
            assert_eq!(
                msg, "cannot write to /usr/local/bin: Permission denied",
                "`error: ` 前缀应被剥掉，与直接助手的读法一致"
            );
        }
        TransientHelper::Ready(_) => panic!("报错的 helper 却返回 Ready"),
    }
}

/// 正常的 ready 握手：确认 `launch_with` 不会自己吞掉 ready，
/// 且 helper 进程能被后续流程回收。
#[test]
fn test_ready_handshake_succeeds() {
    let fake = FakeRun::new("ready", "echo ready\nsleep 5");
    let dir = TmpDir::new("ready-h");
    match launch_with(
        RunTarget::Explicit(fake.0.clone()),
        Path::new("/usr/local/bin/shell-remote"),
        &dir.join("handoff.json"),
        "up5",
        Duration::from_secs(60), // 安全网，见上
    ) {
        TransientHelper::Ready(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
        }
        TransientHelper::Failed(msg) => panic!("ready 握手失败: {msg}"),
    }
}

/// `stop_unit` 只认自己创建的名字：绝不允许停掉旧 service 或别人的 unit。
#[test]
fn test_stop_unit_refuses_foreign_names() {
    // 这些调用必须**什么都不做**（且不 panic）——用真实名字也没关系，
    // 因为函数第一件事就是前缀校验。
    #[cfg(debug_assertions)]
    {
        // debug 构建里会 debug_assert：用 `catch_unwind` 确认它确实拒绝了。
        let r = std::panic::catch_unwind(|| stop_unit("shell-remote-agent"));
        assert!(r.is_err(), "非本模块创建的 unit 必须被拒绝");
    }
    #[cfg(not(debug_assertions))]
    stop_unit("shell-remote-agent");
}

// ── 4. 重复进程识别（MYS-2425-D）────────────────────────────────────

/// 后台子进程封装：drop 时确保收掉，不留孤儿。
struct Spawned(std::process::Child);

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 把本进程的二进制复制一份到临时目录（`exe` 路径因此不同）。
fn copied_binary(dir: &TmpDir) -> Option<PathBuf> {
    let me = std::fs::read_link("/proc/self/exe").ok()?;
    let copy = dir.join("agent-copy");
    std::fs::copy(&me, &copy).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755));
    }
    Some(copy)
}

/// **不按进程名判断**：`exe` 不同的同名二进制绝不能被认成重复实例。
///
/// 同机跑两份 shell-remote（不同路径、不同设备）是常态；按名字扫会把别人的
/// agent 杀掉。这条是那段判定最要紧的边界。
#[cfg(target_os = "linux")]
#[test]
fn test_other_binary_in_the_same_cgroup_is_not_a_duplicate() {
    let Some(cg) = current_cgroup_path() else {
        return; // 没有 cgroup v2 的环境跳过
    };
    let dir = TmpDir::new("dup-exe");
    let Some(copy) = copied_binary(&dir) else {
        return;
    };
    let child = Spawned(
        Command::new(&copy)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a copy of this binary"),
    );
    let pid = child.0.id();
    if std::fs::read_link(format!("/proc/{pid}/exe")).is_err() {
        return; // 进程已退出：这条用例没有观察窗口
    }
    assert!(
        !is_service_side_duplicate(pid, &cg),
        "exe 与本体不同的同名二进制绝不能被判成重复实例"
    );
}

/// 自己永远不是自己的重复实例。
#[test]
fn test_self_is_never_a_duplicate() {
    assert!(!is_service_side_duplicate(
        std::process::id(),
        "/system.slice/shell-remote-agent.service"
    ));
}

/// 读不到 `/proc/<pid>`（不存在的 pid）时不认定——宁可漏报，不误判。
#[cfg(target_os = "linux")]
#[test]
fn test_missing_process_is_not_a_duplicate() {
    // 取一个几乎不可能存在的 pid：先确认它真的不存在。
    let pid = 4_000_000u32;
    if std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok() {
        return;
    }
    assert!(!is_service_side_duplicate(
        pid,
        "/system.slice/shell-remote-agent.service"
    ));
}

/// 自己的服务 cgroup 读不到时，直接判定"不适用"——不去全盘扫进程。
#[test]
fn test_settle_is_a_noop_without_a_service_cgroup() {
    let state = SettleState::new(None);
    assert_eq!(
        settle_service_duplicate(&state),
        SettleOutcome::NotApplicable
    );
}

/// 从 cgroup 路径取 unit 名：运维要拿它拼 `systemctl stop <unit>`。
#[test]
fn test_service_unit_name_is_extracted_from_the_cgroup_path() {
    assert_eq!(
        service_unit_name("/system.slice/shell-remote-agent.service"),
        "shell-remote-agent.service"
    );
    // 末尾带换行（`/proc/self/cgroup` 原样读出来就是这样）也要能处理。
    assert_eq!(
        service_unit_name("/system.slice/shell-remote-agent.service\n"),
        "shell-remote-agent.service"
    );
    // 认不出来的形态原样返回，不猜。
    assert_eq!(service_unit_name("/system.slice"), "/system.slice");
}

/// 观察是**有界**的，且只会给出两种结论之一（不会无限挂着）。
///
/// "不终止任何进程"这条纪律由构造保证：[`settle_service_duplicate`] 里没有
/// 任何发信号的调用（自动终止实测会与 `Restart=always` 形成杀掉—重启循环，
/// `NRestarts` 反而上涨；`systemctl stop` 又被非 root 身份的 polkit 拒绝）。
/// 这里断言的是它可被安全调用、按时返回、结论在预期集合内。
#[cfg(target_os = "linux")]
#[test]
fn test_settle_is_bounded_and_returns_a_known_verdict() {
    let Some(cg) = current_cgroup_path() else {
        return; // 没有 cgroup v2 的环境跳过
    };
    // 注入一个短窗口：这条断言的是"有界"，不是"等满 20 秒"。
    let window = Duration::from_millis(600);
    let started = std::time::Instant::now();
    let verdict = settle_service_duplicate_with(&SettleState::new(Some(cg)), window);
    assert!(
        started.elapsed() <= window + Duration::from_secs(5),
        "观察必须封顶在窗口内，实际 {:?}",
        started.elapsed()
    );
    assert!(
        matches!(
            verdict,
            SettleOutcome::Settled | SettleOutcome::DuplicatePresent { .. }
        ),
        "意外结论: {verdict:?}"
    );
}
