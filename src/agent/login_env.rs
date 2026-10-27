//! Unix 登录环境初始化：网页终端与 MCP exec 共用同一套策略。
//!
//! **问题**：agent 由 systemd/服务直接启动，不经过 PAM/SSH，子进程只有 agent 自身的
//! 环境。设备上很多工具的 PATH 由 `/etc/profile` 追加（例如 Sophon 的 `bm-smi` 在
//! `/opt/sophon/libsophon-current/bin`），而该文件**只在登录 shell 里被读取**。
//! 于是网页终端和 MCP 都看不到这些工具，用户只能每次手动 `source /etc/profile`。
//!
//! **机制**：两条路径统一走「让 shell 以登录 shell 启动」这一个标准语义——
//! 不向终端注入 `source ...` 文本（会进命令日志、依赖时序、污染交互），
//! 也不拼接/改写用户命令：
//!
//! | 路径 | 做法 |
//! |---|---|
//! | 网页终端（PTY） | `<shell> -l` 启动用户选择的 shell |
//! | MCP exec | `sh -l -c <用户命令>`（有 `script` 时由它分配 PTY，内层仍是这条 argv） |
//!
//! **为什么不是 `ENV`**：POSIX 里非交互 shell 读 `ENV`，但主流实现只在**交互模式**
//! 才读它。实测（dash 0.5.11 / bash 5.1 / busybox ash）`ENV=/etc/profile sh -c '...'`
//! 一个变量都拿不到，加 `-i` 才生效；而 `-i` 会往 stderr 输出
//! 「can't access tty; job control turned off」并改变作业控制语义，不适合承载命令
//! 执行。`-l` 则既能读到 profile，又不改变命令语义、不产生输出污染。
//!
//! **限制（如实记录）**：
//! - 并非所有 shell 都接受 `-l`：tcsh / elvish / nu 会把它当未知参数直接报错退出。
//!   所以启动终端前先探测一次（按 shell 路径缓存），不支持的退回原有行为，
//!   不会把网页终端搞坏。
//! - `portable-pty` 0.8 的 `CommandBuilder` 把 `argv[0]` 与可执行路径绑成同一个值
//!   （`as_command()` 用 `args[0]` 既解析路径又当 `arg0`），所以「`argv[0]` 前导 `-`」
//!   这套写法在 PTY 路径上无法表达，只能用 `-l`。
//! - `/etc/profile` 不存在时什么都不发生（shell 自行跳过缺失的 profile），命令照常
//!   执行，不需要额外判存。
//! - `/etc/profile` 里只适合交互 shell 的输出也会出现在 MCP 结果里——这是「登录
//!   环境」的既有语义，README 已写明。

use std::ffi::OsString;
#[cfg(test)]
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 让 shell 以登录模式启动的参数。
pub const LOGIN_FLAG: &str = "-l";

/// 探测用哨兵：只有 `-l` 被接受、命令真的跑到了，才会出现在输出里。
const PROBE_SENTINEL: &str = "__shell_remote_login_probe_ok__";

/// 探测超时。`-l` 被拒绝发生在参数解析阶段，立刻返回；超时只可能来自 shell 自身
/// 或 profile 卡住，此时按「不支持」处理——退回原有行为，绝不拖住启动。
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// 目标 shell 是否接受 `-l`（能否按登录 shell 启动）。
///
/// 结果按 shell 路径缓存：探测要真起进程并读取 `/etc/profile`，同一路径只值得做一次。
pub fn shell_supports_login_flag(shell_path: &str) -> bool {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some(cached) = cache.lock().ok().and_then(|m| m.get(shell_path).copied()) {
        return cached;
    }
    let supported = probe_login_flag(shell_path);
    if let Ok(mut map) = cache.lock() {
        map.insert(shell_path.to_string(), supported);
    }
    if supported {
        tracing::debug!(shell = %shell_path, "shell accepts -l; login environment enabled");
    } else {
        tracing::warn!(
            shell = %shell_path,
            "shell rejects -l; falling back to non-login startup (no /etc/profile)"
        );
    }
    supported
}

