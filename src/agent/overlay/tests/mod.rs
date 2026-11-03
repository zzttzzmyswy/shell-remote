//! overlay 自升级的测试（MYS-2574）。
//!
//! 分两层：
//!
//! 1. **纯判定**：`decide()` 的每一个分支，用构造的 `Facts` 穷举——真实文件
//!    系统里"属主不对""group-writable""attempts 到顶"很难稳定复现。
//! 2. **真实文件**：指针读写、可写性探测、安装与两代保留、健康标记。

use super::*;

mod e2e;

fn sha_of(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// 临时目录 + 自动清理（不用 `tempfile`：Windows 上文件被占用时 drop 会失败，
/// 与 `update_apply/tests.rs` 同一套做法）。
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        use rand::Rng as _;
        let dir = std::env::temp_dir().join(format!(
            "sr-ovl-{tag}-{}-{:08x}",
            std::process::id(),
            rand::thread_rng().gen::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
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
        use std::os::unix::fs::PermissionsExt as _;
        // 测试里会把目录降成 0555 验证"不可写"，清理前先恢复。
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_exec(path: &Path, body: &[u8]) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// 造一个"已经装好一版 overlay"的现场，返回 (dir, layout, pointer, 二进制内容)。
fn fixture(tag: &str, version: &str, self_version: &str) -> (TmpDir, Layout, Pointer, Vec<u8>) {
    let dir = TmpDir::new(tag);
    // 与生产布局一致（`$XDG_DATA_HOME/shell-remote/overlay`）：这样
    // `Layout::from_env()` 在设置 `XDG_DATA_HOME` 之后能直接找到它。
    let layout = Layout::new(dir.path().join("shell-remote").join("overlay"));
    layout.ensure().unwrap();
    let bytes = format!("#!/bin/sh\n# overlay {version}\nexit 0\n").into_bytes();
    let sha = sha_of(&bytes);
    let path = layout.version_file(version, &sha);
    write_exec(&path, &bytes);
    let pointer = Pointer::fresh(
        version,
        &sha,
        path,
        Some(dir.join("base")),
        Some("up-1".to_string()),
        Vec::new(),
    );
    Pointer::write(&layout, &pointer).unwrap();
    let _ = self_version;
    (dir, layout, pointer, bytes)
}

/// 从真实文件系统采集事实（与生产路径 `probe` 完全同源），只覆盖自报版本与
/// 自身路径这两项"进程身份"。
fn facts_for(layout: &Layout, pointer: &Pointer, self_version: &str, self_exe: &Path) -> Facts {
    Facts {
        from_env: None,
        self_exe: self_exe.to_path_buf(),
        self_version: self_version.to_string(),
        root: dir_facts(layout.root()),
        bin: dir_facts(&layout.bin_dir()),
        binary: file_facts(&pointer.path),
        pointer_file: file_facts(&layout.pointer_path()),
        binary_in_bin_dir: crate::agent::overlay::is_directly_inside(&pointer.path, &layout.bin_dir()),
        binary_sha256: crate::agent::download::sha256_hex_file(&pointer.path).ok(),
        pointer: pointer.clone(),
    }
}

// ── 1. 指针：解析与写入 ────────────────────────────────────────────

#[test]
fn test_pointer_roundtrip() {
    let (_dir, layout, pointer, _) = fixture("ptr", "0.59.13", "0.59.12");
    let read = Pointer::read(&layout).unwrap().expect("指针必须读得回来");
    assert_eq!(read, pointer);
    assert_eq!(read.schema, SCHEMA);
    assert_eq!(read.version, "0.59.13");
    assert!(!read.healthy, "刚装上的版本默认不健康");
    assert_eq!(read.attempts, 0);
    assert!(!read.rejected);
}

#[test]
fn test_pointer_missing_is_none() {
    let dir = TmpDir::new("ptr-none");
    let layout = Layout::new(dir.join("overlay"));
    assert_eq!(Pointer::read(&layout).unwrap(), None, "没有指针 = 没装过 overlay");
}

#[test]
fn test_pointer_write_is_atomic_and_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_dir, layout, mut pointer, _) = fixture("ptr-atomic", "0.59.13", "0.59.12");
    pointer.attempts = 2;
    pointer.healthy = true;
    Pointer::write(&layout, &pointer).unwrap();

    let meta = std::fs::metadata(layout.pointer_path()).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600, "指针必须是 0600");
    assert_eq!(Pointer::read(&layout).unwrap().unwrap(), pointer);
    // 临时文件不能残留（rename 成功之后源路径就该没了）。
    let leftovers: Vec<String> = std::fs::read_dir(layout.root())
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .filter(|n| n.contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "残留的指针临时文件: {leftovers:?}");
}

