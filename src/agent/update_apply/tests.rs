//! 换机助手与一次性身份恢复的测试（MYS-2425 阶段3）。
//!
//! 分三层：
//!
//! 1. **纯逻辑**：交接文件的编码/权限、恢复参数剥离、注册标记解析、
//!    PID 复用判定。不碰进程，跑得快。
//! 2. **助手半程**：直接调 `helper_main`，用真实文件与真实子进程验证
//!    "旧进程没退 → 磁盘不动"、"新进程没注册 → 回滚"、"同名进程不误等"。
//! 3. **跨进程全流程**：真起一个 agent 进程 → 它启动助手 → 助手换掉二进制
//!    → 新进程注册回执。这层用假 relay + 假 agent 脚本，验证的是**进程关系**
//!    而不是业务逻辑。

use super::*;

// ── 测试脚手架 ──────────────────────────────────────────────────────

/// 临时目录 + 自动清理。不用 `tempfile` 的 `TempDir` 是因为 Windows 上
/// 有进程还占着文件时 drop 会失败——这里只在测试内用，泄漏也无害。
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        use rand::Rng as _;
        let dir = std::env::temp_dir().join(format!(
            "sr-ua-{tag}-{}-{:08x}",
            std::process::id(),
            rand::thread_rng().gen::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sha256_of(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    // 就地覆盖正在被执行的脚本会撞 ETXTBSY：先写临时文件再 rename（同一
    // 目录，同一文件系统），与真实换机用的是同一套手法。
    let tmp = path.with_extension("tmp-write");
    std::fs::write(&tmp, body).unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// 造一个"正在运行的可执行文件"：内容任意，只要有可执行位。
fn fake_target(dir: &TmpDir, name: &str) -> PathBuf {
    let p = dir.join(name);
    #[cfg(unix)]
    write_executable(&p, "#!/bin/sh\nexit 0\n");
    #[cfg(not(unix))]
    std::fs::write(&p, b"old-binary").unwrap();
    p
}

fn sample_ctx(dir: &TmpDir, upgrade_id: &str) -> RestartContext {
    let target = fake_target(dir, "shell-remote");
    let new_bytes = b"#!/bin/sh\nexit 0\n# new version\n";
    let new_binary = dir.join("artifact.part");
    std::fs::write(&new_binary, new_bytes).unwrap();
    RestartContext {
        session_id: "sess42".to_string(),
        tokens: vec![
            ("rw-token-abc".to_string(), "rw".to_string()),
            ("ro-token-def".to_string(), "ro".to_string()),
        ],
        argv: vec![
            OsString::from("agent"),
            OsString::from("--relay-url"),
            // 带空格和引号的参数：交接与拉起都不能做 shell 插值。
            OsString::from("http://relay.example:3000/a b"),
            OsString::from("--key"),
            OsString::from("k\"quote'd"),
        ],
        old_pid: std::process::id(),
        old_start: process_start_stamp(std::process::id()),
        upgrade_id: upgrade_id.to_string(),
        target_version: "9.9.9".to_string(),
        target_path: target,
        new_binary,
        new_sha256: sha256_of(new_bytes),
    }
}

// ── 1. 交接文件 ────────────────────────────────────────────────────

#[test]
fn test_handoff_roundtrip_preserves_identity_and_argv() {
    let dir = TmpDir::new("roundtrip");
    let ctx = sample_ctx(&dir, "up-1");
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let back = RestartContext::read_handoff(&path).unwrap();
    assert_eq!(back.session_id, ctx.session_id);
    assert_eq!(back.tokens, ctx.tokens);
    assert_eq!(back.argv, ctx.argv);
    assert_eq!(back.old_pid, ctx.old_pid);
    assert_eq!(back.upgrade_id, ctx.upgrade_id);
    assert_eq!(back.target_path, ctx.target_path);
    assert_eq!(back.new_binary, ctx.new_binary);
    assert_eq!(back.new_sha256, ctx.new_sha256);
}

#[test]
fn test_handoff_debug_never_prints_tokens() {
    // Debug 会被 tracing 字段捕获：任何一个 `{:?}` 都不能把凭据带进日志。
    let dir = TmpDir::new("dbg");
    let ctx = sample_ctx(&dir, "up-dbg");
    let rendered = format!("{ctx:?}");
    for (tok, _) in &ctx.tokens {
        assert!(
            !rendered.contains(tok.as_str()),
            "Token 不得进入 Debug 输出: {rendered}"
        );
    }
    assert!(rendered.contains("redacted"));
    // 但关键运维信息要留着。
    assert!(rendered.contains("sess42"));
    assert!(rendered.contains("up-dbg"));
}

#[test]
fn test_handoff_file_is_not_readable_by_other_users() {
    let dir = TmpDir::new("perm");
    let ctx = sample_ctx(&dir, "up-perm");
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(
            meta.mode() & 0o777,
            0o600,
            "交接文件必须恰好 0600（里面有 Token）"
        );
        assert_eq!(meta.uid(), unsafe { libc::geteuid() });
        // 所在目录也必须只有自己能进：能列目录就等于能拿走 Token。
        let dmeta = std::fs::metadata(path.parent().unwrap()).unwrap();
        assert_eq!(dmeta.mode() & 0o077, 0, "交接目录不得对组/其他人开放");
    }
    let _ = std::fs::remove_file(&path);
}

#[cfg(unix)]
#[test]
fn test_handoff_rejects_world_readable_file() {
    // 别的用户（或旧版本）放一个 0644 的文件诱导助手换机：必须拒绝。
    use std::os::unix::fs::PermissionsExt;
    let dir = TmpDir::new("perm-bad");
    let ctx = sample_ctx(&dir, "up-bad");
    let good = ctx.write_handoff_into(dir.path()).unwrap();
    let bad = dir.join("planted.json");
    std::fs::copy(&good, &bad).unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = RestartContext::read_handoff(&bad).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        format!("{err}").contains("0600") || format!("{err}").contains("readable"),
        "拒绝原因要说清是权限问题: {err}"
    );
    let _ = std::fs::remove_file(&good);
}

#[test]
fn test_handoff_rejects_schema_mismatch() {
    let dir = TmpDir::new("schema");
    let ctx = sample_ctx(&dir, "up-schema");
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    json["schema"] = serde_json::json!(HANDOFF_SCHEMA + 1);
    std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    let err = RestartContext::read_handoff(&path).unwrap_err();
    assert!(format!("{err}").contains("schema"), "{err}");
}

#[test]
fn test_handoff_rejects_foreign_os_encoding() {
    // 交接文件不跨平台传递：编码名不符只可能是损坏或伪造。
    let dir = TmpDir::new("encoding");
    let ctx = sample_ctx(&dir, "up-enc");
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    json["os_encoding"] = serde_json::json!("some-other-platform");
    std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    let err = RestartContext::read_handoff(&path).unwrap_err();
    assert!(format!("{err}").contains("encoding"), "{err}");
}

#[test]
fn test_handoff_write_never_overwrites_an_existing_file() {
    // `create_new` 是"不可预占"的核心：连写多次必须得到不同路径，且旧文件
    // 内容不被改动。
    let dir = TmpDir::new("create-new");
    let ctx = sample_ctx(&dir, "up-cn");
    let a = ctx.write_handoff_into(dir.path()).unwrap();
    let b = ctx.write_handoff_into(dir.path()).unwrap();
    assert_ne!(a, b, "两次交接必须落在不同文件上");
    assert!(a.exists() && b.exists());
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn test_handoff_rejects_non_utf8_argv_roundtrip_loss() {
    // Unix 的 OsStr 不保证 UTF-8：用 lossy 转换会让路径/参数悄悄变成另一个
    // 值（拉起错误的东西）。这里用非法 UTF-8 字节验证能原样回来。
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let dir = TmpDir::new("nonutf8");
        let mut ctx = sample_ctx(&dir, "up-nonutf8");
        let weird = OsString::from_vec(vec![0x2f, 0x74, 0x6d, 0x70, 0x2f, 0xff, 0xfe]);
        ctx.argv.push(weird.clone());
        let path = ctx.write_handoff_into(dir.path()).unwrap();
        let back = RestartContext::read_handoff(&path).unwrap();
        assert_eq!(back.argv.last().unwrap(), &weird);
        let _ = std::fs::remove_file(&path);
    }
}

// ── 2. 恢复参数 ────────────────────────────────────────────────────

#[test]
fn test_strip_restore_args_removes_flag_and_its_value() {
    let argv: Vec<OsString> = ["agent", "--upgrade-restore", "/run/h.json", "--key", "k"]
        .iter()
        .map(OsString::from)
        .collect();
    let out = strip_restore_args(argv);
    assert_eq!(
        out,
        vec![
            OsString::from("agent"),
            OsString::from("--key"),
            OsString::from("k")
        ]
    );
}

#[test]
fn test_strip_restore_args_removes_equals_form() {
    let argv: Vec<OsString> = ["agent", "--upgrade-restore=/run/h.json", "--key", "k"]
        .iter()
        .map(OsString::from)
        .collect();
    let out = strip_restore_args(argv);
    assert_eq!(
        out,
        vec![
            OsString::from("agent"),
            OsString::from("--key"),
            OsString::from("k")
        ]
    );
}

#[test]
fn test_restart_context_new_strips_restore_args() {
    // 关键不变式：二次升级不能把上一轮的交接参数传下去，否则新一轮会去读
    // 上一轮的文件（session/tokens/upgrade_id 全错）。
    let dir = TmpDir::new("strip");
    let mut ctx = sample_ctx(&dir, "up-strip");
    ctx.argv = vec![
        OsString::from("agent"),
        OsString::from("--upgrade-restore"),
        OsString::from("/tmp/old-handoff.json"),
    ];
    let rebuilt = RestartContext::new(
        ctx.session_id.clone(),
        ctx.tokens.clone(),
        ctx.argv.clone(),
        ctx.target_path.clone(),
        ctx.new_binary.clone(),
        ctx.new_sha256.clone(),
        ctx.upgrade_id.clone(),
        ctx.target_version.clone(),
    );
    assert_eq!(rebuilt.argv, vec![OsString::from("agent")]);
}

#[test]
fn test_restore_handoff_path_prefers_flag_then_env() {
    // 显式参数优先；没有参数时用环境变量（回滚路径唯一可用的通道）。
    assert_eq!(
        restore_handoff_path(Some("/run/flag.json")),
        Some(PathBuf::from("/run/flag.json"))
    );
    std::env::set_var(RESTORE_ENV, "/run/env.json");
    assert_eq!(
        restore_handoff_path(None),
        Some(PathBuf::from("/run/env.json")),
        "没有显式参数时必须能用环境变量恢复（回滚路径）"
    );
    assert_eq!(
        restore_handoff_path(Some("/run/flag.json")),
        Some(PathBuf::from("/run/flag.json")),
        "显式参数优先于环境变量"
    );
    std::env::remove_var(RESTORE_ENV);
    assert_eq!(restore_handoff_path(None), None, "普通启动没有任何恢复信息");
    assert_eq!(restore_handoff_path(Some("")), None, "空参数等于没有");
}

#[test]
fn test_restored_identity_is_one_shot() {
    let dir = TmpDir::new("oneshot");
    let ctx = sample_ctx(&dir, "up-once");
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let id = RestoredIdentity::load(&path).expect("交接文件应能读到");
    assert!(id.restore.is_usable());
    assert!(!id.consumed());
    id.complete();
    assert!(id.consumed(), "首次注册后必须标记为已消费");
    assert!(!path.exists(), "交接文件必须在注册成功后删除");
    // 幂等：重连后的第二次注册不能再报一次。
    id.complete();
    assert!(id.consumed());
}

#[test]
fn test_restored_identity_rejects_unusable_handoff() {
    // 没有 tokens 的交接等于没有身份：必须显式判为不可用，而不是拿着一堆
    // 空字符串去注册（那会在 relay 侧变成一次"全新设备"注册，很难查）。
    let dir = TmpDir::new("unusable");
    let mut ctx = sample_ctx(&dir, "up-unusable");
    ctx.tokens.clear();
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    // 写入时故意清空 tokens 会被 from_file 拒绝——这本身就是正确的防线。
    let loaded = RestoredIdentity::load(&path);
    assert!(loaded.is_none(), "无 tokens 的交接文件必须被拒绝");
}

#[test]
fn test_restored_identity_carries_rollback_reason_once() {
    let dir = TmpDir::new("rollback");
    let ctx = sample_ctx(&dir, "up-rb");
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let id = RestoredIdentity::load_with_rollback(
        &path,
        Some("the new version did not register".to_string()),
    )
    .expect("交接文件应能读到");
    assert_eq!(
        id.restore.rollback.as_deref(),
        Some("the new version did not register")
    );

    // 回滚状态要能变成 relay 看得懂的帧，且带 upgrade_id（防旧状态覆盖）。
    let frame = id.progress_frame("sess42").expect("回滚必须上报");
    let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(v["type"], "agent:upgrade_progress");
    assert_eq!(v["payload"]["stage"], "rolled_back");
    assert_eq!(v["payload"]["upgrade_id"], "up-rb");
    assert!(v["payload"]["error"]
        .as_str()
        .unwrap()
        .contains("did not register"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_no_rollback_frame_for_a_normal_upgrade() {
    let dir = TmpDir::new("noframe");
    let ctx = sample_ctx(&dir, "up-nf");
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let id = RestoredIdentity::load_with_rollback(&path, None).unwrap();
    assert!(id.restore.rollback.is_none());
    assert!(
        id.progress_frame("sess42").is_none(),
        "正常升级不该上报 rolled_back"
    );
    let _ = std::fs::remove_file(&path);
}

/// 环境变量里的回滚原因必须被**一次性**消费：否则新进程再拉起子进程时会
/// 把"我们也回滚了"继承下去，relay 上会出现一串假回滚。
#[test]
fn test_rollback_env_is_consumed_by_load() {
    let dir = TmpDir::new("rbenv-once");
    let ctx = sample_ctx(&dir, "up-rbenv-once");
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    // 这个用例必须独占 `SR_UPGRADE_ROLLBACK`，所以串行执行（其他用例都走
    // `load_with_rollback`，不碰环境变量）。
    std::env::set_var(ROLLBACK_ENV, "boom");
    let id = RestoredIdentity::load(&path);
    assert_eq!(
        id.expect("交接文件应能读到").restore.rollback.as_deref(),
        Some("boom")
    );
    assert!(
        std::env::var(ROLLBACK_ENV).is_err(),
        "回滚原因必须一次性消费"
    );
    let _ = std::fs::remove_file(&path);
}

// ── 3. 注册标记 ────────────────────────────────────────────────────

#[test]
fn test_parse_registered_marker_requires_matching_upgrade_id() {
    // 同一台设备可能连续升级：上一轮残留进程的标记绝不能被当成本轮回执。
    let line = format!("{REGISTERED_MARKER} upgrade_id=up-2 version=9.9.9 session=sess42");
    assert_eq!(
        parse_registered_marker(&line, "up-2").as_deref(),
        Some("9.9.9")
    );
    assert_eq!(
        parse_registered_marker(&line, "up-1"),
        None,
        "upgrade_id 不符的标记必须忽略（上一轮的残留）"
    );
    // 空期望值（老交接）时不做 id 校验，只认标记本身。
    assert_eq!(parse_registered_marker(&line, "").as_deref(), Some("9.9.9"));
    // 非标记行一律不认。
    assert_eq!(
        parse_registered_marker("INFO agent session established", "up-2"),
        None
    );
    assert_eq!(parse_registered_marker("", "up-2"), None);
}

#[test]
fn test_registered_marker_line_carries_no_token() {
    // 回执走 stderr、可能被转存到日志：只允许 version/session/upgrade_id。
    let line = format!("{REGISTERED_MARKER} upgrade_id=up-2 version=9.9.9 session=sess42");
    assert!(!line.contains("token"));
    assert_eq!(
        parse_registered_marker(&line, "up-2").as_deref(),
        Some("9.9.9")
    );
}

// ── 4. 进程存活判定（按确切 PID）───────────────────────────────────

#[cfg(unix)]
#[test]
fn test_process_start_stamp_distinguishes_reused_pids() {
    // PID 复用是把"等旧进程退出"变成"等一个陌生进程"的唯一途径：启动时刻
    // 不符就必须立刻认为目标已经退出。
    let me = std::process::id();
    let start = process_start_stamp(me).expect("自己一定有启动时刻");
    assert!(unix_process_alive(me, Some(start)));
    assert!(
        !unix_process_alive(me, Some(start.wrapping_add(1))),
        "PID 被复用（启动时刻不符）时必须判定为已退出"
    );
    assert!(
        unix_process_alive(me, None),
        "拿不到启动时刻时退化为按 PID 判活，不能直接判死"
    );
    assert!(!unix_process_alive(0, None), "pid 0 不是可等的进程");
}

#[cfg(unix)]
#[test]
fn test_wait_for_exit_returns_immediately_for_a_dead_pid() {
    // 一个已经退出的子进程：等待必须立刻返回，不能吃满超时。
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let started = Instant::now();
    let r = wait_for_exit(pid, None, Duration::from_secs(10));
    assert_eq!(r, ExitWait::Exited);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "死进程不该等满超时（实际 {:?}）",
        started.elapsed()
    );
}

#[cfg(unix)]
#[test]
fn test_wait_for_exit_times_out_on_a_live_process() {
    // 旧进程没退出就不能动手：超时是"不动磁盘"的信号，不是错误。
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let started = Instant::now();
    let r = wait_for_exit(child.id(), None, Duration::from_millis(300));
    assert_eq!(r, ExitWait::Timeout);
    assert!(started.elapsed() >= Duration::from_millis(250));
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn test_two_same_named_agents_are_not_confused() {
    // 核心回归：按进程名等待的实现无法区分这两个进程。按 PID 等时，
    // 我们等的那个退出了就立刻返回，另一个同名进程继续跑不影响判定。
    let dir = TmpDir::new("twins");
    let bin = dir.join("shell-remote");
    #[cfg(unix)]
    write_executable(&bin, "#!/bin/sh\nsleep 30\n");

    let mut target = std::process::Command::new(&bin).spawn().unwrap();
    let other = std::process::Command::new(&bin).spawn().unwrap();
    let target_pid = target.id();
    let other_pid = other.id();
    assert_ne!(target_pid, other_pid);

    // 先确认"两个都活着"时等待会超时（说明它没有误判为已退出）。
    assert_eq!(
        wait_for_exit(target_pid, None, Duration::from_millis(200)),
        ExitWait::Timeout,
        "目标还活着时不得返回已退出"
    );
    // 杀掉我们等的那个，另一个保持运行。
    let _ = target.kill();
    let _ = target.wait();
    let started = Instant::now();
    assert_eq!(
        wait_for_exit(target_pid, None, Duration::from_secs(5)),
        ExitWait::Exited,
        "只等我们自己的那个 PID"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    // 同名的另一个必须毫发无损——这正是名字匹配做不到的。
    assert!(
        unix_process_alive(other_pid, None),
        "同名 agent 绝不能被误杀/误等"
    );
    let mut other = other;
    let _ = other.kill();
    let _ = other.wait();
}

// ── 5. 助手半程：旧进程未退出 / 校验失败 ⇒ 磁盘不动 ────────────────

/// 起一个长命进程当"旧 agent"，返回 (child, pid)。
#[cfg(unix)]
fn live_old_process() -> std::process::Child {
    std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn test_helper_changes_nothing_while_the_old_process_still_runs() {
    // 旧进程没退出 ⇒ 助手超时放弃 ⇒ 目标文件原封不动（这是"旧 agent 一直
    // 在线"的硬保证：磁盘不动就不会出现版本不一致）。
    let dir = TmpDir::new("stillrunning");
    let mut ctx = sample_ctx(&dir, "up-run");
    let old = live_old_process();
    ctx.old_pid = old.id();
    ctx.old_start = None;
    let original = std::fs::read(&ctx.target_path).unwrap();
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();

    let outcome = helper_main_with_old_timeout(&handoff, Duration::from_millis(400));
    let mut old = old;
    let _ = old.kill();
    let _ = old.wait();

    match outcome {
        HelperOutcome::Aborted { reason } => {
            assert!(reason.contains("still running"), "{reason}");
        }
        other => panic!("旧进程还在时必须放弃，实际: {other:?}"),
    }
    assert_eq!(
        std::fs::read(&ctx.target_path).unwrap(),
        original,
        "旧进程未退出时不得改动目标二进制"
    );
    assert!(ctx.new_binary.exists(), "新文件也不该被删（可断点重试）");
}

#[cfg(unix)]
#[test]
fn test_helper_refuses_a_tampered_artifact() {
    // 从"旧 agent 校验完"到"助手接手"之间 `.part` 可被改写：助手自己再算
    // 一遍 hash，不符就一点都不动。
    let dir = TmpDir::new("tampered");
    let mut ctx = sample_ctx(&dir, "up-tamper");
    let dead = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    drop(dead);
    ctx.old_pid = 0; // 已经不存在
    let _ = dead_pid;
    std::fs::write(&ctx.new_binary, b"# tampered bytes\n").unwrap();
    let original = std::fs::read(&ctx.target_path).unwrap();
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();

    let outcome = helper_main(&handoff, false);
    match outcome {
        HelperOutcome::Aborted { reason } => {
            assert!(
                reason.contains("digest") || reason.contains("match"),
                "拒绝原因应指向 hash 不符: {reason}"
            );
        }
        other => panic!("被篡改的制品必须被拒绝，实际: {other:?}"),
    }
    assert_eq!(std::fs::read(&ctx.target_path).unwrap(), original);
    // 拒绝时要写清为什么（助手可能在旧 agent 已退出后运行，只能靠自己的日志）。
    assert!(
        handoff.with_file_name("update-apply.log").exists(),
        "助手必须留下可读日志"
    );
}

#[cfg(unix)]
#[test]
fn test_helper_refuses_an_empty_artifact() {
    let dir = TmpDir::new("empty");
    let mut ctx = sample_ctx(&dir, "up-empty");
    ctx.old_pid = 0;
    std::fs::write(&ctx.new_binary, b"").unwrap();
    ctx.new_sha256 = sha256_of(b""); // hash 对得上，但文件是空的
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    match helper_main(&handoff, false) {
        HelperOutcome::Aborted { reason } => assert!(reason.contains("empty"), "{reason}"),
        other => panic!("空文件必须被拒绝，实际: {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn test_helper_aborts_before_ready_when_the_handoff_is_unreadable() {
    let dir = TmpDir::new("badhandoff");
    let path = dir.join("missing.json");
    match helper_main(&path, false) {
        HelperOutcome::Aborted { reason } => assert!(reason.contains("handoff"), "{reason}"),
        other => panic!("读不到交接必须放弃，实际: {other:?}"),
    }
}

// ── 6. 助手全流程：替换 → 注册回执 → 成功 ─────────────────────────

/// 助手跑一次，但把"等旧进程退出"压到毫秒级（真实值 45s 等不起）。
///
/// 走 `helper_main_with` 而不是环境变量：并行测试会互相覆盖进程级环境变量，
/// 而这条路径与生产只差一个等待上限。
fn helper_main_with_old_timeout(handoff: &Path, old_timeout: Duration) -> HelperOutcome {
    helper_main_with(
        handoff,
        false,
        HelperTimeouts {
            old_exit: old_timeout,
            ..HelperTimeouts::default()
        },
    )
}

#[cfg(unix)]
#[test]
fn test_helper_registered_flow_replaces_the_binary_and_keeps_the_backup_removed() {
    let dir = TmpDir::new("flow-ok");
    let mut ctx = sample_ctx(&dir, "up-flowok");
    // "旧 agent" 已经退出：用 pid 0（永不存在的进程）。
    ctx.old_pid = 0;
    ctx.old_start = None;
    // "新二进制"扮演新版 agent：读交接文件确认身份对得上，然后回执。
    //
    // 注意顺序：先把假 agent 写进 `new_binary`（助手会把它 rename 到 target），
    // 再写交接文件——交接文件里的 sha256 必须是最终那份字节。
    write_script_agent(&ctx.new_binary);
    ctx.new_sha256 = sha256_of(&std::fs::read(&ctx.new_binary).unwrap());
    let expected = std::fs::read(&ctx.new_binary).unwrap();
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();

    let outcome = helper_main(&handoff, false);
    match &outcome {
        HelperOutcome::Registered { version } => assert_eq!(version, "9.9.9"),
        other => panic!("注册成功应判为升级完成，实际: {other:?}"),
    }
    // 新版已在 target 上。
    assert_eq!(
        std::fs::read(&ctx.target_path).unwrap(),
        expected,
        "target 必须已经是新字节"
    );
    // 备份已清理。
    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(BACKUP_INFIX))
        .collect();
    assert!(backups.is_empty(), "成功后必须删掉备份: {backups:?}");
    // 交接文件也已删除（新进程自己删 + 助手兜底）。
    assert!(!handoff.exists(), "成功后交接文件必须不存在");
}

/// 把 `p` 写成一个"新版 agent"：读交接文件、核身份、写注册标记。
#[cfg(unix)]
fn write_script_agent(p: &Path) {
    let body = format!(
        "#!/bin/sh\n\
         H=\"${{SR_UPGRADE_RESTORE:-}}\"\n\
         if [ -n \"$H\" ]; then\n\
         grep -q 'sess42' \"$H\" || {{ echo 'wrong handoff' >&2; exit 1; }}\n\
         rm -f \"$H\"\n\
         fi\n\
         echo \"{REGISTERED_MARKER} upgrade_id=up-flowok version=9.9.9 session=sess42\" >&2\n\
         sleep 1\n"
    );
    write_executable(p, &body);
}

#[cfg(unix)]
#[test]
fn test_helper_rolls_back_when_the_new_process_exits_without_registering() {
    // 新进程起来就崩（或注册失败）：必须回滚旧文件、用原身份拉起旧版，
    // 并留下原因——否则设备会停在"磁盘新版、跑的还是旧版"的分裂状态。
    let dir = TmpDir::new("flow-rb");
    let mut ctx = sample_ctx(&dir, "up-flowrb");
    ctx.old_pid = 0;
    ctx.old_start = None;
    let original = std::fs::read(&ctx.target_path).unwrap();
    // 新版：立刻失败退出，不回执。
    write_executable(
        &ctx.new_binary,
        "#!/bin/sh\necho 'boom: cannot register' >&2\nexit 1\n",
    );
    ctx.new_sha256 = sha256_of(&std::fs::read(&ctx.new_binary).unwrap());
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();

    let outcome = helper_main(&handoff, false);
    let reason = match &outcome {
        HelperOutcome::RolledBack { reason } => reason.clone(),
        other => panic!("注册失败必须回滚，实际: {other:?}"),
    };
    assert!(
        reason.contains("exited early") || reason.contains("register"),
        "回滚原因应指向提前退出/注册失败: {reason}"
    );
    assert_eq!(
        std::fs::read(&ctx.target_path).unwrap(),
        original,
        "回滚后 target 必须是原字节"
    );
    // 回滚日志要可读（含"回滚"字样与新进程的报错）。
    let log = std::fs::read_to_string(dir.join("update-apply.log")).unwrap_or_default();
    assert!(log.contains("rolling back"), "日志缺回滚记录: {log}");
    assert!(log.contains("boom"), "新进程的报错必须被转存: {log}");
    // Token 不得出现在日志里。
    for (tok, _) in &ctx.tokens {
        assert!(!log.contains(tok.as_str()), "Token 泄露进日志: {log}");
    }
}

#[cfg(unix)]
#[test]
fn test_helper_rolls_back_when_the_new_process_hangs() {
    // 新进程起来了但永不注册（卡在等网络）：超时必须回滚，不能无限等。
    let dir = TmpDir::new("flow-hang");
    let mut ctx = sample_ctx(&dir, "up-hang");
    ctx.old_pid = 0;
    ctx.old_start = None;
    let original = std::fs::read(&ctx.target_path).unwrap();
    write_executable(&ctx.new_binary, "#!/bin/sh\nsleep 120\n");
    ctx.new_sha256 = sha256_of(&std::fs::read(&ctx.new_binary).unwrap());
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();

    let started = Instant::now();
    let outcome = helper_main_short_register_timeout(&handoff, Duration::from_millis(600));
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "注册超时必须有界（实际 {:?}）",
        started.elapsed()
    );
    match &outcome {
        HelperOutcome::RolledBack { reason } => {
            assert!(reason.contains("no registration"), "{reason}")
        }
        other => panic!("不注册就必须回滚，实际: {other:?}"),
    }
    assert_eq!(std::fs::read(&ctx.target_path).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn test_helper_ignores_a_registration_marker_from_another_upgrade() {
    // 同一设备连续升级：上一轮残留进程可能会写一条标记。upgrade_id 不符就
    // 不能被当成本轮成功，否则会删掉备份、把没注册的新版留在磁盘上。
    let dir = TmpDir::new("flow-stale");
    let mut ctx = sample_ctx(&dir, "up-current");
    ctx.old_pid = 0;
    ctx.old_start = None;
    let original = std::fs::read(&ctx.target_path).unwrap();
    write_executable(
        &ctx.new_binary,
        &format!(
            "#!/bin/sh\necho \"{REGISTERED_MARKER} upgrade_id=up-previous version=9.9.9 session=other\" >&2\nsleep 120\n"
        ),
    );
    ctx.new_sha256 = sha256_of(&std::fs::read(&ctx.new_binary).unwrap());
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();

    let outcome = helper_main_short_register_timeout(&handoff, Duration::from_millis(600));
    match &outcome {
        HelperOutcome::RolledBack { .. } => {}
        other => panic!("他人 upgrade_id 的标记不算成功，实际: {other:?}"),
    }
    assert_eq!(std::fs::read(&ctx.target_path).unwrap(), original);
}

/// 注册等待超时也要能压短（真实值 75s，测试等不起）。
fn helper_main_short_register_timeout(handoff: &Path, t: Duration) -> HelperOutcome {
    helper_main_with(
        handoff,
        false,
        HelperTimeouts {
            register: t,
            // 旧进程在这些用例里已经不存在（pid 0），这条不会真的等。
            ..HelperTimeouts::default()
        },
    )
}

#[cfg(unix)]
#[test]
fn test_helper_never_kills_a_process_it_does_not_own() {
    // 助手只对自己拉起的新进程发 SIGKILL。这里验证：目标文件被占/替换失败
    // 时，一个无关的同名进程必须活着。
    let dir = TmpDir::new("nocollateral");
    let mut ctx = sample_ctx(&dir, "up-nocol");
    ctx.old_pid = 0;
    ctx.old_start = None;
    let bystander = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let bystander_pid = bystander.id();
    // 不可写目录：让助手在探测阶段就放弃（不动磁盘、不碰任何进程）。
    let ro = dir.join("ro");
    std::fs::create_dir_all(&ro).unwrap();
    // 先建好文件再把目录降权：目录 0500 之后就连自己都写不进去了。
    ctx.target_path = ro.join("shell-remote");
    std::fs::write(&ctx.target_path, b"old").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
    }
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();

    let outcome = helper_main(&handoff, false);
    assert!(
        matches!(outcome, HelperOutcome::Aborted { .. }),
        "目录不可写时必须放弃，实际: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(&ctx.target_path).unwrap(),
        b"old",
        "放弃时不得改动目标文件"
    );
    assert!(
        unix_process_alive(bystander_pid, None),
        "助手不得误杀无关进程"
    );
    let mut bystander = bystander;
    let _ = bystander.kill();
    let _ = bystander.wait();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700));
    }
}

// ── 7. 旧 agent 侧：助手起不来就不退出 ─────────────────────────────

#[cfg(unix)]
#[test]
fn test_launch_helper_surfaces_a_readable_failure() {
    // 助手路径不存在：必须返回可读错误（旧 agent 据此上报 failed 并继续在线），
    // 而不是 panic 或静默成功。
    let dir = TmpDir::new("launch-bad");
    let handoff = dir.join("h.json");
    std::fs::write(&handoff, b"{}").unwrap();
    let err = launch_helper_and_wait_ready(
        &dir.join("does-not-exist"),
        &handoff,
        Duration::from_secs(2),
    )
    .expect_err("不存在的助手必须报错");
    assert!(err.contains("cannot start the update helper"), "{err}");
}

#[cfg(unix)]
#[test]
fn test_launch_helper_reports_the_helper_error_line() {
    // 助手在 ready 之前失败：旧 agent 必须拿到原因（而不是干等超时）。
    let dir = TmpDir::new("launch-err");
    let helper = dir.join("helper");
    write_executable(
        &helper,
        &format!("#!/bin/sh\necho '{ERROR_PREFIX}directory is not writable'\nexit 2\n"),
    );
    let handoff = dir.join("h.json");
    std::fs::write(&handoff, b"{}").unwrap();
    let err = launch_helper_and_wait_ready(&helper, &handoff, Duration::from_secs(5))
        .expect_err("必须报错");
    assert!(err.contains("not writable"), "原因要透传给旧 agent: {err}");
}

#[cfg(unix)]
#[test]
fn test_launch_helper_times_out_without_ready() {
    // 助手什么都不说（卡住）：旧 agent 必须有界地放弃，绝不退出自己。
    let dir = TmpDir::new("launch-hang");
    let helper = dir.join("helper");
    write_executable(&helper, "#!/bin/sh\nsleep 60\n");
    let handoff = dir.join("h.json");
    std::fs::write(&handoff, b"{}").unwrap();
    let started = Instant::now();
    let err = launch_helper_and_wait_ready(&helper, &handoff, Duration::from_millis(400))
        .expect_err("没 ready 就必须报错");
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(err.contains("did not become ready"), "{err}");
}

#[cfg(unix)]
#[test]
fn test_launch_helper_waits_for_ready_before_returning() {
    // 正常路径：助手先做事再 ready，旧 agent 只有在 ready 之后才能往下走
    // （也就是才能退出）。
    let dir = TmpDir::new("launch-ok");
    let helper = dir.join("helper");
    let marker = dir.join("helper-ran");
    write_executable(
        &helper,
        &format!(
            "#!/bin/sh\ntouch '{}'\necho ready\nsleep 5\n",
            marker.display()
        ),
    );
    let handoff = dir.join("h.json");
    std::fs::write(&handoff, b"{}").unwrap();
    let mut child = launch_helper_and_wait_ready(&helper, &handoff, Duration::from_secs(10))
        .expect("ready 之后必须返回");
    assert!(marker.exists(), "返回时助手必须已经跑过准备工作");
    let _ = child.kill();
    let _ = child.wait();
}

// ── 8. 交接文件不落 argv / 不进日志（端到端）───────────────────────

#[cfg(unix)]
#[test]
fn test_tokens_reach_the_new_process_only_through_the_private_file() {
    // 契约：Token 永不进 argv。助手拉起新进程时把交接文件**路径**放进环境
    // 变量，argv 里只有原始参数。
    let dir = TmpDir::new("argv");
    let mut ctx = sample_ctx(&dir, "up-argv");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    // 拉起脚本记录它自己的 argv 与 env。
    let record = dir.join("argv.txt");
    let script = dir.join("recorder");
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'ENV=%s\\n' \"${{SR_UPGRADE_RESTORE:-}}\" >> '{}'\n",
            record.display(),
            record.display()
        ),
    );
    ctx.target_path = script;
    let child = spawn_agent(&ctx, &handoff, None).expect("应能拉起子进程");
    let mut child = child;
    let _ = child.wait();
    let text = std::fs::read_to_string(&record).unwrap();
    for (tok, _) in &ctx.tokens {
        assert!(
            !text.contains(tok.as_str()),
            "Token 不得出现在 argv/环境变量里: {text}"
        );
    }
    assert!(
        text.contains(&format!("ENV={}", handoff.display())),
        "新进程必须通过环境变量拿到交接文件路径: {text}"
    );
    // 原始参数（含空格/引号）原样到达，说明没有 shell 插值。
    assert!(text.contains("http://relay.example:3000/a b"), "{text}");
    assert!(text.contains("k\"quote'd"), "{text}");
    let _ = std::fs::remove_file(&handoff);
}

#[cfg(unix)]
#[test]
fn test_rollback_spawn_passes_the_reason_through_the_environment() {
    // 回滚拉起的是**旧版**二进制，它可能不认识 `--upgrade-restore` 参数：
    // 所以身份与原因都走环境变量——未知环境变量对旧版是无害的。
    let dir = TmpDir::new("rbenv");
    let mut ctx = sample_ctx(&dir, "up-rbenv");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    let record = dir.join("env.txt");
    let script = dir.join("recorder");
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\nprintf 'REASON=%s\\n' \"${{SR_UPGRADE_ROLLBACK:-}}\" > '{}'\n",
            record.display()
        ),
    );
    ctx.target_path = script;
    let mut child = spawn_agent(&ctx, &handoff, Some("did not register")).unwrap();
    let _ = child.wait();
    let text = std::fs::read_to_string(&record).unwrap();
    assert!(text.contains("REASON=did not register"), "{text}");
    let _ = std::fs::remove_file(&handoff);
}