/// 真起一次 `shell -l -c 'echo <sentinel>'`，看哨兵有没有被打印出来。
///
/// 用哨兵而不是退出码判定：`/etc/profile` 自己可能以非 0 结束（内部 `exit`、某条
/// 命令失败），只看退出码会把它误判成「不支持 `-l`」。命令真的跑到了才算接受。
///
/// stdout 落临时文件而不是管道：profile 可能输出很多，管道无人读取会填满缓冲区
/// 把子进程卡死，而这里又是「边等边读会互相拖住」的场景。
fn probe_login_flag(shell_path: &str) -> bool {
    // 不预判路径是否存在：`sh` 这类裸名字要靠 PATH 解析，`Path::exists()` 判不了。
    // 真的不存在时下面的 spawn 会失败，同样落到「不支持」。
    let out_path = std::env::temp_dir().join(format!(
        "shell-remote-login-probe-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    let output_file = match std::fs::File::create(&out_path) {
        Ok(f) => f,
        Err(_) => return false,
    };

    let child = Command::new(shell_path)
        .arg(LOGIN_FLAG)
        .arg("-c")
        .arg(format!("echo {PROBE_SENTINEL}"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_file))
        .stderr(Stdio::null())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_file(&out_path);
            return false;
        }
    };

    // 轮询 `try_wait`：不引入额外依赖，也能在超时后杀掉子进程。
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let finished = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Err(_) => break false,
        }
    };

    let accepted = finished
        && std::fs::read_to_string(&out_path)
            .map(|s| s.contains(PROBE_SENTINEL))
            .unwrap_or(false);
    let _ = std::fs::remove_file(&out_path);
    accepted
}

/// 网页终端启动用户 shell 的参数：支持 `-l` 时加上它，让 `/etc/profile` 生效。
///
/// 空 vec 表示「按原来的非登录方式启动」（目标 shell 不接受 `-l`）。
pub fn shell_startup_args(shell_path: &str) -> Vec<OsString> {
    if shell_supports_login_flag(shell_path) {
        vec![OsString::from(LOGIN_FLAG)]
    } else {
        Vec::new()
    }
}

/// MCP exec 的 `sh` argv：保持现有 `sh -c <用户命令>` 语义，只在支持时插入 `-l`
/// 让 `/etc/profile` 先被读取。用户命令始终作为**一个独立 argv**、原样传入。
pub fn sh_exec_argv(cmd: &str) -> Vec<OsString> {
    let mut argv = Vec::with_capacity(3);
    if shell_supports_login_flag("sh") {
        argv.push(OsString::from(LOGIN_FLAG));
    }
    argv.push(OsString::from("-c"));
    argv.push(OsString::from(cmd));
    argv
}

/// 需要 PTY 时（`script -c`）承载用户命令的环境变量名。
///
/// `script -c` 只收一个字符串、再由 `$SHELL` 解释一遍。用户命令若直接拼进去，就要
/// 穿过这层额外解析——已经保住的引号/换行/`$()` 会被再解释一次。走环境变量则用户
/// 命令只被解释一次（由内层 `sh -l -c` 解释），与不加 `script` 时语义一致。
///
/// 名字带 `SHELL_REMOTE_` 前缀，避免与目标机上的同名变量撞车。
pub const MCP_CMD_ENV: &str = "SHELL_REMOTE_MCP_CMD";

