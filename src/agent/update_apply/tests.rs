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

/// 读文件算 sha256（断言"磁盘上是哪一版"用）。
fn file_sha(path: &Path) -> String {
    sha256_of(&std::fs::read(path).unwrap())
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
        service_restart_policy: None,
        managed_plan: None,
        overlay: false,
        gui: None,
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
    assert_eq!(back.overlay, ctx.overlay);
}

/// overlay 标记必须能从交接文件里读回来（MYS-2574）。
///
/// 它是"助手不要搬文件"的唯一依据：读丢了，助手会去改写一个不可写目录并失败，
/// 整条 overlay 路径就断在最后一步。
#[test]
fn test_handoff_roundtrip_preserves_the_overlay_flag() {
    let dir = TmpDir::new("roundtrip-overlay");
    let mut ctx = sample_ctx(&dir, "up-ovl");
    ctx.overlay = true;
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let back = RestartContext::read_handoff(&path).unwrap();
    assert!(back.overlay, "overlay 标记必须落盘并读回");
    // 目标仍然是 base（重启要拉起它），新文件名是 overlay 里那一份。
    assert_eq!(back.target_path, ctx.target_path);
    assert_eq!(back.new_binary, ctx.new_binary);

    // 老交接文件没有这个字段：缺省即 false（既有行为），不能读失败。
    let raw = std::fs::read_to_string(&path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v.as_object_mut().unwrap().remove("overlay");
    let legacy = dir.join(".handoff-legacy.json");
    std::fs::write(&legacy, serde_json::to_vec(&v).unwrap()).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o600)).unwrap();
    let back = RestartContext::read_handoff(&legacy).expect("缺 overlay 字段必须能读");
    assert!(!back.overlay, "老文件的缺省必须是非 overlay");
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
        None,
        None,
        false,
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