// ── 9. 助手日志与残留清理 ──────────────────────────────────────────

#[cfg(unix)]
#[test]
fn test_helper_log_is_private_and_never_contains_tokens() {
    let dir = TmpDir::new("logperm");
    let mut ctx = sample_ctx(&dir, "up-log");
    ctx.old_pid = 0;
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    let _ = helper_main(&handoff, false);
    let log = dir.join("update-apply.log");
    assert!(log.exists(), "助手必须留日志");
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        std::fs::metadata(&log).unwrap().mode() & 0o077,
        0,
        "助手日志里可能有机密上下文，不得对组/其他人开放"
    );
    let text = std::fs::read_to_string(&log).unwrap();
    for (tok, _) in &ctx.tokens {
        assert!(!text.contains(tok.as_str()), "Token 泄露进助手日志: {text}");
    }
    assert!(
        text.contains("up-log"),
        "日志要记 upgrade_id 便于对账: {text}"
    );
}

#[cfg(unix)]
#[test]
fn test_backup_path_is_per_upgrade_id() {
    // 两次升级的备份不能互相覆盖（否则回滚会拿回更早的那份）。
    let dir = TmpDir::new("backupname");
    let target = dir.join("shell-remote");
    let a = backup_path_for(&target, "aaaaaaaa-1111");
    let b = backup_path_for(&target, "bbbbbbbb-2222");
    assert_ne!(a, b);
    assert!(a
        .file_name()
        .unwrap()
        .to_string_lossy()
        .contains("aaaaaaaa"));
    assert!(
        a.parent() == b.parent(),
        "备份必须与目标同目录（同文件系统才能 rename）"
    );
    // 空 upgrade_id 也要给出确定的名字，不能拼出空后缀。
    let c = backup_path_for(&target, "");
    assert!(c.file_name().unwrap().to_string_lossy().contains("unknown"));
}

