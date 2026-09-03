//! Agent-mode atomic self-upgrade.
//!
//! The relay's admin panel triggers an upgrade by sending `agent:upgrade`
//! with the artifact URL (served by the relay itself) plus a SHA-256 digest.
//! This module downloads the new binary, verifies it, atomically replaces the
//! running executable, and re-executes the agent:
//!
//!   1. download to a temp file in the *same directory* as the real binary
//!      (same filesystem → rename is atomic),
//!   2. verify the SHA-256 digest (integrity: the file is what the admin
//!      staged),
//!   3. smoke-test the new binary on unix (`--version` must run) so a wrong-arch
//!      or truncated artifact never replaces a working agent,
//!   4. rename over the current executable (never a partial/corrupt file),
//!   5. spawn the new binary with the original argv and exit this process.
//!
//! On Unix `rename(2)` over a running executable is atomic and safe (the old
//! inode stays alive until this process exits). Windows cannot replace a
//! running `.exe` directly, so there we fall back to a helper `.bat` that waits
//! for this process to exit, swaps the file and starts the new binary.

use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Sender;
use tokio_stream::StreamExt;

use crate::proto::Message;

/// Overall download timeout — artifacts are single binaries, a few tens of MB.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to let the terminal `agent:upgrade_progress` POST flush before the
/// process exits (the sender loop drains control messages asynchronously).
const EXIT_FLUSH_GRACE: Duration = Duration::from_millis(400);

/// Upgrade directives received by the agent. All fields come from the relay,
/// which read them from the staged artifact at trigger time.
#[derive(Debug, Clone)]
pub struct UpgradeRequest {
    pub version: String,
    /// Relative URL on the relay, e.g. `/agent/upgrade/blob/shell-remote-x86_64`.
    pub url: String,
    /// Hex SHA-256 the downloaded artifact must match.
    pub sha256: String,
}

impl UpgradeRequest {
    pub fn from_payload(p: &serde_json::Value) -> Option<Self> {
        Some(Self {
            version: p.get("version")?.as_str()?.to_string(),
            url: p.get("url")?.as_str()?.to_string(),
            sha256: p.get("sha256")?.as_str()?.to_string(),
        })
    }
}

/// Result of installing the new binary.
struct InstallOutcome {
    /// True when the caller must spawn the new binary and exit. False only on
    /// Windows where the swap was deferred to a helper `.bat` (already
    /// spawned) — the caller should just exit so the bat can finish the swap.
    restart_now: bool,
}

/// Push an `agent:upgrade_progress` frame to the relay for the admin panel.
async fn send_progress(
    ctrl_tx: &Sender<String>,
    session_id: &str,
    stage: &str,
    percent: Option<u64>,
    detail: Option<String>,
    version: &str,
) {
    let mut payload = json!({ "stage": stage, "version": version });
    if let Some(p) = percent {
        payload["percent"] = json!(p);
    }
    if let Some(d) = detail {
        payload["detail"] = json!(d);
    }
    let msg = Message {
        msg_type: "agent:upgrade_progress".to_string(),
        session_id: session_id.to_string(),
        payload,
    };
    if let Ok(s) = serde_json::to_string(&msg) {
        let _ = ctrl_tx.send(s).await;
    }
}

/// Resolve the real path of the running executable (following symlinks, so a
/// `/usr/local/bin/shell-remote` symlink keeps pointing at the replaced file).
/// Production code resolves this once at startup instead of calling this here —
/// kept for the unit test and as the canonicalization reference.
#[allow(dead_code)]
fn current_exe_real() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe()?.canonicalize()
}

/// Download `url` (authenticated with the agent's read-write token) to `dest`,
/// returning the total byte count. Reports progress via `ctrl_tx`.
async fn download_to_file(
    http: &reqwest::Client,
    relay_base: &str,
    token: &str,
    url: &str,
    dest: &std::path::Path,
    ctrl_tx: &Sender<String>,
    session_id: &str,
    version: &str,
) -> anyhow::Result<u64> {
    let full_url = format!("{}/{}", relay_base.trim_end_matches('/'), url.trim_start_matches('/'));
    let resp = tokio::time::timeout(DOWNLOAD_TIMEOUT, http.get(&full_url).header("X-SR-Token", token).send())
        .await
        .map_err(|_| anyhow::anyhow!("download timed out after {DOWNLOAD_TIMEOUT:?}"))?
        .map_err(|e| anyhow::anyhow!("download request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("download returned HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| anyhow::anyhow!("cannot create temp file: {e}"))?;
    let mut stream = resp.bytes_stream();
    let mut written: u64 = 0;
    let mut last_pct = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("download stream error: {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| anyhow::anyhow!("write error: {e}"))?;
        written += chunk.len() as u64;
        let pct = if total > 0 {
            (written * 100).min(total * 100) / total
        } else {
            0
        };
        if pct != last_pct {
            last_pct = pct;
            send_progress(
                ctrl_tx,
                session_id,
                "downloading",
                Some(pct),
                None,
                version,
            )
            .await;
        }
    }
    file.flush().await.ok();
    let _ = file.sync_all().await;
    Ok(written)
}

/// Hex-encoded SHA-256 of a file, streamed to bound memory use.
pub fn sha256_hex_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Constant-time digest comparison (case-insensitive hex).
fn verify_sha256(actual: &str, expected: &str) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    let a = actual.as_bytes();
    let e = expected.as_bytes();
    a.iter()
        .zip(e.iter())
        .fold(0u8, |acc, (x, y)| acc | x.to_ascii_lowercase() ^ y.to_ascii_lowercase())
        == 0
}