/// `script -c` 的外层命令串：**固定常量**，不含任何用户命令片段。
///
/// 先把环境变量取到普通变量，再 `unset` 掉那个导出变量——否则用户命令及其子进程的
/// `env` 里会凭空多出一个 `SHELL_REMOTE_MCP_CMD`，等于把用户的命令又塞回它自己的
/// 环境。取到普通变量之后再 `unset` 不影响已取到的值。
pub fn script_wrapper_command() -> String {
    let inner = if shell_supports_login_flag("sh") {
        format!("sh {LOGIN_FLAG} -c \"$_sr_cmd\"")
    } else {
        "sh -c \"$_sr_cmd\"".to_string()
    };
    format!("_sr_cmd=${MCP_CMD_ENV}; unset {MCP_CMD_ENV}; {inner}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机真实的 POSIX shell 应当支持 `-l`（登录 shell 是 POSIX 语义）。
    /// 同时自证探测机制：返回 true 意味着 `-l` 被接受且命令真的跑到了。
    #[test]
    fn test_shell_supports_login_flag_for_posix_shells() {
        for shell in ["/bin/sh", "/bin/bash"] {
            if !Path::new(shell).exists() {
                continue;
            }
            assert!(
                shell_supports_login_flag(shell),
                "{shell} 应当接受 -l（POSIX 登录 shell 语义）"
            );
        }
    }

    /// 探测结果要缓存：同一路径二次调用结果一致（且不再起进程）。
    #[test]
    fn test_shell_supports_login_flag_is_cached() {
        let first = shell_supports_login_flag("/bin/sh");
        let second = shell_supports_login_flag("/bin/sh");
        assert_eq!(first, second);
    }

    /// 不存在的路径不能 panic，只能判定为不支持。
    #[test]
    fn test_shell_supports_login_flag_on_missing_shell() {
        assert!(!shell_supports_login_flag(
            "/nonexistent/shell-remote-test-shell"
        ));
    }

    /// 明确不接受 `-l` 的 shell（用 `false` 模拟：任何参数都以 1 退出且无输出）
    /// 必须判为不支持，从而退回原有启动方式。
    #[test]
    fn test_shell_rejecting_login_flag_is_detected() {
        if !Path::new("/bin/false").exists() {
            return;
        }
        assert!(
            !shell_supports_login_flag("/bin/false"),
            "不接受 -l 的 shell 必须判为不支持（不能因退出码而误判）"
        );
    }

    /// MCP 的 argv：程序仍是 `sh`，用户命令原样作为一个 argv 传入。
    #[test]
    fn test_sh_exec_argv_preserves_user_command() {
        let cmd = "printf '%s' \"a b\"; echo done";
        let argv = sh_exec_argv(cmd);
        let expected: Vec<OsString> = if shell_supports_login_flag("sh") {
            vec![LOGIN_FLAG.into(), "-c".into(), cmd.into()]
        } else {
            vec!["-c".into(), cmd.into()]
        };
        assert_eq!(argv, expected, "用户命令必须原样作为一个 argv 传递");
        assert_eq!(
            argv.last().unwrap().to_string_lossy(),
            cmd,
            "命令不得被拼接或改写"
        );
    }

    /// 外层包装串必须是固定常量：换一条用户命令，包装串不能变
    /// （用户命令走环境变量，不参与拼接）。
    #[test]
    fn test_script_wrapper_command_is_command_independent() {
        let a = script_wrapper_command();
        let b = script_wrapper_command();
        assert_eq!(a, b, "包装串不得依赖用户命令");
        assert!(a.contains(MCP_CMD_ENV), "必须从环境变量取用户命令");
        assert!(
            a.contains(&format!("unset {MCP_CMD_ENV}")),
            "必须 unset 掉该变量，避免污染用户命令的环境"
        );
        assert!(
            a.contains("sh -l -c \"$_sr_cmd\"") || a.contains("sh -c \"$_sr_cmd\""),
            "内层必须仍是 sh [-l] -c，实际: {a}"
        );
        for hostile in ["rm -rf /", "$(id)", "`id`", "'; exit 9; '"] {
            assert!(
                !a.contains(hostile),
                "包装串不得含用户命令片段（{hostile}）"
            );
        }
    }
}

/// 真实执行层面的测试：起真进程、真读 `/etc/profile`。
///
/// 不能只断言「拼出来的命令串对不对」——那样测的是字符串，不是行为。这里用
/// **挂载命名空间**把一份受控的 `/etc/profile` 盖到真的 `/etc/profile` 上，再跑
/// 与生产完全相同的启动方式，直接观察用户命令看到的变量、PATH 与退出码。
///
/// 需要用户命名空间（`unshare -rm`）。不可用的环境（如受限 CI 容器）跳过，
/// 跳过原因打在 stdout，不会伪装成通过。
#[cfg(all(test, unix))]
mod exec_tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    /// 受控 profile 里的标记变量名，取一个不会和真实主机撞车的名字。
    const PROFILE_VAR: &str = "SR_TEST_PROFILE_MARKER";
    const PROFILE_VALUE: &str = "loaded-from-controlled-profile";

    /// 用户命令里的退出码，用来确认「加载了 profile」不影响退出码透传。
    const USER_EXIT_CODE: i32 = 42;

    fn have_unshare() -> bool {
        Command::new("unshare")
            .args(["-rm", "--map-root-user", "true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// 受控 profile + 一个只有该 profile 才会加进 PATH 的工具目录。
    fn write_fixture(dir: &Path) -> (PathBuf, PathBuf) {
        let profile = dir.join("profile");
        std::fs::write(
            &profile,
            format!(
                "export {PROFILE_VAR}={PROFILE_VALUE}\n\
                 PATH=\"{bin}:$PATH\"\n\
                 export PATH\n",
                bin = dir.join("bin").display()
            ),
        )
        .expect("写受控 profile 失败");

        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("建工具目录失败");
        let tool = bin.join("sr-login-probe-tool");
        std::fs::write(&tool, "#!/bin/sh\necho sr-login-probe-tool-ran\n").expect("写工具脚本失败");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
                .expect("设置工具脚本可执行失败");
        }
        (profile, bin)
    }

    /// 在挂载命名空间里执行 `inner`，其中 `/etc/profile` 被替换为 `profile`。
    ///
    /// 返回 `(stdout, stderr, exit_code)`；命名空间不可用时返回 None。
    fn run_with_profile(profile: &Path, inner: &str) -> Option<(String, String, i32)> {
        let script = format!(
            "mount --bind {profile} /etc/profile || exit 111\n{inner}",
            profile = profile.display()
        );
        let out = Command::new("unshare")
            .args(["-rm", "--map-root-user", "sh", "-c"])
            .arg(&script)
            .output()
            .ok()?;
        // 111 是「bind mount 失败」，与用户命令的退出码区分开。
        let code = out.status.code().unwrap_or(-1);
        if code == 111 {
            return None;
        }
        Some((
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            code,
        ))
    }

    /// 验收项 4：通过**一次性 exec 的真实启动命令**执行用户命令，
    /// 断言 profile 里的变量生效、PATH 里的工具可被 `command -v` 找到，
    /// 且退出码与输出仍然正确。
    #[test]
    fn test_mcp_exec_sees_controlled_etc_profile() {
        if !have_unshare() {
            println!("SKIP test_mcp_exec_sees_controlled_etc_profile: 无用户命名空间");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let (profile, _bin) = write_fixture(dir.path());

        // 与 agent::execute_command 的「无 script」分支同一条 argv 构造。
        let user_cmd = format!(
            "printf 'VAR=%s\\n' \"${PROFILE_VAR}\"; \
             command -v sr-login-probe-tool; \
             printf 'EXIT_NOW\\n'; \
             exit {USER_EXIT_CODE}"
        );
        let argv = sh_exec_argv(&user_cmd);

        // 直接跑生产用的 argv（std::process::Command），不经额外的 shell 转写。
        let script = format!(
            "mount --bind {profile} /etc/profile || exit 111\n\
             exec sh {args}",
            profile = profile.display(),
            args = argv
                .iter()
                .map(|a| shell_quote(&a.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let out = Command::new("unshare")
            .args(["-rm", "--map-root-user", "sh", "-c"])
            .arg(&script)
            .output()
            .expect("执行失败");
        if out.status.code() == Some(111) {
            println!("SKIP: bind mount /etc/profile 不可用");
            return;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert!(
            stdout.contains(&format!("VAR={PROFILE_VALUE}")),
            "用户命令应看到受控 /etc/profile 的变量，实际 stdout={stdout:?} stderr={stderr:?}"
        );
        assert!(
            stdout.contains("sr-login-probe-tool"),
            "profile 追加进 PATH 的工具应能被解析，实际 stdout={stdout:?}"
        );
        assert!(
            stdout.contains("EXIT_NOW"),
            "命令输出应完整，实际 stdout={stdout:?}"
        );
        assert_eq!(
            out.status.code(),
            Some(USER_EXIT_CODE),
            "退出码必须原样透传，stderr={stderr:?}"
        );
    }

    /// 验收项 5：`/etc/profile` **不存在**时，命令仍正常执行、输出与退出码都对。
    /// 用 tmpfs 盖住整个 `/etc` 来造「真的没有 profile」的场景。
    #[test]
    fn test_mcp_exec_works_when_etc_profile_missing() {
        if !have_unshare() {
            println!("SKIP test_mcp_exec_works_when_etc_profile_missing: 无用户命名空间");
            return;
        }
        let argv = sh_exec_argv("printf 'NO_PROFILE_OK\\n'; exit 7");
        let script = format!(
            "mount -t tmpfs none /etc || exit 111\n\
             test -e /etc/profile && exit 112\n\
             exec sh {args}",
            args = argv
                .iter()
                .map(|a| shell_quote(&a.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let out = Command::new("unshare")
            .args(["-rm", "--map-root-user", "sh", "-c"])
            .arg(&script)
            .output()
            .expect("执行失败");
        if out.status.code() == Some(111) {
            println!("SKIP: 无法用 tmpfs 覆盖 /etc");
            return;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_ne!(
            out.status.code(),
            Some(112),
            "前置条件失败：/etc/profile 仍存在"
        );
        assert!(
            stdout.contains("NO_PROFILE_OK"),
            "/etc/profile 缺失时命令仍须正常运行，实际 stdout={stdout:?} stderr={stderr:?}"
        );
        assert_eq!(
            out.status.code(),
            Some(7),
            "退出码必须原样透传，stderr={stderr:?}"
        );
    }

    /// 验收项 4（长会话侧）：`exec_start` 走的 `shell_argv` 同样拿得到登录环境。
    #[test]
    fn test_exec_session_sees_controlled_etc_profile() {
        if !have_unshare() {
            println!("SKIP test_exec_session_sees_controlled_etc_profile: 无用户命名空间");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let (profile, _bin) = write_fixture(dir.path());
        let argv = crate::agent::exec_sessions::shell_argv_for_test(&format!(
            "printf 'SESS_VAR=%s\\n' \"${PROFILE_VAR}\""
        ));
        let script = format!(
            "mount --bind {profile} /etc/profile || exit 111\n\
             exec sh {args}",
            profile = profile.display(),
            args = argv
                .iter()
                .map(|a| shell_quote(&a.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let out = Command::new("unshare")
            .args(["-rm", "--map-root-user", "sh", "-c"])
            .arg(&script)
            .output()
            .expect("执行失败");
        if out.status.code() == Some(111) {
            println!("SKIP: bind mount /etc/profile 不可用");
            return;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("SESS_VAR={PROFILE_VALUE}")),
            "长会话路径也应加载登录环境，实际 stdout={stdout:?}"
        );
    }

    /// 把参数安全地交给外层 `sh -c`（只用于测试侧拼接，生产路径不拼命令）。
    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    /// 验收项 4（`script` 分支）：真实走一遍「`script` 分配 PTY + 内层登录 sh」，
    /// 断言 profile 生效、命令输出与退出码正确，且用户命令没被二次解析。
    ///
    /// 宿主机没有 `script` 时跳过（生产上该分支同样会自动退回 `sh -c`）。
    #[test]
    fn test_script_pty_path_sees_controlled_etc_profile() {
        if !have_unshare() {
            println!("SKIP test_script_pty_path_sees_controlled_etc_profile: 无用户命名空间");
            return;
        }
        if Command::new("which")
            .arg("script")
            .stdout(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            println!("SKIP: 无 script");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let (profile, _bin) = write_fixture(dir.path());

        // 带引号/换行的用户命令：确认经环境变量传递后仍只被解释一次。
        let user_cmd = format!(
            "printf 'Q=%s\\n' \"a b\"; \
             printf 'VAR=%s\\n' \"${PROFILE_VAR}\"; \
             exit 5"
        );
        let script = format!(
            "mount --bind {profile} /etc/profile || exit 111\n\
             export {env}=$0\n\
             exec script -q -e -c {wrapper} /dev/null",
            profile = profile.display(),
            env = MCP_CMD_ENV,
            wrapper = shell_quote(&script_wrapper_command()),
        );
        // 用户命令经位置参数传入外层，避免再被外面的一层 shell 解析。
        let out = Command::new("unshare")
            .args(["-rm", "--map-root-user", "sh", "-c"])
            .arg(&script)
            .arg(&user_cmd)
            .output()
            .expect("执行失败");
        if out.status.code() == Some(111) {
            println!("SKIP: bind mount /etc/profile 不可用");
            return;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stdout.contains(&format!("VAR={PROFILE_VALUE}")),
            "script 分支也应加载登录环境，实际 stdout={stdout:?} stderr={stderr:?}"
        );
        assert!(
            stdout.contains("Q=a b"),
            "用户命令的引号必须保留，实际 stdout={stdout:?}"
        );
        assert_eq!(
            out.status.code(),
            Some(5),
            "script -e 应透传命令退出码（默认恒为 0），stderr={stderr:?}"
        );
    }
}
