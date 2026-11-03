//! overlay 自升级的**真实集成测试**（MYS-2574 验收 2）。
//!
//! 这组用例跑真 relay、真二进制、真的 `chmod 0555` 目录，验证整条链路：
//! 目录不可写 → 装进 overlay → 重启 → 交接 → 重新注册为新版本 →（模拟机器
//! 重启后 systemd 拉起 base）仍然是新版本 → 坏版本三次后自动回落 → 权限被
//! 放宽时拒绝交接。
//!
//! 需要两个版本号不同的构建，因此和 `real_old_binary` 一样用**环境变量门控**，
//! 没有就明确打印跳过原因（绝不静默不测）：
//!
//! ```text
//! SR_OVERLAY_BASE_BIN  正在运行的旧版二进制（0.59.12，放进 0555 目录当 base）
//! SR_OVERLAY_NEW_BIN   目标版本二进制（0.59.13，既是 relay 也是升级制品）
//! ```
//!
//! 跑法见 `docs/upgrade.md` 的「overlay 升级的实测」一节。

#![cfg(unix)]

use super::super::*;
use super::{sha_of, TmpDir};
use std::io::{BufRead, BufReader};
use std::process::{Child, Stdio};
use std::time::Duration;

fn env_bin(key: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os(key)?);
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("跳过：{key}={} 不是文件", p.display());
        None
    }
}

/// 一个跑完就自动杀掉的子进程，并且在启动时就**起一个回收线程**。
///
/// 回收线程不是可选的：换机助手靠 `kill(pid, 0)` 判断"旧进程还在不在"，而
/// 已退出但**未被 `wait()` 回收**的子进程仍是僵尸，`kill(pid, 0)` 对它返回
/// 成功——助手会一直等到 45s 超时，把一次正常升级判成失败。生产环境里 systemd
/// 会立刻回收它的子进程，所以只有测试需要显式做这件事。
struct Kill {
    child: std::sync::Arc<std::sync::Mutex<Option<Child>>>,
}

impl Kill {
    fn new(child: Child) -> Self {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
        let reaper = slot.clone();
        std::thread::Builder::new()
            .name("sr-ovl-reaper".into())
            .spawn(move || {
                // 先 `wait` 再丢锁：子进程一退出就立刻被回收，助手看到的
                // `kill(pid, 0)` 结果才是真实的。
                if let Some(mut child) = reaper.lock().unwrap().take() {
                    let _ = child.wait();
                }
            })
            .expect("cannot start the reaper thread");
        Self { child: slot }
    }

}

impl Drop for Kill {
    fn drop(&mut self) {
        // 回收线程可能还持有 Child：拿不到锁（它正在 wait 里）就不强杀，
        // 让它自然退出——测试收尾时多一个进程不影响断言。
        if let Ok(mut slot) = self.child.try_lock() {
            if let Some(child) = slot.as_mut() {
                let _ = child.kill();
            }
        }
    }
}


/// 真 relay（跑**新版**二进制，这样它自报的版本高于 base agent，才会判定
/// "需要升级"）。
struct Relay {
    _proc: Kill,
    base: String,
    admin: String,
    cookie: String,
    client: reqwest::Client,
}