/// Unix: run the artifact once (`--version`) to ensure it actually executes on
/// this platform before replacing the running binary. A wrong-arch or truncated
/// artifact fails here and the old agent stays untouched.
#[cfg(not(windows))]
async fn smoke_test(path: &std::path::Path) -> bool {
    let status = tokio::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    matches!(status, Ok(s) if s.success())
}

#[cfg(windows)]
async fn smoke_test(_path: &std::path::Path) -> bool {
    // Running the payload on Windows would briefly start a second agent; skip
    // the smoke test there — integrity is already covered by SHA-256.
    true
}

/// Atomically replace `target` with `tmp` (same directory).
#[cfg(not(windows))]
fn install_binary(tmp: &std::path::Path, target: &std::path::Path) -> std::io::Result<InstallOutcome> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(tmp, target)?;
    Ok(InstallOutcome { restart_now: true })
}

/// Windows: try a direct replace; if the running `.exe` blocks it (access
/// denied), defer the swap to a helper `.bat` that waits for this process to
/// exit, then moves the file into place and starts the new binary.
#[cfg(windows)]
fn install_binary(tmp: &std::path::Path, target: &std::path::Path) -> std::io::Result<InstallOutcome> {
    match std::fs::rename(tmp, target) {
        Ok(()) => Ok(InstallOutcome { restart_now: true }),
        Err(e)
            if e.kind() == std::io::ErrorKind::PermissionDenied
                || e.kind() == std::io::ErrorKind::Other =>
        {
            let exe_name = target
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("shell-remote.exe");
            let bat = target.with_extension("update.bat");
            let args = std::env::args()
                .skip(1)
                .map(|a| format!("\"{}\"", a))
                .collect::<Vec<_>>()
                .join(" ");
            let script = format!(
                "@echo off\r\n\
                 :wait\r\n\
                 tasklist /FI \"IMAGENAME eq {exe}\" | findstr /I \"{exe}\" >nul 2>nul\r\n\
                 if not errorlevel 1 (\r\n\
                 timeout /t 1 /nobreak >nul\r\n\
                 goto wait\r\n\
                 )\r\n\
                 move /y \"{tmp}\" \"{target}\" >nul\r\n\
                 del \"%~f0\"\r\n\
                 start \"\" \"{target}\" {args}\r\n",
                exe = exe_name,
                tmp = tmp.display(),
                target = target.display(),
                args = args,
            );
            std::fs::write(&bat, script)?;
            let _ = std::process::Command::new("cmd").arg("/c").arg(&bat).spawn();
            Ok(InstallOutcome { restart_now: false })
        }
        Err(e) => Err(e),
    }
}

/// Spawn the (already-replaced) binary with the original argv, detached. On
/// unix the child gets its own process group so the parent's terminal logout
/// (SIGHUP to the old foreground group) cannot kill the upgraded agent.
fn restart_process(target: &std::path::Path) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(target);
    cmd.args(std::env::args().skip(1))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map(|_| ())
}

