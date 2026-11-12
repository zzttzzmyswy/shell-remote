//! UI 连接控制层（MYS-2806 阶段4）：把"一份 [`UiConfig`]"变成"一个跑起来、
//! 可以随时停掉、停掉之后还能再改再连"的 agent 实例。
//!
//! 界面（[`crate::gui`]）只跟这个类型打交道：编辑草稿 → `connect(草稿.clone())`
//! → 每帧 `poll()` + 读 `status()`/`is_running()`/`last_error()` → 要停就
//! `disconnect()`（立即返回，不阻塞 UI 线程）→ 等 `is_running()` 变 false 后
//! 再改配置、再 `connect()`。
//!
//! 三条语义约定：
//! 1. **只有一个实例**：`connect()` 在已有实例（含正在停止的）时返回错误，绝不
//!    起第二个 agent 线程——否则两个 agent 会抢同一个 `session_id`，在 relay 侧
//!    互相顶号。
//! 2. **配置快照隔离**：`connect()` 拿走的是调用方传进来的那份配置（连同密钥），
//!    界面之后怎么改草稿都影响不到正在跑的实例。
//! 3. **停止是有界的**：`disconnect()` 只是置停机标志；agent 端在注册、退避等待、
//!    正常会话三条路径上都会在 100–500ms 内看到它并收尾（见
//!    `agent::mod.rs` 的 `until_shutdown`），本层在 `poll()` 里只做非阻塞观测。
//!
//! 密钥：只在内存（不落盘，见 [`UiConfig`]），从不进日志；错误文本在交给界面
//! 之前还会再脱敏一次。

use std::time::{Duration, Instant};

use anyhow::anyhow;

use crate::status::{AgentStatus, Phase};
use crate::ui_config::UiConfig;

/// 停机收敛的观测上限。任务卡要求正常 / 离线重试 / 注册超时三条路径都在 5 秒内
/// 停干净；超过它只做一次可见告警（不阻塞、不强杀线程）。
pub const STOP_DEADLINE: Duration = Duration::from_secs(5);

/// 本进程里由 [`UiController`] 起、且尚未结束的 agent 线程数。
///
/// 只用于可观测性（日志/测试断言）：`is_running()` 看的是自己那个 `JoinHandle`，
/// 而这个计数回答的是"进程里还有没有本层遗留的 agent 线程"。按线程名去
/// `/proc` 里数是不行的——同进程其它地方（UI 二进制）也起 `sr-agent` 线程。
static LIVE_SESSIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 线程退出时递减 [`LIVE_SESSIONS`]（panic 展开时同样生效）。
struct LiveGuard;

