//! Best-effort host machine probing for the admin device-management panel.
//!
//! Every field is independently optional: a command that fails, times out, or
//! returns junk leaves that field `None`. The probe runs once per agent
//! registration and must stay fast (each command capped at 2s) so a weird
//! host cannot delay the relay handshake.

use crate::proto::DeviceInfo;
use std::time::Duration;

const CMD_TIMEOUT: Duration = Duration::from_secs(2);

/// Which `platform` label a host reports (used as a filter key in the admin
/// device list). Static per build target.
pub fn os_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

pub async fn probe() -> DeviceInfo {
    #[cfg(windows)]
    {
        probe_windows().await
    }
    #[cfg(not(windows))]
    {
        probe_unix().await
    }
}

#[cfg(not(windows))]
async fn probe_unix() -> DeviceInfo {
    let (arch, os, kernel, hostname_fut) = tokio::join!(
        run_command(&["uname", "-m"]),
        run_command(&["uname", "-s"]),
        run_command(&["uname", "-r"]),
        hostname_unix(),
    );
    let cpu_model = cpu_model_unix().await;
    DeviceInfo {
        hostname: hostname_fut,
        platform: Some(os_platform().to_string()),
        arch,
        os,
        kernel,
        cpu_model,
    }
}

#[cfg(not(windows))]
async fn hostname_unix() -> Option<String> {
    // Fast path on Linux: /proc/sys/kernel/hostname avoids spawning.
    if cfg!(target_os = "linux") {
        if let Some(h) = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return Some(h);
        }
    }
    run_command(&["hostname"]).await
}

#[cfg(not(windows))]
async fn cpu_model_unix() -> Option<String> {
    if cfg!(target_os = "linux") {
        if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
            if let Some(model) = parse_cpu_model(&text) {
                return Some(model);
            }
        }
    }
    if cfg!(target_os = "macos") {
        // model-name may need sudo in rare cases; accept normal failure.
        if let Some(model) = run_command(&["sysctl", "-n", "machdep.cpu.brand_string"]).await {
            return Some(model);
        }
    }
    None
}

#[cfg(windows)]
async fn probe_windows() -> DeviceInfo {
    let hostname = std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty());
    let arch = std::env::var("PROCESSOR_ARCHITECTURE")
        .ok()
        .filter(|s| !s.is_empty());
    let os = std::env::var("OS").ok().filter(|s| !s.is_empty());
    let cpu_model = std::env::var("PROCESSOR_IDENTIFIER")
        .ok()
        .filter(|s| !s.is_empty());
    let kernel = run_command(&["cmd", "/c", "ver"])
        .await
        .and_then(|v| parse_windows_ver(&v));
    DeviceInfo {
        hostname,
        platform: Some(os_platform().to_string()),
        arch,
        os,
        kernel,
        cpu_model,
    }
}

/// Run a command to completion (stdout only, stderr discarded) with a 2s cap.
async fn run_command(args: &[&str]) -> Option<String> {
    let mut cmd = tokio::process::Command::new(args[0]);
    cmd.args(&args[1..])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(CMD_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let s = text.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Extract the first CPU model name from a `/proc/cpuinfo` dump. Accepts the
/// common `model name` line and the arm `Hardware`/`model` fallbacks, mirroring
/// how `lscpu` shortens overlong strings.
pub fn parse_cpu_model(cpuinfo: &str) -> Option<String> {
    let mut candidates = Vec::new();
    for line in cpuinfo.lines() {
        let lower = line.to_ascii_lowercase();
        let key = if lower.starts_with("model name") {
            Some("model name")
        } else if lower.starts_with("hardware") {
            Some("hardware")
        } else if lower.starts_with("model") {
            Some("model")
        } else {
            None
        };
        if let Some(k) = key {
            if let Some(value) = line.splitn(2, ':').nth(1) {
                candidates.push((k, value.trim().to_string()));
            }
        }
    }
    // Prefer `model name` (x86/arm with full string); fall back to `Hardware`
    // (many armv7 boards) then bare `model` (Raspberry Pi dtb).
    for key in ["model name", "hardware", "model"] {
        for (k, v) in candidates.iter() {
            if *k == key && !v.is_empty() {
                return Some(v.clone());
            }
        }
    }
    None
}

/// Parse the kernel version out of `cmd /c ver` output:
/// `Microsoft Windows [Version 10.0.19045.4046]`
pub fn parse_windows_ver(out: &str) -> Option<String> {
    let line = out.lines().next()?;
    let start = line.find("[Version ")? + "[Version ".len();
    let end = line[start..].find(']')? + start;
    let ver = line[start..end].trim();
    if ver.is_empty() {
        None
    } else {
        Some(ver.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cpu_model_x86() {
        let cpuinfo = "\
processor\t: 0
vendor_id\t: GenuineIntel
cpu family\t: 6
model name\t: Intel(R) Core(TM) i9-10900X CPU @ 3.70GHz
";
        assert_eq!(
            parse_cpu_model(cpuinfo).as_deref(),
            Some("Intel(R) Core(TM) i9-10900X CPU @ 3.70GHz")
        );
    }

    #[test]
    fn test_parse_cpu_model_arm_hardware() {
        // Common armv7 board output: no `model name`, only `Hardware`.
        let cpuinfo = "\
processor\t: 0
model name\t: ARMv7 Processor rev 4 (v7l)
Hardware\t: BCM2835
";
        assert_eq!(
            parse_cpu_model(cpuinfo).as_deref(),
            Some("ARMv7 Processor rev 4 (v7l)")
        );
    }

    #[test]
    fn test_parse_cpu_model_arm_bare_model() {
        // Raspberry Pi dtb style: only `Model` with a space before colon.
        let cpuinfo = "\
processor\t: 0
model name\t: ARMv6-compatible processor rev 7 (v6l)
Model\t: Raspberry Pi Model B Plus Rev 1.2
";
        assert_eq!(
            parse_cpu_model(cpuinfo).as_deref(),
            Some("ARMv6-compatible processor rev 7 (v6l)")
        );
    }

    #[test]
    fn test_parse_cpu_model_ignores_noise() {
        let cpuinfo = "processor\t: 0\nflags\t\t: fpu vme de\n";
        assert_eq!(parse_cpu_model(cpuinfo), None);
    }

    #[test]
    fn test_parse_windows_ver() {
        assert_eq!(
            parse_windows_ver("Microsoft Windows [Version 10.0.19045.4046]").as_deref(),
            Some("10.0.19045.4046")
        );
        assert_eq!(parse_windows_ver("not windows"), None);
        assert_eq!(parse_windows_ver("Microsoft Windows [Version 6.1]").as_deref(), Some("6.1"));
    }

    #[test]
    fn test_os_platform_static() {
        let p = os_platform();
        assert!(matches!(p, "linux" | "macos" | "windows" | "other"));
    }
}