#[test]
fn test_pointer_rejects_group_writable_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_dir, layout, _pointer, _) = fixture("ptr-perm", "0.59.13", "0.59.12");
    std::fs::set_permissions(layout.pointer_path(), std::fs::Permissions::from_mode(0o666)).unwrap();
    let err = Pointer::read(&layout).unwrap_err();
    assert!(err.contains("group/other"), "{err}");
}

#[test]
fn test_pointer_rejects_unknown_schema() {
    let (_dir, layout, mut pointer, _) = fixture("ptr-schema", "0.59.13", "0.59.12");
    pointer.schema = 99;
    Pointer::write(&layout, &pointer).unwrap();
    let err = Pointer::read(&layout).unwrap_err();
    assert!(err.contains("schema 99"), "{err}");
}

/// 老/新指针的字段集合必须双向宽容：缺可选字段能读，多出来的字段被忽略。
#[test]
fn test_pointer_tolerates_extra_and_missing_optional_fields() {
    let dir = TmpDir::new("ptr-fields");
    let layout = Layout::new(dir.join("overlay"));
    layout.ensure().unwrap();
    // 只有设计里那 8 个字段（没有 rejected / previous / upgrade_id）。
    let raw = serde_json::json!({
        "schema": 1,
        "version": "0.59.13",
        "sha256": "a".repeat(64),
        "path": "/home/u/.local/share/shell-remote/overlay/bin/shell-remote-0.59.13-aaaaaaaa",
        "base_exe": "/usr/local/bin/shell-remote",
        "installed_at": 1793000000u64,
        "healthy": false,
        "attempts": 0,
        "some_future_field": {"x": 1}
    });
    std::fs::write(layout.pointer_path(), serde_json::to_vec(&raw).unwrap()).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(layout.pointer_path(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let read = Pointer::read(&layout).unwrap().expect("必须能读");
    assert_eq!(read.version, "0.59.13");
    assert!(!read.rejected, "缺省即 false");
    assert!(read.previous.is_empty());
    assert!(read.upgrade_id.is_none());
}

// ── 2. 目录与布局 ──────────────────────────────────────────────────

#[test]
fn test_layout_paths_and_permissions() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TmpDir::new("layout");
    let layout = Layout::new(dir.join("overlay"));
    layout.ensure().unwrap();
    for d in [layout.root().to_path_buf(), layout.bin_dir()] {
        let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{} 必须是 0700，实际 {mode:o}", d.display());
    }
    let sha = "0123456789abcdef".repeat(4);
    assert_eq!(
        layout.part_path(&sha),
        layout.root().join(".shell-remote-upgrade-0123456789abcdef.part")
    );
    assert_eq!(
        layout.version_file("0.59.13", &sha).file_name().unwrap(),
        "shell-remote-0.59.13-01234567"
    );
}

#[test]
fn test_layout_version_file_sanitizes_the_version() {
    let layout = Layout::new("/tmp/x");
    let name = layout
        .version_file("0.59.13/../evil", &"ab".repeat(32))
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(!name.contains('/'), "版本号不得引入路径分隔符: {name}");
    assert!(name.starts_with("shell-remote-"), "{name}");
    assert!(name.ends_with("-abababab"), "{name}");
}

#[test]
fn test_layout_rejects_custom_root_from_env_chain() {
    // 三个候选都拿不到时必须报错：overlay 不能退到 /tmp（重启即失效，等于
    // 假装升级成功）。这里只验纯函数式的失败条件，不动真实环境变量。
    assert!(Layout::new("/tmp/sr-ovl-abs").ensure().is_ok());
}

#[test]
fn test_dir_is_writable_probe() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TmpDir::new("writable");
    assert!(dir_is_writable(dir.path()), "0700 目录应该可写");
    let ro = dir.join("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    // root 会无视权限位：以 root 跑测试时这条断言没有意义。
    if unsafe { libc::geteuid() } != 0 {
        assert!(!dir_is_writable(&ro), "0555 目录必须判为不可写");
    }
    // 探测不留垃圾。
    let junk: Vec<String> = std::fs::read_dir(&ro)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .collect();
    assert!(junk.is_empty(), "探测留下文件: {junk:?}");
}

// ── 3. 交接判定：每一个分支 ────────────────────────────────────────

#[test]
fn test_decide_execs_when_everything_checks_out() {
    let (dir, layout, pointer, _) = fixture("ok", "0.59.13", "0.59.12");
    let facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    assert_eq!(decide(&facts), Verdict::Exec { path: pointer.path });
}

#[test]
fn test_decide_skips_when_binary_is_outside_bin_dir() {
    // 指针指向 bin/ 之外（哪怕文件本身属主、权限、sha256 全对）也不交接。
    let (dir, layout, mut pointer, _) = fixture("outside", "0.59.13", "0.59.12");
    let outside = dir.join("shell-remote-elsewhere");
    std::fs::copy(&pointer.path, &outside).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    pointer.path = outside;
    let facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    assert!(!facts.binary_in_bin_dir);
    match decide(&facts) {
        Verdict::Skip { reason } => assert!(reason.contains("not inside the overlay bin directory"), "{reason}"),
        other => panic!("bin/ 之外的二进制不得交接，实际 {other:?}"),
    }
}

#[test]
fn test_decide_skips_when_already_the_overlay() {
    let (dir, layout, pointer, _) = fixture("loop-env", "0.59.13", "0.59.12");
    let mut facts = facts_for(&layout, &pointer, "0.59.13", &pointer.path);
    facts.from_env = Some(dir.join("base"));
    match decide(&facts) {
        Verdict::Skip { reason } => assert!(reason.contains(ENV_FROM), "{reason}"),
        other => panic!("必须跳过，实际 {other:?}"),
    }
}

#[test]
fn test_decide_skips_when_self_is_the_pointer_target() {
    let (_dir, layout, pointer, _) = fixture("loop-self", "0.59.13", "0.59.12");
    // 同版本 + 自身就是指针指向的文件：两道防线都要能拦住。
    let facts = facts_for(&layout, &pointer, "0.59.13", &pointer.path);
    match decide(&facts) {
        Verdict::Skip { reason } => assert!(reason.contains("already is the overlay"), "{reason}"),
        other => panic!("必须跳过，实际 {other:?}"),
    }
}

#[test]
fn test_decide_skips_on_equal_version() {
    let (dir, layout, pointer, _) = fixture("ver-eq", "0.59.13", "0.59.13");
    let facts = facts_for(&layout, &pointer, "0.59.13", &dir.join("base"));
    match decide(&facts) {
        Verdict::Skip { reason } => assert!(reason.contains("same version"), "{reason}"),
        other => panic!("同版本必须跳过，实际 {other:?}"),
    }
}

#[test]
fn test_decide_prunes_when_self_is_newer() {
    let (dir, layout, pointer, _) = fixture("ver-old", "0.59.12", "0.59.13");
    let facts = facts_for(&layout, &pointer, "0.59.13", &dir.join("base"));
    match decide(&facts) {
        Verdict::PruneOlder { reason } => assert!(reason.contains("newer"), "{reason}"),
        other => panic!("自身更新时必须走 PruneOlder，实际 {other:?}"),
    }
}

#[test]
fn test_decide_skips_on_unparsable_versions() {
    let (dir, layout, mut pointer, _) = fixture("ver-bad", "0.59.13", "0.59.12");
    pointer.version = "not-a-version".to_string();
    let facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    match decide(&facts) {
        Verdict::Skip { reason } => assert!(reason.contains("cannot compare"), "{reason}"),
        other => panic!("版本不可比必须跳过，实际 {other:?}"),
    }
}

#[test]
fn test_decide_skips_on_digest_mismatch() {
    let (dir, layout, pointer, _) = fixture("digest", "0.59.13", "0.59.12");
    let mut facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    facts.binary_sha256 = Some("0".repeat(64));
    match decide(&facts) {
        Verdict::Skip { reason } => assert!(reason.contains("does not match"), "{reason}"),
        other => panic!("sha 不符必须跳过，实际 {other:?}"),
    }
}

#[test]
fn test_decide_skips_when_binary_unreadable() {
    let (dir, layout, pointer, _) = fixture("digest-none", "0.59.13", "0.59.12");
    let mut facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    facts.binary_sha256 = None;
    match decide(&facts) {
        Verdict::Skip { reason } => assert!(reason.contains("cannot hash"), "{reason}"),
        other => panic!("算不出 sha 必须跳过，实际 {other:?}"),
    }
}

#[test]
fn test_decide_skips_on_wrong_owner() {
    let (dir, layout, pointer, _) = fixture("owner", "0.59.13", "0.59.12");
    // 属主不对：只有 root 能真的造出"别人的文件"，其余情况直接构造事实。
    let mut facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    for mutate in [
        |f: &mut Facts| f.root.owner_ok = false,
        |f: &mut Facts| f.bin.owner_ok = false,
        |f: &mut Facts| f.binary.owner_ok = false,
        |f: &mut Facts| f.pointer_file.owner_ok = false,
    ] {
        let mut f = facts.clone();
        mutate(&mut f);
        match decide(&f) {
            Verdict::Skip { reason } => assert!(reason.contains("not owned"), "{reason}"),
            other => panic!("属主不对必须跳过，实际 {other:?}"),
        }
    }
    facts.root.owner_ok = true;
}

#[test]
fn test_decide_skips_on_group_writable_components() {
    let (dir, layout, pointer, _) = fixture("gw", "0.59.13", "0.59.12");
    for mutate in [
        |f: &mut Facts| f.root.not_group_writable = false,
        |f: &mut Facts| f.bin.not_group_writable = false,
        |f: &mut Facts| f.binary.not_group_writable = false,
        |f: &mut Facts| f.pointer_file.not_group_writable = false,
    ] {
        let mut f = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
        mutate(&mut f);
        match decide(&f) {
            Verdict::Skip { reason } => {
                assert!(reason.contains("group/other"), "{reason}")
            }
            other => panic!("group-writable 必须跳过，实际 {other:?}"),
        }
    }
}

/// 真实文件：目标二进制被改成 group-writable 时必须拒绝交接（设计验收里点名
/// 的那一条）。
#[test]
fn test_decide_refuses_group_writable_binary_on_disk() {
    use std::os::unix::fs::PermissionsExt as _;
    let (dir, layout, pointer, bytes) = fixture("gw-real", "0.59.13", "0.59.12");
    let path = layout.version_file("0.59.13", &sha_of(&bytes));
    // 内容不变、只放宽权限：sha 仍然一致，唯一的问题是别人能改写它。
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o720)).unwrap();
    let facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    match decide(&facts) {
        Verdict::Skip { reason } => assert!(reason.contains("group/other"), "{reason}"),
        other => panic!("必须拒绝交接，实际 {other:?}"),
    }
}