impl LiveGuard {
    fn acquire() -> Self {
        LIVE_SESSIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        LiveGuard
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        LIVE_SESSIONS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 一次连接尝试的运行体：后台 agent 线程 + 它写入的状态句柄。
struct Session {
    /// 与 agent 线程共享的状态（阶段 / 会话 id / token / 延迟 / 上报的错误）。
    status: AgentStatus,
    /// agent 线程句柄。**只在确认 `is_finished()` 之后才 join**，否则会阻塞 UI。
    handle: std::thread::JoinHandle<anyhow::Result<()>>,
    /// 本次连接使用的固定密钥，只用于错误文本脱敏（不写日志、不落盘）。
    key: Option<String>,
    /// `disconnect()` 的调用时刻，用于观测停机是否在 [`STOP_DEADLINE`] 内收敛。
    stopping_since: Option<Instant>,
}

/// GUI 侧的连接控制器。`new()` 之后没有任何后台活动；不 `connect()` 就不会
/// 联网（用户需求：无参数启动只显示配置窗，不自动连接）。
pub struct UiController {
    current: Option<Session>,
    last_error: Option<String>,
    stop_overdue_reported: bool,
}

/// 手写 `Debug`：内部 `Session` 持有固定密钥（脱敏用），不能整体 `derive`。
impl std::fmt::Debug for UiController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiController")
            .field("running", &self.is_running())
            .field("last_error", &self.last_error)
            .field(
                "key",
                &self
                    .current
                    .as_ref()
                    .and_then(|s| s.key.as_ref())
                    .map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl Default for UiController {
    fn default() -> Self {
        Self::new()
    }
}

impl UiController {
    pub fn new() -> Self {
        Self {
            current: None,
            last_error: None,
            stop_overdue_reported: false,
        }
    }

    /// 用一份配置启动 agent。**先校验，再起线程**：配置不合法时不会有任何后台
    /// 副作用，错误直接可展示。
    ///
    /// 已有实例**仍在运行时**返回错误，不会启动第二个。线程已经结束但尚未被
    /// `poll()` 回收时，这里顺手收掉它再继续——界面即使漏调了一次 `poll()`
    /// 也不会被"假运行中"卡住。
    pub fn connect(&mut self, config: UiConfig) -> anyhow::Result<()> {
        if self.is_running() {
            return Err(anyhow!(
                "已有连接在运行或正在停止：请先断开并等待停止完成，再连接新的配置"
            ));
        }
        // 已结束的旧实例：非阻塞回收（is_running 为 false 意味着 join 立刻返回）。
        self.reap_finished();

        config.validate()?;

        // 命令日志开关是进程级状态（与 CLI 的 `--no-cmd-log` 同一处）。
        crate::agent::cmd_log::set_enabled(!config.no_cmd_log);

        let status = AgentStatus::new(config.relay_url.clone());
        let relay_url = config.relay_url.clone();
        let key = config.key.clone();
        let root = config.root.clone().unwrap_or_else(crate::agent::home_dir);
        let token_type = config.token_type.clone();
        let shell = config.shell.clone();
        let session_id = config.session_id.clone();
        let insecure = config.relay_insecure;
        // 桌面参数在起线程前就映射好（与 UI 二进制的字段映射逐项一致）。
        #[cfg(feature = "desktop")]
        let desktop_cfg = config.to_desktop_config();

        let thread_status = status.clone();
        let live = LiveGuard::acquire();
        let handle = std::thread::Builder::new()
            // 线程名固定且唯一：`is_running` 之外，测试/运维还能从进程里认出
            // 本层起的 agent 线程（CLI 的 agent 线程叫 `sr-agent`）。
            .name("sr-agent-ui".into())
            .spawn(move || {
                // 无论正常返回、出错还是 panic 展开，退出时都要减掉登记数。
                let _live = live;
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(async move {
                    #[cfg(feature = "desktop")]
                    {
                        crate::agent::start_with_restore(
                            relay_url,
                            key,
                            root,
                            token_type,
                            shell,
                            session_id,
                            desktop_cfg,
                            Some(thread_status),
                            insecure,
                            None,
                        )
                        .await
                    }
                    #[cfg(not(feature = "desktop"))]
                    {
                        crate::agent::start_with_restore(
                            relay_url,
                            key,
                            root,
                            token_type,
                            shell,
                            session_id,
                            Some(thread_status),
                            insecure,
                            None,
                        )
                        .await
                    }
                })
            })
            .map_err(|e| anyhow!("启动 agent 线程失败: {e}"))?;

        tracing::info!(relay = %config.relay_url, "UI: agent instance started");
        self.current = Some(Session {
            status,
            handle,
            key: config.key.clone(),
            stopping_since: None,
        });
        self.last_error = None;
        self.stop_overdue_reported = false;
        Ok(())
    }

    /// 请求停止当前实例。**立即返回**：只置停机标志，真正的收尾在 agent 线程里
    /// 完成，由 [`Self::poll`] 非阻塞观测。没有实例时是无操作。
    pub fn disconnect(&mut self) {
        let Some(session) = self.current.as_mut() else {
            return;
        };
        if session.stopping_since.is_none() {
            session.stopping_since = Some(Instant::now());
            tracing::info!("UI: shutdown requested for the running agent");
        }
        session.status.request_shutdown();
    }

    /// 每帧调用（界面 500ms 重绘一次即可）。做三件事，全部非阻塞：
    /// - 把 agent 上报的连接错误同步给界面（已脱敏）；
    /// - 线程结束时（无论正常停止还是出错）回收句柄、记录结果；
    /// - 停机超过 [`STOP_DEADLINE`] 时告警一次（不阻塞、不强杀）。
    pub fn poll(&mut self) {
        let Some(session) = self.current.as_ref() else {
            return;
        };

        if !session.handle.is_finished() {
            let reported = session.status.last_error();
            let redacted = reported.map(|e| crate::agent::redact_secret(&e, session.key.as_deref()));
            let connected = session.status.phase() == Phase::Connected;
            let overdue = session
                .stopping_since
                .map(|t| t.elapsed() > STOP_DEADLINE)
                .unwrap_or(false);
            match (redacted, connected, overdue) {
                (Some(e), _, _) => self.last_error = Some(e),
                // agent 注册成功后清掉了自己的错误 → 界面也跟着清（旧错误不再
                // 说得通：连接已经建立）。
                (None, true, _) => self.last_error = None,
                (None, false, true) if !self.stop_overdue_reported => {
                    self.stop_overdue_reported = true;
                    self.last_error = Some(format!(
                        "已请求断开，但 agent 超过 {} 秒仍未停止",
                        STOP_DEADLINE.as_secs()
                    ));
                }
                _ => {}
            }
            return;
        }

        self.reap_finished();
    }

    /// 回收已经结束的实例（join 不会阻塞，因为调用方已确认线程结束）。
    fn reap_finished(&mut self) {
        let Some(session) = self.current.take() else {
            return;
        };
        let result = session.handle.join();
        match result {
            Ok(Ok(())) => {
                tracing::info!("UI: agent instance stopped");
            }
            Ok(Err(e)) => {
                let msg = crate::agent::redact_secret(&format!("{e:#}"), session.key.as_deref());
                tracing::warn!(error = %msg, "UI: agent instance failed");
                self.last_error = Some(msg);
            }
            Err(panic) => {
                let detail = panic_text(&*panic);
                let msg = crate::agent::redact_secret(&detail, session.key.as_deref());
                tracing::error!(error = %msg, "UI: agent thread panicked");
                self.last_error = Some(format!("agent 线程异常退出：{msg}"));
            }
        }
    }

    /// 当前实例的状态句柄；没有实例或线程已结束 → `None`（界面据此显示"未连接"）。
    ///
    /// token 刷新仍走既有 [`AgentStatus`] 方法：`status().request_refresh()`。
    pub fn status(&self) -> Option<AgentStatus> {
        self.current
            .as_ref()
            .filter(|s| !s.handle.is_finished())
            .map(|s| s.status.clone())
    }

    /// agent 线程是否还活着（连接中/已连接/正在停止都算）。
    /// 停止后变为 false，此时可以修改配置并再次 [`Self::connect`]。
    pub fn is_running(&self) -> bool {
        self.current
            .as_ref()
            .map(|s| !s.handle.is_finished())
            .unwrap_or(false)
    }

    /// 最近一次可展示的错误（连接失败原因 / 启动失败 / 线程 panic）；
    /// 连接成功后自动清除。**已脱敏**，可直接显示。
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

impl Drop for UiController {
    fn drop(&mut self) {
        let Some(session) = self.current.take() else {
            return;
        };
        // 只停自己这一份实例：request_shutdown 作用于本次连接的共享状态，
        // 不会波及其它进程里的 agent。
        session.status.request_shutdown();
        // 有界等待（进程退出时总得留点时间给 PTY/监听端口的收尾），但绝不
        // 无界阻塞：超时就把线程留在这里，进程退出时随之一并消失。
        let deadline = Instant::now() + Duration::from_secs(3);
        while !session.handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if session.handle.is_finished() {
            let _ = session.handle.join();
        } else {
            tracing::warn!("UI: agent thread still running at shutdown — leaving it to process exit");
        }
    }
}

/// panic payload → 可读文本（与 `lib.rs` 的 `panic_detail` 同策略：`&str` 与
/// `String` 两种 payload 都取到真正的原因）。
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-str payload)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, OnceLock};

    /// 生命周期用例会起真实 relay / 真实 agent 线程，彼此并行会互相干扰
    /// （线程计数、端口、注册表），因此这些用例共用一把串行锁。
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 起一个最小的进程内 relay（只挂 agent 注册 + SSE 两条路由），
    /// 返回它的 base URL。
    async fn spawn_relay() -> String {
        use axum::routing::{get, post};
        let state = Arc::new(crate::relay::SharedState::new(
            String::new(),
            64 * 1024 * 1024,
            None,
            String::new(),
            String::new(),
            None,
        ));
        let app = axum::Router::new()
            .route("/agent/send", post(crate::relay::ws::agent_send_handler))
            .route("/agent/events", get(crate::relay::ws::agent_events_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://127.0.0.1:{port}")
    }

    /// 一个"黑洞"监听：接受 TCP 连接但永不回包，用来复现"注册请求卡住"。
    async fn spawn_black_hole() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream); // 只持有，不写任何响应
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// 一个已关闭的端口（绑定后立即释放）——注册会立刻 connection refused。
    async fn dead_relay() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    fn test_config(relay_url: &str) -> UiConfig {
        UiConfig {
            relay_url: relay_url.to_string(),
            // 测试里不碰桌面（默认 auto 会挂时钟校准等额外任务）。
            desktop_capture: "none".into(),
            shell: "/bin/sh".into(),
            ..Default::default()
        }
    }

    /// 轮询直到条件成立或超时；返回是否成立。
    async fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    /// 本层起的 agent 线程登记表：线程名本身在进程里不可靠地区分（同进程其它
    /// 测试也会起 `sr-agent*` 线程），所以按"存活登记数"精确断言，避免与并行
    /// 跑的其它用例互相干扰。
    ///
    /// 计数只看**本层 spawn 出来的线程**：`live_sessions()` 自增在 spawn 成功
    /// 之后、线程体退出时自减，与 `JoinHandle::is_finished` 无关。
    fn live_sessions() -> usize {
        LIVE_SESSIONS.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn test_ui_controller_releases_lan_port_and_pty_children_on_disconnect() {
        let _g = serial();
        let relay = spawn_relay().await;
        // 先占一个空闲端口号再释放，用它当本次连接的 LAN 端口。
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let mut cfg = test_config(&relay);
        cfg.desktop_lan_port = port;
        let mut c = UiController::new();
        c.connect(cfg).expect("connect");
        assert!(
            wait_until(
                || c.status().map(|s| s.phase()) == Some(Phase::Connected),
                Duration::from_secs(10)
            )
            .await,
            "应注册成功"
        );
        // 注册成功后才起 LAN server 与 PTY；等它们真的出现。
        assert!(
            wait_until(|| port_in_use(port), Duration::from_secs(10)).await,
            "配置了 LAN 端口就必须真的监听"
        );
        let children_before = child_pids();
        assert!(
            wait_until(|| !child_pids().is_empty(), Duration::from_secs(10)).await,
            "已连接的会话应持有 PTY 子进程"
        );

        c.disconnect();
        assert!(
            wait_until(
                || {
                    c.poll();
                    !c.is_running()
                },
                STOP_DEADLINE
            )
            .await
        );
        // 端口必须释放（否则下次连接改配置重连会 bind 失败）。
        assert!(
            wait_until(|| !port_in_use(port), Duration::from_secs(5)).await,
            "断开后 LAN 监听端口必须释放"
        );
        // PTY 子进程必须被回收（Shell::drop 会 kill+wait）。
        let leaked: Vec<u32> = child_pids()
            .into_iter()
            .filter(|pid| !children_before.contains(pid))
            .collect();
        assert!(
            leaked.is_empty(),
            "断开后不得遗留 PTY 子进程: {leaked:?}"
        );
    }

    /// 端口是否被本进程监听（连上去能成功即视为在监听）。
    fn port_in_use(port: u16) -> bool {
        std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(200),
        )
        .is_ok()
    }

    /// 本进程当前的直接子进程 pid（Linux：扫 /proc/<pid>/stat 的 ppid）。
    #[cfg(target_os = "linux")]
    fn child_pids() -> Vec<u32> {
        let me = std::process::id();
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/proc") {
            for e in rd.flatten() {
                let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
                    continue;
                };
                // 格式：pid (comm) state ppid …；comm 里可能含空格与括号，
                // 所以从最后一个 ')' 之后开始切。
                let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
                    continue;
                };
                let ppid = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok());
                if ppid == Some(me) {
                    out.push(pid);
                }
            }
        }
        out
    }
    #[cfg(not(target_os = "linux"))]
    fn child_pids() -> Vec<u32> {
        Vec::new()
    }

    #[tokio::test]
    async fn test_ui_controller_new_has_no_side_effects() {
        let mut c = UiController::new();
        assert!(!c.is_running(), "新建的控制器不得自动连接");
        assert!(c.status().is_none());
        c.poll(); // 空转不 panic
        c.disconnect(); // 没实例时是无操作
        assert!(c.last_error().is_none());
    }

    #[tokio::test]
    async fn test_ui_controller_validates_before_starting_anything() {
        let _g = serial();
        let mut c = UiController::new();
        let baseline = live_sessions();

        let mut bad = test_config(&dead_relay().await);
        bad.relay_url = String::new();
        let err = c.connect(bad).err().expect("空 relay 地址必须被拦下");
        assert!(err.to_string().contains("Relay"), "{err}");
        assert!(!c.is_running(), "校验失败不得起线程");
        assert_eq!(live_sessions(), baseline, "校验失败不得起线程");

        let mut bad_fps = test_config(&dead_relay().await);
        bad_fps.desktop_fps = 999.0;
        assert!(c.connect(bad_fps).is_err());
        assert!(!c.is_running());
        assert_eq!(live_sessions(), baseline, "校验失败不得留下线程");
    }

    #[tokio::test]
    async fn test_ui_controller_connects_then_disconnects_within_deadline() {
        let _g = serial();
        let relay = spawn_relay().await;
        let mut c = UiController::new();
        c.connect(test_config(&relay)).expect("connect");
        assert!(c.is_running(), "connect 之后线程必须活着");
        assert!(
            wait_until(
                || c.status().map(|s| s.phase()) == Some(Phase::Connected),
                Duration::from_secs(10)
            )
            .await,
            "10 秒内应注册成功并进入已连接"
        );
        let st = c.status().expect("status while running");
        assert_eq!(st.relay_url(), relay, "状态里的 relay 地址应是本次配置");
        assert!(!st.session_id().is_empty());
        assert!(!st.tokens().is_empty(), "注册成功应拿到 token");

        let t0 = Instant::now();
        c.disconnect();
        assert!(
            wait_until(
                || {
                    c.poll();
                    !c.is_running()
                },
                STOP_DEADLINE
            )
            .await,
            "断开必须在 {} 秒内收敛，实际 {:?}",
            STOP_DEADLINE.as_secs(),
            t0.elapsed()
        );
        c.poll();
        assert!(c.status().is_none(), "停止后不再有活动状态句柄");
        assert!(
            wait_until(|| live_sessions() == 0, Duration::from_secs(5)).await,
            "不得遗留 agent 线程"
        );
        // 正常停止不是错误（用户主动断开）。
        assert!(c.last_error().is_none(), "正常断开不应产生错误: {:?}", c.last_error());
    }

    #[tokio::test]
    async fn test_ui_controller_disconnect_during_retry_backoff_converges() {
        let _g = serial();
        // relay 端口是关的：agent 会一路退避重试（1s/2s/4s…），停机信号必须
        // 在退避等待里也能被看到，而不是等退避睡醒。
        let mut c = UiController::new();
        c.connect(test_config(&dead_relay().await)).expect("connect");
        assert!(
            wait_until(
                || c.status().map(|s| s.phase() != Phase::Connected) == Some(true),
                Duration::from_secs(3)
            )
            .await,
            "离线 relay 不应进入已连接"
        );
        // 让第一次注册失败、进入退避（此时 agent 正睡在 backoff 里）。
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let t0 = Instant::now();
        c.disconnect();
        assert!(
            wait_until(
                || {
                    c.poll();
                    !c.is_running()
                },
                STOP_DEADLINE
            )
            .await,
            "退避等待中请求断开也必须在 {} 秒内收敛，实际 {:?}",
            STOP_DEADLINE.as_secs(),
            t0.elapsed()
        );
        assert!(
            wait_until(|| live_sessions() == 0, Duration::from_secs(5)).await,
            "不得遗留 agent 线程"
        );
        c.poll();
        // 离线期间 agent 上报过连接错误 → 界面能显示原因。
        let err = c.last_error().expect("离线连接失败应留下可见错误");
        assert!(!err.trim().is_empty());
    }

    #[tokio::test]
    async fn test_ui_controller_disconnect_while_registration_hangs_converges() {
        let _g = serial();
        // 黑洞 relay：注册 POST 永远不回包。断开必须能取消在途请求。
        let mut c = UiController::new();
        c.connect(test_config(&spawn_black_hole().await))
            .expect("connect");
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(c.is_running(), "注册卡住时线程仍在（表现为连接中）");
        let t0 = Instant::now();
        c.disconnect();
        assert!(
            wait_until(
                || {
                    c.poll();
                    !c.is_running()
                },
                STOP_DEADLINE
            )
            .await,
            "注册挂起时断开也必须在 {} 秒内收敛，实际 {:?}",
            STOP_DEADLINE.as_secs(),
            t0.elapsed()
        );
        assert!(
            wait_until(|| live_sessions() == 0, Duration::from_secs(5)).await,
            "不得遗留 agent 线程"
        );
    }

    #[tokio::test]
    async fn test_ui_controller_second_connect_is_rejected_then_reconnect_works() {
        let _g = serial();
        let relay = spawn_relay().await;
        let mut c = UiController::new();
        c.connect(test_config(&relay)).expect("first connect");
        // 线程名由新线程自己在启动时设置，等它真的被记上再取基线。
        assert!(
            wait_until(|| live_sessions() >= 1, Duration::from_secs(5)).await,
            "connect 之后应能观测到 agent 线程"
        );
        let before = live_sessions();

        // 连续点击：第二次必须被拒，且不产生第二个 agent 线程。
        let err = c
            .connect(test_config(&relay))
            .err()
            .expect("重复 connect 必须被拒");
        assert!(err.to_string().contains("已有连接"), "{err}");
        assert_eq!(live_sessions(), before, "重复点击不得起第二个实例");

        // 断开 → 等停止 → 用改过的配置再连。
        c.disconnect();
        assert!(
            wait_until(
                || {
                    c.poll();
                    !c.is_running()
                },
                STOP_DEADLINE
            )
            .await
        );

        let mut second = test_config(&relay);
        second.session_id = Some("uisess01".into());
        c.connect(second).expect("reconnect after stop");
        assert!(
            wait_until(
                || c.status().map(|s| s.phase()) == Some(Phase::Connected),
                Duration::from_secs(10)
            )
            .await,
            "断开后改配置必须能再次连上"
        );
        assert_eq!(
            c.status().map(|s| s.session_id()),
            Some("uisess01".to_string()),
            "第二次连接必须使用新配置（变更生效）"
        );
        c.disconnect();
        assert!(wait_until(
            || {
                c.poll();
                !c.is_running()
            },
            STOP_DEADLINE
        )
        .await);
    }

    #[tokio::test]
    async fn test_ui_controller_config_snapshot_is_isolated_from_edit_draft() {
        let _g = serial();
        let relay = spawn_relay().await;
        let mut draft = test_config(&relay);
        draft.session_id = Some("snapshot".into());

        let mut c = UiController::new();
        c.connect(draft.clone()).expect("connect");
        // 界面继续编辑草稿：不能影响正在跑的实例。
        draft.relay_url = "http://127.0.0.1:1".into();
        draft.session_id = Some("changedid".into());
        draft.desktop_fps = 5.0;

        assert!(
            wait_until(
                || c.status().map(|s| s.phase()) == Some(Phase::Connected),
                Duration::from_secs(10)
            )
            .await
        );
        let st = c.status().expect("status");
        assert_eq!(st.relay_url(), relay, "运行实例用的是连接时的快照");
        assert_eq!(st.session_id(), "snapshot", "草稿改动不得影响运行实例");

        c.disconnect();
        assert!(wait_until(
            || {
                c.poll();
                !c.is_running()
            },
            STOP_DEADLINE
        )
        .await);
    }

    #[tokio::test]
    async fn test_ui_controller_errors_never_contain_the_key() {
        let _g = serial();
        let secret = "UI-TEST-SECRET-f0f0";
        let mut c = UiController::new();
        let mut cfg = test_config(&dead_relay().await);
        cfg.key = Some(secret.into());
        c.connect(cfg).expect("connect");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        c.poll();
        let err = c.last_error().expect("离线应有可见错误");
        assert!(!err.contains(secret), "错误文本泄露了密钥: {err}");
        assert!(!format!("{c:?}").contains(secret));
        c.disconnect();
        assert!(wait_until(
            || {
                c.poll();
                !c.is_running()
            },
            STOP_DEADLINE
        )
        .await);
    }

    #[tokio::test]
    async fn test_ui_controller_drop_requests_its_own_instance_stop() {
        let _g = serial();
        let relay = spawn_relay().await;
        {
            let mut c = UiController::new();
            c.connect(test_config(&relay)).expect("connect");
            assert!(wait_until(
                || c.status().map(|s| s.phase()) == Some(Phase::Connected),
                Duration::from_secs(10)
            )
            .await);
            // 不显式 disconnect，直接 drop：drop 也必须请求停止。
        }
        assert!(
            wait_until(|| live_sessions() == 0, STOP_DEADLINE).await,
            "drop 之后不得遗留 agent 线程"
        );
    }

    #[tokio::test]
    async fn test_ui_controller_status_is_none_after_stop_so_gui_can_show_disconnected() {
        let _g = serial();
        let relay = spawn_relay().await;
        let mut c = UiController::new();
        c.connect(test_config(&relay)).expect("connect");
        assert!(wait_until(
            || c.status().map(|s| s.phase()) == Some(Phase::Connected),
            Duration::from_secs(10)
        )
        .await);
        c.disconnect();
        assert!(wait_until(
            || {
                c.poll();
                !c.is_running()
            },
            STOP_DEADLINE
        )
        .await);
        assert!(c.status().is_none(), "停止后 status() 必须是 None");
        // 再 poll 多少次都不该复活或 panic。
        for _ in 0..5 {
            c.poll();
        }
        assert!(!c.is_running());
        assert!(c.status().is_none());
    }
}