impl Relay {
    async fn start(new_bin: &Path, download_dir: &Path) -> Option<Self> {
        // 端口自己挑：relay 的 "listening on" 日志打印的是**请求的** bind 字符串
        // （`127.0.0.1:0` 原样输出），从日志里读不出真实端口。
        let port = free_port()?;
        let bind = format!("127.0.0.1:{port}");
        let mut child = std::process::Command::new(new_bin)
            .args([
                "relay",
                "--bind",
                &bind,
                "--auth",
                "e2e-pass",
                "--no-tls",
                "--download-dir",
                &download_dir.to_string_lossy(),
                // 没有官方清单 → 走 local/custom 源（relay 镜像），不联网。
                "--allow-custom-upgrade",
                "--manifest-api-base",
                "http://127.0.0.1:1",
                "--admin-path",
                "/sr-admin",
                "--admin-user",
                "admin",
                "--admin-pass",
                "e2e-admin",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // relay 的日志走 stdout（tracing 默认）；给它一份独立日志文件，
            // 起不来时能贴出原因。
            .env("SR_LOG_DIR", download_dir.join("relay-logs"))
            .spawn()
            .ok()?;
        // 两条流都必须有人读：管道写满（64KB）后 relay 会阻塞在自己的日志 write
        // 上，注册请求随之挂死。
        let mut pumps = Vec::new();
        let stdout = child.stdout.take()?;
        let stderr = child.stderr.take()?;
        pumps.push(spawn_drain(stdout));
        pumps.push(spawn_drain(stderr));
        let _ = &pumps;
        let proc = Kill::new(child);
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        // 等 relay 真的开始服务（TCP 连得上）再返回。
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut ready = false;
        while std::time::Instant::now() < deadline {
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !ready {
            eprintln!(
                "跳过：relay 没能在 30s 内监听 {bind}；日志目录 {}",
                download_dir.join("relay-logs").display()
            );
            return None;
        }
        let resp = client
            .post(format!("{base}/sr-admin/login"))
            .json(&serde_json::json!({"user": "admin", "pass": "e2e-admin"}))
            .send()
            .await
            .ok()?;
        let cookie = resp
            .headers()
            .get("set-cookie")?
            .to_str()
            .ok()?
            .split(';')
            .next()?
            .to_string();
        Some(Self {
            _proc: proc,
            admin: format!("{base}/sr-admin"),
            base,
            cookie,
            client,
        })
    }

    /// 管理后台概览里某台设备的版本（`None` = 设备还没注册/已下线）。
    async fn online_version(&self, sid: &str) -> Option<(String, bool)> {
        let v: serde_json::Value = self
            .client
            .get(format!("{}/api/overview", self.admin))
            .header("cookie", &self.cookie)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let sessions = v["sessions"].as_array()?;
        let s = sessions.iter().find(|s| s["session_id"] == sid)?;
        Some((
            s["agent_version"].as_str().unwrap_or("").to_string(),
            s["online"].as_bool().unwrap_or(false),
        ))
    }

    /// 当前在线设备的 (session, 版本) 列表。
    ///
    /// 断言"设备跑的是哪一版"时用它而不是钉死某个 session id：这些用例里的 agent
    /// 没有 `--key`、也没有 `--session-id`（临时设备），每次**全新**启动都会拿到
    /// 新会话——那是 relay 的既有语义，与 overlay 无关。overlay 要保证的是"重启
    /// 之后跑的仍是新版本"，那就按版本判。
    async fn online(&self) -> Vec<(String, String)> {
        let Ok(resp) = self
            .client
            .get(format!("{}/api/overview", self.admin))
            .header("cookie", &self.cookie)
            .send()
            .await
        else {
            return Vec::new();
        };
        let Ok(v) = resp.json::<serde_json::Value>().await else {
            return Vec::new();
        };
        v["sessions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|s| s["online"].as_bool().unwrap_or(false))
                    .map(|s| {
                        (
                            s["session_id"].as_str().unwrap_or("").to_string(),
                            s["agent_version"].as_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 有没有**任何一台**在线设备跑着 `version`（前缀匹配，容忍预发布后缀）。
    async fn any_online_version(&self, version: &str) -> bool {
        self.online()
            .await
            .iter()
            .any(|(_, v)| v.starts_with(version))
    }

    /// 触发单台升级。返回 HTTP 状态与响应体（失败原因要能贴进断言）。
    async fn trigger_upgrade(&self, sid: &str) -> (u16, serde_json::Value) {
        let resp = self
            .client
            .post(format!("{}/api/agent/upgrade", self.admin))
            .header("cookie", &self.cookie)
            .json(&serde_json::json!({"session_id": sid}))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.json().await.unwrap_or(serde_json::Value::Null);
        (status, body)
    }
}

/// 一套隔离的"设备"环境：HOME/XDG 全指向临时目录，base 放在 0555 目录里。
struct Device {
    root: PathBuf,
    base: PathBuf,
    home: PathBuf,
    xdg: PathBuf,
    log: PathBuf,
}

impl Device {
    fn new(tag: &str, base_bin: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TmpDir::new(tag);
        let root = dir.path().to_path_buf();
        // 目录本身要活到测试结束：泄漏到 /tmp 由系统清理，比 Drop 时和正在
        // 运行的进程抢文件更安全。
        std::mem::forget(dir);
        let bin_dir = root.join("bin");
        let home = root.join("home");
        let xdg = root.join("xdg");
        for d in [&bin_dir, &home, &xdg, &root.join("work")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let base = bin_dir.join("shell-remote");
        std::fs::copy(base_bin, &base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        // 关键：**不可写**的安装目录（root 所有的 /usr/local/bin 的等价物）。
        std::fs::set_permissions(&bin_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            log: root.join("agent.log"),
            base,
            home,
            xdg,
            root,
        }
    }

    fn overlay_root(&self) -> PathBuf {
        self.xdg.join("shell-remote").join("overlay")
    }

    fn pointer(&self) -> Option<Pointer> {
        let raw = std::fs::read(self.overlay_root().join(POINTER_FILE)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    /// 起一个 base agent 进程（模拟 systemd 拉起）。
    fn start_agent(&self, relay: &Relay) -> Kill {
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        let child = std::process::Command::new(&self.base)
            .args([
                "agent",
                "--relay-url",
                &relay.base,
                "--shell",
                "/bin/sh",
                "--root",
                &self.root.join("work").to_string_lossy(),
                "--no-cmd-log",
            ])
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.xdg)
            .env("XDG_RUNTIME_DIR", self.home.join("run"))
            // 父进程的 overlay 标记绝不能漏进子进程，否则每一次启动都会被当成
            // "我是 overlay，别再交接"。
            .env_remove(ENV_FROM)
            .env_remove("SR_UPGRADE_RESTORE")
            // cargo test 本身就跑在某个 systemd service 里，`INVOCATION_ID` 会
            // 被继承；不清掉的话 agent 会以为自己也受那个 unit 托管（cgroup 也
            // 确实指向它），从而选错重启策略。这里模拟的是"没有 systemd 的
            // 普通进程"，由助手负责重新拉起 base。
            .env_remove("INVOCATION_ID")
            // 不设 `SR_LOG_DIR`：日志留在 stderr，测试才能按行断言（设了它
            // tracing 会改写到文件里，stderr 只剩一行 pid）。
            .env_remove("SR_LOG_DIR")
            .stdin(Stdio::null())
            // tracing 默认写 stdout，`eprintln!` 写 stderr：两条都收进同一份日志。
            .stdout(Stdio::from(out.try_clone().unwrap()))
            .stderr(Stdio::from(out))
            .spawn()
            .expect("base agent 必须能启动");
        Kill::new(child)
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// 杀掉这台"设备"上的所有 agent 进程（按 argv 里的设备目录匹配）。
    ///
    /// 升级之后 agent 是助手拉起的新进程，不再是被测进程的子进程；`Drop` 里那个
    /// `kill` 覆盖不到它。这里按 `--root <设备目录>` 精确匹配，不会误杀别的 agent
    /// （同一台机器上并行跑别的用例时尤其重要）。
    fn kill_agents(&self) {
        let needle = self.root.to_string_lossy().to_string();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                continue;
            };
            let cmd = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            // 只碰本设备的 agent（argv 里同时有 `agent` 与设备目录）。
            if cmd.contains(" agent ") && cmd.contains(&needle) && !cmd.contains(" relay ") {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        // 给内核一点时间把进程真正收掉。
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// 等某个条件成立（带超时）。返回是否成立。
async fn wait_until<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// 从日志里找出本设备的 session id（agent 会打印 `session=<sid>`）。
fn session_from_log(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(idx) = line.find("session=") {
            let rest = &line[idx + "session=".len()..];
            let sid: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !sid.is_empty() {
                return Some(sid);
            }
        }
    }
    None
}

/// 把一条子进程管道读到 EOF（不读会把 relay 阻塞在自己的日志 write 上）。
fn spawn_drain<R: std::io::Read + Send + 'static>(pipe: R) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("sr-ovl-relay-log".into())
        .spawn(move || {
            let mut reader = BufReader::new(pipe);
            let mut line = String::new();
            while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                line.clear();
            }
        })
        .expect("cannot start the relay log drain")
}

/// 让内核挑一个空闲端口（relay 用 `--bind 127.0.0.1:0` 时日志里看不到真实端口）。
fn free_port() -> Option<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    Some(l.local_addr().ok()?.port())
}

/// 0.59.12 和 0.59.13 两份构建；没有就跳过。
fn binaries() -> Option<(PathBuf, PathBuf)> {
    let base = env_bin("SR_OVERLAY_BASE_BIN")?;
    let new = env_bin("SR_OVERLAY_NEW_BIN")?;
    Some((base, new))
}

/// 整条主链路：不可写目录 → overlay 安装 → 重启 → 交接 → 注册成新版本 →
/// 再次启动（模拟机器重启）仍然是新版本。
#[tokio::test]
async fn test_overlay_upgrade_from_a_read_only_directory() {
    let Some((base_bin, new_bin)) = binaries() else {
        return;
    };
    // 前提：以普通用户跑。root 会绕过权限位，"目录不可写"就造不出来，这组用例
    // 也就失去意义——与其给出假绿，不如明确跳过。
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("跳过：以 root 运行时 chmod 0555 不生效，无法构造\"目录不可写\"的现场");
        return;
    }
    let staging = TmpDir::new("e2e-stage");
    let artifact_name = format!("shell-remote-{}", crate::agent::download::own_arch_key());
    std::fs::copy(&new_bin, staging.join(&artifact_name)).unwrap();

    let Some(relay) = Relay::start(&new_bin, staging.path()).await else {
        eprintln!("跳过：无法启动真 relay");
        return;
    };
    let dev = Device::new("e2e-main", &base_bin);

    // 1. base 起在 0555 目录里，正常注册（0.59.12）。
    let _agent = dev.start_agent(&relay);
    assert!(
        wait_until(Duration::from_secs(60), || async {
            dev.log_text().contains("agent session established")
        })
        .await,
        "base agent 必须完成首次注册；日志:\n{}",
        dev.log_text()
    );
    let sid = session_from_log(&dev.log_text()).expect("日志里必须有 session id");
    assert!(
        wait_until(Duration::from_secs(30), || async {
            relay
                .online_version(&sid)
                .await
                .is_some_and(|(v, online)| online && v.starts_with("0.59.12"))
        })
        .await,
        "relay 上应看到 0.59.12 在线；实际 {:?}",
        relay.online_version(&sid).await
    );

    // 2. 触发升级：目录不可写 ⇒ agent 自己走 overlay。
    let (status, body) = relay.trigger_upgrade(&sid).await;
    assert_eq!(status, 200, "升级必须被接受: {body}");

    let upgraded = wait_until(Duration::from_secs(120), || async {
        relay.any_online_version("0.59.13").await
    })
    .await;
    let log = dev.log_text();
    assert!(upgraded, "升级后 relay 上必须看到 0.59.13 在线；日志:\n{log}");
    // 关键日志行：走了 overlay、发生了 exec 交接。
    assert!(
        log.contains("installing the new version into the overlay"),
        "必须走 overlay 安装路径；日志:\n{log}"
    );
    // 交接是否真的发生，看**磁盘上的证据**而不是日志行：`healthy` 只在
    // "本进程是 overlay 版本（`SR_OVERLAY_FROM` 已设置）**并且** `current_exe`
    // 就是指针指向的那个文件"时才会被置位（见 `overlay::after_registration`）。
    // 这一条同时证明了三件事：exec 发生了、执行的是指针里那份、它注册成功了。
    //
    // 不查 "handing the exec over" 这行日志，是因为它由 base 打印在 exec 之前，
    // 而这条路径上 base 的 stdout 是助手继承来的管道——旧 agent 已经退出，写端
    // 早已 EPIPE（tracing 会自己报 Broken pipe）。

    // 3. 磁盘状态：指针指向 bin/ 里那份，且已标健康；base 一个字节没动。
    let pointer = dev.pointer().expect("必须写了 current.json");
    assert_eq!(pointer.version, "0.59.13");
    assert!(pointer.healthy, "注册成功后必须标健康");
    assert_eq!(pointer.attempts, 0, "健康之后尝试次数要归零");
    assert!(pointer.path.is_file(), "指针指向的文件必须存在");
    assert_eq!(
        pointer.base_exe.as_deref(),
        Some(dev.base.as_path()),
        "指针必须记下真正的 base"
    );
    assert_eq!(
        std::fs::read(&dev.base).unwrap(),
        std::fs::read(&base_bin).unwrap(),
        "base 二进制必须原封不动"
    );
    // 新版本文件在 0700 权限下，且内容就是新构建。
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(&pointer.path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "overlay 二进制必须是 0700");
    assert_eq!(std::fs::read(&pointer.path).unwrap(), std::fs::read(&new_bin).unwrap());

    // 4. 模拟"机器重启后 systemd 再次拉起 base"：它必须 exec 到 overlay，
    //    版本仍然是新版本。
    //
    // 升级后的 agent 已经是助手拉起的**新进程**（不再是 `agent` 那个子进程），
    // 所以按设备目录匹配 argv 把它们全杀掉——等价于"机器断电重启"。
    dev.kill_agents();
    assert!(
        wait_until(Duration::from_secs(30), || async {
            relay.online().await.is_empty()
        })
        .await,
        "重启前必须确认旧进程真的退出；实际 {:?}",
        relay.online().await
    );
    let before = dev.log_text().len();
    let _agent2 = dev.start_agent(&relay);
    let rehanded = wait_until(Duration::from_secs(60), || async {
        dev.log_text()[before..].contains("handing the exec over to the overlay binary")
    })
    .await;
    let log2 = dev.log_text();
    assert!(
        rehanded,
        "重启后 base 必须再次交接给 overlay；新增日志:\n{}",
        &log2[before..]
    );
    assert!(
        wait_until(Duration::from_secs(60), || async {
            relay.any_online_version("0.59.13").await
        })
        .await,
        "重启后版本必须仍是 0.59.13；实际 {:?}",
        relay.online().await
    );
    // 磁盘上仍然是同一个健康指针：重启不该重置成功状态。
    let after = dev.pointer().expect("指针必须还在");
    assert!(after.healthy, "重启后指针必须仍然是 healthy");
    assert_eq!(after.path, pointer.path, "重启不该换掉已装好的版本");
}

/// 持久化的坏版本：连续 3 次交接都起不来之后回落到 base，并置 `rejected`。
#[tokio::test]
async fn test_overlay_rolls_back_after_three_failed_starts() {
    let Some((base_bin, new_bin)) = binaries() else {
        return;
    };
    // 前提：以普通用户跑。root 会绕过权限位，"目录不可写"就造不出来，这组用例
    // 也就失去意义——与其给出假绿，不如明确跳过。
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("跳过：以 root 运行时 chmod 0555 不生效，无法构造\"目录不可写\"的现场");
        return;
    }
    let staging = TmpDir::new("e2e-stage2");
    let artifact_name = format!("shell-remote-{}", crate::agent::download::own_arch_key());
    std::fs::copy(&new_bin, staging.join(&artifact_name)).unwrap();
    let Some(relay) = Relay::start(&new_bin, staging.path()).await else {
        return;
    };
    let dev = Device::new("e2e-rollback", &base_bin);
    let _agent = dev.start_agent(&relay);
    assert!(
        wait_until(Duration::from_secs(60), || async {
            dev.log_text().contains("agent session established")
        })
        .await,
        "base 必须注册成功；日志:\n{}",
        dev.log_text()
    );
    let sid = session_from_log(&dev.log_text()).unwrap();
    let (status, body) = relay.trigger_upgrade(&sid).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        wait_until(Duration::from_secs(120), || async {
            dev.pointer().is_some_and(|p| p.healthy)
        })
        .await,
        "先要有一个健康的 overlay 版本"
    );
    drop(_agent);

    // 把 overlay 二进制换成一个**立即退出**的坏程序，并同步改掉指针里的 sha：
    // 于是安全检查全过，问题只剩"它起不来"——这正是回滚要处理的场景。
    //
    // 先把设备上仍在跑的 overlay 进程杀干净：就地覆盖一个**正在执行**的文件会
    // 撞 ETXTBSY。生产环境不会这么做（版本文件是按 sha 命名的、只增不改），
    // 这里是在人为制造"坏版本"，所以要先让文件空闲下来。
    dev.kill_agents();
    assert!(
        wait_until(Duration::from_secs(30), || async {
            relay.online().await.is_empty()
        })
        .await,
        "改文件之前必须确认设备上的 agent 都已退出；实际 {:?}",
        relay.online().await
    );
    let pointer = dev.pointer().unwrap();
    let bad = b"#!/bin/sh\nexit 7\n";
    std::fs::write(&pointer.path, bad).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&pointer.path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut tampered = pointer.clone();
    tampered.sha256 = sha_of(bad);
    tampered.healthy = false;
    tampered.attempts = 0;
    Pointer::write(&Layout::new(dev.overlay_root()), &tampered).unwrap();

    // 连续启动 3 次：每次 base 交接给坏程序，坏程序立刻退出。
    for attempt in 1..=MAX_ATTEMPTS {
        let before = dev.log_text().len();
        {
            let _a = dev.start_agent(&relay);
            let handed = wait_until(Duration::from_secs(30), || async {
                dev.log_text()[before..].contains("handing the exec over to the overlay binary")
            })
            .await;
            assert!(handed, "第 {attempt} 次必须尝试交接；日志:\n{}", &dev.log_text()[before..]);
            // 坏程序立刻退出，等它死透。
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(
            dev.pointer().unwrap().attempts,
            attempt,
            "每次交接前必须把 attempts 加一"
        );
    }

    // 第 4 次启动：次数到顶 ⇒ 记 rejected，**不再交接**，base 自己注册上线。
    let before = dev.log_text().len();
    let _agent = dev.start_agent(&relay);
    let rejected = wait_until(Duration::from_secs(30), || async {
        dev.pointer().is_some_and(|p| p.rejected)
    })
    .await;
    let log = dev.log_text();
    assert!(
        rejected,
        "三次之后必须置 rejected；新增日志:\n{}",
        &log[before..]
    );
    assert!(
        log[before..].contains("never became healthy"),
        "必须写明回退原因；新增日志:\n{}",
        &log[before..]
    );
    assert!(
        !log[before..].contains("handing the exec over"),
        "rejected 之后不得再交接；新增日志:\n{}",
        &log[before..]
    );
    assert!(
        wait_until(Duration::from_secs(60), || async {
            relay.any_online_version("0.59.12").await
        })
        .await,
        "回落后 base 必须自己注册上线（0.59.12）；实际 {:?}",
        relay.online().await
    );
    // 失败要如实上报给 relay（管理面板能看到的 `failed` 帧）。
    //
    // 这条帧由**回落后新注册的 base 进程**发出（`overlay::after_registration`
    // 在注册成功之后读 `rejected` 指针），所以要去那一台上找——原来的 session
    // 已经随坏版本一起退出了。
    let progress = relay
        .client
        .get(format!("{}/api/overview", relay.admin))
        .header("cookie", &relay.cookie)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let upg = progress["sessions"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|s| {
                    s["online"].as_bool().unwrap_or(false)
                        && s["agent_version"]
                            .as_str()
                            .unwrap_or("")
                            .starts_with("0.59.12")
                })
                .map(|s| s["upgrade"].clone())
        })
        .unwrap_or(serde_json::Value::Null);
    assert_eq!(upg["stage"], "failed", "必须上报 failed；实际 {upg}");
    assert!(
        upg["detail"]
            .as_str()
            .unwrap_or("")
            .contains("mode: overlay"),
        "阶段说明要注明 overlay 模式: {upg}"
    );
}

/// overlay 二进制被改成 group-writable ⇒ 拒绝交接，base 照常运行。
#[tokio::test]
async fn test_overlay_refuses_a_group_writable_binary() {
    let Some((base_bin, new_bin)) = binaries() else {
        return;
    };
    // 前提：以普通用户跑。root 会绕过权限位，"目录不可写"就造不出来，这组用例
    // 也就失去意义——与其给出假绿，不如明确跳过。
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("跳过：以 root 运行时 chmod 0555 不生效，无法构造\"目录不可写\"的现场");
        return;
    }
    let staging = TmpDir::new("e2e-stage3");
    let artifact_name = format!("shell-remote-{}", crate::agent::download::own_arch_key());
    std::fs::copy(&new_bin, staging.join(&artifact_name)).unwrap();
    let Some(relay) = Relay::start(&new_bin, staging.path()).await else {
        return;
    };
    let dev = Device::new("e2e-gw", &base_bin);
    // 先正常升级一次，得到一个健康的 overlay。
    let agent = dev.start_agent(&relay);
    assert!(
        wait_until(Duration::from_secs(60), || async {
            dev.log_text().contains("agent session established")
        })
        .await
    );
    let sid = session_from_log(&dev.log_text()).unwrap();
    let (status, body) = relay.trigger_upgrade(&sid).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        wait_until(Duration::from_secs(120), || async {
            dev.pointer().is_some_and(|p| p.healthy)
        })
        .await,
        "先要有一个健康的 overlay"
    );
    drop(agent);

    // 放宽权限：内容不变、sha 仍然一致，唯一的问题是别人能改写它。
    let pointer = dev.pointer().unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&pointer.path, std::fs::Permissions::from_mode(0o720)).unwrap();
    }
    let before = dev.log_text().len();
    let _agent = dev.start_agent(&relay);
    let refused = wait_until(Duration::from_secs(30), || async {
        dev.log_text()[before..].contains("writable by group/other")
    })
    .await;
    let log = dev.log_text();
    assert!(
        refused,
        "group-writable 的 overlay 必须被拒绝；新增日志:\n{}",
        &log[before..]
    );
    assert!(
        !log[before..].contains("handing the exec over"),
        "拒绝之后不得交接；新增日志:\n{}",
        &log[before..]
    );
    // base 照常注册上线：看**这一段**日志里的版本行，而不是 relay 上的会话列表
    // ——上一轮升级留下的 0.59.13 会话在 relay 看来仍可能短暂在线，用版本判定
    // 会把它误当成"还是新版在跑"。
    assert!(
        wait_until(Duration::from_secs(60), || async {
            let text = dev.log_text();
            text[before..].contains("shell-remote v0.59.12")
                && text[before..].contains("agent session established")
        })
        .await,
        "拒绝交接后 base 必须照常注册上线；新增日志:\n{}",
        &dev.log_text()[before..]
    );
    assert_eq!(
        dev.pointer().unwrap().version,
        "0.59.13",
        "拒绝只是这一次不交接，指针本身不该被改动"
    );
}