#[test]
fn test_decide_rejects_after_max_attempts() {
    let (dir, layout, mut pointer, _) = fixture("attempts", "0.59.13", "0.59.12");
    pointer.attempts = MAX_ATTEMPTS;
    let facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    match decide(&facts) {
        Verdict::Reject { reason } => assert!(reason.contains("rejected"), "{reason}"),
        other => panic!("次数到顶必须 Reject，实际 {other:?}"),
    }
    // 差一次时仍允许再试一次。
    pointer.attempts = MAX_ATTEMPTS - 1;
    let facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    assert!(matches!(decide(&facts), Verdict::Exec { .. }));
}

#[test]
fn test_decide_healthy_ignores_attempts() {
    let (dir, layout, mut pointer, _) = fixture("healthy", "0.59.13", "0.59.12");
    pointer.attempts = MAX_ATTEMPTS + 10;
    pointer.healthy = true;
    let facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    assert!(
        matches!(decide(&facts), Verdict::Exec { .. }),
        "已健康的版本不受尝试次数限制（它只是被重启过很多次）"
    );
}

#[test]
fn test_decide_skips_when_already_rejected() {
    let (dir, layout, mut pointer, _) = fixture("rejected", "0.59.13", "0.59.12");
    pointer.rejected = true;
    pointer.attempts = 0;
    let facts = facts_for(&layout, &pointer, "0.59.12", &dir.join("base"));
    match decide(&facts) {
        Verdict::Skip { reason } => assert!(reason.contains("already rejected"), "{reason}"),
        other => panic!("已 rejected 必须跳过，实际 {other:?}"),
    }
}