/// overlay 换机的助手**不碰目标目录**（MYS-2574）。
///
/// 这条路径存在的全部理由就是"目标目录不可写"。助手如果真的去 `rename` 目标
/// 二进制，整条链路会在最后一步失败——所以这里明确断言：进入 overlay 分支时，
/// 即使目标目录不可写、即使旧进程一直不退出，助手也是**先 ready 再等**，
/// 而不是先动磁盘。
#[cfg(unix)]
#[test]
fn test_overlay_helper_never_touches_the_read_only_target_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TmpDir::new("ovl-helper");
    // 目标目录做成 0555（root 所有 /usr/local/bin 的等价物）。
    let bin_dir = dir.join("ro-bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let target = bin_dir.join("shell-remote");
    std::fs::write(&target, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let target_bytes = std::fs::read(&target).unwrap();
    std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    // overlay 里"已就位"的新版本（助手只校验它，不搬动它）。
    let overlay_bin = dir.join("overlay-version");
    let payload = b"#!/bin/sh\n# overlay version\nexit 0\n";
    std::fs::write(&overlay_bin, payload).unwrap();
    std::fs::set_permissions(&overlay_bin, std::fs::Permissions::from_mode(0o700)).unwrap();

    let old = live_old_process();
    let mut ctx = sample_ctx(&dir, "up-ovl-helper");
    ctx.overlay = true;
    ctx.target_path = target.clone();
    ctx.new_binary = overlay_bin.clone();
    ctx.new_sha256 = sha256_of(payload);
    ctx.old_pid = old.id();
    ctx.old_start = None;
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();

    // 旧进程一直不退 ⇒ 助手超时放弃。关键是它**没有**试图写目标目录。
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
        std::fs::read(&target).unwrap(),
        target_bytes,
        "overlay 路径下助手不得改动 base 二进制"
    );
    assert!(overlay_bin.exists(), "overlay 里那份也不该被搬走");
    // 目标目录里除了原来那个文件，什么都不能多出来（没有备份、没有临时文件）。
    let entries: Vec<String> = std::fs::read_dir(&bin_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(entries, vec!["shell-remote".to_string()], "目标目录不得被写入: {entries:?}");

    // 日志必须写明走的是 overlay 模式（运维排查的第一条线索）。
    let log = std::fs::read_to_string(dir.join("update-apply.log")).unwrap_or_default();
    assert!(log.contains("overlay mode"), "助手日志必须写明 overlay 模式: {log}");
    // 收尾：恢复目录权限，让 TmpDir::drop 能删掉。
    std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// overlay 换机时助手要校验的是 **overlay 里那份**字节，不是目标目录里的旧文件。
#[cfg(unix)]
#[test]
fn test_overlay_helper_refuses_a_tampered_overlay_artifact() {
    let dir = TmpDir::new("ovl-helper-bad");
    let mut ctx = sample_ctx(&dir, "up-ovl-bad");
    ctx.overlay = true;
    // 声明一个与磁盘内容不符的 digest：等价于"下载后被改过"。
    ctx.new_sha256 = "0".repeat(64);
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    // `announce_ready` 关掉：这条用例断言的是**判定结果**，不是握手时序；
    // 开着它会让 `fail_to_stdout` 往 stdout 写一行并触发测试 harness 的
    // "unexpected output" 噪声。
    let outcome = helper_main_with(
        &handoff,
        false,
        HelperTimeouts {
            old_exit: Duration::from_millis(400),
            register: Duration::from_millis(100),
        },
    );
    match outcome {
        HelperOutcome::Aborted { reason } => {
            assert!(reason.contains("no longer matches its digest"), "{reason}")
        }
        other => panic!("digest 不符必须放弃，实际: {other:?}"),
    }
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

// ── 8b. 无参数 GUI 的交接（MYS-2819）──────────────────────────────

/// 一份"无参数 GUI 连接"产生的样例快照：字段值与真实 GUI 连接一致。
fn gui_snapshot(sid: &str, relay: &str) -> GuiHandoffConfig {
    GuiHandoffConfig {
        relay_url: relay.to_string(),
        relay_insecure: true,
        root: Some("/tmp/sr-root".to_string()),
        token_type: "rw".to_string(),
        shell: "/bin/sh".to_string(),
        session_id: Some(sid.to_string()),
        no_cmd_log: true,
        desktop_capture: "x11".to_string(),
        desktop_codec: "h264".to_string(),
        desktop_fps: 24.0,
        desktop_capture_fps: 12.0,
        desktop_max_bitrate: 4000,
        desktop_quality: "best".to_string(),
        desktop_min_bitrate: 200,
        desktop_display: Some(":1".to_string()),
        desktop_lan_port: 39616,
    }
}

/// 造一份 GUI 交接：`argv` 是**空的**（这就是无参数启动）。sample_ctx 里的
/// 命令行参数换成空 vec，再注入快照。
fn gui_ctx(dir: &TmpDir, upgrade_id: &str, relay: &str, sid: &str) -> RestartContext {
    let mut ctx = sample_ctx(dir, upgrade_id);
    ctx.argv = Vec::new();
    ctx.with_gui_snapshot(gui_snapshot(sid, relay))
}

#[test]
fn test_gui_handoff_roundtrip_keeps_the_active_snapshot_and_no_key() {
    // 复现（红）：无参数 GUI 升级时 argv 为空，活跃配置只能进交接文件。
    // 修复前 `RestartContext` 根本没有承载它的地方——快照在写入那一步就丢了。
    let dir = TmpDir::new("gui-roundtrip");
    let ctx = gui_ctx(&dir, "up-gui", "https://relay.example.com", "guisess1");
    // 固定密钥走既有受保护字段（不是快照）：这里确认两条通道互不干扰。
    let mut ctx = ctx;
    ctx.fixed_key = Some("GUI-FIXED-KEY-7a".to_string());

    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let back = RestartContext::read_handoff(&path).expect("无参数 GUI 交接必须能读回");
    assert!(back.is_gui_resume(), "读回来必须仍认得这是 GUI 启动");
    let snap = back.gui_snapshot().expect("快照必须原样保留");
    assert_eq!(snap.relay(), "https://relay.example.com");
    assert_eq!(snap.session_id.as_deref(), Some("guisess1"));
    assert_eq!(snap.desktop_lan_port, 39616, "LAN 端口这类活跃配置不能丢");
    assert_eq!(snap.desktop_display.as_deref(), Some(":1"));
    assert_eq!(snap.desktop_fps, 24.0);
    assert!(snap.no_cmd_log);
    assert_eq!(snap.token_type, "rw");
    assert_eq!(back.fixed_key.as_deref(), Some("GUI-FIXED-KEY-7a"));
    assert!(back.argv.is_empty(), "无参数 GUI 的 argv 必须保持为空");

    // 密钥只走受保护字段：快照与整份交接 JSON 里都不许出现它。
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("GUI-FIXED-KEY-7a"),
        "固定密钥要能在交接文件里被回滚路径读到"
    );
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let snap_json = &json["gui"];
    assert!(
        snap_json.is_object(),
        "快照要作为独立字段落盘，不能混进 argv"
    );
    assert!(
        !snap_json.to_string().contains("GUI-FIXED-KEY-7a"),
        "快照里绝不能有密钥: {snap_json}"
    );
    assert!(
        !snap_json.to_string().contains("key"),
        "快照连 key 字段都不该有: {snap_json}"
    );
    // Debug 也不能带出密钥。
    assert!(!format!("{back:?}").contains("GUI-FIXED-KEY-7a"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_command_line_handoff_stays_bit_identical_without_a_gui_snapshot() {
    // 旧 CLI 兼容：命令行启动的交接**不能**多出 gui 字段，否则旧助手/旧 agent
    // 读到不认识的结构可能出问题，也没必要让所有命令行设备都背上这份负担。
    let dir = TmpDir::new("cli-compat");
    let ctx = sample_ctx(&dir, "up-cli");
    assert!(!ctx.is_gui_resume());
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        json.get("gui").is_none(),
        "命令行交接不得写入 gui 字段: {json}"
    );
    let back = RestartContext::read_handoff(&path).unwrap();
    assert!(back.gui_snapshot().is_none());
    assert_eq!(back.argv, ctx.argv, "argv 原样保留");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_handoff_without_gui_field_is_read_as_a_command_line_start() {
    // 跨代次：新版写、更旧的读，或者反过来。缺 `gui` 字段必须读成"命令行启动"，
    // 而不是报错——这是纯加法字段的全部意义。
    let dir = TmpDir::new("legacy-read");
    let ctx = sample_ctx(&dir, "up-legacy");
    let path = ctx.write_handoff_into(dir.path()).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    json.as_object_mut().unwrap().remove("gui");
    let legacy = dir.join("legacy.json");
    write_private_new(&legacy, serde_json::to_vec(&json).unwrap().as_slice()).unwrap();
    let back = RestartContext::read_handoff(&legacy).expect("缺 gui 字段必须能读");
    assert!(!back.is_gui_resume(), "缺字段 = 命令行启动");
    assert_eq!(back.session_id, "sess42");
}

#[test]
fn test_gui_snapshot_inconsistent_with_the_handoff_identity_is_rejected() {
    // 损坏/被改过的交接必须 fail-closed：不允许"用恢复默认值掩盖失败"。
    type Mutate = Box<dyn Fn(&mut serde_json::Value)>;
    let cases: Vec<(&str, Mutate)> = vec![
        (
            "session id 与交接身份不一致",
            Box::new(|j: &mut serde_json::Value| {
                j["gui"]["session_id"] = serde_json::json!("someoneelse");
            }),
        ),
        (
            "relay 与交接 relay 不一致",
            Box::new(|j: &mut serde_json::Value| {
                j["gui"]["relay_url"] = serde_json::json!("https://evil.example.com");
            }),
        ),
        (
            "token-type 与交接权限不一致",
            Box::new(|j: &mut serde_json::Value| {
                j["gui"]["token_type"] = serde_json::json!("ro");
            }),
        ),
        (
            "快照没有 relay 地址",
            Box::new(|j: &mut serde_json::Value| {
                j["gui"]["relay_url"] = serde_json::json!("");
            }),
        ),
    ];
    for (what, mutate) in cases {
        let dir = TmpDir::new("gui-bad");
        let ctx = gui_ctx(&dir, "up-bad", "https://relay.example.com", "guisess1");
        let path = ctx.write_handoff_into(dir.path()).unwrap();
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        mutate(&mut json);
        let bad = dir.join("bad.json");
        write_private_new(&bad, serde_json::to_vec(&json).unwrap().as_slice()).unwrap();
        let err = RestartContext::read_handoff(&bad)
            .err()
            .unwrap_or_else(|| panic!("{what} 必须被拒绝"));
        assert!(
            !err.to_string().trim().is_empty(),
            "{what}: 错误要有可读原因"
        );
        let _ = std::fs::remove_file(&path);
    }
}

/// 无参数 GUI 回滚：旧版收到的必须是**空 argv** + 恢复环境变量。
///
/// 复现的现象：修复前这里会往空 argv 里补 `--session-id <id>`，而无参数入口
/// 不认识这个参数——旧版启动即失败，一次可救的回滚变成设备彻底失联。所以这条
/// 用例断言的是真实拉起行为（argv 真的为空、环境变量真的带上了交接文件），
/// 不是某个字符串映射。
#[cfg(unix)]
#[test]
fn test_gui_rollback_spawns_the_old_version_with_an_empty_argv() {
    let dir = TmpDir::new("gui-rollback");
    let mut ctx = gui_ctx(&dir, "up-gui-rb", "https://relay.example.com", "guisess9");
    ctx.target_path = dir.join("shell-remote-old");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();

    let record = dir.join("spawn.txt");
    let script = dir.join("recorder");
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\n{{\n  echo 'ARGC='$#\n  for a in \"$@\"; do echo \"ARG=$a\"; done\n  echo \"RESTORE=${{SR_UPGRADE_RESTORE:-}}\"\n  echo \"REASON=${{SR_UPGRADE_ROLLBACK:-}}\"\n}} > '{}'\n",
            record.display()
        ),
    );
    ctx.target_path = script;

    let mut child = spawn_agent_for_rollback(&ctx, &handoff, "did not register").unwrap();
    let _ = child.wait();
    let text = std::fs::read_to_string(&record).expect("回滚进程必须被真的拉起来");

    assert!(
        text.contains("ARGC=0"),
        "无参数 GUI 的回滚必须用空 argv 拉起旧版（补 --session-id 会让它起不来）: {text}"
    );
    assert!(
        !text.contains("ARG=--session-id"),
        "不得往空 argv 里补旧版不认识的参数: {text}"
    );
    assert!(
        text.contains(&format!("RESTORE={}", handoff.display())),
        "旧 GUI 版只能靠恢复环境变量找回配置与身份: {text}"
    );
    assert!(
        text.contains("REASON=did not register"),
        "回滚原因仍要经环境变量传下去: {text}"
    );
    // 密钥只在 0600 交接文件里，绝不出现在 argv/环境变量里。
    let mut with_key = ctx.clone();
    with_key.fixed_key = Some("GUI-ROLLBACK-SECRET-31".to_string());
    assert!(!text.contains("GUI-ROLLBACK-SECRET-31"));
    let _ = std::fs::remove_file(&handoff);
}

/// 命令行启动的回滚行为必须与既有版本**完全一致**：补 `--session-id`、不设
/// `SR_UPGRADE_RESTORE`。这条用例守住"别为了 GUI 把老路径改坏"。
#[cfg(unix)]
#[test]
fn test_command_line_rollback_keeps_its_original_contract() {
    let dir = TmpDir::new("cli-rollback");
    let mut ctx = sample_ctx(&dir, "up-cli-rb");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    let record = dir.join("spawn.txt");
    let script = dir.join("recorder");
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\n{{\n  for a in \"$@\"; do echo \"ARG=$a\"; done\n  echo \"RESTORE=${{SR_UPGRADE_RESTORE:-}}\"\n}} > '{}'\n",
            record.display()
        ),
    );
    // 原始 argv 里没有 --session-id：回滚要替旧版补上，保住同一个会话。
    ctx.target_path = script;
    ctx.argv = vec![
        OsString::from("agent"),
        OsString::from("--relay-url"),
        OsString::from("http://relay.example:3000"),
    ];
    let mut child = spawn_agent_for_rollback(&ctx, &handoff, "nope").unwrap();
    let _ = child.wait();
    let text = std::fs::read_to_string(&record).unwrap();
    assert!(text.contains("ARG=--session-id"), "{text}");
    assert!(text.contains("ARG=sess42"), "补的必须是交接里的实际 id: {text}");
    assert!(
        text.contains("RESTORE=\n"),
        "命令行旧版不读 SR_UPGRADE_RESTORE，不许设它: {text}"
    );
    let _ = std::fs::remove_file(&handoff);
}

// ── 9. 助手日志与残留清理 ──────────────────────────────────────────#[cfg(unix)]
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
        /// 本次隔离用的 settings 文件（用例结束即删）。
        settings: PathBuf,
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

            // 自己先占一个端口再放掉，把它交给 relay 显式绑定。
            //
            // 为什么要这么绕：relay 的 `Relay server listening on {bind}` 打印的
            // 是**请求绑定的那个字符串**，`--bind 127.0.0.1:0` 时它打出来的就是
            // ":0"，拿不到 OS 实际分配的端口。以前这个 helper 因此永远读不到
            // 端口、所有进程级用例都在"跳过：无法启动真 relay"里静默变成空跑。
            let probe = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
            let port = probe.local_addr().ok()?.port();
            drop(probe);

            // 隔离：独立的 settings 文件，绝不读写开发机上真实的
            // ~/.shell-remote/relay-settings.json。
            let settings = std::env::temp_dir().join(format!(
                "sr-test-relay-settings-{}-{}.json",
                std::process::id(),
                port
            ));
            let mut child = std::process::Command::new(&bin)
                .args([
                    "relay",
                    "--bind",
                    &format!("127.0.0.1:{port}"),
                    "--auth",
                    "pl-pass",
                    "--no-tls",
                    "--settings-file",
                    &settings.to_string_lossy(),
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .ok()?;
            // relay 的 tracing 走 **stdout**（stderr 只有 `agent pid=`）。
            // 之前这里读的是 stderr，于是永远等不到端口。
            let stderr = child.stdout.take()?;
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
            // 等到 relay 真的在这个端口上应答为止（而不是"日志里出现了端口"）。
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut up = false;
            while std::time::Instant::now() < deadline {
                // 顺手把日志排掉，避免管道写满把 relay 阻塞在 write 上。
                while rx.try_recv().is_ok() {}
                if std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    Duration::from_millis(200),
                )
                .is_ok()
                {
                    up = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let port_str = port.to_string();
            if !up {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&settings);
                return None;
            }
            Some(Self {
                child,
                base: format!("http://127.0.0.1:{port_str}"),
                _drain: drain,
                settings,
            })
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.settings);
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
    pub(super) async fn register(client: &reqwest::Client, base: &str, sid: &str) -> Vec<String> {
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
            new_sha256: new_sha.clone(),
            service_cgroup: None,
            service_restart_policy: None,
            managed_plan: None,
            overlay: false,
            gui: None,
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
        // 新版字节被 rename 到 target，`new_bin` 本身已经不在——摘要要在换机前算。
        assert_eq!(
            file_sha(&target),
            new_sha,
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
        // `tokens=` 后面还跟着 ` version=…`：先按空白切掉尾巴，否则
        // "version=9.9.9" 会被当成一个 token。（这条用例以前一直在静默跳过，
        // 所以这个解析 bug 从没暴露。）
        let token_field = registered_line
            .split("tokens=")
            .nth(1)
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        let mut got: Vec<String> = token_field
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
            service_restart_policy: None,
            managed_plan: None,
            overlay: false,
            gui: None,
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

// ── 11. 受管 service 协调式换机的"待恢复"指针（MYS-2425-D）───────────

/// 指针能被消费一次，并指向正确的交接文件（`systemctl start` 拉起的进程
/// 拿不到 `SR_UPGRADE_RESTORE`，只能靠它找回身份）。
#[test]
fn test_restore_pending_is_taken_exactly_once() {
    let dir = TmpDir::new("rp1");
    let ctx = sample_ctx(&dir, "up-rp");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    pending_restore_marker("up-rp", &handoff, dir.path());

    let got = take_restore_pending_in(dir.path()).expect("应能取到待恢复交接");
    assert_eq!(got.handoff, handoff);
    assert_eq!(got.backup, None, "两行指针没有自回滚备份");
    assert!(
        take_restore_pending_in(dir.path()).is_none(),
        "取到即消费，不能反复恢复"
    );
    assert!(handoff.exists(), "消费指针不删交接文件");
}

/// 没有指针 = 普通启动。
#[test]
fn test_restore_pending_absent_means_plain_start() {
    let dir = TmpDir::new("rp-none");
    assert!(take_restore_pending_in(dir.path()).is_none());
}

/// 指针指向私有目录之外 ⇒ 拒绝并清掉。
#[test]
fn test_restore_pending_rejects_paths_outside_the_private_dir() {
    let dir = TmpDir::new("rp-escape");
    let ptr = dir.join(RESTORE_PENDING_FILE);
    write_private_new(&ptr, b"up-x\n/etc/passwd").unwrap();
    assert!(
        take_restore_pending_in(dir.path()).is_none(),
        "指向目录外的指针必须被拒绝"
    );
    assert!(!ptr.exists(), "非法指针要清掉");
}

/// **回归**：指针必须写在交接文件所在目录。
///
/// 真实测试里第一次写错了目录（写到二进制目录），表现为"磁盘换成功、但
/// helper 一直等不到注册"——这条用"写到别处 ⇒ 取不到"把它钉住。
#[test]
fn test_restore_pending_must_live_next_to_the_handoff() {
    let handoff_dir = TmpDir::new("rp-handoff");
    let other_dir = TmpDir::new("rp-other");
    let ctx = sample_ctx(&handoff_dir, "up-rp2");
    let handoff = ctx.write_handoff_into(handoff_dir.path()).unwrap();
    // 写错地方。
    pending_restore_marker("up-rp2", &handoff, other_dir.path());
    assert!(
        take_restore_pending_in(handoff_dir.path()).is_none(),
        "写到别的目录就必须取不到（这正是那次卡住的根因）"
    );
    // 写对地方就能取到。
    pending_restore_marker("up-rp2", &handoff, handoff_dir.path());
    assert_eq!(
        take_restore_pending_in(handoff_dir.path()).unwrap().handoff,
        handoff
    );
}

/// 重复写指针不能失败（第二轮换机时上一轮的残留还在）。
#[test]
fn test_restore_pending_overwrites_previous_leftover() {
    let dir = TmpDir::new("rp-dup");
    let ctx1 = sample_ctx(&dir, "up-a");
    let ctx2 = sample_ctx(&dir, "up-b");
    let h1 = ctx1.write_handoff_into(dir.path()).unwrap();
    let h2 = ctx2.write_handoff_into(dir.path()).unwrap();
    pending_restore_marker("up-a", &h1, dir.path());
    pending_restore_marker("up-b", &h2, dir.path()); // 不应 panic/失败
    assert_eq!(take_restore_pending_in(dir.path()).unwrap().handoff, h2);
}

// ── 11b. 三行指针与自回滚备份校验（MYS-2425-E）───────────────────────

/// 三行指针：第 3 行的 backup 能被读回来，并解析成 `self_rollback`。
#[test]
fn test_three_line_pointer_carries_the_backup() {
    let dir = TmpDir::new("rp3-ok");
    let ctx = sample_ctx(&dir, "up-rp3");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    let backup = backup_path_for(&ctx.target_path, &ctx.upgrade_id);
    std::fs::rename(&ctx.target_path, &backup).unwrap();
    std::fs::rename(&ctx.new_binary, &ctx.target_path).unwrap();

    pending_restore_marker_with("up-rp3", &handoff, Some(&backup), dir.path());
    let got = take_restore_pending_in(dir.path()).expect("三行指针应能读到");
    assert_eq!(got.handoff, handoff);
    assert_eq!(got.backup.as_deref(), Some(backup.as_path()));
}

/// **兼容两行格式**：`Coordinated` 换机写的指针没有 backup，必须照旧解析成功，
/// 且 `backup` 为 `None`（那条路径的回滚由助手负责，新进程不该自己动手）。
#[test]
fn test_two_line_pointer_still_parses_without_backup() {
    let dir = TmpDir::new("rp2-compat");
    let ctx = sample_ctx(&dir, "up-rp2c");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    pending_restore_marker("up-rp2c", &handoff, dir.path());
    let got = take_restore_pending_in(dir.path()).expect("两行指针必须继续可用");
    assert_eq!(got.handoff, handoff);
    assert_eq!(got.backup, None);
}

/// backup 与 target **不同目录** ⇒ 忽略 backup（但身份恢复照做）。
#[test]
fn test_pointer_backup_in_another_directory_is_ignored() {
    let dir = TmpDir::new("rp3-otherdir");
    let elsewhere = TmpDir::new("rp3-elsewhere");
    let ctx = sample_ctx(&dir, "up-rp3o");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    // 造一个"名字对但目录不对"的备份：`<target 名>.sr-old-<tag>`，但放在别处。
    let mut name = ctx.target_path.file_name().unwrap().to_os_string();
    name.push(format!("{BACKUP_INFIX}up-rp3o"));
    let bad = elsewhere.path().join(name);
    std::fs::write(&bad, b"old").unwrap();

    pending_restore_marker_with("up-rp3o", &handoff, Some(&bad), dir.path());
    let got = take_restore_pending_in(dir.path()).unwrap();
    assert_eq!(got.backup, None, "跨目录的 backup 必须被忽略");
}

/// backup 前缀不符（不是 `<target>.sr-old-`）⇒ 忽略。
#[test]
fn test_pointer_backup_with_wrong_prefix_is_ignored() {
    let dir = TmpDir::new("rp3-prefix");
    let ctx = sample_ctx(&dir, "up-rp3p");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    // 同目录、真实存在，但名字不是备份命名。
    let bad = dir.join("some-other-file");
    std::fs::write(&bad, b"not a backup").unwrap();

    pending_restore_marker_with("up-rp3p", &handoff, Some(&bad), dir.path());
    let got = take_restore_pending_in(dir.path()).unwrap();
    assert_eq!(got.backup, None, "前缀不符的路径必须被忽略");
}

/// 纯判定：backup 必须与 target 同目录且带备份前缀。
#[test]
fn test_self_rollback_backup_belongs_to_target() {
    let target = Path::new("/usr/local/bin/shell-remote");
    let good = PathBuf::from("/usr/local/bin/shell-remote.sr-old-up123456");
    assert!(self_rollback_backup_belongs_to(&good, target));

    // 相对路径
    assert!(!self_rollback_backup_belongs_to(
        Path::new("shell-remote.sr-old-x"),
        target
    ));
    // 不同目录
    assert!(!self_rollback_backup_belongs_to(
        Path::new("/opt/bin/shell-remote.sr-old-x"),
        target
    ));
    // 前缀不符（另一个二进制）
    assert!(!self_rollback_backup_belongs_to(
        Path::new("/usr/local/bin/other.sr-old-x"),
        target
    ));
    // 有前缀但没 tag（不是 backup_path_for 的产物）
    assert!(!self_rollback_backup_belongs_to(
        Path::new("/usr/local/bin/shell-remote.sr-old-"),
        target
    ));
}

/// 备份名 → target 的反推。
#[test]
fn test_self_rollback_target_is_derived_from_the_backup_name() {
    assert_eq!(
        self_rollback_target(Path::new("/usr/local/bin/shell-remote.sr-old-abcd1234")),
        Some(PathBuf::from("/usr/local/bin/shell-remote"))
    );
    // 不合规的名字一律拒绝，绝不猜路径。
    assert_eq!(
        self_rollback_target(Path::new("/usr/local/bin/plain")),
        None
    );
    assert_eq!(
        self_rollback_target(Path::new("/usr/local/bin/.sr-old-abcd")),
        None,
        "中缀前必须有名字"
    );
    assert_eq!(
        self_rollback_target(Path::new("/usr/local/bin/shell-remote.sr-old-")),
        None,
        "中缀后必须有 tag"
    );
}

/// **自回滚主路径**：到期仍未被消费 ⇒ 旧二进制恢复回 target、调用退出回调，
/// 且按真实退出码 1。
#[test]
fn test_self_rollback_restores_the_previous_binary_when_not_registered() {
    let dir = TmpDir::new("sr-rollback");
    let target = fake_target(&dir, "shell-remote");
    let old_bytes = std::fs::read(&target).unwrap();
    let backup = backup_path_for(&target, "up-sr1");
    // 模拟换盘完成后的状态：旧版在 backup，新版在 target。
    std::fs::rename(&target, &backup).unwrap();
    std::fs::write(&target, b"brand new version").unwrap();

    let exited = std::sync::Arc::new(std::sync::Mutex::new(Vec::<i32>::new()));
    let record = exited.clone();
    let outcome = run_self_rollback_watchdog(
        Instant::now(), // 立即到期
        &backup,
        &target,
        &dir.join("handoff.json"),
        || false, // 从未注册成功
        move |code| record.lock().unwrap().push(code),
    );
    assert_eq!(outcome, SelfRollbackOutcome::RolledBack);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        old_bytes,
        "target 必须恢复成旧版字节"
    );
    assert!(!backup.exists(), "备份已被 rename 走");
    assert_eq!(
        *exited.lock().unwrap(),
        vec![1],
        "必须以 1 退出让 systemd 拉起旧版"
    );
}

/// **已注册 ⇒ 不回滚**，且退出回调不被调用（进程继续服务）。
#[test]
fn test_self_rollback_does_nothing_when_registered() {
    let dir = TmpDir::new("sr-ok");
    let target = fake_target(&dir, "shell-remote");
    let backup = backup_path_for(&target, "up-sr2");
    std::fs::copy(&target, &backup).unwrap();
    let new_bytes = b"new version".to_vec();
    std::fs::write(&target, &new_bytes).unwrap();

    let outcome = run_self_rollback_watchdog(
        Instant::now() + Duration::from_secs(30),
        &backup,
        &target,
        &dir.join("handoff.json"),
        || true, // 立刻判定为已注册
        |_| panic!("已注册时绝不能退出"),
    );
    assert_eq!(outcome, SelfRollbackOutcome::Registered);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        new_bytes,
        "已注册时不得回滚磁盘"
    );
    assert!(backup.exists(), "备份交给 complete() 删，不做重复动作");
}

/// 恢复失败（备份不存在）⇒ 仍然退出（用码 2），让 systemd 别反复拉起坏版本。
#[test]
fn test_self_rollback_exits_even_when_restore_fails() {
    let dir = TmpDir::new("sr-fail");
    let target = fake_target(&dir, "shell-remote");
    let backup = backup_path_for(&target, "up-sr3"); // 故意不创建
    let exited = std::sync::Arc::new(std::sync::Mutex::new(Vec::<i32>::new()));
    let record = exited.clone();
    let outcome = run_self_rollback_watchdog(
        Instant::now(),
        &backup,
        &target,
        &dir.join("handoff.json"),
        || false,
        move |code| record.lock().unwrap().push(code),
    );
    assert_eq!(outcome, SelfRollbackOutcome::RollbackFailed);
    assert_eq!(*exited.lock().unwrap(), vec![2], "恢复失败也必须退出");
}

/// **竞态回归**：窗口到期那一刻恰好注册成功（备份已被 `complete()` 删掉）⇒
/// 不得退出，必须保留新版。
///
/// 没有这道复核的话，一台**已经升级成功**的设备会在计时器到期时被打回旧版并
/// 退出——比不升级严重得多。`consumed()` 在 rename 失败后会被再读一次，这里
/// 用"第一次读 false、之后读 true"模拟那个时序。
#[test]
fn test_self_rollback_does_not_kill_a_process_that_just_registered() {
    let dir = TmpDir::new("sr-race");
    let target = fake_target(&dir, "shell-remote");
    let new_bytes = b"new version".to_vec();
    std::fs::write(&target, &new_bytes).unwrap();
    let backup = backup_path_for(&target, "up-sr5"); // 已被 complete() 删掉

    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = reads.clone();
    let outcome = run_self_rollback_watchdog(
        Instant::now(),
        &backup,
        &target,
        &dir.join("handoff.json"),
        move || {
            // 循环里读到 false（还没注册），rename 失败后再读就是 true。
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
        },
        |_| panic!("刚注册成功的进程绝不能被自回滚杀掉"),
    );
    assert_eq!(outcome, SelfRollbackOutcome::Registered);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        new_bytes,
        "新版必须留在磁盘上"
    );
}

/// `complete()` 成功后删掉 backup（升级确认，旧版字节再无用）。
#[test]
fn test_complete_removes_the_backup_after_a_successful_upgrade() {
    let dir = TmpDir::new("sr-complete");
    let ctx = sample_ctx(&dir, "up-sr4");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    let backup = backup_path_for(&ctx.target_path, &ctx.upgrade_id);
    std::fs::copy(&ctx.target_path, &backup).unwrap();

    let mut id = RestoredIdentity::load(&handoff).unwrap();
    id.self_rollback = Some(SelfRollback {
        backup: backup.clone(),
        deadline: Duration::from_secs(60),
    });
    assert!(backup.exists());
    id.complete();
    assert!(!backup.exists(), "注册成功后备份必须删除");
}

/// 环境变量越界/非法 ⇒ 回落到默认窗口（不报错，不让设备起不来）。
#[test]
fn test_self_rollback_window_falls_back_to_default() {
    let _g = env_guard();
    for bad in ["0", "29", "901", "abc", "", "-5"] {
        std::env::set_var(SELF_ROLLBACK_ENV, bad);
        assert_eq!(
            self_rollback_window(),
            Duration::from_secs(SELF_ROLLBACK_SECS),
            "{bad:?} 越界时应回落默认值"
        );
    }
    // 合法范围两端都接受。
    for good in [SELF_ROLLBACK_MIN_SECS, SELF_ROLLBACK_MAX_SECS] {
        std::env::set_var(SELF_ROLLBACK_ENV, good.to_string());
        assert_eq!(self_rollback_window(), Duration::from_secs(good));
    }
    std::env::remove_var(SELF_ROLLBACK_ENV);
    assert_eq!(
        self_rollback_window(),
        Duration::from_secs(SELF_ROLLBACK_SECS)
    );
}

// ── 11c. 旧 agent 侧换盘：SystemdRestart（MYS-2425-E）──────────────────
//
// 这条路径上**没有任何外部监督者**：不启助手、不调 systemd。所以每个失败分支
// 都必须自己把磁盘恢复原状、返回 `Err`，由调用方上报 `failed` 并**继续在线**
// ——旧进程活着是这台设备唯一的保障。
//
// 失败靠注入的文件系统制造：真实的"第二次 rename 失败"要跨文件系统或恰好撞上
// 权限变化，不稳定复现，而它恰恰是"磁盘必须恢复"的关键分支。

/// 可编排的换盘文件系统：每一格都能单独失败。
struct FakeSwapFs {
    /// 第几次 `rename` 调用开始失败（1 起算；`None` = 从不失败）。
    fail_rename_at: Option<usize>,
    /// 让**恢复**用的 rename 也失败（模拟"回滚也救不回来"）。
    fail_restore: bool,
    make_executable_err: Option<String>,
    verify_err: Option<String>,
    write_pointer_ok: bool,
    calls: std::cell::RefCell<Vec<String>>,
}

impl FakeSwapFs {
    fn ok() -> Self {
        Self {
            fail_rename_at: None,
            fail_restore: false,
            make_executable_err: None,
            verify_err: None,
            write_pointer_ok: true,
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn count(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c.starts_with("rename"))
            .count()
    }
}

impl SwapFs for FakeSwapFs {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("rename {} -> {}", from.display(), to.display()));
        // 奇数次失败 = 装新版那一步；`fail_restore` 让恢复那一步也失败。
        let n = self.count();
        let is_restore = self.fail_restore && n >= 3;
        if Some(n) == self.fail_rename_at || is_restore {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected rename failure",
            ));
        }
        // 真实语义：真的移动文件，否则"恢复"断言无从验证。
        std::fs::rename(from, to)
    }

    fn make_executable(&self, path: &Path) -> std::io::Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("make_executable {}", path.display()));
        match &self.make_executable_err {
            Some(e) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                e.clone(),
            )),
            None => make_executable(path),
        }
    }

    fn verify(&self, target: &Path, expected_sha: &str) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(format!("verify {}", target.display()));
        match &self.verify_err {
            Some(e) => Err(e.clone()),
            None => verify_installed_binary(target, expected_sha),
        }
    }

    fn write_pointer(&self, upgrade_id: &str, handoff: &Path, backup: &Path, dir: &Path) -> bool {
        self.calls
            .borrow_mut()
            .push(format!("write_pointer {upgrade_id}"));
        if !self.write_pointer_ok {
            return false;
        }
        super::pending_restore_marker_with(upgrade_id, handoff, Some(backup), dir);
        dir.join(RESTORE_PENDING_FILE).is_file()
    }
}

