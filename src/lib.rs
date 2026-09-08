//! shell-remote 库：relay / agent（终端 + 可选桌面）/ proto / web。
//!
//! 拆分为两个二进制后共享同一份核心：
//! - `shell-remote`（CLI）：relay + agent（仅终端转发），lean 构建
//!   `--no-default-features` 时不编译任何桌面代码。
//! - `shell-remote-ui`（UI）：agent（终端 + 桌面转发），必须 `--features desktop`。

pub mod agent;
pub mod proto;
pub mod relay;
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
        let msg = format!(
            "==== shell-remote panic ====\nat_unix_ms: {now_ms}\npid: {pid}\nthread: {}\nlocation: {:?}\ninfo: {}\nbacktrace:\n{bt}\n",
            std::thread::current().name().unwrap_or("?").to_string(),
            info.location(),
            info.payload().downcast_ref::<&str>().copied().unwrap_or("(non-str payload)"),
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

/// 初始化日志：关闭 ANSI 颜色控制符（在不支持色彩的终端/重定向/日志文件/
/// Windows 旧终端里会产生大量转义序列垃圾；用户要求无法检测时直接关闭）。
/// 设 `SR_LOG_DIR=<目录>` 时额外写入滚动文件（每小时一个，non-blocking，
/// 不阻塞业务线程）；默认只输出 stderr，行为与旧版完全一致。
/// 返回的 guard 必须活到 main 末尾（drop 会关闭 non-blocking writer）。
pub fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into());
    match std::env::var("SR_LOG_DIR") {
        Ok(dir) if !dir.is_empty() => {
            let file_appender = tracing_appender::rolling::hourly(&dir, "shell-remote.log");
            let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(file_writer)
                .with_env_filter(filter)
                .init();
            tracing::info!(dir = %dir, "log rotation enabled (hourly rolling file)");
            Some(guard)
        }
        _ => {
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_env_filter(filter)
                .init();
            None
        }
    }
}