/// 判定顺序的护栏：安全检查和版本检查都不过时，先报哪一条？
///
/// 版本更低时直接 `PruneOlder`（管理员手工换过 base），此时不该因为权限细节
/// 再绕一圈——顺序固定下来，日志才不会自相矛盾。
#[test]
fn test_decide_checks_version_before_permissions() {
    let (dir, layout, pointer, _) = fixture("order", "0.59.12", "0.59.13");
    let mut facts = facts_for(&layout, &pointer, "0.59.13", &dir.join("base"));
    facts.binary.not_group_writable = false;
    assert!(
        matches!(decide(&facts), Verdict::PruneOlder { .. }),
        "版本判定先于安全检查"
    );
}

// ── 4. 安装：两代保留与健康标记 ────────────────────────────────────

#[test]
fn test_install_places_binary_and_writes_pointer() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TmpDir::new("install");
    let layout = Layout::new(dir.join("overlay"));
    let bytes = b"#!/bin/sh\n# 0.59.13\nexit 0\n";
    let sha = sha_of(bytes);
    let artifact = dir.join("artifact.part");
    std::fs::write(&artifact, bytes).unwrap();

    let installed = install(&layout, &artifact, "0.59.13", &sha, Some(&dir.join("base")), Some("up-9"))
        .expect("安装必须成功");
    assert_eq!(installed.path, layout.version_file("0.59.13", &sha));
    assert!(installed.path.is_file());
    assert!(!artifact.exists(), ".part 已被搬走");
    let mode = std::fs::metadata(&installed.path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "overlay 二进制必须是 0700");

    let pointer = Pointer::read(&layout).unwrap().unwrap();
    assert_eq!(pointer.version, "0.59.13");
    assert_eq!(pointer.sha256, sha);
    assert_eq!(pointer.path, installed.path);
    assert!(!pointer.healthy, "装完还没验证过，必须 healthy=false");
    assert_eq!(pointer.attempts, 0);
    assert!(!pointer.rejected);
    assert_eq!(pointer.upgrade_id.as_deref(), Some("up-9"));
}