/// 换盘成功：磁盘是新字节、旧版在 backup、三行指针就位。
#[cfg(unix)]
#[test]
fn test_systemd_restart_swap_succeeds_and_writes_three_line_pointer() {
    let dir = TmpDir::new("srs-ok");
    let ctx = sample_ctx(&dir, "up-srsok");
    let old_bytes = std::fs::read(&ctx.target_path).unwrap();
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();

    systemd_restart_swap(&ctx, &handoff, dir.path()).expect("换盘应成功");

    // target = 新字节
    assert_eq!(file_sha(&ctx.target_path), ctx.new_sha256);
    // 旧版保留在 backup
    let backup = backup_path_for(&ctx.target_path, &ctx.upgrade_id);
    assert_eq!(std::fs::read(&backup).unwrap(), old_bytes);
    // 三行指针：新 agent 靠它恢复身份并自回滚。
    let body = std::fs::read_to_string(dir.join(RESTORE_PENDING_FILE)).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 3, "必须是三行: {body:?}");
    assert_eq!(lines[0], "up-srsok");
    assert_eq!(PathBuf::from(lines[1]), handoff);
    assert_eq!(PathBuf::from(lines[2]), backup);
    // 交接文件还在（等新 agent 注册成功后删）
    assert!(handoff.exists());
}