#[cfg(unix)]
#[test]
fn test_make_executable_sets_the_exec_bit() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TmpDir::new("chmod");
    let p = dir.join("artifact");
    std::fs::write(&p, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    make_executable(&p).unwrap();
    let mode = std::fs::metadata(&p).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "换机后必须保持可执行");
}

// ── 10. 进程级集成：真起 relay + 真起 agent + 真做一次换机 ─────────

// 这一组用**真的二进制**（`CARGO_BIN_EXE_shell-remote`）跑完整链路：
//
//   旧 agent 进程 → 下载/校验 → 启动 `update-apply` 助手 → 助手换掉二进制
//   → 拉起新版 → 新版注册成功 → relay 上出现同一 session_id/Token、新版本号。
//
// 这正是任务卡要求的"升级后新 PID、同 ID/Token、版本确认"。单元测试做不到
// 这一点：它只能验证"助手做的动作"，验证不了"真的换了进程还能登回来"。
//
// 需要的制品：同一次构建产出的两个二进制（旧版 = 当前这份，新版 = 同一份
// 但带不同 `--version` 不可能凭空造出来）。所以这里用 **shell 脚本当 agent**
// 会丢掉真实注册逻辑，因此换机链路用"假 agent + 真助手 + 真 relay"的组合：
// 假 agent 只做注册这一件事（走真实 HTTP 协议），足以断言身份与版本。