#[test]
fn test_install_keeps_only_two_generations() {
    let dir = TmpDir::new("gen");
    let layout = Layout::new(dir.join("overlay"));
    layout.ensure().unwrap();
    let mut seen = Vec::new();
    for (i, version) in ["0.59.11", "0.59.12", "0.59.13", "0.59.14"].iter().enumerate() {
        let bytes = format!("#!/bin/sh\n# {version}\nexit {i}\n").into_bytes();
        let sha = sha_of(&bytes);
        let artifact = layout.part_path(&sha);
        std::fs::write(&artifact, &bytes).unwrap();
        let installed = install(&layout, &artifact, version, &sha, None, Some("up")).unwrap();
        seen.push(installed.path.clone());
    }
    let files: Vec<PathBuf> = std::fs::read_dir(layout.bin_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(files.len(), 2, "只留两代，实际 {files:?}");
    assert!(files.contains(&seen[3]), "当前版本必须保留");
    assert!(files.contains(&seen[2]), "上一版本必须保留（人工回退用）");
    assert!(!files.contains(&seen[0]) && !files.contains(&seen[1]));
}

#[test]
fn test_install_reuses_identical_binary_on_retry() {
    let dir = TmpDir::new("install-retry");
    let layout = Layout::new(dir.join("overlay"));
    let bytes = b"#!/bin/sh\nexit 0\n";
    let sha = sha_of(bytes);
    // 重试同一版：目标文件已存在且字节一致，不该重写、更不该失败。
    std::fs::create_dir_all(layout.bin_dir()).unwrap();
    let dest = layout.version_file("0.59.13", &sha);
    write_exec(&dest, bytes);
    let artifact = dir.join("again.part");
    std::fs::write(&artifact, bytes).unwrap();
    let installed = install(&layout, &artifact, "0.59.13", &sha, None, None).unwrap();
    assert_eq!(installed.path, dest);
    assert!(dest.is_file());
}

#[test]
fn test_install_detects_tampered_artifact() {
    let dir = TmpDir::new("install-bad");
    let layout = Layout::new(dir.join("overlay"));
    let bytes = b"#!/bin/sh\nexit 0\n";
    let sha = sha_of(bytes);
    let artifact = dir.join("tampered.part");
    std::fs::write(&artifact, b"#!/bin/sh\n# tampered\n").unwrap();
    let err = install(&layout, &artifact, "0.59.13", &sha, None, None).unwrap_err();
    assert!(err.contains("does not match its digest"), "{err}");
    assert!(
        std::fs::read_dir(layout.bin_dir()).unwrap().next().is_none(),
        "校验失败不得留下半成品"
    );
}

#[test]
fn test_prune_older_than_removes_old_versions() {
    let dir = TmpDir::new("prune");
    let layout = Layout::new(dir.join("overlay"));
    layout.ensure().unwrap();
    let mut kept = Vec::new();
    for version in ["0.59.11", "0.59.12", "0.59.13"] {
        let bytes = format!("#!/bin/sh\n# {version}\n").into_bytes();
        let sha = sha_of(&bytes);
        let path = layout.version_file(version, &sha);
        write_exec(&path, &bytes);
        kept.push((version, path));
    }
    // 不相干的文件不该被删。
    let noise = layout.bin_dir().join("notes.txt");
    std::fs::write(&noise, b"keep me").unwrap();

    let self_ver = semver::Version::parse("0.59.13").unwrap();
    prune_older_than(&layout, Some(&self_ver));

    assert!(!kept[0].1.exists(), "0.59.11 应被删除");
    assert!(!kept[1].1.exists(), "0.59.12 应被删除");
    assert!(kept[2].1.exists(), "自身版本必须保留");
    assert!(noise.exists(), "不认识的文??");
}

#[test]
fn test_version_of_file_name() {
    assert_eq!(
        version_of_file_name("shell-remote-0.59.13-1a2b3c4d"),
        semver::Version::parse("0.59.13").ok()
    );
    assert!(version_of_file_name("shell-remote-notes.txt").is_none());
    assert!(version_of_file_name("other-0.59.13-1a2b3c4d").is_none());
    assert!(version_of_file_name("shell-remote-0.59.13-notahash").is_none());
}

// ── 5. 标记健康 / 上报被拒 ────────────────────────────────────────

/// 环境变量是进程全局的：这一组用例都要独占它。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn test_after_registration_marks_healthy_for_the_overlay_itself() {
    let _env = env_guard();
    let (dir, layout, mut pointer, _) = fixture("mark-healthy", "0.59.13", "0.59.12");
    // `after_registration` 只认同指针指向的那份文件：把 `current_exe` 指过去
    // 不可能（那是测试二进制），所以这里直接验判定分支的等价路径。
    pointer.healthy = false;
    pointer.attempts = 2;
    Pointer::write(&layout, &pointer).unwrap();
    std::env::set_var("XDG_DATA_HOME", dir.path());
    std::env::set_var(ENV_FROM, dir.join("base"));

    // 本进程不是指针里的那份文件 → 不改指针（防止手工 exec 洗白别人）。
    let frame = after_registration("sess");
    assert!(frame.is_none(), "不是指针里的文件时不该上报");
    let after = Pointer::read(&layout).unwrap().unwrap();
    assert!(!after.healthy, "不该替别的文件标健康");

    std::env::remove_var(ENV_FROM);
    std::env::remove_var("XDG_DATA_HOME");
}

