//! shell-remote 库：relay / agent（终端 + 可选桌面）/ proto / web。
//!
//! 拆分为两个二进制后共享同一份核心：
//! - `shell-remote`（CLI）：relay + agent（仅终端转发），lean 构建
//!   `--no-default-features` 时不编译任何桌面代码。
//! - `shell-remote-ui`（UI）：agent（终端 + 桌面转发），必须 `--features desktop`。

pub mod agent;
#[cfg(feature = "gui")]
pub mod gui;
pub mod proto;
pub mod relay;
#[cfg(feature = "tui")]
pub mod status;
#[cfg(feature = "tui")]
pub mod tui;
pub mod web;
pub mod tlsutil;

#[cfg(test)]
mod integration_test;

/// 崩溃诊断：任何 Rust panic 都留痕到 crash.log（含时间戳、pid、panic
/// 消息、代码位置与 backtrace），避免 release 构建静默闪退无从排查
/// （MYS-886 Windows agent 桌面闪退定位）。panic=unwind 下 hook 在 unwind
/// 前调用，panic=abort 下在 abort 前调用——两种配置都能留下日志。
/// 路径优先 SR_LOG_DIR（与日志轮转同目录），否则当前目录；append 模式
/// 保留多次崩溃记录（此前 fs::write 覆盖只留最后一次，多闪退排查丢现场）。
/// 写盘失败也必定打印到 stderr —— crash.log 所在目录本身可能不可写。
pub fn install_panic_hook() {
    let crash_path = std::env::var("SR_LOG_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(|d| std::path::PathBuf::from(d).join("crash.log"))
        .unwrap_or_else(|| std::path::PathBuf::from("shell-remote-crash.log"));
    std::panic::set_hook(Box::new(move |info| {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let pid = std::process::id();
        let bt = std::backtrace::Backtrace::force_capture();
        let detail = panic_detail(&*info.payload());
        let msg = format!(
            "==== shell-remote panic ====\nat_unix_ms: {now_ms}\npid: {pid}\nthread: {}\nlocation: {:?}\ninfo: {detail}\nbacktrace:\n{bt}\n",
            std::thread::current().name().unwrap_or("?").to_string(),
            info.location(),
        );
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&crash_path)
        {
            let _ = f.write_all(msg.as_bytes());
        }
        eprintln!("{msg}");
    }));
}

/// 渲染 panic payload 的可读文本。
///
/// `panic!("literal")` 给 `&str`，而 `Option::expect` / `Result::expect` 给
/// `String`；只认 `&str` 会把后者的原因整个丢掉（MYS-2461 现场只剩
/// `(non-str payload)`，等于没有证据）。
fn panic_detail(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-str payload)".to_string()
    }
}

/// 交互视图（窗口/TUI）下默认的日志目录：`<home>/.shell-remote`。
///
/// `home` 取 `agent::home_dir()` 的语义（`HOME` → `%USERPROFILE%` → `.`），
/// 与文件管理器根目录、PTY 子进程 cwd 保持同一条规则。Windows 桌面环境通常
/// 只有 `USERPROFILE` 而没有 `HOME`，只读 `HOME` 会让目录退化成相对路径
/// `.shell-remote`，随启动 cwd 漂移（MYS-2461 闪退根因之一）。
pub fn default_log_dir() -> std::path::PathBuf {
    default_log_dir_from(
        std::env::var("HOME").ok(),
        std::env::var("USERPROFILE").ok(),
    )
}

fn default_log_dir_from(home: Option<String>, userprofile: Option<String>) -> std::path::PathBuf {
    std::path::PathBuf::from(agent::home_dir_from(home, userprofile)).join(".shell-remote")
}

/// 打开按小时滚动的日志文件 appender；不可写时返回 `Err` 而不是 panic。
///
/// `tracing_appender::rolling::hourly()` 内部对目录创建/文件打开失败走
/// `expect`，会让进程直接退出；这里统一走 `Builder::build()` 的 `Result`。
fn build_file_appender(
    dir: &str,
) -> Result<tracing_appender::rolling::RollingFileAppender, tracing_appender::rolling::InitError> {
    tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::HOURLY)
        .filename_prefix("shell-remote.log")
        .build(dir)
}