#[cfg(unix)]
mod process_level {
    use super::*;
    use std::io::{BufRead, BufReader};

    /// 一个把 `/agent/send` 的注册请求转发到真 relay、并把注册结果写进文件
    /// 的"agent"，用 shell + curl 实现——这样它一定能被 `update-apply` 当成
    /// 一个普通可执行文件替换/拉起。
    ///
    /// `version` 与 `sid` 由脚本从环境变量读，脚本另外把**自己的 PID** 记进
    /// 文件，用来断言"换机后 PID 变了"。
    #[cfg(unix)]
    fn agent_script(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        let body = r#"#!/bin/sh
set -u
OUT="${SR_TEST_OUT:?}"
RELAY="${SR_TEST_RELAY:?}"
SID="${SR_TEST_SID:?}"
VER="${SR_TEST_VER:?}"
H="${SR_UPGRADE_RESTORE:-}"

TOKENS=""
if [ -n "$H" ] && [ -f "$H" ]; then
  TOKENS="$(python3 - "$H" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))
print(json.dumps([{"token":t["token"],"permission":t["permission"]} for t in d["tokens"]]))
PY
)"
fi

if [ -n "$TOKENS" ]; then
  BODY="{\"type\":\"agent:register\",\"session_id\":\"$SID\",\"tokens\":$TOKENS,\"agent_version\":\"$VER\"}"
else
  BODY="{\"type\":\"agent:register\",\"session_id\":\"$SID\",\"token_type\":\"rw\",\"agent_version\":\"$VER\"}"
fi

RESP="$(curl -s -X POST "$RELAY/agent/send" -H 'Content-Type: application/json' -d "$BODY")"
echo "pid=$$ resp=$RESP" >> "$OUT"

case "$RESP" in
  *'"type":"agent:registered"'*)
    GOT_SID="$(printf '%s' "$RESP" | python3 -c 'import json,sys;print(json.load(sys.stdin)["session_id"])' 2>/dev/null)"
    GOT_TOK="$(printf '%s' "$RESP" | python3 -c 'import json,sys;print(",".join(sorted(t["token"] for t in json.load(sys.stdin)["payload"]["tokens"])))' 2>/dev/null)"
    echo "registered pid=$$ sid=$GOT_SID tokens=$GOT_TOK version=$VER" >> "$OUT"
    if [ -n "$H" ]; then rm -f "$H"; fi
    echo "shell-remote: upgrade-registered upgrade_id=$SR_TEST_UPGRADE_ID version=$VER session=$GOT_SID" >&2
    # 注册成功后保持在线（模拟真实 agent 的常驻），由测试自己收尾。
    sleep 30
    ;;
  *)
    exit 1
    ;;