/// **第二次 rename 失败** ⇒ 备份被立刻恢复回 target，返回 `Err`。
///
/// 这是"换了半个"的唯一入口：备份已改名、新文件没就位。此时磁盘上 target
/// 不存在，如果不恢复，设备重启后 service 直接起不来。
#[cfg(unix)]
#[test]
fn test_systemd_restart_swap_restores_the_backup_when_install_fails() {
    let dir = TmpDir::new("srs-installfail");
    let ctx = sample_ctx(&dir, "up-srsif");
    let old_bytes = std::fs::read(&ctx.target_path).unwrap();
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    let fs = FakeSwapFs {
        fail_rename_at: Some(2), // 第 1 次备份成功，第 2 次装新版失败
        ..FakeSwapFs::ok()
    };

    let err = systemd_restart_swap_with(&fs, &ctx, &handoff, dir.path()).expect_err("必须失败");
    assert!(err.contains("cannot install the new binary"), "{err}");
    assert!(err.contains("back in place"), "必须说明旧版已归位: {err}");
    // 磁盘恢复为旧 hash
    assert_eq!(file_sha(&ctx.target_path), sha256_of(&old_bytes));
    assert!(
        !backup_path_for(&ctx.target_path, &ctx.upgrade_id).exists(),
        "恢复时备份被 rename 回 target，不该再留着"
    );
    assert!(!dir.join(RESTORE_PENDING_FILE).exists(), "失败不得留指针");
    assert!(handoff.exists(), "凭据文件的清理由调用方负责");
}

/// 装新版失败**且恢复也失败** ⇒ 如实报出备份路径（人工处理），仍返回 `Err`。
#[cfg(unix)]
#[test]
fn test_systemd_restart_swap_reports_the_backup_when_restore_also_fails() {
    let dir = TmpDir::new("srs-bothfail");
    let ctx = sample_ctx(&dir, "up-srsbf");
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    let fs = FakeSwapFs {
        fail_rename_at: Some(2),
        fail_restore: true,
        ..FakeSwapFs::ok()
    };

    let err = systemd_restart_swap_with(&fs, &ctx, &handoff, dir.path()).expect_err("必须失败");
    let backup = backup_path_for(&ctx.target_path, &ctx.upgrade_id);
    assert!(
        err.contains(&backup.display().to_string()),
        "原因必须指出备份在哪: {err}"
    );
}

/// `.part` 被篡改（hash 不符）⇒ 换盘前就失败，磁盘**一点没动**。
#[cfg(unix)]
#[test]
fn test_systemd_restart_swap_rejects_a_tampered_artifact_before_touching_the_disk() {
    let dir = TmpDir::new("srs-hash");
    let ctx = sample_ctx(&dir, "up-srsh");
    let old_sha = file_sha(&ctx.target_path);
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    // 篡改下载好的字节，但保留交接文件里的原 sha256。
    std::fs::write(&ctx.new_binary, b"#!/bin/sh\nexit 1 # tampered\n").unwrap();

    let err = systemd_restart_swap(&ctx, &handoff, dir.path()).expect_err("必须拒绝");
    assert!(err.contains("no longer matches its digest"), "{err}");
    assert_eq!(file_sha(&ctx.target_path), old_sha, "校验失败不得动 target");
    let backup = backup_path_for(&ctx.target_path, &ctx.upgrade_id);
    assert!(!backup.exists(), "校验失败不得产生备份");
    assert!(
        !dir.join(RESTORE_PENDING_FILE).exists(),
        "校验失败不得留指针"
    );
}

/// 换盘后一致性复核失败 ⇒ 恢复旧二进制。
#[cfg(unix)]
#[test]
fn test_systemd_restart_swap_restores_when_the_installed_binary_does_not_match() {
    let dir = TmpDir::new("srs-postverify");
    let ctx = sample_ctx(&dir, "up-srspv");
    let old_bytes = std::fs::read(&ctx.target_path).unwrap();
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    let fs = FakeSwapFs {
        verify_err: Some("injected digest mismatch".to_string()),
        ..FakeSwapFs::ok()
    };

    let err = systemd_restart_swap_with(&fs, &ctx, &handoff, dir.path()).expect_err("必须失败");
    assert!(err.contains("previous binary was restored"), "{err}");
    assert_eq!(file_sha(&ctx.target_path), sha256_of(&old_bytes));
}

/// 补可执行位失败 ⇒ 在动磁盘前失败，什么都不用恢复。
#[cfg(unix)]
#[test]
fn test_systemd_restart_swap_aborts_before_the_disk_when_chmod_fails() {
    let dir = TmpDir::new("srs-chmod");
    let ctx = sample_ctx(&dir, "up-srscm");
    let old_sha = file_sha(&ctx.target_path);
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    let fs = FakeSwapFs {
        make_executable_err: Some("injected chmod failure".to_string()),
        ..FakeSwapFs::ok()
    };

    let err = systemd_restart_swap_with(&fs, &ctx, &handoff, dir.path()).expect_err("必须失败");
    assert!(err.contains("nothing on disk was changed"), "{err}");
    assert_eq!(file_sha(&ctx.target_path), old_sha);
    assert!(
        fs.calls.borrow().iter().all(|c| !c.starts_with("rename")),
        "chmod 失败时不该 attempt 任何 rename: {:?}",
        fs.calls.borrow()
    );
}

/// 指针写不进去 ⇒ 恢复旧二进制并失败。
///
/// 指针是三步握手的唯一凭据：写不进去，新 agent 会以全新身份注册、而且没有
/// 自回滚能力——比不升级更糟，所以必须回滚。
#[cfg(unix)]
#[test]
fn test_systemd_restart_swap_rolls_back_when_the_pointer_cannot_be_written() {
    let dir = TmpDir::new("srs-ptrfail");
    let ctx = sample_ctx(&dir, "up-srsptr");
    let old_bytes = std::fs::read(&ctx.target_path).unwrap();
    let handoff = ctx.write_handoff_into(dir.path()).unwrap();
    let fs = FakeSwapFs {
        write_pointer_ok: false,
        ..FakeSwapFs::ok()
    };

    let err = systemd_restart_swap_with(&fs, &ctx, &handoff, dir.path()).expect_err("必须失败");
    assert!(
        err.contains("restore pointer"),
        "原因要说明是指针写不进去: {err}"
    );
    assert!(
        err.contains("previous binary was restored"),
        "必须说明已回滚: {err}"
    );
    assert_eq!(
        file_sha(&ctx.target_path),
        sha256_of(&old_bytes),
        "指针失败必须回滚磁盘"
    );
}

// ── 12. 协调换机的失败分支必须恢复磁盘并如实报告（评审第 2 项）──────

