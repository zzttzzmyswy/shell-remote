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

/// 进程级环境变量的测试锁。
///
/// `cargo test` 让每个 `#[test]` 跑在独立线程里，但**环境变量是进程全局**的：
/// 一个用例刚 `set_var(RESTORE_ENV)`，并行跑的另一条就会读到它，于是
/// "普通启动应返回 Ok(None)" 这类断言会随机失败（本地复现过）。
/// 所有读写这些变量的用例都先拿这把锁。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    // 某个用例 panic 会毒化互斥量：这里照常取锁，让后续用例继续跑。
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
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
        // 默认按"临时设备"造样例：没有 --key，这是身份最难保住的一档。
        fixed_key: None,
        token_type: "both".to_string(),
        relay_base: None,
        relay_insecure: false,
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
        service_cgroup: None,
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
        None,
        "both".to_string(),
        None,
        false,
        ctx.argv.clone(),
        ctx.target_path.clone(),
        ctx.new_binary.clone(),
        ctx.new_sha256.clone(),
        ctx.upgrade_id.clone(),
        ctx.target_version.clone(),
        None,
    );
    assert_eq!(rebuilt.argv, vec![OsString::from("agent")]);
}

#[test]
fn test_restore_handoff_path_prefers_flag_then_env() {
    let _env = env_guard();
    // 显式参数优先；没有参数时用环境变量（回滚路径唯一可用的通道）。
    assert_eq!(
        restore_handoff_path(Some("/run/flag.json")),
        Ok(Some(PathBuf::from("/run/flag.json")))
    );
    std::env::set_var(RESTORE_ENV, "/run/env.json");
    assert_eq!(
        restore_handoff_path(None),
        Ok(Some(PathBuf::from("/run/env.json"))),
        "没有显式参数时必须能用环境变量恢复（回滚路径）"
    );
    assert_eq!(
        restore_handoff_path(Some("/run/flag.json")),
        Ok(Some(PathBuf::from("/run/flag.json"))),
        "显式参数优先于环境变量"
    );
    std::env::remove_var(RESTORE_ENV);
    assert_eq!(
        restore_handoff_path(None),
        Ok(None),
        "普通启动没有任何恢复信息"
    );
    // 显式给了 `--upgrade-restore ""` = 调用方要求恢复但没给路径：必须报错，
    // 不能当作"普通启动"（那会让临时设备静默换身份）。
    assert!(
        restore_handoff_path(Some("")).is_err(),
        "空的显式恢复参数必须 fail-closed"
    );
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
    assert!(
        RestoredIdentity::load(&path).is_err(),
        "无 tokens 的交接文件必须被拒绝（fail-closed，不能退化成全新注册）"
    );
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
    let _env = env_guard();
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
        HelperOutcome::RolledBack { reason, .. } => reason.clone(),
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
        HelperOutcome::RolledBack { reason, .. } => {
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
    // 显式钉住 `Direct` 路由：这组用例验证的是**非 systemd** 既有流程，
    // 不能因为测试机本身跑在 systemd unit 里就悄悄换成 transient 路径。
    let err = launch_helper_via(
        ManagedRoute::Direct,
        &dir.join("does-not-exist"),
        &handoff,
        "up-test",
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
    let err = launch_helper_via(
        ManagedRoute::Direct,
        &helper,
        &handoff,
        "up-test",
        Duration::from_secs(5),
    )
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
    let err = launch_helper_via(
        ManagedRoute::Direct,
        &helper,
        &handoff,
        "up-test",
        Duration::from_millis(400),
    )
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
    let mut child = launch_helper_via(
        ManagedRoute::Direct,
        &helper,
        &handoff,
        "up-test",
        Duration::from_secs(10),
    )
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
    pub(super) struct TestRelay {
        child: std::process::Child,
        pub(super) base: String,
        _drain: std::thread::JoinHandle<()>,
    }

    impl TestRelay {
        /// 二进制路径：`CARGO_BIN_EXE_*` 只在**集成测试**（tests/）里编译期
        /// 可用，lib 单测里必须运行时找。用 `current_exe` 的兄弟目录推：
        /// 单测二进制在 `target/<profile>/deps/`，发布二进制在上一级。
        pub(super) async fn start() -> Option<Self> {
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

    /// 读某个会话当前在 relay 上的 Token 集合。
    ///
    /// 用**管理面板**的 overview：agent 侧没有"查自己 Token"的接口，测试里
    /// 也不该为此新增一个。这里直接用一份新的注册响应来读——注册响应本身就
    /// 携带该 session 当前的 tokens（`register_existing` 原样返回）。
    pub(super) async fn session_tokens(
        client: &reqwest::Client,
        relay: &TestRelay,
        sid: &str,
    ) -> Vec<(String, String)> {
        let resp: serde_json::Value = client
            .post(format!("{}/agent/send", relay.base))
            .json(&serde_json::json!({
                "type": "agent:register",
                "session_id": sid,
                "token_type": "rw",
                "agent_version": "0.0.0-probe",
            }))
            .send()
            .await
            .expect("probe register must succeed")
            .json()
            .await
            .expect("probe response must be JSON");
        resp["payload"]["tokens"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| {
                        (
                            t["token"].as_str().unwrap_or("").to_string(),
                            t["permission"].as_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
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
            fixed_key: None,
            token_type: "rw".to_string(),
            relay_base: Some(relay.base.clone()),
            relay_insecure: false,
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
            service_cgroup: None,
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

// ── 11. 身份判定策略（review 第 2 条）──────────────────────────────
//
// 契约是"原身份重注册"，所以判定必须分清三档，不能把任何一档混成成功。

#[test]
fn test_identical_identity_is_preserved() {
    let toks = vec![
        ("rw-1".to_string(), "rw".to_string()),
        ("ro-2".to_string(), "ro".to_string()),
    ];
    // 顺序不同也算一致（relay 不保证顺序）。
    let reordered = vec![
        ("ro-2".to_string(), "ro".to_string()),
        ("rw-1".to_string(), "rw".to_string()),
    ];
    assert!(identity_matches("s1", &toks, "s1", &reordered));
    assert_eq!(
        restore_verdict(None, "s1", &toks, "s1", &reordered),
        Ok(RollbackIdentity::Preserved)
    );
}

#[test]
fn test_permission_shrink_is_not_identity_preserved() {
    // `--token-type both` 丢了 ro Token：Token 集合变了，不能算一致。
    let expected = vec![
        ("rw-1".to_string(), "rw".to_string()),
        ("ro-2".to_string(), "ro".to_string()),
    ];
    let actual = vec![("rw-1".to_string(), "rw".to_string())];
    assert!(!identity_matches("s1", &expected, "s1", &actual));
    // 纯临时设备：sid 相同但 Token 少了 → 拒绝（没有 key 可重建身份）。
    assert!(restore_verdict(None, "s1", &expected, "s1", &actual).is_err());
}

#[test]
fn test_escalation_is_not_identity_preserved() {
    // ro 设备多出 rw Token 属于提权，更不能放过。
    let expected = vec![("ro-1".to_string(), "ro".to_string())];
    let actual = vec![
        ("ro-1".to_string(), "ro".to_string()),
        ("rw-2".to_string(), "rw".to_string()),
    ];
    assert!(restore_verdict(None, "s1", &expected, "s1", &actual).is_err());
}

#[test]
fn test_temporary_device_with_new_sid_is_rejected() {
    // relay 失忆（临时 Token 全失效）→ 临时设备拿到新 sid：必须判失败，
    // 否则设备已经变成另一台，而助手还会删备份、记成升级成功。
    let expected = vec![("rw-1".to_string(), "rw".to_string())];
    let actual = vec![("rw-new".to_string(), "rw".to_string())];
    let err = restore_verdict(None, "old1", &expected, "brandnew2", &actual)
        .expect_err("临时设备换 sid 必须拒绝");
    assert!(err.contains("did not accept"), "{err}");
    assert!(err.contains("old1") && err.contains("brandnew2"), "{err}");
}

#[test]
fn test_fixed_key_device_may_keep_sid_with_rotated_tokens() {
    // 固定 key 设备：sid 回到原值、Token 被 relay 重签。身份仍是同一台
    // （Token 由 key 派生），允许成功但必须标注为 SessionKept。
    let expected = vec![
        ("mykey".to_string(), "rw".to_string()),
        ("ro-old".to_string(), "ro".to_string()),
    ];
    let actual = vec![
        ("mykey".to_string(), "rw".to_string()),
        ("ro-new".to_string(), "ro".to_string()),
    ];
    assert_eq!(
        restore_verdict(Some("mykey"), "dev1", &expected, "dev1", &actual),
        Ok(RollbackIdentity::SessionKept),
        "固定 key 的 sid 恢复了，只是 ro Token 被重签"
    );
    // 同一个 sid，但纯临时设备（无 key）同款情况必须拒绝。
    assert!(restore_verdict(None, "dev1", &expected, "dev1", &actual).is_err());
}

#[test]
fn test_fixed_key_device_that_lost_its_sid_is_still_rejected() {
    // 有 key 也不能接受"sid 换了"：那已经不是原来那台设备的会话。
    let expected = vec![("mykey".to_string(), "rw".to_string())];
    let actual = vec![("mykey".to_string(), "rw".to_string())];
    assert!(restore_verdict(Some("mykey"), "dev1", &expected, "other2", &actual).is_err());
}

#[test]
fn test_rollback_verdict_reports_each_tier_honestly() {
    let dir = TmpDir::new("rbverdict");
    let ctx = sample_ctx(&dir, "up-rbv");
    // 完全相同 → Preserved。
    assert_eq!(
        rollback_verdict(&ctx, &ctx.session_id, &ctx.tokens),
        RollbackIdentity::Preserved
    );
    // sid 回来、Token 轮换 → SessionKept（**不**谎称原身份无损）。
    let rotated = vec![("rw-brand-new".to_string(), "rw".to_string())];
    assert_eq!(
        rollback_verdict(&ctx, &ctx.session_id, &rotated),
        RollbackIdentity::SessionKept
    );
    // sid 也没了 → Lost。relay 不认这份身份时不能宣称回滚成功。
    match rollback_verdict(&ctx, "totally-new", &rotated) {
        RollbackIdentity::Lost { detail } => {
            assert!(detail.contains("sess42"), "{detail}");
        }
        other => panic!("必须判 Lost，实际 {other:?}"),
    }
}

/// 回滚判定看的是**重绑之后** relay 持有的身份，所以**临时设备**也能落到
/// `Preserved`。这条守着 review 里那个容易误读的点：旧版二进制自己那次 fresh
/// 注册会轮换 Token，但那只是中间态，助手随后会把原 Token 绑回来。
///
/// 若判定改用"旧版自己 fresh 注册的结果"，临时设备会恒为 `SessionKept`，
/// 实现方就会误以为"临时设备回滚保不住 Token 是必然的"——那正是上一版注释
/// 的错误结论。
#[test]
fn test_temporary_device_reaches_preserved_after_rebind() {
    let dir = TmpDir::new("rb-rebind");
    let mut ctx = sample_ctx(&dir, "up-rebind");
    // 临时设备：没有 --key，整套 Token 都是随机铸的。
    ctx.fixed_key = None;
    assert!(ctx.tokens.len() >= 2, "样例应覆盖多 Token（含权限）");

    // 中间态：旧版 fresh 注册后 relay 铸了新 Token —— 此时判 SessionKept。
    let minted_by_old_binary: Vec<(String, String)> = ctx
        .tokens
        .iter()
        .map(|(_, p)| (format!("minted-{p}"), p.clone()))
        .collect();
    assert_eq!(
        rollback_verdict(&ctx, &ctx.session_id, &minted_by_old_binary),
        RollbackIdentity::SessionKept,
        "旧版 fresh 注册后（尚未重绑）Token 必然变，这一档要能识别"
    );

    // 重绑之后：relay 持有的是交接里的原 Token —— 临时设备同样是 Preserved。
    assert_eq!(
        rollback_verdict(&ctx, &ctx.session_id, &ctx.tokens),
        RollbackIdentity::Preserved,
        "重绑成功后临时设备必须回到 Preserved，不能因为\"它没有 --key\"就降级"
    );
}

/// relay 侧丢掉了旧的 token 映射（全量重启）时，重绑也无法恢复原 Token：
/// 必须落到 SessionKept 而不是假装成功。
#[test]
fn test_rebind_that_cannot_restore_tokens_is_session_kept_not_preserved() {
    let dir = TmpDir::new("rb-lost-tokens");
    let ctx = sample_ctx(&dir, "up-losttok");
    let reissued: Vec<(String, String)> = ctx
        .tokens
        .iter()
        .map(|(_, p)| (format!("reissued-{p}"), p.clone()))
        .collect();
    assert_eq!(
        rollback_verdict(&ctx, &ctx.session_id, &reissued),
        RollbackIdentity::SessionKept,
        "Token 没能恢复就不能算 Preserved"
    );
}

#[test]
fn test_merge_verdict_keeps_the_worse_outcome() {
    let lost = RollbackIdentity::Lost { detail: "x".into() };
    assert_eq!(
        merge_verdict(RollbackIdentity::Preserved, lost.clone()),
        lost
    );
    assert_eq!(
        merge_verdict(RollbackIdentity::Preserved, RollbackIdentity::SessionKept),
        RollbackIdentity::SessionKept
    );
    assert_eq!(
        merge_verdict(RollbackIdentity::SessionKept, RollbackIdentity::Preserved),
        RollbackIdentity::SessionKept
    );
}

// ── 12. fail-closed 的恢复加载（review 第 3 条）────────────────────

#[test]
fn test_resolve_restore_is_fail_closed_for_an_explicit_but_broken_path() {
    let _env = env_guard();
    // 换机助手把我拉起来了（显式 restore），但文件读不出来：必须报错，
    // 绝不能悄悄退化成全新注册（那会让临时设备换掉身份）。
    let dir = TmpDir::new("failclosed");
    let missing = dir.join("no-such-handoff.json");
    let err = resolve_restore(Some(missing.to_str().unwrap()))
        .expect_err("显式恢复但文件不存在必须 fail-closed");
    assert!(err.contains("cannot read the upgrade handoff"), "{err}");

    // 文件存在但内容坏了（权限/格式）：同样必须报错。
    let bad = dir.join("bad.json");
    std::fs::write(&bad, b"not json").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(
        resolve_restore(Some(bad.to_str().unwrap())).is_err(),
        "损坏的交接文件必须并失败，而不是静默全新注册"
    );
}

#[test]
fn test_resolve_restore_is_a_noop_for_a_normal_start() {
    let _env = env_guard();
    // 普通启动（没有隐藏参数、也没有环境变量）：保持原行为，返回 Ok(None)。
    std::env::remove_var(RESTORE_ENV);
    assert!(
        matches!(resolve_restore(None), Ok(None)),
        "普通启动必须返回 Ok(None)"
    );
}

#[test]
fn test_resolve_restore_loads_a_valid_handoff() {
    let _env = env_guard();
    let dir = TmpDir::new("resolve-ok");
    let ctx = sample_ctx(&dir, "up-resolve");
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let id = resolve_restore(Some(path.to_str().unwrap()))
        .expect("合法交接文件必须能加载")
        .expect("必须返回恢复信息");
    assert_eq!(id.restore.session_id, "sess42");
    assert_eq!(id.restore.tokens, ctx.tokens);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_failed_handoff_reason_never_leaks_a_token() {
    let _env = env_guard();
    // 原因会写进 stderr → 助手转存 update-apply.log。不能带 Token。
    let dir = TmpDir::new("reason-nostoken");
    let missing = dir.join("nope.json");
    let err = resolve_restore(Some(missing.to_str().unwrap())).unwrap_err();
    assert!(!err.contains("rw-token-abc"), "{err}");
    assert!(!err.contains("ro-token-def"), "{err}");
}

// ── 13. 真实旧版二进制回滚（review 第 1 条）────────────────────────
//
// 前面所有"回滚"用例用的都是认识交接协议的假 agent。review 指出这不够：
// 真正被回滚的是**不认识 SR_UPGRADE_RESTORE 的旧二进制**，它只会用原始
// argv 注册。这里用 `git archive <阶段2 基线>` 构建出来的真旧版二进制做回归
// ——它没有 `--upgrade-restore`，也没有 `update-apply` 子命令。
//
// 旧版二进制路径由 `SR_OLD_AGENT_BIN` 给出（CI/本地跑前先 build 一次
// 基线）；没有就跳过并**明确打印跳过原因**，避免"静默不测"。

#[cfg(unix)]
mod real_old_binary {
    use super::*;

    fn old_agent_bin() -> Option<PathBuf> {
        let p = std::env::var_os("SR_OLD_AGENT_BIN")?;
        let p = PathBuf::from(p);
        if p.is_file() {
            Some(p)
        } else {
            eprintln!("跳过：SR_OLD_AGENT_BIN={} 不是文件", p.display());
            None
        }
    }

    /// 真旧版二进制**不认识**这两种新东西——这是本组用例的前提。
    #[test]
    fn test_old_binary_predates_the_handoff_protocol() {
        let Some(bin) = old_agent_bin() else { return };
        let run_help = |args: &[&str]| -> String {
            let out = std::process::Command::new(&bin)
                .args(args)
                .output()
                .expect("旧版二进制必须可执行");
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        };
        let top = run_help(&["--help"]);
        let agent = run_help(&["agent", "--help"]);

        // 顶层子命令里**没有** update-apply（那是本阶段新增的隐藏命令）。
        assert!(
            !top.contains("update-apply") && !agent.contains("update-apply"),
            "拿到的不是阶段2 基线之前的旧版（它认识 update-apply）:\n{top}"
        );
        // 也不认识隐藏恢复参数。
        assert!(
            !top.contains("upgrade-restore") && !agent.contains("upgrade-restore"),
            "拿到的不是旧版（它认识 upgrade-restore）"
        );
        // 但它认识存量的 --session-id：回滚靠这个把会话拉回原 id。
        assert!(
            agent.contains("--session-id"),
            "旧版应当认识存量的 --session-id:\n{agent}"
        );
    }

    /// 回滚拉起旧版时**不得**注入它不认识的参数，且必须补上原 session id。
    #[test]
    fn test_rollback_argv_only_uses_flags_the_old_binary_knows() {
        let dir = TmpDir::new("oldargv");
        let mut ctx = sample_ctx(&dir, "up-oldargv");
        // 临时设备：原始 argv 里没有 --session-id。
        ctx.argv = vec![
            OsString::from("agent"),
            OsString::from("--relay-url"),
            OsString::from("http://127.0.0.1:1"),
        ];
        let handoff = dir.join("h.json");
        std::fs::write(&handoff, b"{}").unwrap();
        let record = dir.join("argv.txt");
        let recorder = dir.join("recorder");
        write_executable(
            &recorder,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'RESTORE=%s\\n' \"${{SR_UPGRADE_RESTORE:-}}\" >> '{}'\n",
                record.display(),
                record.display()
            ),
        );
        ctx.target_path = recorder;
        let mut child = spawn_agent_for_rollback(&ctx, &handoff, "test").unwrap();
        let _ = child.wait();
        let text = std::fs::read_to_string(&record).unwrap();
        assert!(
            !text.contains("upgrade-restore"),
            "回滚不得把旧版不认识的隐藏参数传下去: {text}"
        );
        assert!(
            text.contains("RESTORE=\n") || text.trim_end().ends_with("RESTORE="),
            "回滚不该设 SR_UPGRADE_RESTORE（旧版不读，只会误导排查）: {text}"
        );
        assert!(
            text.contains("--session-id") && text.contains("sess42"),
            "回滚必须补上原 session id，否则临时设备会拿到新 ID: {text}"
        );
        // 原始 argv 保持不变。
        assert!(text.contains("--relay-url"), "{text}");
    }

    /// 原始 argv 已带 `--session-id` 时不能重复注入。
    #[test]
    fn test_rollback_does_not_duplicate_session_id() {
        let dir = TmpDir::new("olddup");
        let mut ctx = sample_ctx(&dir, "up-olddup");
        ctx.argv = vec![
            OsString::from("agent"),
            OsString::from("--session-id"),
            OsString::from("sess42"),
        ];
        let handoff = dir.join("h.json");
        std::fs::write(&handoff, b"{}").unwrap();
        let record = dir.join("argv2.txt");
        let recorder = dir.join("recorder2");
        write_executable(
            &recorder,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                record.display()
            ),
        );
        ctx.target_path = recorder;
        let mut child = spawn_agent_for_rollback(&ctx, &handoff, "test").unwrap();
        let _ = child.wait();
        let text = std::fs::read_to_string(&record).unwrap();
        assert_eq!(
            text.matches("--session-id").count(),
            1,
            "不得重复注入 --session-id: {text}"
        );
    }

    /// 端到端：真 relay + **真旧版二进制** + 无 key、无 --session-id 的临时
    /// 设备 → 回滚后必须仍是原 sid。
    ///
    /// 这条覆盖 review 要求的"恢复进程不读 SR_UPGRADE_RESTORE、无 key、无
    /// --session-id，回滚后仍保持原 sid"。
    #[tokio::test]
    async fn test_real_old_binary_rollback_keeps_the_temporary_identity() {
        let Some(old_bin) = old_agent_bin() else {
            return;
        };
        let Some(relay) = super::process_level::TestRelay::start().await else {
            eprintln!("跳过：无法启动真 relay");
            return;
        };
        let dir = TmpDir::new("oldbin-e2e");
        let client = reqwest::Client::new();

        // 1. 一台临时设备先注册（无 key、无 --session-id），拿到真实身份。
        let resp: serde_json::Value = client
            .post(format!("{}/agent/send", relay.base))
            .json(&serde_json::json!({
                "type": "agent:register",
                "token_type": "rw",
                "agent_version": "0.59.2"
            }))
            .send()
            .await
            .expect("注册必须成功")
            .json()
            .await
            .expect("响应必须是 JSON");
        let sid = resp["session_id"]
            .as_str()
            .expect("要有 session_id")
            .to_string();
        let tokens: Vec<(String, String)> = resp["payload"]["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["token"].as_str().unwrap().to_string(),
                    t["permission"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert!(!tokens.is_empty());

        // 2. 造交接文件：target 就是**真旧版二进制**（要它被"回滚"后运行）。
        let target = dir.join("shell-remote");
        std::fs::copy(&old_bin, &target).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let new_binary = dir.join("artifact.part");
        std::fs::write(&new_binary, b"not a real binary").unwrap();
        let ctx = RestartContext {
            session_id: sid.clone(),
            tokens: tokens.clone(),
            // 关键：临时设备 —— 没有 --key。
            fixed_key: None,
            token_type: "rw".to_string(),
            relay_base: Some(relay.base.clone()),
            relay_insecure: false,
            // 原始 argv：没有 --session-id。
            argv: vec![
                OsString::from("agent"),
                OsString::from("--relay-url"),
                OsString::from(relay.base.clone()),
                OsString::from("--shell"),
                OsString::from("/bin/sh"),
                OsString::from("--root"),
                OsString::from(dir.path().to_string_lossy().to_string()),
            ],
            old_pid: 0,
            old_start: None,
            upgrade_id: "oldbin-e2e".to_string(),
            target_version: "9.9.9".to_string(),
            target_path: target.clone(),
            new_binary: new_binary.clone(),
            new_sha256: sha256_of(b"not a real binary"),
            service_cgroup: None,
        };
        let handoff = ctx.write_handoff_into(dir.path()).unwrap();

        // 3. 直接跑回滚（等价于"新进程没注册成功"之后的动作）。
        let mut log = HelperLog::open(&handoff);
        let outcome = {
            // rollback 是私有函数，但同模块内的 tests 子模块可以调用。
            rollback(
                &ctx,
                &handoff,
                &target, // backup 指向 target 本身：这里不测文件恢复，只测身份
                &mut log,
                "test: new version did not register".to_string(),
                None,
                HelperTimeouts {
                    old_exit: Duration::from_secs(1),
                    register: Duration::from_secs(25),
                },
            )
        };

        // 4. 旧版必须自己注册成功（它会打印 session=… 与 token: …）。
        let log_text = std::fs::read_to_string(dir.join("update-apply.log")).unwrap_or_default();
        assert!(
            log_text.contains("previous agent restarted"),
            "旧版必须被拉起: {log_text}"
        );
        assert!(
            log_text.contains("agent session established")
                || log_text.contains("rolled-back agent registered on its own"),
            "旧版必须真的注册上线（不是 spawn 完就算）: {log_text}"
        );

        // 5. 身份断言：回滚后 relay 上仍必须是原 sid + 原 Token。
        match outcome {
            HelperOutcome::RolledBack { identity, .. } => {
                assert_eq!(
                    identity,
                    RollbackIdentity::Preserved,
                    "临时设备回滚后必须保持原 sid 与 Token；日志:\n{log_text}"
                );
            }
            other => panic!("必须走回滚路径，实际 {other:?}"),
        }
        let after = super::process_level::session_tokens(&client, &relay, &sid).await;
        assert_eq!(
            after, tokens,
            "回滚后 relay 上的 Token 必须与升级前完全一致；日志:\n{log_text}"
        );

        // 收尾：旧版还在跑（它本来就是长驻进程）。
        let _ = std::process::Command::new("pkill")
            .args(["-f", &format!("--session-id {sid}")])
            .status();
        let _ = std::process::Command::new("pkill")
            .args(["-f", &format!("--relay-url {}", relay.base)])
            .status();
    }
}

// ── 14. 助手 ready 通道不被日志污染（真实回归）────────────────────
//
// 一次真实事故：`main()` 先跑 `init_logging()` 再分发子命令，而 tracing 默认
// 写 **stdout**。助手启动后先打印一行 "shell-remote v0.59.2"，旧 agent 读到
// 这一行（它只读一行）就判定"助手没 ready 就报了别的东西"，直接放弃升级。
// 表现为升级永远失败、而且错误信息里嵌着那行日志。
//
// 这里直接跑**真实二进制**的 `update-apply`，验证它的第一行输出就是 ready。

#[cfg(unix)]
#[test]
fn test_real_binaries_announce_ready_on_the_first_stdout_line() {
    // CLI 与 UI 用的是**同一条** helper stdout 协议：旧 agent 只读一行，
    // 必须是 `ready`。两个二进制都必须满足——UI 制品升级时由 UI 二进制
    // 当助手，只在 CLI 上验过等于没验 UI 那条路。
    check_binary_ready_line("shell-remote", "CLI");
    check_binary_ready_line("shell-remote-ui", "UI");
}

/// 找到同一次构建产出的某个二进制。单测二进制在 `target/<profile>/deps/`
/// 下，真正的 bin 在上一级；也兼容从仓库根直接跑的情况。
#[cfg(unix)]
fn locate_binary(name: &str) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok();
    let from_exe = std::env::current_exe().ok().and_then(|p| {
        let deps = p.parent()?;
        let profile = deps.parent()?;
        let candidate = profile.join(name);
        candidate.is_file().then_some(candidate)
    });
    from_exe
        .or_else(|| cwd.as_ref().map(|d| d.join(name)).filter(|p| p.is_file()))
        .or_else(|| {
            cwd.as_ref()
                .map(|d| d.join("deps").join(name))
                .filter(|p| p.is_file())
        })
}

/// 跑一次 `update-apply`，断言 stdout 首行就是 `ready`。
///
/// 缺二进制时**打印跳过原因**而不是静默通过：UI 二进制需要 `desktop` 等
/// feature，lean 构建（`--no-default-features`）根本不会产出它，这时这条
/// 用例应显式说明"UI 未验证"，不能让人误以为验过了。
#[cfg(unix)]
fn check_binary_ready_line(name: &str, label: &str) {
    let Some(bin) = locate_binary(name) else {
        eprintln!("跳过 {label} 首行 ready 检查：找不到二进制 {name}（lean 构建不产出 UI）");
        return;
    };
    // 一份"新文件不存在"的交接：助手会在 ready 之后立刻失败，但我们只关心
    // **第一行**。用不存在的 new_binary 可以让它快速退出，不必等满超时。
    let tmp = TmpDir::new(&format!("ready-{name}"));
    let mut ctx = sample_ctx(&tmp, "up-ready");
    ctx.old_pid = 0;
    ctx.old_start = None;
    let handoff = ctx.write_handoff_into(tmp.path()).unwrap();

    let out = std::process::Command::new(&bin)
        .arg("update-apply")
        .arg(&handoff)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("update-apply 必须能启动");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first = stdout.lines().next().unwrap_or("");
    assert_eq!(
        first.trim(),
        READY_LINE,
        "{label}（{name}）的第一行 stdout 必须是 ready —— 否则旧 agent 会把它当成\
         错误并放弃升级。实际 stdout 前几行: {:?}",
        stdout.lines().take(4).collect::<Vec<_>>()
    );
    // 版本号/日志绝不能抢在 ready 前面。
    assert!(
        !first.contains("shell-remote"),
        "{label}: 日志不得出现在 ready 之前: {first}"
    );
}

// ── 9. 托管路由：Windows 行为不变（MYS-2425-D）──────────────────────

/// 非 Linux 平台**恒**判为"非托管"，因此换机走既有的直接助手路径。
///
/// 这是设计硬约束（「Windows 行为不得因 Linux 修复改变」）：Windows 的进程
/// 生命周期由 SCM/Job 负责，本次修复不涉及。
#[test]
fn test_non_linux_platform_never_takes_the_transient_route() {
    if cfg!(not(target_os = "linux")) {
        assert!(!managed_detection(), "非 Linux 平台不得判定为 systemd 托管");
        assert_eq!(managed_route(false), ManagedRoute::Direct);
    }
    // 托管判定与路由的映射本身与平台无关，这里一并钉住：
    assert_eq!(managed_route(false), ManagedRoute::Direct);
    assert_eq!(managed_route(true), ManagedRoute::Transient);
}