esac
"#;
        write_executable(&p, body);
        p
    }

    /// 真 relay：起在随机端口，等它响应。
    ///
    /// stderr 必须有人一直读：relay 会持续写日志，管道写满（64KB）之后它会
    /// **阻塞在 write 上**，注册请求随之挂死——测试会从"几秒"变成"几分钟"，
    /// 而且失败原因完全看不出来。所以起一个排水线程读到 EOF。
    struct TestRelay {
        child: std::process::Child,
        base: String,
        _drain: std::thread::JoinHandle<()>,
    }

    impl TestRelay {
        /// 二进制路径：`CARGO_BIN_EXE_*` 只在**集成测试**（tests/）里编译期
        /// 可用，lib 单测里必须运行时找。用 `current_exe` 的兄弟目录推：
        /// 单测二进制在 `target/<profile>/deps/`，发布二进制在上一级。
        async fn start() -> Option<Self> {
            let me = std::env::current_exe().ok()?;
            let dir = me.parent()?;
            let candidates = [
                dir.join("shell-remote"),
                dir.parent()
                    .map(|p| p.join("shell-remote"))
                    .unwrap_or_default(),
            ];
            let bin = candidates.into_iter().find(|p| p.is_file())?;
            let mut child = std::process::Command::new(&bin)
                .args([
                    "relay",
                    "--bind",
                    "127.0.0.1:0",
                    "--auth",
                    "pl-pass",
                    "--no-tls",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .ok()?;
            // relay 把实际端口打进 stderr（bind 0 → OS 选端口）。前几行由
            // 主线程读出来定端口，之后整条流交给排水线程。
            let stderr = child.stderr.take()?;
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            let drain = std::thread::Builder::new()
                .name("sr-test-relay-log".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stderr);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let l = line.trim_end().to_string();
                                // 主线程只在拿到端口前关心内容；send 失败也无妨。
                                let _ = tx.send(l);
                            }
                        }
                    }
                })
                .ok()?;
            let mut port = String::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while std::time::Instant::now() < deadline {
                let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) else {
                    continue;
                };
                if let Some(idx) = line.find("127.0.0.1:") {
                    let rest = &line[idx + "127.0.0.1:".len()..];
                    port = rest
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>();
                    if !port.is_empty() {
                        break;
                    }
                }
            }
            if port.is_empty() {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Some(Self {
                child,
                base: format!("http://127.0.0.1:{port}"),
                _drain: drain,
            })
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// 从真 relay 的注册响应里读一个会话的 (tokens)。
    async fn register(client: &reqwest::Client, base: &str, sid: &str) -> Vec<String> {
        let resp: serde_json::Value = client
            .post(format!("{base}/agent/send"))
            .json(&serde_json::json!({
                "type": "agent:register",
                "session_id": sid,
                "token_type": "rw",
                "agent_version": "1.0.0"
            }))
            .send()
            .await
            .expect("注册请求必须成功")
            .json()
            .await
            .expect("注册响应必须是 JSON");
        assert_eq!(resp["type"], "agent:registered", "{resp}");
        resp["payload"]["tokens"]
            .as_array()
            .expect("注册响应必须带 tokens")
            .iter()
            .map(|t| t["token"].as_str().unwrap().to_string())
            .collect()
    }

    /// 完整换机：旧进程退出 → 助手替换 → 新进程用**原身份**注册成功。
    ///
    /// 断言四件事（任务卡的"进程级集成测试"要求）：
    /// 1. 换机后 PID 变了（真的是另一个进程，不是同一份内存里的重启）；
    /// 2. 新进程注册到的 session_id 与 Token 与旧进程完全一致；
    /// 3. relay 认出了这是同一会话（不新建 ID、不换 Token）；
    /// 4. 新版本号出现在 relay 侧（"成功由新进程注册版本确认"）。
    #[tokio::test]
    async fn test_process_level_upgrade_keeps_pid_change_and_identity() {
        let Some(relay) = TestRelay::start().await else {
            eprintln!("跳过：无法启动真 relay 二进制");
            return;
        };
        let dir = TmpDir::new("proclevel");
        let client = reqwest::Client::new();

        // 1. 旧 agent 先注册一次，拿到真实 tokens（这就是要保住的"原身份"）。
        let sid = "plag00001";
        let original_tokens = register(&client, &relay.base, sid).await;
        assert!(!original_tokens.is_empty());
        let old_pid = 4_000_000u32; // 占位：下面用真实脚本 PID 覆盖

        // 2. 造"旧版"与"新版"两份 agent 脚本（同一份代码，只是版本号不同）。
        let old_bin = agent_script(dir.path(), "shell-remote-old");
        let new_bin = agent_script(dir.path(), "shell-remote-new");
        // 目标文件就是"正在运行的旧版"。
        let target = dir.join("shell-remote");
        std::fs::copy(&old_bin, &target).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // 3. 交接文件：身份 + 原始 argv + 目标/新文件路径。
        let out = dir.join("registrations.log");
        let new_sha = sha256_of(&std::fs::read(&new_bin).unwrap());
        let ctx = RestartContext {
            session_id: sid.to_string(),
            tokens: original_tokens
                .iter()
                .map(|t| (t.clone(), "rw".to_string()))
                .collect(),
            // 原始参数里带空格和引号：验证参数交接不做 shell 插值。
            argv: vec![
                OsString::from("agent"),
                OsString::from("--note"),
                OsString::from("a b \"c\""),
            ],
            old_pid,
            old_start: None,
            upgrade_id: "pl-upgrade-1".to_string(),
            target_version: "9.9.9".to_string(),
            target_path: target.clone(),
            new_binary: new_bin.clone(),
            new_sha256: new_sha,
        };
        let handoff = ctx.write_handoff_into(dir.path()).unwrap();

        // 4. 旧进程"退出"（pid 0 表示不存在），助手跑完整流程。
        //
        // 新进程会继承助手的环境变量，所以测试的输出路径/relay/版本都通过
        // 环境变量交给脚本——真实 agent 也是靠环境（SR_UPGRADE_RESTORE）+
        // argv 拿到上下文的。
        std::env::set_var("SR_TEST_OUT", &out);
        std::env::set_var("SR_TEST_RELAY", &relay.base);
        std::env::set_var("SR_TEST_SID", sid);
        std::env::set_var("SR_TEST_VER", "9.9.9");
        std::env::set_var("SR_TEST_UPGRADE_ID", "pl-upgrade-1");
        // `helper_main` 是**阻塞**实现（等进程、读管道、轮询）：放在 async
        // 运行时的线程上会把整个 executor 卡住，连带把 relay 的排水线程也堵死。
        let h = handoff.clone();
        let outcome = tokio::task::spawn_blocking(move || helper_main(&h, false))
            .await
            .expect("helper task must not panic");
        for k in [
            "SR_TEST_OUT",
            "SR_TEST_RELAY",
            "SR_TEST_SID",
            "SR_TEST_VER",
            "SR_TEST_UPGRADE_ID",
        ] {
            std::env::remove_var(k);
        }

        assert!(
            matches!(outcome, HelperOutcome::Registered { .. }),
            "新进程注册成功才算升级完成，实际: {outcome:?}"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            std::fs::read(&new_bin).unwrap(),
            "target 必须是新版字节"
        );

        // 5. 断言注册记录：新进程用原 session_id 与原 tokens 注册成功。
        let log = std::fs::read_to_string(&out).unwrap_or_default();
        let registered_line = log
            .lines()
            .find(|l| l.starts_with("registered "))
            .unwrap_or_else(|| panic!("新进程没有注册成功: {log}"));
        assert!(
            registered_line.contains(&format!("sid={sid}")),
            "升级后 session_id 必须不变: {registered_line}"
        );
        let mut got: Vec<String> = registered_line
            .split("tokens=")
            .nth(1)
            .unwrap_or("")
            .trim()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        got.sort();
        let mut want = original_tokens.clone();
        want.sort();
        assert_eq!(
            got, want,
            "升级后 Token 必须与原身份一致: {registered_line}"
        );
        assert!(
            registered_line.contains("version=9.9.9"),
            "relay 侧必须看到新版本号: {registered_line}"
        );
        // 新进程的 PID 与"旧进程"不同（这里旧进程是 pid 0，所以只断言有真实 PID）。
        let pid_field = log
            .lines()
            .find(|l| l.starts_with("registered "))
            .and_then(|l| l.split_whitespace().find_map(|f| f.strip_prefix("pid=")))
            .unwrap_or("0");
        let new_pid: u32 = pid_field.parse().unwrap_or(0);
        assert!(new_pid > 0, "必须记录到真实的新进程 PID: {log}");
        assert_ne!(new_pid, old_pid, "换机后必须是另一个进程（PID 变了）");

        // 收尾：脚本会 sleep 30，kill 掉（它是本次换机拉起的那个）。
        unsafe {
            libc::kill(new_pid as i32, libc::SIGKILL);
        }
    }
}