/// 初始化日志：关闭 ANSI 颜色控制符（在不支持色彩的终端/重定向/日志文件/
/// Windows 旧终端里会产生大量转义序列垃圾；用户要求无法检测时直接关闭）。
/// 设 `SR_LOG_DIR=<目录>` 时额外写入滚动文件（每小时一个，non-blocking，
/// 不阻塞业务线程）；默认只输出 stderr，行为与旧版完全一致。
/// 返回的 guard 必须活到 main 末尾（drop 会关闭 non-blocking writer）。
///
/// **日志目录不可写不得致命**（MYS-2461）：文件日志打不开时降级到 stderr
/// 并显式告警，agent 照常启动——日志是观测手段，不该成为启动的前置条件。
pub fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    match std::env::var("SR_LOG_DIR") {
        Ok(dir) if !dir.is_empty() => match build_file_appender(&dir) {
            Ok(file_appender) => {
                let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
                tracing_subscriber::fmt()
                    .with_ansi(false)
                    .with_writer(file_writer)
                    .with_env_filter(filter)
                    .init();
                tracing::info!(dir = %dir, "log rotation enabled (hourly rolling file)");
                Some(guard)
            }
            Err(e) => {
                // 降级：stderr 日志照常可用，agent 继续跑。
                tracing_subscriber::fmt()
                    .with_ansi(false)
                    .with_env_filter(filter)
                    .init();
                tracing::warn!(
                    dir = %dir,
                    error = %e,
                    "log directory unusable — file logging disabled, falling back to stderr"
                );
                None
            }
        },
        _ => {
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_env_filter(filter)
                .init();
            None
        }
    }
}

#[cfg(test)]
mod logging_tests {
    use super::*;

    #[test]
    fn test_default_log_dir_uses_home_when_present() {
        assert_eq!(
            default_log_dir_from(Some("/home/u".into()), Some("/ignored".into())),
            std::path::PathBuf::from("/home/u/.shell-remote"),
            "HOME 优先，Unix 路径选择不回归"
        );
    }

    #[test]
    fn test_default_log_dir_falls_back_to_userprofile_without_home() {
        // Windows 桌面常态：有 USERPROFILE、没有 HOME。
        assert_eq!(
            default_log_dir_from(None, Some(r"C:\Users\u".into())),
            std::path::PathBuf::from(r"C:\Users\u").join(".shell-remote"),
            "无 HOME 时必须用 USERPROFILE，而不是相对 cwd 的 .shell-remote"
        );
    }

    #[test]
    fn test_default_log_dir_is_not_relative_without_home_or_userprofile() {
        // 两者都没有时沿用 agent::home_dir() 的 `.` 语义（与 --root 默认值一致）。
        assert_eq!(
            default_log_dir_from(None, None),
            std::path::PathBuf::from("./.shell-remote")
        );
    }

    #[test]
    fn test_build_file_appender_writable_dir_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let appender = build_file_appender(&dir.path().to_string_lossy())
            .expect("可写目录必须能建日志 appender");
        drop(appender);
    }

    #[test]
    fn test_build_file_appender_unusable_dir_returns_err_not_panic() {
        // SR_LOG_DIR 指向一个普通文件：目录创建必然失败。
        // 旧实现在这里 panic → Windows 上进程静默退出（MYS-2461）。
        let dir = tempfile::tempdir().expect("tempdir");
        let not_a_dir = dir.path().join("occupied");
        std::fs::write(&not_a_dir, b"x").expect("write placeholder file");

        let err = build_file_appender(&not_a_dir.to_string_lossy())
            .err()
            .expect("不可写目录必须返回 Err");
        assert!(
            !err.to_string().trim().is_empty(),
            "错误信息要能定位原因，实际: {err}"
        );
    }

    #[test]
    fn test_panic_detail_keeps_string_payload() {
        // `expect()` 的 panic payload 是 String；只认 &str 会把唯一的原因
        // 丢掉，现场只留下 "(non-str payload)"（MYS-2461）。
        let e: Result<(), &str> = Err("initializing rolling file appender failed");
        let payload = std::panic::catch_unwind(|| e.expect("boom")).unwrap_err();
        let detail = panic_detail(&*payload);
        assert!(
            detail.contains("initializing rolling file appender failed"),
            "String payload 的原因必须保留，实际: {detail}"
        );
        assert_ne!(detail, "(non-str payload)");
    }

    #[test]
    fn test_panic_detail_keeps_str_payload() {
        let payload = std::panic::catch_unwind(|| panic!("plain literal")).unwrap_err();
        assert_eq!(panic_detail(&*payload), "plain literal");
    }
}