/// Orchestrate one upgrade. Sends progress frames through `ctrl_tx`; on
/// success the process is replaced and this function never returns. On failure
/// it reports `failed` and returns so the agent keeps running unchanged.
///
/// `target` is the canonical path of the running executable, resolved at agent
/// startup — read it eagerly because `std::env::current_exe()` can fail later
/// (e.g. the file under us was rebuilt/rotated in place, which on Linux makes
/// the `/proc/self/exe` symlink dangle).
pub async fn perform_upgrade(
    http: reqwest::Client,
    relay_base: String,
    token: String,
    session_id: String,
    ctrl_tx: Sender<String>,
    req: UpgradeRequest,
    target: std::path::PathBuf,
) {
    send_progress(&ctrl_tx, &session_id, "started", None, None, &req.version).await;

    let Some(dir) = target.parent().map(|d| d.to_path_buf()) else {
        report_failed(&ctrl_tx, &session_id, &req.version, "cannot locate executable directory".into()).await;
        return;
    };
    let tmp = dir.join(format!(".shell-remote-upgrade-{}.new", std::process::id()));

    // 1. Download (already tracks progress).
    if let Err(e) = download_to_file(&http, &relay_base, &token, &req.url, &tmp, &ctrl_tx, &session_id, &req.version).await {
        report_failed_cleanup(&ctrl_tx, &session_id, &req.version, &tmp, format!("download failed: {e}")).await;
        return;
    }

    // 2. Verify integrity.
    send_progress(&ctrl_tx, &session_id, "verifying", Some(100), None, &req.version).await;
    let digest = match sha256_hex_file(&tmp) {
        Ok(d) => d,
        Err(e) => {
            report_failed_cleanup(&ctrl_tx, &session_id, &req.version, &tmp, format!("cannot hash downloaded file: {e}")).await;
            return;
        }
    };
    if !verify_sha256(&digest, &req.sha256) {
        report_failed_cleanup(&ctrl_tx, &session_id, &req.version, &tmp, "SHA-256 mismatch — artifact is corrupted or wrong".into()).await;
        return;
    }

    // 3. Confirm the payload runs on this platform before touching the binary.
    // The download lands with restrictive permissions (File::create), so mark
    // it executable first — the smoke test must exec it as-is.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        {
            report_failed_cleanup(
                &ctrl_tx,
                &session_id,
                &req.version,
                &tmp,
                format!("cannot mark artifact executable: {e}"),
            )
            .await;
            return;
        }
    }
    if !smoke_test(&tmp).await {
        report_failed_cleanup(&ctrl_tx, &session_id, &req.version, &tmp, "new binary does not execute on this platform".into()).await;
        return;
    }

    // 4. Atomic replace.
    let outcome = match install_binary(&tmp, &target) {
        Ok(o) => o,
        Err(e) => {
            report_failed_cleanup(&ctrl_tx, &session_id, &req.version, &tmp, format!("cannot replace the running binary: {e}").into()).await;
            return;
        }
    };
    let _ = std::fs::remove_file(&tmp);

    // 5. Restart. Give the terminal progress frame a moment to reach the relay
    //    before this process (and its sender loop) disappears.
    send_progress(&ctrl_tx, &session_id, "installing", None, Some("installed, restarting".to_string()), &req.version).await;
    tokio::time::sleep(EXIT_FLUSH_GRACE).await;
    if outcome.restart_now {
        if let Err(e) = restart_process(&target) {
            report_failed(&ctrl_tx, &session_id, &req.version, format!("binary replaced but restart failed: {e}")).await;
            return;
        }
    }
    std::process::exit(0);
}

async fn report_failed_cleanup(
    ctrl_tx: &Sender<String>,
    session_id: &str,
    version: &str,
    tmp: &std::path::Path,
    error: String,
) {
    let _ = std::fs::remove_file(tmp);
    report_failed(ctrl_tx, session_id, version, error).await;
}

async fn report_failed(ctrl_tx: &Sender<String>, session_id: &str, version: &str, error: String) {
    let mut payload = json!({ "stage": "failed", "version": version });
    payload["error"] = json!(error);
    let msg = Message {
        msg_type: "agent:upgrade_progress".to_string(),
        session_id: session_id.to_string(),
        payload,
    };
    if let Ok(s) = serde_json::to_string(&msg) {
        let _ = ctrl_tx.send(s).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_hex_file_known_vector() {
        let dir = std::env::temp_dir().join(format!("sr-upg-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("data.bin");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_hex_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_verify_sha256() {
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_sha256(good, good));
        assert!(verify_sha256(&good.to_uppercase(), good)); // hex case-insensitive
        assert!(!verify_sha256(&format!("f{}", &good[1..]), good));
        assert!(!verify_sha256(good, "ba7816bf"));
    }

    #[test]
    fn test_upgrade_request_from_payload() {
        let req = UpgradeRequest::from_payload(&json!({
            "version": "0.19.0",
            "url": "/agent/upgrade/blob/shell-remote-x86_64",
            "sha256": "abcd",
        }))
        .unwrap();
        assert_eq!(req.version, "0.19.0");
        assert_eq!(req.sha256, "abcd");
        assert!(UpgradeRequest::from_payload(&json!({"version": "1"})).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_install_binary_atomic_replace() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sr-upg-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("new");
        let target = dir.join("running");
        std::fs::write(&tmp, b"new-content").unwrap();
        std::fs::write(&target, b"old-content").unwrap();
        let outcome = install_binary(&tmp, &target).unwrap();
        assert!(outcome.restart_now);
        assert_eq!(std::fs::read(&target).unwrap(), b"new-content");
        assert!(!tmp.exists());
        // executable bit preserved for the swapped file
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "installed binary must stay executable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_install_binary_missing_tmp_fails() {
        let dir = std::env::temp_dir().join(format!("sr-upg-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("missing");
        let target = dir.join("running");
        std::fs::write(&target, b"old").unwrap();
        assert!(install_binary(&tmp, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_smoke_test_rejects_non_executable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sr-upg-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("fake-bin");
        std::fs::write(&f, b"this is not an executable").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!smoke_test(&f).await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_current_exe_real_points_at_something() {
        let p = current_exe_real().unwrap();
        assert!(p.is_absolute());
        assert!(p.exists());
    }
}