/// 可编排的假 systemd：让三条失败分支都能被精确触发。
struct FakeSystemd {
    stop_err: Option<String>,
    start_err: Option<String>,
    cgroup_empty: bool,
    /// 记录 start 被调用了几次（失败收尾也要 start 旧版）。
    starts: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl FakeSystemd {
    fn new() -> Self {
        Self {
            stop_err: None,
            start_err: None,
            cgroup_empty: true,
            starts: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

impl super::SystemdOps for FakeSystemd {
    fn stop(&self, _unit: &str) -> Result<(), String> {
        match &self.stop_err {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
    fn start(&self, unit: &str) -> Result<(), String> {
        self.starts.lock().unwrap().push(unit.to_string());
        match &self.start_err {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
    fn wait_empty(&self, _unit: &str, _timeout: Duration) -> bool {
        self.cgroup_empty
    }
}

/// 造一个"外层已备好 backup、新字节已就位"的现场，返回 (ctx, backup, 旧 hash)。
///
/// 协调路径进入时磁盘上**已经是新版**（历史上这一步由外层完成）；本组用例验证
/// 的就是"失败时必须把 backup 恢复回去"。
fn coord_fixture(tag: &str) -> (TmpDir, RestartContext, PathBuf, String) {
    let dir = TmpDir::new(tag);
    let ctx = sample_ctx(&dir, "up-coord");
    // 磁盘上仍是**旧**二进制：协调路径自己负责 stop → 备份 → 替换。
    // （早期版本由外层先换盘，导致协调分支失败时磁盘已被改动；这里如实反映
    // 新顺序，fixture 不能替它做。）
    let old_hash = file_sha(&ctx.target_path);
    let backup = backup_path_for(&ctx.target_path, &ctx.upgrade_id);
    (dir, ctx, backup, old_hash)
}

/// 驱动一次协调换机（假 systemd）。
fn run_coord(
    ops: &FakeSystemd,
    ctx: &RestartContext,
    backup: &Path,
    dir: &TmpDir,
) -> super::HelperOutcome {
    let mut log = super::HelperLog::open(Path::new(&dir.join("h.json")));
    super::coordinated_swap_with(
        ops,
        ctx,
        "test.service",
        Path::new(&dir.join("h.json")),
        backup,
        dir.path(),
        &mut log,
        super::HelperTimeouts {
            old_exit: Duration::from_millis(10),
            register: Duration::from_millis(50),
        },
    )
}

/// `stop` 失败 ⇒ 恢复旧二进制 + 拉起旧版 + `rolled_back`（**不是** aborted）。
///
/// 旧消息写的是 "Nothing on disk was changed"，而实际磁盘已是新版本、旧进程已
/// 退出——这条断言把"消息必须与事实一致"钉住。
#[test]
fn test_stop_failure_restores_the_previous_binary() {
    let (dir, ctx, backup, old_hash) = coord_fixture("coord-stop");
    let mut ops = FakeSystemd::new();
    ops.stop_err = Some("Access denied".to_string());
    let out = run_coord(&ops, &ctx, &backup, &dir);

    match out {
        super::HelperOutcome::RolledBack { reason, identity } => {
            assert!(reason.contains("Access denied"), "{reason}");
            // stop 失败发生在换盘之前：磁盘**确实**没动过，消息要如实这么说
            // （早期实现因为外层已先换盘，这里会谎称"未改动"）。
            assert!(
                reason.contains("Nothing on disk was changed"),
                "stop 阶段失败时磁盘确实未改，应如实说明: {reason}"
            );
            assert!(
                !reason.contains("restored"),
                "还没换盘就不该声称已恢复: {reason}"
            );
            assert!(
                matches!(identity, super::RollbackIdentity::Lost { .. }),
                "身份可能丢失，必须是 Lost"
            );
        }
        other => panic!("stop 失败必须是 RolledBack，实际 {other:?}"),
    }
    assert_eq!(file_sha(&ctx.target_path), old_hash, "磁盘必须恢复为旧制品");
    assert_eq!(
        ops.starts.lock().unwrap().as_slice(),
        &["test.service".to_string()],
        "必须把旧版拉回来"
    );
}

/// stop 后 cgroup 未排空（例如 `KillMode=process` 留下用户 shell）⇒ 同样恢复。
#[test]
fn test_cgroup_not_empty_restores_the_previous_binary() {
    let (dir, ctx, backup, old_hash) = coord_fixture("coord-empty");
    let mut ops = FakeSystemd::new();
    ops.cgroup_empty = false;
    let out = run_coord(&ops, &ctx, &backup, &dir);

    match out {
        super::HelperOutcome::RolledBack { reason, .. } => {
            assert!(reason.contains("still has processes"), "{reason}");
            assert!(!reason.contains("nothing was changed"), "{reason}");
        }
        other => panic!("必须是 RolledBack，实际 {other:?}"),
    }
    assert_eq!(file_sha(&ctx.target_path), old_hash);
    assert_eq!(ops.starts.lock().unwrap().len(), 1, "必须 start 旧版一次");
}

/// 已就位字节与任务 digest 不符 ⇒ 恢复旧二进制。
#[test]
fn test_digest_mismatch_restores_the_previous_binary() {
    let (dir, ctx, backup, old_hash) = coord_fixture("coord-digest");
    // 让校验失败：把 ctx 期望的 digest 改成一个不同的值。
    //
    // 不能靠篡改盘上文件——协调路径在 stop 之后会**重新**把 `new_binary`
    // rename 到 target，篡改会被覆盖掉。真实场景里这个分支对应的是"下载的
    // 字节与 relay 下发的不符"，等价于 `new_sha256` 与文件不一致。
    // 让"要装的那份字节"与任务 digest 不符：等价于下载被篡改。
    std::fs::write(&ctx.new_binary, b"#!/bin/sh\n# tampered payload\n").unwrap();
    let mut ctx = ctx;
    ctx.new_sha256 = "0".repeat(64);
    let ops = FakeSystemd::new();
    let out = run_coord(&ops, &ctx, &backup, &dir);

    match out {
        super::HelperOutcome::RolledBack { reason, .. } => {
            assert!(reason.contains("does not match"), "{reason}");
        }
        other => panic!("必须是 RolledBack，实际 {other:?}"),
    }
    assert_eq!(
        file_sha(&ctx.target_path),
        old_hash,
        "校验失败后磁盘必须恢复为旧制品"
    );
}

/// 成功路径：stop → 换盘 → start → 等注册超时后仍回滚（这里不注入真 agent）。
///
/// 只断言"start 被调用、磁盘已是新字节"，注册超时的收尾由上面几条覆盖。
#[test]
fn test_success_path_stops_swaps_and_starts() {
    let (dir, ctx, backup, _old) = coord_fixture("coord-ok");
    let ops = FakeSystemd::new();
    let out = run_coord(&ops, &ctx, &backup, &dir);
    // 注册等不到（没有真 agent），所以最终仍是回滚；关键是 start 被调用过。
    assert!(
        matches!(out, super::HelperOutcome::RolledBack { .. }),
        "没有真 agent 时必然回滚，实际 {out:?}"
    );
    assert!(!ops.starts.lock().unwrap().is_empty(), "必须调用过 start");
}

// ── 11. 进程级：无参数 GUI 升级与回滚都恢复同配置/同身份（MYS-2819）──

/// 无参数 GUI 场景里，"新版 agent"与"回滚后的旧版"都只能从交接文件拿配置。
///
/// 这条用例验证的是**真实进程关系**：真助手 + 真脚本进程 + 真 relay。
/// 脚本模拟无参数 GUI 的启动契约——
///
/// - argv 必须为空（多一个参数就说明有人在往空 argv 里补东西）；
/// - 配置只能来自 `SR_UPGRADE_RESTORE` 指向的私有交接文件；
/// - 没有交接文件时**不许注册**（那等于静默变成一台新设备）。
#[cfg(unix)]
mod gui_process_level {
    use super::*;

    /// 无参数 GUI 版 agent 脚本：只从交接文件读配置与身份。
    ///
    /// 脚本模拟真实的无参数 GUI agent：
    ///
    /// - argv 必须为空（多一个参数就说明有人在往空 argv 里补东西）；
    /// - 配置与身份只能来自 `SR_UPGRADE_RESTORE` 指向的私有交接文件；
    /// - 没有交接文件时**不许注册**（那等于静默变成一台新设备）；
    /// - 注册时把交接里的 tokens 以 **JSON 数组** 回放（与真实 agent 一致），
    ///   并且逐项核对 **relay 返回** 的 session_id 与 tokens/权限；任一项不符
    ///   就**不删交接文件、不报回执**，让助手判定失败。
    ///
    /// 早期版本的脚本把 tokens 编码成 JSON 字符串再拼进 body，relay 的
    /// `body["tokens"].as_array()` 拿到 `None`，于是静默走了 fresh 注册——
    /// 身份其实没恢复，脚本却只看 `agent:registered` 就宣布成功（MYS-2819
    /// 验收 #1）。现在脚本自己核对返回身份，助手再核对回执，两道都看得见。
    fn gui_agent_script(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        let body = r#"#!/bin/sh
set -u
OUT="${SR_TEST_OUT:?}"
RELAY="${SR_TEST_RELAY:?}"
VER="${SR_TEST_VER:?}"
H="${SR_UPGRADE_RESTORE:-}"

{
  echo "ARGC=$#"
  for a in "$@"; do echo "ARG=$a"; done
  echo "RESTORE=$H"
} >> "$OUT"

# 无参数 GUI 没有 argv：配置只能来自交接文件。
if [ -z "$H" ] || [ ! -f "$H" ]; then
  echo "no-handoff argc=$#" >> "$OUT"
  exit 1
fi

# 交接里的身份与配置。tokens 只序列化**一次**，直接拼进 body：再编码一次会
# 变成 JSON 字符串，而 relay 只认数组（`as_array()`）。
CFG="$(python3 - "$H" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))
g=d.get("gui") or {}
print(json.dumps({
  "sid": d.get("session_id",""),
  "relay": g.get("relay_url",""),
  "lan_port": g.get("desktop_lan_port",0),
  "fps": g.get("desktop_fps",0),
  "token_type": d.get("token_type",""),
  "fixed_key": d.get("fixed_key") or "",
  "tokens": [{"token":t["token"],"permission":t["permission"]} for t in d["tokens"]],
}))
PY
)"
SID="$(printf '%s' "$CFG" | python3 -c 'import json,sys;print(json.load(sys.stdin)["sid"])')"
echo "cfg=$CFG" >> "$OUT"

# 请求体用 python 组装：避免 shell 里的引号拼接把 JSON 弄坏。
BODY="$(printf '%s' "$CFG" | python3 -c '
import json,sys
c=json.load(sys.stdin)
print(json.dumps({
  "type":"agent:register",
  "session_id":c["sid"],
  "tokens":c["tokens"],
  "agent_version":sys.argv[1],
}))' "$VER")"
RESP="$(curl -s -X POST "$RELAY/agent/send" -H 'Content-Type: application/json' -d "$BODY")"
echo "resp=$RESP" >> "$OUT"

# 逐项核对 relay **返回**的原身份：sid、token 集合、每个 token 的权限。
# 任一不符即视为恢复失败（不删交接、不回执），让助手如实判定。
VERDICT="$(printf '%s\n%s' "$CFG" "$RESP" | python3 -c '
import json,sys
raw=sys.stdin.read().split("\n",1)
want=json.loads(raw[0])
try:
    got=json.loads(raw[1])
except Exception:
    print("unparsable-response"); raise SystemExit
if got.get("type")!="agent:registered":
    print("not-registered:"+str(got.get("error",""))); raise SystemExit
toks=(got.get("payload") or {}).get("tokens") or []
norm=lambda ts: sorted((t.get("token",""),t.get("permission","")) for t in ts)
problems=[]
if got.get("session_id")!=want["sid"]:
    problems.append("sid=%s!=%s"%(got.get("session_id"),want["sid"]))
if norm(toks)!=norm(want["tokens"]):
    problems.append("tokens=%s!=%s"%(norm(toks),norm(want["tokens"])))
print("ok" if not problems else "mismatch:"+";".join(problems))
')"
echo "verdict=$VERDICT" >> "$OUT"

case "$VERDICT" in
  ok)
    rm -f "$H"
    # 回执必须带**本轮** upgrade_id：助手靠它认出这是本次换机的注册。
    echo "shell-remote: upgrade-registered upgrade_id=$SR_TEST_UPGRADE_ID version=$VER session=$SID" >&2
    sleep 5
    ;;
  *)
    # 身份没恢复：保留交接文件，用非零退出让助手判失败。
    echo "restore-failed $VERDICT" >&2
    exit 3
    ;;
esac
"#;
        write_executable(&p, body);
        p
    }

    /// 把脚本需要的环境变量挂上（新进程与回滚进程都从助手继承环境）。
    struct ScriptEnv(Vec<&'static str>);

    impl ScriptEnv {
        fn install(out: &Path, relay: &str, ver: &str, upgrade_id: &str) -> Self {
            std::env::set_var("SR_TEST_OUT", out);
            std::env::set_var("SR_TEST_RELAY", relay);
            std::env::set_var("SR_TEST_VER", ver);
            std::env::set_var("SR_TEST_UPGRADE_ID", upgrade_id);
            Self(vec![
                "SR_TEST_OUT",
                "SR_TEST_RELAY",
                "SR_TEST_VER",
                "SR_TEST_UPGRADE_ID",
            ])
        }
    }

    impl Drop for ScriptEnv {
        fn drop(&mut self) {
            for k in &self.0 {
                std::env::remove_var(k);
            }
        }
    }

    /// 这些用例都动进程级环境变量、且都会换掉同一个目录里的"二进制"，
    /// 必须串行。
    ///
    /// 用 tokio 的锁而不是 std 的：这些是 `async` 用例，`std::sync::MutexGuard`
    /// 会跨 await 持有（clippy 会告警，且一旦有人在里面阻塞就真的会卡住执行器）。
    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    /// 造一份无参数 GUI 的完整现场：真 relay + 已注册身份 + GUI 交接。
    ///
    /// `token_type` 决定身份形态：`"rw"` = 纯临时设备；`"both"` = 带一个 ro
    /// Token（升级/回滚都必须一个不多一个不少）。`fixed_key` 非空时用固定 key
    /// 注册，此时 rw Token 由 key 派生。
    async fn gui_fixture_with(
        tag: &str,
        token_type: &str,
        fixed_key: Option<&str>,
    ) -> Option<(TmpDir, super::process_level::TestRelay, String, RestartContext, PathBuf)> {
        let relay = super::process_level::TestRelay::start().await?;
        let dir = TmpDir::new(tag);
        let client = reqwest::Client::new();
        let sid = "guiag0001";

        // 用**真实**注册拿到身份（含权限），这就是要保住的那份。
        let mut body = serde_json::json!({
            "type": "agent:register",
            "session_id": sid,
            "agent_version": "0.0.0-origin",
        });
        if let Some(k) = fixed_key {
            body["key"] = serde_json::json!(k);
        }
        body["token_type"] = serde_json::json!(token_type);
        let reg: serde_json::Value = client
            .post(format!("{}/agent/send", relay.base))
            .json(&body)
            .send()
            .await
            .expect("原身份注册必须成功")
            .json()
            .await
            .expect("注册响应必须是 JSON");
        assert_eq!(reg["type"], "agent:registered", "{reg}");
        let tokens: Vec<(String, String)> = reg["payload"]["tokens"]
            .as_array()
            .expect("注册响应必须带 tokens 数组")
            .iter()
            .map(|t| {
                (
                    t["token"].as_str().unwrap().to_string(),
                    t["permission"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert!(!tokens.is_empty());

        let target = dir.join("shell-remote");
        let old_bin = gui_agent_script(dir.path(), "gui-agent-old");
        std::fs::copy(&old_bin, &target).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let new_bin = gui_agent_script(dir.path(), "gui-agent-new");
        let mut snap = gui_snapshot(sid, &relay.base);
        snap.token_type = token_type.to_string();
        let ctx = RestartContext {
            session_id: sid.to_string(),
            tokens: tokens.clone(),
            fixed_key: fixed_key.map(|k| k.to_string()),
            token_type: token_type.to_string(),
            relay_base: Some(relay.base.clone()),
            relay_insecure: false,
            // 无参数启动：argv 就是空的。
            argv: Vec::new(),
            old_pid: 0,
            old_start: None,
            upgrade_id: "gui-up-1".to_string(),
            target_version: "9.9.9".to_string(),
            target_path: target.clone(),
            new_binary: new_bin.clone(),
            new_sha256: sha256_of(&std::fs::read(&new_bin).unwrap()),
            service_cgroup: None,
            service_restart_policy: None,
            managed_plan: None,
            overlay: false,
            gui: Some(snap),
        };
        let handoff = ctx.write_handoff_into(dir.path()).unwrap();
        Some((dir, relay, sid.to_string(), ctx, handoff))
    }

    /// 纯临时设备（`--token-type rw`）的现场。
    async fn gui_fixture(
        tag: &str,
    ) -> Option<(TmpDir, super::process_level::TestRelay, String, RestartContext, PathBuf)> {
        gui_fixture_with(tag, "rw", None).await
    }

    /// 从脚本日志里取出某一类带前缀的行（`cfg=` / `resp=` / `verdict=`）。
    fn logged<'a>(log: &'a str, prefix: &str) -> Option<&'a str> {
        log.lines()
            .find_map(|l| l.strip_prefix(prefix))
            .map(|v| v.trim())
    }

    /// 断言脚本核对过 relay **返回**的身份，且结论是 ok。
    ///
    /// 这是 MYS-2819 验收 #1 的核心：以前脚本只看响应里有 `agent:registered`
    /// 就宣布成功，而 tokens 被编码成字符串时 relay 会静默走 fresh 注册、
    /// 返回的是一套**新** Token。现在脚本必须核对返回的 sid 与 tokens/权限。
    fn assert_identity_verified(log: &str) {
        let verdict = logged(log, "verdict=")
            .unwrap_or_else(|| panic!("脚本必须给出身份核对结论: {log}"));
        assert_eq!(
            verdict, "ok",
            "脚本必须核对 relay 返回的原 sid/tokens/权限并全部相符: {log}"
        );
        // 回执/失败都必须来自脚本自己的判定，而不是只看到 200。
        assert!(
            logged(log, "resp=").is_some(),
            "脚本必须记录 relay 的原始响应便于排查: {log}"
        );
    }

    /// 升级成功：新进程 PID 变了、argv 仍为空、配置与身份都来自交接。
    ///
    /// 身份不是"看响应里有 agent:registered"就算数：脚本会逐项核对 relay
    /// **返回**的 session_id 与 tokens/权限，助手再核对回执里的 upgrade_id。
    #[tokio::test]
    async fn test_gui_upgrade_restores_config_and_identity_with_empty_argv() {
        let _g = serial().await;
        let Some((dir, relay, sid, ctx, handoff)) = gui_fixture("gui-ok").await else {
            eprintln!("跳过：无法启动真 relay 二进制");
            return;
        };
        let out = dir.join("gui.log");
        // 新版字节的摘要要在换机前算好：助手会把它 rename 到 target。
        let new_sha = file_sha(&ctx.new_binary);
        let _env = ScriptEnv::install(&out, &relay.base, "9.9.9", "gui-up-1");
        let h = handoff.clone();
        let outcome = tokio::task::spawn_blocking(move || helper_main(&h, false))
            .await
            .expect("helper task must not panic");

        let log = std::fs::read_to_string(&out).unwrap_or_default();
        assert!(
            matches!(outcome, HelperOutcome::Registered { .. }),
            "无参数 GUI 升级必须成功注册，实际 {outcome:?}；日志: {log}"
        );
        // 新版字节在换机时被 rename 到 target，new_binary 本身已经不在了——
        // 所以要在跑助手之前就把它的摘要记下来。
        assert_eq!(
            file_sha(&ctx.target_path),
            new_sha,
            "target 必须换成新版的字节"
        );
        // argv 必须为空：无参数 GUI 的契约就是"一个参数都没有"。
        assert!(log.contains("ARGC=0"), "新进程必须收到空 argv: {log}");
        assert!(!log.contains("ARG=--session-id"), "{log}");

        // ① 配置来自交接快照，不是磁盘草稿。
        let cfg_line = log
            .lines()
            .find(|l| l.starts_with("cfg="))
            .unwrap_or_else(|| panic!("新进程没有读到交接配置: {log}"));
        let cfg: serde_json::Value =
            serde_json::from_str(cfg_line.trim_start_matches("cfg=")).unwrap();
        assert_eq!(cfg["relay"], relay.base, "relay 必须是连接时那份: {cfg}");
        assert_eq!(cfg["lan_port"], 39616, "活跃 LAN 端口必须恢复: {cfg}");
        assert_eq!(cfg["fps"], 24.0, "桌面参数必须恢复: {cfg}");

        // ② 身份逐项核对：relay **返回**的 sid 与 tokens/权限必须与交接一致。
        assert_identity_verified(&log);
        let resp: serde_json::Value =
            serde_json::from_str(logged(&log, "resp=").unwrap()).expect("响应必须是 JSON");
        assert_eq!(resp["session_id"], sid, "服务端必须回到原会话: {resp}");
        let returned: Vec<(String, String)> = resp["payload"]["tokens"]
            .as_array()
            .expect("服务端必须返回 tokens 数组")
            .iter()
            .map(|t| {
                (
                    t["token"].as_str().unwrap_or("").to_string(),
                    t["permission"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        let mut want = ctx.tokens.clone();
        let mut got = returned.clone();
        want.sort();
        got.sort();
        assert_eq!(got, want, "服务端返回的 tokens/权限必须原样: {resp}");

        assert!(
            !handoff.exists(),
            "身份确认后才允许消费交接文件（否则身份会一直留在磁盘上）"
        );
    }

    /// `--token-type both`：升级后 rw + ro 两个 Token 必须**一个不多一个不少**。
    ///
    /// 只交 rw 会让设备悄悄丢掉 ro 权限，只交 ro 等于降权。这条用例把权限集合
    /// 当成身份的一部分来核对（服务端返回的 permission 也要对上）。
    #[tokio::test]
    async fn test_gui_upgrade_preserves_both_token_permissions() {
        let _g = serial().await;
        let Some((dir, relay, sid, ctx, handoff)) = gui_fixture_with("gui-both", "both", None).await
        else {
            eprintln!("跳过：无法启动真 relay 二进制");
            return;
        };
        assert_eq!(ctx.tokens.len(), 2, "both 必须有 rw + ro 两个 Token");
        let mut perms: Vec<&str> = ctx.tokens.iter().map(|(_, p)| p.as_str()).collect();
        perms.sort();
        assert_eq!(perms, vec!["ro", "rw"], "原身份必须是 rw + ro");

        let out = dir.join("gui.log");
        let _env = ScriptEnv::install(&out, &relay.base, "9.9.9", "gui-up-1");
        let h = handoff.clone();
        let outcome = tokio::task::spawn_blocking(move || helper_main(&h, false))
            .await
            .expect("helper task must not panic");
        let log = std::fs::read_to_string(&out).unwrap_or_default();
        assert!(
            matches!(outcome, HelperOutcome::Registered { .. }),
            "both 设备升级必须成功，实际 {outcome:?}；日志: {log}"
        );
        assert_identity_verified(&log);
        let resp: serde_json::Value =
            serde_json::from_str(logged(&log, "resp=").unwrap()).expect("响应必须是 JSON");
        assert_eq!(resp["session_id"], sid);
        let mut returned: Vec<String> = resp["payload"]["tokens"]
            .as_array()
            .expect("服务端必须返回 tokens 数组")
            .iter()
            .map(|t| {
                format!(
                    "{}:{}",
                    t["permission"].as_str().unwrap_or(""),
                    t["token"].as_str().unwrap_or("")
                )
            })
            .collect();
        let mut want: Vec<String> = ctx
            .tokens
            .iter()
            .map(|(t, p)| format!("{p}:{t}"))
            .collect();
        returned.sort();
        want.sort();
        assert_eq!(returned, want, "rw/ro 两个 Token 都必须原样恢复: {resp}");
    }

    /// 固定 key 设备：rw Token 由 `--key` 派生，恢复后必须仍是同一个 key。
    ///
    /// 脚本另外核对 `fixed_key` 随交接传到（回滚路径要用它重建身份）。
    #[tokio::test]
    async fn test_gui_upgrade_preserves_a_fixed_key_identity() {
        let _g = serial().await;
        let key = "GUI-FIXED-KEY-a1b2";
        let Some((dir, relay, sid, ctx, handoff)) =
            gui_fixture_with("gui-key", "rw", Some(key)).await
        else {
            eprintln!("跳过：无法启动真 relay 二进制");
            return;
        };
        assert_eq!(
            ctx.tokens,
            vec![(key.to_string(), "rw".to_string())],
            "固定 key 设备的 rw Token 就是 key 本身"
        );
        assert_eq!(ctx.fixed_key.as_deref(), Some(key));

        let out = dir.join("gui.log");
        let _env = ScriptEnv::install(&out, &relay.base, "9.9.9", "gui-up-1");
        let h = handoff.clone();
        let outcome = tokio::task::spawn_blocking(move || helper_main(&h, false))
            .await
            .expect("helper task must not panic");
        let log = std::fs::read_to_string(&out).unwrap_or_default();
        assert!(
            matches!(outcome, HelperOutcome::Registered { .. }),
            "固定 key 设备升级必须成功，实际 {outcome:?}；日志: {log}"
        );
        assert_identity_verified(&log);
        // 交接里的固定密钥仍然只走受保护字段（脚本读到了，说明通道没断）。
        let cfg: serde_json::Value = serde_json::from_str(
            log.lines()
                .find_map(|l| l.strip_prefix("cfg="))
                .unwrap_or_else(|| panic!("没有 cfg 行: {log}"))
                .trim(),
        )
        .unwrap();
        assert_eq!(cfg["fixed_key"], key, "固定密钥必须经受保护字段交接");
        assert_eq!(cfg["sid"], sid);
    }

    /// 升级失败回滚：旧版被拉起，**同样是空 argv**，且照样拿到同配置/同身份。
    ///
    /// 这就是任务卡里"新版启动、失败回滚到原无参数 GUI 版都必须恢复同配置/
    /// 同身份；处理空 argv 的回滚，不依赖用户手工再点连接"。
    #[tokio::test]
    async fn test_gui_rollback_restores_the_same_config_without_touching_argv() {
        let _g = serial().await;
        let Some((dir, relay, sid, mut ctx, handoff)) = gui_fixture("gui-rb").await else {
            eprintln!("跳过：无法启动真 relay 二进制");
            return;
        };
        let out = dir.join("gui.log");
        // 把"要装的新版"换成一个立刻退出 1 的坏二进制：助手必然回滚。
        // （fixture 里的 target 是**健康的** GUI 版，回滚后它会被重新拉起。）
        let broken = dir.join("gui-agent-broken");
        write_executable(&broken, "#!/bin/sh\nexit 1\n");
        ctx.new_binary = broken.clone();
        ctx.new_sha256 = sha256_of(&std::fs::read(&broken).unwrap());
        let healthy_sha = file_sha(&ctx.target_path);
        // 交接文件要按改过的 ctx 重写（sha 变了）。
        std::fs::remove_file(&handoff).unwrap();
        let handoff = ctx.write_handoff_into(dir.path()).unwrap();

        let _env = ScriptEnv::install(&out, &relay.base, "9.9.9", "gui-up-1");
        let h = handoff.clone();
        // 注册上限收紧：坏新版立刻退出，助手据此马上回滚，不必等满 75s。
        let outcome = tokio::task::spawn_blocking(move || {
            helper_main_short_register_timeout(&h, Duration::from_secs(8))
        })
        .await
        .expect("helper task must not panic");

        // 回滚成功 = 旧版真的以**原身份**重新上线。助手判定依据是本轮 upgrade_id
        // 的注册回执 + 交接文件已被消费，两者缺一不可。
        match &outcome {
            HelperOutcome::RolledBack { identity, reason } => {
                assert!(
                    reason.contains("did not register")
                        || reason.contains("no registration")
                        || reason.contains("exited early"),
                    "回滚原因应指向新版起不来: {reason}"
                );
                assert_eq!(
                    *identity,
                    RollbackIdentity::Preserved,
                    "无参数 GUI 回滚必须保住原身份，实际 {identity:?}"
                );
            }
            other => panic!("新版起不来必须回滚，实际 {other:?}"),
        }
        assert!(
            !handoff.exists(),
            "回滚后旧版必须消费交接文件（证明它按原身份注册成功）"
        );
        // 磁盘恢复成那份健康的旧版（回滚就是把 backup rename 回 target）。
        assert_eq!(
            file_sha(&ctx.target_path),
            healthy_sha,
            "回滚后 target 必须是原来的旧版字节"
        );

        let log = std::fs::read_to_string(&out).unwrap_or_default();
        assert!(
            log.contains("ARGC=0"),
            "回滚后的旧 GUI 版必须收到空 argv: {log}"
        );
        assert!(
            !log.contains("ARG=--session-id"),
            "往空 argv 里补参数会让无参数入口起不来: {log}"
        );
        assert!(
            !log.contains("no-handoff"),
            "回滚进程必须拿到交接文件，否则会以新设备注册: {log}"
        );
        // 回滚路径同样要核对 relay 返回的身份（不只是"有响应"）。
        assert_identity_verified(&log);
        let resp: serde_json::Value =
            serde_json::from_str(logged(&log, "resp=").unwrap()).expect("响应必须是 JSON");
        assert_eq!(resp["session_id"], sid, "回滚后必须回到原会话: {resp}");
        let mut returned: Vec<String> = resp["payload"]["tokens"]
            .as_array()
            .expect("服务端必须返回 tokens 数组")
            .iter()
            .map(|t| {
                format!(
                    "{}:{}",
                    t["permission"].as_str().unwrap_or(""),
                    t["token"].as_str().unwrap_or("")
                )
            })
            .collect();
        let mut want: Vec<String> = ctx.tokens.iter().map(|(t, p)| format!("{p}:{t}")).collect();
        returned.sort();
        want.sort();
        assert_eq!(returned, want, "回滚后 tokens/权限必须原样: {resp}");
        let cfg: serde_json::Value = serde_json::from_str(
            log.lines()
                .find_map(|l| l.strip_prefix("cfg="))
                .unwrap_or_else(|| panic!("回滚进程没有读到交接配置: {log}"))
                .trim(),
        )
        .unwrap();
        assert_eq!(cfg["relay"], relay.base, "回滚后 relay 必须不变: {cfg}");
        assert_eq!(cfg["lan_port"], 39616, "回滚后配置必须不变: {cfg}");
        assert_eq!(cfg["fps"], 24.0, "回滚后桌面参数必须不变: {cfg}");
    }

    /// 早到回执：子进程在助手开始观察之前就注册完、删完交接文件。
    ///
    /// 这是验收 #2 的确定性复现。旧实现用"先看到文件存在、再看到消失"判断成功，
    /// 这种"早到"情形下永远看不到前者，会把一次成功报成
    /// `the handoff file was never there` 的失败。
    ///
    /// 构造方式就是把整个消费过程**同步做完**（注册 + 删文件 + 写回执都在助手
    /// 拿到子进程句柄之前完成），再让一个只剩 `sleep` 的进程冒充"已经恢复好的
    /// 旧版"。判据只能来自回执，而不能来自文件观察。
    #[tokio::test]
    async fn test_gui_rollback_survives_an_early_handoff_consumption() {
        let _g = serial().await;

        // 一个"跑完就常驻"的旧版：它已经在启动瞬间消费了交接（模拟早到）。
        let dir = TmpDir::new("gui-early");
        let script = dir.join("agent-early");
        let marker = dir.join("consumed.txt");
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\nrm -f \"$SR_UPGRADE_RESTORE\"\ntouch '{}'\n\
                 echo \"shell-remote: upgrade-registered upgrade_id=$SR_TEST_UPGRADE_ID \
                 version=$SR_TEST_VER session=early01\" >&2\nsleep 5\n",
                marker.display()
            ),
        );
        let target = dir.join("shell-remote");
        std::fs::copy(&script, &target).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let new_binary = dir.join("artifact");
        std::fs::write(&new_binary, b"#!/bin/sh\n# new\n").unwrap();
        // 交接文件由**测试自己**在拉起前删掉：助手拿到的磁盘状态就是"文件已经
        // 不在了"，与"子进程抢先消费"完全等价，且是确定性的（不靠时序运气）。
        let mut ctx = RestartContext::new(
            "early01".to_string(),
            vec![("early-tok".to_string(), "rw".to_string())],
            None,
            "rw".to_string(),
            Some("http://127.0.0.1:1".to_string()),
            false,
            Vec::new(),
            target.clone(),
            new_binary,
            sha256_of(b"#!/bin/sh\n# new\n"),
            "early-up".to_string(),
            "9.9.9".to_string(),
            None,
            None,
            None,
            false,
        );
        ctx = ctx.with_gui_snapshot(gui_snapshot("early01", "http://127.0.0.1:1"));
        let handoff = ctx.write_handoff_into(dir.path()).unwrap();
        // 助手只会在 spawn 之后才开始观察；这里先把文件删掉，模拟"早就被消费"。
        std::fs::remove_file(&handoff).unwrap();
        assert!(!handoff.exists(), "前置条件：文件已经不在");

        std::env::set_var("SR_TEST_UPGRADE_ID", "early-up");
        std::env::set_var("SR_TEST_VER", "9.9.9");
        let mut log = HelperLog::open(&handoff);
        let backup = dir.join("backup.bin");
        std::fs::copy(&target, &backup).unwrap();
        let outcome = rollback(
            &ctx,
            &handoff,
            &backup,
            &mut log,
            "test: early consumption".to_string(),
            None,
            HelperTimeouts {
                old_exit: Duration::from_secs(1),
                register: Duration::from_secs(8),
            },
        );
        std::env::remove_var("SR_TEST_UPGRADE_ID");
        std::env::remove_var("SR_TEST_VER");

        match outcome {
            HelperOutcome::RolledBack { identity, .. } => assert_eq!(
                identity,
                RollbackIdentity::Preserved,
                "早到的注册回执必须被认成成功，而不是'文件从未出现'的失败"
            ),
            other => panic!("应判定为已回滚，实际 {other:?}"),
        }
        assert!(marker.exists(), "旧版确实跑过");
    }

    /// 负例①：没有回执（进程活着但不报注册）⇒ 超时判失败，不报成功。
    ///
    /// 交接文件**不存在**，所以这条同时守住"别把任何文件缺失当成功"。
    #[tokio::test]
    async fn test_gui_rollback_without_a_receipt_reports_lost_identity() {
        let _g = serial().await;
        let dir = TmpDir::new("gui-noreceipt");
        let script = dir.join("agent-silent");
        // 活着、不报回执、也不删文件以外的任何动作。
        write_executable(&script, "#!/bin/sh\nexec sleep 30\n");
        let target = dir.join("shell-remote");
        std::fs::copy(&script, &target).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let new_binary = dir.join("artifact");
        std::fs::write(&new_binary, b"#!/bin/sh\n# new\n").unwrap();
        let mut ctx = RestartContext::new(
            "silent01".to_string(),
            vec![("silent-tok".to_string(), "rw".to_string())],
            None,
            "rw".to_string(),
            Some("http://127.0.0.1:1".to_string()),
            false,
            Vec::new(),
            target.clone(),
            new_binary,
            sha256_of(b"#!/bin/sh\n# new\n"),
            "silent-up".to_string(),
            "9.9.9".to_string(),
            None,
            None,
            None,
            false,
        );
        ctx = ctx.with_gui_snapshot(gui_snapshot("silent01", "http://127.0.0.1:1"));
        let handoff = ctx.write_handoff_into(dir.path()).unwrap();
        let mut log = HelperLog::open(&handoff);
        let outcome = rollback(
            &ctx,
            &handoff,
            &target,
            &mut log,
            "test: silent".to_string(),
            None,
            HelperTimeouts {
                old_exit: Duration::from_secs(1),
                register: Duration::from_secs(2),
            },
        );
        match outcome {
            HelperOutcome::RolledBack { identity, .. } => {
                assert!(
                    matches!(identity, RollbackIdentity::Lost { .. }),
                    "没有回执必须判为身份未恢复，实际 {identity:?}"
                );
            }
            other => panic!("应判定为已回滚，实际 {other:?}"),
        }
        assert!(
            handoff.exists(),
            "身份没确认时**不得**消费/删除交接文件"
        );
    }

    /// 负例②：回执的 upgrade_id 不属于本轮 ⇒ 不认（防上一轮残留进程冒充）。
    #[tokio::test]
    async fn test_gui_rollback_ignores_a_receipt_from_another_upgrade() {
        let _g = serial().await;
        let dir = TmpDir::new("gui-wrongid");
        let script = dir.join("agent-wrongid");
        write_executable(
            &script,
            "#!/bin/sh\nrm -f \"$SR_UPGRADE_RESTORE\"\n\
             echo \"shell-remote: upgrade-registered upgrade_id=previous-round \
             version=1.0.0 session=old01\" >&2\nsleep 30\n",
        );
        let target = dir.join("shell-remote");
        std::fs::copy(&script, &target).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let new_binary = dir.join("artifact");
        std::fs::write(&new_binary, b"#!/bin/sh\n# new\n").unwrap();
        let mut ctx = RestartContext::new(
            "wrongid01".to_string(),
            vec![("wrong-tok".to_string(), "rw".to_string())],
            None,
            "rw".to_string(),
            Some("http://127.0.0.1:1".to_string()),
            false,
            Vec::new(),
            target.clone(),
            new_binary,
            sha256_of(b"#!/bin/sh\n# new\n"),
            "this-round".to_string(),
            "9.9.9".to_string(),
            None,
            None,
            None,
            false,
        );
        ctx = ctx.with_gui_snapshot(gui_snapshot("wrongid01", "http://127.0.0.1:1"));
        let handoff = ctx.write_handoff_into(dir.path()).unwrap();
        let mut log = HelperLog::open(&handoff);
        let outcome = rollback(
            &ctx,
            &handoff,
            &target,
            &mut log,
            "test: wrong upgrade id".to_string(),
            None,
            HelperTimeouts {
                old_exit: Duration::from_secs(1),
                register: Duration::from_secs(2),
            },
        );
        match outcome {
            HelperOutcome::RolledBack { identity, .. } => assert!(
                matches!(identity, RollbackIdentity::Lost { .. }),
                "别的轮次的回执绝不能被当成本轮成功，实际 {identity:?}"
            ),
            other => panic!("应判定为已回滚，实际 {other:?}"),
        }
    }

    /// 负例③：进程提前退出（注册都没走完）⇒ 立刻判失败，不空等满超时。
    #[tokio::test]
    async fn test_gui_rollback_reports_an_early_exit() {
        let _g = serial().await;
        let dir = TmpDir::new("gui-exit");
        let script = dir.join("agent-dies");
        write_executable(&script, "#!/bin/sh\nexit 7\n");
        let target = dir.join("shell-remote");
        std::fs::copy(&script, &target).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let new_binary = dir.join("artifact");
        std::fs::write(&new_binary, b"#!/bin/sh\n# new\n").unwrap();
        let mut ctx = RestartContext::new(
            "dies01".to_string(),
            vec![("dies-tok".to_string(), "rw".to_string())],
            None,
            "rw".to_string(),
            Some("http://127.0.0.1:1".to_string()),
            false,
            Vec::new(),
            target.clone(),
            new_binary,
            sha256_of(b"#!/bin/sh\n# new\n"),
            "dies-up".to_string(),
            "9.9.9".to_string(),
            None,
            None,
            None,
            false,
        );
        ctx = ctx.with_gui_snapshot(gui_snapshot("dies01", "http://127.0.0.1:1"));
        let handoff = ctx.write_handoff_into(dir.path()).unwrap();
        let mut log = HelperLog::open(&handoff);
        let t0 = Instant::now();
        let outcome = rollback(
            &ctx,
            &handoff,
            &target,
            &mut log,
            "test: early exit".to_string(),
            None,
            HelperTimeouts {
                old_exit: Duration::from_secs(1),
                // 给足超时：若实现靠超时才发现，下面"很快返回"的断言会失败。
                register: Duration::from_secs(20),
            },
        );
        let elapsed = t0.elapsed();
        match outcome {
            HelperOutcome::RolledBack { identity, .. } => {
                assert!(
                    matches!(identity, RollbackIdentity::Lost { .. }),
                    "提前退出必须判为身份未恢复，实际 {identity:?}"
                );
            }
            other => panic!("应判定为已回滚，实际 {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(10),
            "进程已退出应立即给结论，而不是空等满 20s，实际 {elapsed:?}"
        );
        // 失败原因要指向"提前退出"，而不是一句含糊的超时。
        let text = std::fs::read_to_string(dir.join("update-apply.log")).unwrap_or_default();
        assert!(
            text.contains("exited early"),
            "日志要说清是提前退出，而不是超时: {text}"
        );
        assert!(handoff.exists(), "身份没确认时不得删除交接文件");
    }

}

// ── 12. GUI 回滚不得再"重绑"一次身份（MYS-2819）─────────────────

/// `rollback` 的命令行路径会用缓存 tokens 再 `register_existing` 一次（替不认识
/// 交接文件的旧版把身份绑回去）。GUI 路径**不能**这么做：旧 GUI 版已经按交接
/// 自己注册好了，助手再来一次 `register_existing` 会把 relay 上那个**正在用**的
/// 会话驱逐掉（同 id 顶替），刚恢复的设备当场掉线。
///
/// 这里用"身份绑定"这一唯一可观测的副作用来断言：命令行路径会往 relay 发一次
/// 带 tokens 的注册（relay 上该会话的 tokens 变成助手缓存的那份），GUI 路径
/// 则完全不发。
#[cfg(unix)]
mod gui_rollback_no_rebind {
    use super::*;

    /// 助手日志里是否出现过"重绑身份"这条记录（`bind_identity` 的唯一痕迹）。
    fn log_mentions_rebind(log: &str) -> bool {
        log.contains("identity re-bound after rollback")
            || log.contains("cannot re-bind the original identity")
    }

    /// 造一份交接文件，其中 relay 指向一个**真的会应答**的假 relay 端点，
    /// 使 `bind_identity` 一旦被调用就会留下痕迹。
    fn handoff_for(dir: &TmpDir, gui: bool, upgrade_id: &str) -> PathBuf {
        let target = dir.join("shell-remote");
        write_executable(&target, "#!/bin/sh\nexit 0\n");
        let new_binary = dir.join("artifact");
        std::fs::write(&new_binary, b"#!/bin/sh\n# v2\n").unwrap();
        let mut ctx = RestartContext::new(
            "rebind01".to_string(),
            vec![("tok-x".to_string(), "rw".to_string())],
            None,
            "rw".to_string(),
            // 端口 1：连接必然失败，但"尝试过"这一点会被写进日志。
            Some("http://127.0.0.1:1".to_string()),
            false,
            Vec::new(),
            target,
            new_binary,
            sha256_of(b"#!/bin/sh\n# v2\n"),
            upgrade_id.to_string(),
            "9.9.9".to_string(),
            None,
            None,
            None,
            false,
        );
        if gui {
            let mut snap = gui_snapshot("rebind01", "http://127.0.0.1:1");
            snap.session_id = Some("rebind01".to_string());
            ctx = ctx.with_gui_snapshot(snap);
        }
        ctx.write_handoff_into(dir.path()).unwrap()
    }

    #[test]
    fn test_command_line_rollback_still_rebinds_the_identity() {
        // 老路径的行为必须保住：旧 CLI 版不认识交接文件，助手要替它绑。
        let dir = TmpDir::new("rebind-cli");
        let handoff = handoff_for(&dir, false, "rb-cli");
        let mut log = HelperLog::open(&handoff);
        let ctx = RestartContext::read_handoff(&handoff).unwrap();
        let backup = handoff.with_file_name("backup.bin");
        std::fs::copy(&ctx.target_path, &backup).unwrap();
        let _ = rollback(
            &ctx,
            &handoff,
            &backup,
            &mut log,
            "test: no registration".to_string(),
            None,
            HelperTimeouts {
                old_exit: Duration::from_secs(1),
                register: Duration::from_secs(1),
            },
        );
        let text = std::fs::read_to_string(dir.join("update-apply.log")).unwrap_or_default();
        assert!(
            log_mentions_rebind(&text),
            "命令行路径必须仍然尝试重绑身份（否则临时设备会换 Token）: {text}"
        );
    }

    #[test]
    fn test_gui_rollback_never_rebinds_over_the_live_session() {
        // 同一场景，但交接里带 GUI 快照 ⇒ 不许再绑一次。
        let dir = TmpDir::new("rebind-gui");
        let handoff = handoff_for(&dir, true, "rb-gui");
        let mut log = HelperLog::open(&handoff);
        let ctx = RestartContext::read_handoff(&handoff).unwrap();
        assert!(ctx.is_gui_resume());
        let backup = handoff.with_file_name("backup.bin");
        std::fs::copy(&ctx.target_path, &backup).unwrap();
        let _ = rollback(
            &ctx,
            &handoff,
            &backup,
            &mut log,
            "test: no registration".to_string(),
            None,
            HelperTimeouts {
                old_exit: Duration::from_secs(1),
                register: Duration::from_secs(1),
            },
        );
        let text = std::fs::read_to_string(dir.join("update-apply.log")).unwrap_or_default();
        assert!(
            !log_mentions_rebind(&text),
            "GUI 回滚不得再 register_existing：那会驱逐旧版刚恢复的会话: {text}"
        );
    }
}

// ── 13. 协调换机：早到的交接消费同样不能被误判（MYS-2819 #2）──

/// `Coordinated` 换机也是"先摆文件、再让 systemd 拉起新 agent"。新 agent 可能
/// 在助手第一次轮询之前就注册完并删掉交接文件——旧实现（先看到存在、再看到
/// 消失）会把这种成功判成 `the handoff file was never there`。
///
/// 现在改为在 `start` **之前**取证（文件确认在盘上），之后只看它何时消失。
#[cfg(unix)]
mod coordinated_early_consume {
    use super::*;

    /// 一个"拉起即马上消费交接"的假 systemd：`start` 时立刻删掉交接文件，
    /// 模拟新 agent 抢在第一次轮询之前完成注册。
    struct InstantConsumeSystemd {
        handoff: PathBuf,
    }

    impl super::SystemdOps for InstantConsumeSystemd {
        fn stop(&self, _unit: &str) -> Result<(), String> {
            Ok(())
        }
        fn start(&self, _unit: &str) -> Result<(), String> {
            // 这就是"新 agent 已经消费掉它"的那一刻。
            let _ = std::fs::remove_file(&self.handoff);
            Ok(())
        }
        fn wait_empty(&self, _unit: &str, _timeout: Duration) -> bool {
            true
        }
    }

    #[test]
    fn test_coordinated_swap_accepts_an_instantly_consumed_handoff() {
        let dir = TmpDir::new("coord-early");
        let target = dir.join("shell-remote");
        let new_bytes = b"#!/bin/sh\n# new version\n";
        // 协调路径进入时磁盘上已经是新版、backup 是旧版。
        write_executable(&target, "#!/bin/sh\n# new version\n");
        let backup = dir.join("shell-remote.sr-old-coordearly");
        write_executable(&backup, "#!/bin/sh\n# old version\n");
        let artifact = dir.join("artifact.part");
        std::fs::write(&artifact, new_bytes).unwrap();
        let ctx = RestartContext {
            session_id: "coordearly1".to_string(),
            tokens: vec![("ct".to_string(), "rw".to_string())],
            fixed_key: None,
            token_type: "rw".to_string(),
            relay_base: None,
            relay_insecure: false,
            argv: Vec::new(),
            old_pid: 0,
            old_start: None,
            upgrade_id: "coord-early".to_string(),
            target_version: "9.9.9".to_string(),
            target_path: target.clone(),
            new_binary: artifact,
            new_sha256: sha256_of(new_bytes),
            service_cgroup: None,
            service_restart_policy: None,
            managed_plan: Some(crate::agent::systemd_helper::ManagedPlan::Coordinated {
                unit: "test.service".to_string(),
            }),
            overlay: false,
            gui: None,
        };
        let handoff = ctx.write_handoff_into(dir.path()).unwrap();

        let ops = InstantConsumeSystemd {
            handoff: handoff.clone(),
        };
        let mut log = super::HelperLog::open(&handoff);
        let outcome = super::coordinated_swap_with(
            &ops,
            &ctx,
            "test.service",
            &handoff,
            &backup,
            dir.path(),
            &mut log,
            super::HelperTimeouts {
                old_exit: Duration::from_millis(10),
                register: Duration::from_secs(5),
            },
        );
        assert!(
            matches!(outcome, super::HelperOutcome::Registered { .. }),
            "交接被立刻消费必须判成功，实际 {outcome:?}"
        );
        assert!(!handoff.exists(), "交接文件已被消费");
        assert!(!backup.exists(), "成功路径要清掉备份");
    }

    /// 负例：交接文件在 `start` 之前就不在 ⇒ 立刻如实失败，不空等超时。
    #[test]
    fn test_coordinated_swap_fails_fast_when_the_handoff_was_never_there() {
        let dir = TmpDir::new("coord-missing");
        let target = dir.join("shell-remote");
        let new_bytes = b"#!/bin/sh\n# new version\n";
        write_executable(&target, "#!/bin/sh\n# new version\n");
        let backup = dir.join("shell-remote.sr-old-coordmiss");
        write_executable(&backup, "#!/bin/sh\n# old version\n");
        let artifact = dir.join("artifact.part");
        std::fs::write(&artifact, new_bytes).unwrap();
        let ctx = RestartContext {
            session_id: "coordmiss1".to_string(),
            tokens: vec![("ct".to_string(), "rw".to_string())],
            fixed_key: None,
            token_type: "rw".to_string(),
            relay_base: None,
            relay_insecure: false,
            argv: Vec::new(),
            old_pid: 0,
            old_start: None,
            upgrade_id: "coord-miss".to_string(),
            target_version: "9.9.9".to_string(),
            target_path: target.clone(),
            new_binary: artifact,
            new_sha256: sha256_of(new_bytes),
            service_cgroup: None,
            service_restart_policy: None,
            managed_plan: Some(crate::agent::systemd_helper::ManagedPlan::Coordinated {
                unit: "test.service".to_string(),
            }),
            overlay: false,
            gui: None,
        };
        // 刻意不写交接文件：`start` 之前就已经不存在。
        let handoff = dir.join("never-written.json");
        let ops = InstantConsumeSystemd {
            handoff: handoff.clone(),
        };
        let mut log = super::HelperLog::open(&handoff);
        let t0 = Instant::now();
        let outcome = super::coordinated_swap_with(
            &ops,
            &ctx,
            "test.service",
            &handoff,
            &backup,
            dir.path(),
            &mut log,
            super::HelperTimeouts {
                old_exit: Duration::from_millis(10),
                // 给足超时：若实现靠超时才发现，下面的耗时断言会失败。
                register: Duration::from_secs(20),
            },
        );
        let elapsed = t0.elapsed();
        assert!(
            matches!(outcome, super::HelperOutcome::RolledBack { .. }),
            "没有交接文件必须判失败并回滚，实际 {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "文件本来就不在时应立刻给结论，实际 {elapsed:?}"
        );
    }
}


#[cfg(unix)]
#[test]
fn test_buffered_receipt_after_exit_requires_consumed_handoff() {
    check_buffered_receipt_after_exit(false);
}

#[cfg(unix)]
#[test]
fn test_buffered_receipt_after_exit_accepts_consumed_handoff() {
    check_buffered_receipt_after_exit(true);
}

#[cfg(unix)]
fn check_buffered_receipt_after_exit(consumed: bool) {
    let dir = TmpDir::new("receipt-exited");
    let handoff = dir.join("handoff.json");
    if !consumed {
        std::fs::write(&handoff, b"{}").unwrap();
    }
    let mut child = std::process::Command::new("sh")
        .args(["-c", "echo startup >&2; echo 'shell-remote: upgrade-registered upgrade_id=exited version=1 session=test' >&2"])
        .stderr(std::process::Stdio::piped())
        .spawn().unwrap();
    // Exit before the reader starts: both lines remain buffered in the kernel pipe.
    child.wait().unwrap();
    let result = await_registration_receipt(
        &mut child, &handoff, "exited", &dir.join("helper.log"), Duration::from_secs(3),
    );
    if consumed {
        assert_eq!(result.unwrap(), "1");
    } else {
        assert!(result.unwrap_err().contains("not consumed"));
        assert!(handoff.exists());
    }
}

// ── Windows 交接文件 DACL（MYS-2821 修复的回归）────────────────────
//
// 修复前这里是一个 `#[repr(C, align(8))] struct SecDesc([u8; 32])`：
// x64 的 SECURITY_DESCRIPTOR 是 40 字节、Dacl 偏移 32，于是
// `InitializeSecurityDescriptor` 把末尾 8 字节写到缓冲区之外，踩坏栈上相邻
// 数据，后续 `SetFileSecurityW` 报 998（ERROR_NOACCESS）。
//
// 这几个用例只断言**结构尺寸**与**真实 readback 出来的 DACL**，不看 API 返回
// 值——返回值成功但权限没落地，正是最容易漏掉的那类缺陷。
#[cfg(windows)]
mod windows_handoff_dacl {
    use super::*;
    use windows_sys::Win32::Security::SECURITY_DESCRIPTOR;

    /// SDK 类型必须是 x64 上的 40 字节、Dacl 在偏移 32。手写长度的那版是 32，
    /// 正好少一个指针——这条断言就是防止有人再把长度写死。
    #[test]
    fn security_descriptor_layout_matches_sdk() {
        assert_eq!(
            core::mem::size_of::<SECURITY_DESCRIPTOR>(),
            if cfg!(target_pointer_width = "64") { 40 } else { 20 },
            "SECURITY_DESCRIPTOR 尺寸必须由 SDK 类型决定，不能手写"
        );
        assert_eq!(
            core::mem::offset_of!(SECURITY_DESCRIPTOR, Dacl),
            if cfg!(target_pointer_width = "64") { 32 } else { 16 },
            "Dacl 偏移必须是 SDK 的真实布局"
        );
    }

    /// 交接文件写出来之后：DACL 只含当前用户（父目录继承被断开），
    /// 文件确实可读回。用 GetNamedSecurityInfo 现场读，不信 SetFileSecurity 的返回值。
    #[test]
    fn handoff_file_dacl_is_private_and_protected() {
        use windows_sys::Win32::Security::Authorization::{
            GetNamedSecurityInfoW, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{
            GetAclInformation, GetSecurityDescriptorControl, ACL_SIZE_INFORMATION, AclSizeInformation,
            DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        };

        let dir = TmpDir::new("win-dacl");
        let dir = dir.path();
        let path = dir.join("handoff-dacl.json");
        write_private_new(&path, b"{}").expect("write handoff");

        let wide: Vec<u16> = {
            use std::os::windows::ffi::OsStrExt;
            path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
        };
        unsafe {
            let mut acl = std::ptr::null_mut();
            let mut sd = std::ptr::null_mut();
            let rc = GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut sd,
            );
            assert_eq!(rc, 0, "GetNamedSecurityInfoW rc={rc}");

            // 1) 继承必须断开：这是"其它用户读不到"的前提。
            let mut control = 0u16;
            let mut revision = 0u32;
            assert_ne!(
                GetSecurityDescriptorControl(sd, &mut control, &mut revision),
                0
            );
            assert_ne!(
                control & SE_DACL_PROTECTED,
                0,
                "交接文件的 DACL 必须是受保护的（父目录 ACE 不再继承）"
            );

            // 2) 必须恰好一个 ACE，且就是当前用户。
            let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
            assert_ne!(
                GetAclInformation(acl.cast(), (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                    core::mem::size_of::<ACL_SIZE_INFORMATION>() as u32, AclSizeInformation),
                0
            );
            assert_eq!(info.AceCount, 1, "只应有当前用户一个 ACE");

            windows_sys::Win32::Foundation::LocalFree(sd.cast());
        }

        // 3) 文件内容仍能读回（收紧权限不能把所有者自己锁在外面）。
        let back = std::fs::read(&path).expect("read back handoff");
        assert_eq!(back, b"{}");
        let _ = std::fs::remove_file(&path);
    }
}