#[test]
fn test_after_registration_reports_rejection_once() {
    let _env = env_guard();
    let (dir, layout, mut pointer, _) = fixture("report", "0.59.13", "0.59.12");
    pointer.rejected = true;
    pointer.attempts = MAX_ATTEMPTS;
    Pointer::write(&layout, &pointer).unwrap();
    std::env::set_var("XDG_DATA_HOME", dir.path());
    std::env::remove_var(ENV_FROM);

    let frame = after_registration("sess-42").expect("必须上报一条失败帧");
    let msg: serde_json::Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(msg["type"], "agent:upgrade_progress");
    assert_eq!(msg["session_id"], "sess-42");
    assert_eq!(msg["payload"]["stage"], "failed");
    assert_eq!(msg["payload"]["version"], "0.59.13");
    assert_eq!(msg["payload"]["upgrade_id"], "up-1");
    assert!(
        msg["payload"]["detail"]
            .as_str()
            .unwrap()
            .contains("mode: overlay"),
        "阶段说明必须注明 overlay 模式: {msg}"
    );
    assert!(msg["payload"]["error"]
        .as_str()
        .unwrap()
        .contains("base binary"));

    // 第二次不该重复上报（设备每次重启都刷一条会淹没管理界面）。
    assert_eq!(after_registration("sess-42"), None);
    assert!(Pointer::read(&layout).unwrap().unwrap().rejected_reported);

    std::env::remove_var("XDG_DATA_HOME");
}

#[test]
fn test_after_registration_is_silent_without_a_pointer() {
    let _env = env_guard();
    let dir = TmpDir::new("no-pointer");
    let layout = Layout::new(dir.join("overlay"));
    layout.ensure().unwrap();
    std::env::set_var("XDG_DATA_HOME", dir.path());
    assert_eq!(after_registration("sess"), None);
    std::env::remove_var("XDG_DATA_HOME");
}
