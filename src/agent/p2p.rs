//! Agent 侧 WebRTC P2P 信令处理（Task 2，agent ↔ relay ↔ 浏览器）。
//!
//! 生命周期（浏览器是 offerer，agent 是 answerer，方向见 task-2-context）：
//! - `desktop:p2p-offer {sdp, candidates[]}` → 建 UDP socket、应答、spawn 驱动任务；
//! - `desktop:p2p-candidate {candidate}` → 喂共享 peer（trickle）；
//! - 建连 → 打日志 + 广播 `desktop:p2p-state {state:"connected"}` + 开启 fMP4
//!   下行投递口（Task 3）：`desktop:video` 的字节镜像进 DataChannel（有界
//!   丢旧保新队列，见 [`MirroredQueue`]；final-review #1 防死链 OOM）；
//! - 15s 握手超时 / `Failed` / **死链看门狗**（建连后 30s 无成功 DataChannel
//!   写入即判定 peer 死亡）→ 广播 `desktop:p2p-state {state:"failed"}` + 清理；
//! - `desktop:stop` 或二次 offer（last-wins 覆盖）→ abort 驱动任务、置 None。
//!
//! 键鼠上行仍走 relay（Task 6 评估是否切 DataChannel）。

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::agent::desktop::webrtc::{
    begin_answer, WebrtcError, UdpTransport, WebRtcPeer, WebRtcState,
};
use crate::proto::Message;

/// 一次 P2P 会话：共享 peer + 驱动任务句柄。`id` 用于驱动任务退出时确认
/// "自己仍是当前会话"再清位，避免覆盖（last-wins）后旧任务误清新会话。
pub(crate) struct P2pSession {
    id: u64,
    peer: Arc<Mutex<WebRtcPeer>>,
    driver: Option<tokio::task::JoinHandle<()>>,
    started: Instant,
}

/// 当前 P2P 会话状态（消息循环与驱动任务共享）。
///
/// **单浏览器限制（final-review #2，文档化不改架构）**：agent 同一时刻只持有一个
/// P2P 会话（last-wins，二次 offer 覆盖旧会话），而 relay 会把 p2p-answer/
/// p2p-candidate/p2p-state 广播给**所有**已连接浏览器。并发多浏览器时，非 last 的
/// 浏览器会拿到别人的 answer → `setRemoteDescription` 拒绝 → `.catch →
/// _onP2pFailed()` → 回退 relay 拉流。功能上不黑屏（relay 保底），但只有最新
/// 一个浏览器走 P2P。每浏览器独立 P2P 会话不在本版本范围（大改动，见
/// feature-plan 的范围收口）；发布说明/报告已如实陈述该限制。
#[derive(Clone)]
pub(crate) struct P2pState {
    inner: Arc<Mutex<Option<P2pSession>>>,
    next_id: Arc<AtomicU64>,
    /// 已建连后 agent 侧 fMP4 下行投递口（Task 3）：None = 未连接/未建。
    /// 驱动任务在 Connected 时设为 `Some(MirroredQueue)`，post_fn 把
    /// `desktop:video` 的 fMP4 字节（base64 解码后）镜像投递进来，消费循环
    /// 逐箱写入 DataChannel（relay 路径保底不中断）。
    ///
    /// **有界 + 丢旧保新（final-review #1）**：原 `UnboundedSender` 在死链
    /// （浏览器关页/断网，peer 永不再写入）时会被 30-60fps 的生产者无限积压
    /// → 长活 agent OOM。现为固定容量队列（`MIRROR_QUEUE_CAP` 帧），满丢最旧，
    /// 内存恒有界；消费循环的 2s 写入重试也在死链时被驱动循环的看门狗（
    /// `DEAD_PEER_WATCHDOG`）兜底拆链。
    pub(crate) video_tx:
        Arc<std::sync::RwLock<Option<MirroredQueue>>>,
    /// 最近一次 `desktop:video {kind:"init"}` 的原始字节（ftyp+moov）。P2P 建连
    /// 通常发生在 init 已发出之后——浏览器只拿到 frag 无法解码（init 仅在编码器
    /// 重建/首个关键帧时重出）。驱动任务 Connected 时把这个缓存补推到投递口，
    /// 新接入的浏览器立即拿到参数集。
    pub(crate) last_init: Arc<std::sync::RwLock<Option<Vec<u8>>>>,
}

impl Default for P2pState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            next_id: Arc::new(AtomicU64::new(0)),
            video_tx: Arc::new(std::sync::RwLock::new(None)),
            last_init: Arc::new(std::sync::RwLock::new(None)),
        }
    }
}

/// 握手超时：未连上视为 failed（浏览器回退 relay 路径，Task 3/6）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// 驱动循环 sleep 下限（str0m 事件推进的最小粒度）。
const MIN_DRIVE_TICK: Duration = Duration::from_millis(5);
/// 死链看门狗（final-review #1b）：已建连后这么长时间没有任何成功的 DataChannel
/// 写入，即判定 peer 死亡并整链拆掉（清 `video_tx`、abort 驱动、广播 failed）。
///
/// 阈值取 30s 的根据：桌面编码循环静止 4s（`KF_QUIET_MS`）/ 活跃 6s
/// （`KF_ACTIVE_MS`）必产一个 IDR 心跳帧 → 健康链上 30s 内必有成功写入。
/// 30s 零写入 ≈ 对端已死/桌面已停，拆链安全；"静止画面无数据"的误杀不存在
/// （静止也有 4s 心跳）。此看门狗兜底 ICE 未进入 Disconnected 的死亡场景
/// （浏览器关页不发 desktop:stop、网络硬断等）。
const DEAD_PEER_WATCHDOG: Duration = Duration::from_secs(30);
/// 镜像队列容量（帧）（final-review #1c）：固定上限封顶死链下的内存积压
/// （满丢最旧，fMP4 丢旧保新）。与 relay fan-out 的 viewer 缓冲、LAN feed
/// 的 64 帧同一数量级。
const MIRROR_QUEUE_CAP: usize = 64;

/// 有界 fMP4 镜像队列（丢旧保新），替代原 `UnboundedSender<Vec<u8>>`。
///
/// 死链时（浏览器关页/断网，peer 永不写入）消费循环被 SCTP 背压拖慢到 ~1 条/2s，
/// 而 post_fn 是 30-60fps 生产者——无界队列会在长活 agent 上无限积压 → OOM
/// （final-review #1）。本队列固定 `cap` 帧上限：满时丢最旧保最新，内存恒有界。
///
/// - [`push`](Self::push) 是同步非阻塞口（post_fn 是同步闭包，不能 await）；
/// - [`pop`](Self::pop) 是异步口（消费循环 `drain_video_to_channel`）；
/// - [`close`](Self::close) 置关闭标志并唤醒消费循环——等价既有"所有 sender
///   drop 后 recv 返回 None"的语义，让 drain 循环自然退出。
///
/// 并发安全：`push`/`pop`/`close` 互斥保护 `VecDeque`，`Notify` 消除"空队等待"
/// 的忙轮询；锁不跨 await（`pop` 在拿到帧/判空后立即释放再返回/等待）。
#[derive(Clone)]
pub(crate) struct MirroredQueue {
    inner: Arc<QueueInner>,
    cap: usize,
}

struct QueueInner {
    q: std::sync::Mutex<Option<std::collections::VecDeque<Vec<u8>>>>,
    notify: tokio::sync::Notify,
}

impl MirroredQueue {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(QueueInner {
                q: std::sync::Mutex::new(Some(std::collections::VecDeque::with_capacity(cap))),
                notify: tokio::sync::Notify::new(),
            }),
            cap,
        }
    }

    /// 同步入队（post_fn 调用）：满则丢最旧；已 close 则静默丢弃。
    pub(crate) fn push(&self, bytes: Vec<u8>) {
        let mut g = self.inner.q.lock().unwrap();
        if let Some(q) = g.as_mut() {
            if q.len() >= self.cap {
                q.pop_front(); // 丢旧保新
            }
            q.push_back(bytes);
        }
        drop(g);
        self.inner.notify.notify_one();
    }

    /// 异步出队（消费循环）：队空挂起；close 且队已空返回 None（循环退出）。
    pub(crate) async fn pop(&self) -> Option<Vec<u8>> {
        loop {
            {
                let mut g = self.inner.q.lock().unwrap();
                match g.as_mut() {
                    Some(q) => {
                        if let Some(b) = q.pop_front() {
                            return Some(b);
                        }
                    }
                    None => return None,
                }
            }
            self.inner.notify.notified().await;
        }
    }

    /// 关闭：清空缓冲并唤醒消费循环（拆链后的剩余帧无意义，直接丢）。
    pub(crate) fn close(&self) {
        let mut g = self.inner.q.lock().unwrap();
        *g = None;
        drop(g);
        self.inner.notify.notify_one();
    }

    /// 当前缓冲帧数（测试观测用）。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.q.lock().unwrap().as_ref().map(|q| q.len()).unwrap_or(0)
    }

    /// 缓冲内容快照（测试断言"丢旧保新"用）。
    #[cfg(test)]
    fn snapshot(&self) -> Vec<Vec<u8>> {
        self.inner
            .q
            .lock()
            .unwrap()
            .as_ref()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// 从 `video_tx` 槽位取出队列并 close（若存在）：让消费循环退出、内存尽快回收。
/// 拆链（abort / 驱动退出 / 看门狗）统一走这里——原代码只 `= None`（drop sender），
/// 换成有界队列后队列可能被 drain 的 clone 持有，必须显式 close 才能唤醒它。
fn close_video_tx(video_tx: &Arc<std::sync::RwLock<Option<MirroredQueue>>>) {
    let q = video_tx.write().unwrap().take();
    if let Some(q) = q {
        q.close();
    }
}

/// 收到 `desktop:p2p-offer`：覆盖旧会话 → 建 UDP socket → 应答 →
/// 回 `desktop:p2p-answer` → spawn 驱动任务。调用点：agent 消息循环。
pub(crate) async fn handle_offer(
    state: &P2pState,
    sid: String,
    ctrl: tokio::sync::mpsc::Sender<String>,
    offer_sdp: &str,
    offer_candidates: &[String],
) {
    // last-wins：接管前先中止旧驱动并清位（二次 offer 覆盖语义）。
    abort(state).await;

    // 建 UDP socket：绑定 0.0.0.0:0 拿实际端口；host candidate 的 IP 用
    // 对外路由 IP（同机回测回退 127.0.0.1）。`is` crate 的 ICE 用
    // Receive.destination 匹配本地候选 → local_addr 必须是"广播 IP + 端口"。
    let transport = match UdpTransport::bind("0.0.0.0:0") {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("desktop:p2p-offer accepted but UDP bind failed: {e}");
            send_state(&ctrl, &sid, "failed").await;
            return;
        }
    };
    let port = transport.local_port();
    let advertised = SocketAddr::new(IpAddr::V4(pick_advertised_ip()), port);

    let (peer, answer) = match begin_answer(advertised, Box::new(transport), offer_sdp, offer_candidates)
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("desktop:p2p-offer processing failed: {e}");
            send_state(&ctrl, &sid, "failed").await;
            return;
        }
    };

    let resp = Message {
        msg_type: "desktop:p2p-answer".to_string(),
        session_id: sid.clone(),
        payload: serde_json::json!({ "sdp": answer.sdp, "candidates": answer.candidates }),
    };
    if let Ok(s) = serde_json::to_string(&resp) {
        let _ = ctrl.send(s).await;
    }
    tracing::info!(
        sdp_bytes = answer.sdp.len(),
        cands = answer.candidates.len(),
        "desktop p2p offer answered"
    );

    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let peer = Arc::new(Mutex::new(peer));
    let driver = spawn_driver(state, sid, ctrl, id, peer.clone());
    *state.inner.lock().await = Some(P2pSession {
        id,
        peer,
        driver: Some(driver),
        started: Instant::now(),
    });
}

/// 收到 `desktop:p2p-candidate {candidate}`：喂共享 peer（无会话/坏候选忽略）。
pub(crate) async fn handle_candidate(state: &P2pState, candidate: &str) {
    let mut guard = state.inner.lock().await;
    let Some(session) = guard.as_ref() else {
        tracing::debug!("desktop p2p candidate ignored — no active session");
        return;
    };
    match str0m::Candidate::from_sdp_string(candidate) {
        Ok(cand) => {
            session.peer.lock().await.add_remote_candidate(cand);
            tracing::trace!("desktop p2p candidate applied");
        }
        Err(e) => tracing::warn!("desktop:p2p-candidate rejected ({e}): {candidate}"),
    }
}

/// `desktop:stop` / 会话结束：中止驱动任务并清理会话（含 fMP4 投递口）。
pub(crate) async fn shutdown(state: &P2pState) {
    abort(state).await;
}

/// abort 当前会话的驱动任务并置 None（同时关 fMP4 投递口：close 队列唤醒消费
/// 循环退出；驱动任务被 abort 时不会跑到自己的清理代码，这里兜底）。
async fn abort(state: &P2pState) {
    let mut guard = state.inner.lock().await;
    if let Some(s) = guard.take() {
        if let Some(d) = s.driver {
            d.abort();
        }
        tracing::debug!("desktop p2p session torn down");
    }
    drop(guard);
    close_video_tx(&state.video_tx);
}

/// spawn P2P 驱动任务：循环 drive → 状态机 → 广播。Until Connected：15s
/// 超时判 failed（清理）；Connected：开启 fMP4 镜像投递口（Task 3）并保持
/// 运行推进 SCTP/ICE keepalive，同时挂**死链看门狗**（final-review #1b）：
/// 建连后 30s 无成功 DataChannel 写入 → 判定 peer 死亡，整链拆掉。
fn spawn_driver(
    state: &P2pState,
    sid: String,
    ctrl: tokio::sync::mpsc::Sender<String>,
    id: u64,
    peer: Arc<Mutex<WebRtcPeer>>,
) -> tokio::task::JoinHandle<()> {
    let slot = state.inner.clone();
    let video_tx = state.video_tx.clone();
    let last_init = state.last_init.clone();
    tokio::spawn(async move {
        run_driver(
            slot,
            video_tx,
            last_init,
            &sid,
            &ctrl,
            id,
            peer,
            HANDSHAKE_TIMEOUT,
            DEAD_PEER_WATCHDOG,
        )
        .await;
    })
}

/// 驱动循环主体（`spawn_driver` 的 tokio::spawn 内执行；抽成独立 async fn 以便
/// 测试注入内存传输 peer + 缩短看门狗窗口直测拆链行为）。
///
/// `handshake_timeout` > 0 时为建连前握手超时；`dead_peer_watchdog` 为建连后
/// 死链看门狗窗口（生产分别用 [`HANDSHAKE_TIMEOUT`] / [`DEAD_PEER_WATCHDOG`]，
/// 测试可传更短值以在单测内验证）。
#[allow(clippy::too_many_arguments)]
async fn run_driver(
    slot: Arc<Mutex<Option<P2pSession>>>,
    video_tx: Arc<std::sync::RwLock<Option<MirroredQueue>>>,
    last_init: Arc<std::sync::RwLock<Option<Vec<u8>>>>,
    sid: &str,
    ctrl: &tokio::sync::mpsc::Sender<String>,
    id: u64,
    peer: Arc<Mutex<WebRtcPeer>>,
    handshake_timeout: Duration,
    dead_peer_watchdog: Duration,
) {
    let deadline = tokio::time::Instant::now() + handshake_timeout;
    let mut announced_connected = false;
    // 最近一次成功 DataChannel 写入的时刻：drain 循环/init 直达任务在 send Ok
    // 时刷新，这里检查超时判死链。
    let last_write_ok: Arc<std::sync::Mutex<std::time::Instant>> =
        Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    loop {
        let now = Instant::now();
        let next = {
            let mut guard = peer.lock().await;
            guard.drive(now)
        };
        let st = { peer.lock().await.state() };
        match st {
            WebRtcState::Connected => {
                if !announced_connected {
                    announced_connected = true;
                    tracing::info!("desktop p2p connected");
                    // 开启 fMP4 下行投递口：post_fn（relay 镜像）经有界丢旧
                    // 保新队列把 desktop:video 字节投进来，独立消费循环写
                    // DataChannel。置 None（close）时消费循环退出。
                    let q = MirroredQueue::new(MIRROR_QUEUE_CAP);
                    *video_tx.write().unwrap() = Some(q.clone());
                    // 参数集必达：独立任务直接写 peer（不走会被 frag 积压的
                    // 通道），路径（SCTP 通道 open / cwnd）未就绪时小步重试，
                    // 直到成功或会话结束。浏览器通常在 init 发出后才建连，
                    // 缺参数集则后续 frag 全被 demux 丢弃、画面永不出现。
                    tokio::spawn(init_datachannel_delivery(
                        peer.clone(),
                        slot.clone(),
                        id,
                        last_init.clone(),
                        last_write_ok.clone(),
                    ));
                    tokio::spawn(drain_video_to_channel(q, peer.clone(), last_write_ok.clone()));
                    send_state(ctrl, sid, "connected").await;
                } else {
                    // 死链看门狗：push 心跳保证健康链 ≤6s 必有成功写入，这里
                    // 只用"最后一次成功写入"的年龄判定，天然区分"活但暂时
                    // 静态"与"死了"。
                    let dead_for = last_write_ok.lock().unwrap().elapsed();
                    if dead_for > dead_peer_watchdog {
                        tracing::warn!(
                            dead_ms = dead_for.as_millis(),
                            "desktop p2p dead, tearing down"
                        );
                        send_state(ctrl, sid, "failed").await;
                        close_video_tx(&video_tx);
                        break;
                    }
                }
                // 已建连：无握手超时，持续驱动推进 SCTP/ICE keepalive。
            }
            WebRtcState::Failed => {
                tracing::warn!("desktop p2p failed");
                send_state(ctrl, sid, "failed").await;
                break;
            }
            WebRtcState::Connecting => {
                if !announced_connected && tokio::time::Instant::now() >= deadline {
                    tracing::warn!(
                        timeout_ms = handshake_timeout.as_millis(),
                        "desktop p2p handshake timeout, tearing down"
                    );
                    send_state(ctrl, sid, "failed").await;
                    break;
                }
            }
        }
        let sleep = next
            .map(|w| w.saturating_duration_since(Instant::now()))
            .unwrap_or(MIN_DRIVE_TICK)
            .clamp(MIN_DRIVE_TICK, Duration::from_secs(1));
        tokio::time::sleep(sleep).await;
    }
    // 退出时清投递口（close 队列 → 消费循环退出）；仅当自己仍是当前会话
    // 才清位与投递口（防覆盖后旧任务误清新会话的状态）。
    let mut guard = slot.lock().await;
    if let Some(s) = guard.as_ref() {
        if s.id == id {
            *guard = None;
            drop(guard);
            close_video_tx(&video_tx);
        }
    }
}

/// 保证参数集（init=ftyp+moov）必达 DataChannel 的独立任务。
///
/// 不走 frag 通道：通道会被积压的 frag 占满，排后的 init 会被饿死。这里直接
/// 锁 peer 写（与消费循环串行，无并发问题）；路径未就绪（通道未 open /
/// SCTP 背压）时小步重试。退出条件：① 写入成功；② 会话被覆盖或回收
/// （`slot` 中已不是本会话 id）；③ 非路径类错误。成功写入同样刷新看门狗
/// `last_write_ok`（init 也是健康链的写入证据）。
async fn init_datachannel_delivery(
    peer: Arc<Mutex<WebRtcPeer>>,
    slot: Arc<Mutex<Option<P2pSession>>>,
    id: u64,
    last_init: Arc<std::sync::RwLock<Option<Vec<u8>>>>,
    last_write_ok: Arc<std::sync::Mutex<std::time::Instant>>,
) {
    loop {
        {
            let guard = slot.lock().await;
            if guard.as_ref().map(|s| s.id != id).unwrap_or(true) {
                return; // 会话已被覆盖/回收（abort 驱动的会话槽清位路径）。
            }
        }
        let init = match last_init.read().ok().and_then(|g| g.clone()) {
            Some(i) => i,
            None => {
                // 编码循环还没产出 init（首发中）：等它缓存后再投。
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let result = {
            let mut guard = peer.lock().await;
            guard.send(&init)
        };
        match result {
            Ok(()) => {
                *last_write_ok.lock().unwrap() = std::time::Instant::now();
                tracing::debug!(init_bytes = init.len(), "p2p init delivered to datachannel");
                break;
            }
            Err(WebrtcError::ChannelNotOpen) | Err(WebrtcError::Backpressure) => {
                // 通道/window 未就绪：小步重试（覆盖 cwnd slow-start 起跳）。
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => {
                tracing::trace!("p2p init direct send failed: {e}");
                break;
            }
        }
    }
}

/// 消费 fMP4 投递口的字节并写入 DataChannel。队列 close（video_tx 置 None）
/// 后 `pop` 返回 None，本循环自然退出。
///
/// 有界重试：SCTP 初始拥塞窗口小、通道可能未 open，`send` 会临时返回
/// ChannelNotOpen / Backpressure——同一字节重试到路径可用（上限 ~2s，
/// 对齐 str0m SCTP cwnd 起跳），超过即丢（fMP4 丢旧保新，relay 路径保底）。
/// init 的必达由 [`init_datachannel_delivery`] 独立保证，本循环只处理实时
/// frag（窗口让路时保住提升吞吐，窗口持续关闭时按旧丢新）。
///
/// 每次成功写入刷新 `last_write_ok`（驱动循环死链看门狗的依据；final-review
/// #1b）。死链上写不进去，该时间戳持续陈旧 → 看门狗在 30s 内拆链。
async fn drain_video_to_channel(
    q: MirroredQueue,
    peer: Arc<Mutex<WebRtcPeer>>,
    last_write_ok: Arc<std::sync::Mutex<std::time::Instant>>,
) {
    // ~2s：覆盖 SCTP 通道 open + cwnd 起跳（初始 cwnd≈4×MTU，需 SACK 驱动
    // 增长；LAN 上首个 ACK 通常在 <100ms，但端到端链路可能更慢）。超过即丢
    // （fMP4 丢旧保新，relay 路径保底）。
    const MAX_RETRY_BUDGET_MS: u64 = 2000;
    const RETRY_INTERVAL: Duration = Duration::from_millis(5);
    while let Some(bytes) = q.pop().await {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(MAX_RETRY_BUDGET_MS);
        let mut sent = false;
        loop {
            let result = {
                let mut guard = peer.lock().await;
                guard.send(&bytes)
            };
            match result {
                Ok(()) => {
                    *last_write_ok.lock().unwrap() = std::time::Instant::now();
                    sent = true;
                    break;
                }
                Err(WebrtcError::ChannelNotOpen) | Err(WebrtcError::Backpressure) => {
                    if tokio::time::Instant::now() >= deadline {
                        tracing::trace!("p2p send dropped (write path not ready)");
                        break;
                    }
                    tokio::time::sleep(RETRY_INTERVAL).await;
                }
                Err(e) => {
                    tracing::trace!("p2p send dropped: {e}");
                    break;
                }
            }
        }
        if sent {
            tracing::trace!(bytes = bytes.len(), "p2p datachannel sent");
        }
    }
}

/// 广播 `desktop:p2p-state {state}`（走 control 通道 → relay broadcast）。
async fn send_state(ctrl: &tokio::sync::mpsc::Sender<String>, sid: &str, state: &str) {
    let msg = Message {
        msg_type: "desktop:p2p-state".to_string(),
        session_id: sid.to_string(),
        payload: serde_json::json!({ "state": state }),
    };
    if let Ok(s) = serde_json::to_string(&msg) {
        let _ = ctrl.send(s).await;
    }
}

/// 选出对外广播的 host candidate IP：UDP connect 到公共 IP 让内核挑源地址
/// （纯路由选择，不发包），失败回退 `127.0.0.1`（同机回测/无外网）。
/// `pub(crate)`：同时被 LAN 直连（agent/lan.rs，阶段2）用作广播地址。
pub(crate) fn pick_advertised_ip() -> std::net::Ipv4Addr {
    if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if s.connect(SocketAddr::from(([8, 8, 8, 8], 80))).is_ok() {
            if let Ok(local) = s.local_addr() {
                if let std::net::IpAddr::V4(ip) = local.ip() {
                    return ip;
                }
            }
        }
    }
    std::net::Ipv4Addr::LOCALHOST
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::desktop::webrtc::WebRtcPeer;
    use tokio::sync::mpsc;

    /// 浏览器式 offerer 的真实 SDP offer（str0m `make_offer`，NoopTransport
    /// 足够：offer 生成不外发）。对应 Task 3 `pc.createOffer()` 的产物。
    fn browser_offer() -> String {
        let addr: SocketAddr = "147.147.147.1:57001".parse().unwrap();
        let mut p = WebRtcPeer::new(addr);
        p.make_offer().unwrap()
    }

    /// 模拟消息循环：`desktop:p2p-offer` payload → `handle_offer` →
    /// ctrl 通道首条消息必须是 `desktop:p2p-answer`（含 sdp/candidates）。
    #[tokio::test]
    async fn p2p_offer_message_produces_answer_on_control_channel() {
        let offer = browser_offer();
        let payload = serde_json::json!({
            "sdp": offer,
            "candidates": [],
        });
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<String>(8);
        let state = P2pState::default();
        handle_offer(
            &state,
            "s-p2p-test".to_string(),
            ctrl_tx,
            payload["sdp"].as_str().unwrap(),
            &[],
        )
        .await;

        let got = tokio::time::timeout(Duration::from_secs(2), ctrl_rx.recv())
            .await
            .expect("agent must reply desktop:p2p-answer")
            .expect("channel open");
        let parsed: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(parsed["type"], "desktop:p2p-answer");
        let body = &parsed["payload"];
        assert!(
            !body["sdp"].as_str().unwrap_or("").is_empty(),
            "answer sdp must not be empty: {got}"
        );
        assert_eq!(
            body["candidates"].as_array().map(|a| a.len()).unwrap_or(0),
            1,
            "answer must advertise the agent host candidate: {got}"
        );
    }

    /// 会话上下文：先 offer 建会话，再回落 candidate（无效候选忽略不 panic）。
    #[tokio::test]
    async fn p2p_candidate_after_offer_is_applied_or_ignored() {
        let offer = browser_offer();
        let (ctrl_tx, mut _ctrl_rx) = mpsc::channel::<String>(8);
        let state = P2pState::default();
        handle_offer(
            &state,
            "s-p2p-test".to_string(),
            ctrl_tx,
            &offer,
            &[],
        )
        .await;
        assert!(
            state.inner.lock().await.is_some(),
            "offer must leave an active session"
        );

        // 无效候选：被拒绝并告警，不 panic。
        handle_candidate(&state, "garbage-candidate").await;
        // 无会话时口袋被忽略。
        let state2 = P2pState::default();
        handle_candidate(&state2, "candidate:1 1 udp").await;
    }

    #[tokio::test]
    async fn p2p_second_offer_overrides_first_session() {
        let offer = browser_offer();
        let (ctrl_tx, _rx) = mpsc::channel::<String>(16);
        let state = P2pState::default();
        handle_offer(&state, "s-p2p-test".to_string(), ctrl_tx.clone(), &offer, &[]).await;
        let id1 = state.inner.lock().await.as_ref().unwrap().id;
        handle_offer(&state, "s-p2p-test".to_string(), ctrl_tx, &offer, &[]).await;
        let guard = state.inner.lock().await;
        let s2 = guard.as_ref().expect("second offer must keep a session");
        assert_ne!(s2.id, id1, "overriding offer must create a new session");
    }

    /// 对外广播 IP：不得返回未指定地址（0.0.0.0）。
    #[test]
    fn p2p_pick_advertised_ip_returns_non_unspecified() {
        let ip = pick_advertised_ip();
        assert!(!ip.is_unspecified(), "advertised ip must not be 0.0.0.0");
    }

    // ── Task 3：fMP4 DataChannel 镜像投递 ─────────────────────
    // TDD RED→GREEN：先断言 video_tx 默认 None + shutdown 清理；再断言
    // 投递字节经消费循环写入已建连 peer 的 DataChannel。

    /// 默认 P2pState 的 video_tx / last_init 必须是 None（未建连无投递口）。
    #[test]
    fn p2p_video_tx_default_is_none() {
        let state = P2pState::default();
        assert!(state.video_tx.read().unwrap().is_none());
        assert!(state.last_init.read().unwrap().is_none());
    }

    /// last_init 缓存写入/读取（post_fn 缓存侧的行为，任务书 RED→GREEN）：
    /// 写入后驱动任务 Connected 分支可读到并补推到投递口。
    #[test]
    fn p2p_last_init_cache_roundtrip() {
        let state = P2pState::default();
        {
            let mut w = state.last_init.write().unwrap();
            *w = Some(b"\x00\x00\x00\x18ftypisom".to_vec());
        }
        let cached = state.last_init.read().unwrap();
        assert_eq!(cached.as_deref(), Some(&b"\x00\x00\x00\x18ftypisom"[..]));
    }

    /// offer 建会话（未 Connected，自然不设投递口）→ shutdown 后 video_tx
    /// 仍为 None 且会话槽清空——新增字段与 abort 清理不破坏 Task 2 路径。
    #[tokio::test]
    async fn p2p_video_tx_cleared_by_shutdown() {
        let offer = browser_offer();
        let (ctrl_tx, mut _ctrl_rx) = mpsc::channel::<String>(8);
        let state = P2pState::default();
        handle_offer(
            &state,
            "s-p2p-test".to_string(),
            ctrl_tx,
            &offer,
            &[],
        )
        .await;
        assert!(
            state.inner.lock().await.is_some(),
            "offer must leave an active session"
        );
        // 未 Connected：投递口必须保持 None（驱动任务未到 Connected 分支）。
        assert!(state.video_tx.read().unwrap().is_none());
        shutdown(&state).await;
        assert!(state.inner.lock().await.is_none());
        assert!(state.video_tx.read().unwrap().is_none());
    }

    /// 镜像投递端到端：有界丢旧保新队列（post_fn 写入侧）→
    /// `drain_video_to_channel`（消费循环，与驱动任务同构）→ DataChannel →
    /// 对端 peer 收到原始字节。用 webrtc::testutil 的内存传输握手到 Connected。
    #[tokio::test]
    async fn p2p_video_tx_mirrors_bytes_into_datachannel() {
        use crate::agent::desktop::webrtc::testutil::{pump, test_peer};
        use crate::agent::desktop::webrtc::{begin_answer, WebRtcState};

        let sa: Arc<std::sync::Mutex<_>> =
            Arc::new(std::sync::Mutex::new(crate::agent::desktop::webrtc::testutil::MemState::default()));
        let sb: Arc<std::sync::Mutex<_>> =
            Arc::new(std::sync::Mutex::new(crate::agent::desktop::webrtc::testutil::MemState::default()));
        let mut browser = test_peer(1, 54000, &sb);
        let offer = browser.make_offer().unwrap();

        let agent_addr: SocketAddr = "147.147.147.2:54001".parse().unwrap();
        let t = crate::agent::desktop::webrtc::testutil::MemTransport {
            state: sa.clone(),
            local: agent_addr,
        };
        let (mut agent, answer) = begin_answer(agent_addr, Box::new(t), &offer, &[]).unwrap();
        browser.handle_answer(&answer.sdp).unwrap();
        for c in &answer.candidates {
            browser.add_remote_candidate(str0m::Candidate::from_sdp_string(c).unwrap());
        }
        let browser_cands = browser.local_candidates();
        for c in &browser_cands {
            agent.add_remote_candidate(str0m::Candidate::from_sdp_string(c).unwrap());
        }

        // 握手到 Connected（内存转发）。
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut agent, &mut browser, &sa, &sb);
            if agent.state() == WebRtcState::Connected && browser.state() == WebRtcState::Connected {
                break;
            }
            if Instant::now() > deadline {
                panic!("peers did not reach Connected");
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // 浏览器侧挂接收回调，跑与生产同构的消费循环。
        let got = Arc::new(std::sync::Mutex::new(Vec::new()));
        let got_cb = got.clone();
        browser.on_data(Box::new(move |d| got_cb.lock().unwrap().extend_from_slice(&d)));

        let peer = Arc::new(Mutex::new(agent));
        let q = MirroredQueue::new(64);
        let last_write_ok = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        let drain_handle = tokio::spawn(drain_video_to_channel(q.clone(), peer.clone(), last_write_ok));
        q.push(b"init-bytes".to_vec());
        q.push(vec![1u8, 2, 3, 4, 5]);

        let d2 = Instant::now() + Duration::from_secs(5);
        loop {
            {
                let mut agent_g = peer.lock().await;
                pump(&mut agent_g, &mut browser, &sa, &sb);
            }
            if got.lock().unwrap().len() >= 15 {
                break;
            }
            if Instant::now() > d2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        q.close();
        drain_handle.abort();

        assert_eq!(
            got.lock().unwrap().as_slice(),
            b"init-bytes\x01\x02\x03\x04\x05",
            "datachannel must deliver the mirrored fMP4 bytes in order"
        );
    }

    // ── final-review #1：死链防 OOM（有界丢旧保新 + 驱动循环看门狗）──────────

    /// 镜像队列**有界 + 丢旧保新**（内存上界证明）：连推远超容量的帧，缓冲恒
    /// 停在 cap 帧，且保留的是**最新** cap 帧（fMP4 丢旧保新语义）。
    #[tokio::test]
    async fn mirrored_queue_bounds_memory_and_drops_oldest() {
        let q = MirroredQueue::new(4);
        for i in 0..100 {
            q.push(vec![i]);
        }
        assert_eq!(q.len(), 4, "buffered frames must be capped at capacity");
        let snap = q.snapshot();
        assert_eq!(
            snap,
            vec![vec![96], vec![97], vec![98], vec![99]],
            "oldest frames must be dropped, newest kept"
        );
        // close 后队列清空、消费循环退出语义：pop 返回 None。
        q.close();
        assert_eq!(q.len(), 0);
        assert!(
            q.pop().await.is_none(),
            "closed queue must yield None (drain exits)"
        );
    }

    /// 死链看门狗（anti-OOM 集成测试）：内存传输握手到 Connected 后**杀掉对端**
    /// （浏览器不再被 pump，agent 的 DataChannel 写入无法被 ACK → SCTP 写缓冲
    /// 填满后持续 Backpressure），驱动循环在缩短的看门狗窗口内判定死链并拆链：
    /// `video_tx` 置 None（消费循环退出）。证明死链不会无限积压（配合有界队列）。
    #[tokio::test]
    async fn p2p_dead_peer_watchdog_tears_down_bounded() {
        use crate::agent::desktop::webrtc::testutil::{pump, test_peer};
        use crate::agent::desktop::webrtc::{begin_answer, WebRtcState};

        let sa: Arc<std::sync::Mutex<_>> =
            Arc::new(std::sync::Mutex::new(crate::agent::desktop::webrtc::testutil::MemState::default()));
        let sb: Arc<std::sync::Mutex<_>> =
            Arc::new(std::sync::Mutex::new(crate::agent::desktop::webrtc::testutil::MemState::default()));
        let mut browser = test_peer(1, 55000, &sb);
        let offer = browser.make_offer().unwrap();

        let agent_addr: SocketAddr = "147.147.147.2:55001".parse().unwrap();
        let t = crate::agent::desktop::webrtc::testutil::MemTransport {
            state: sa.clone(),
            local: agent_addr,
        };
        let (mut agent, answer) = begin_answer(agent_addr, Box::new(t), &offer, &[]).unwrap();
        browser.handle_answer(&answer.sdp).unwrap();
        for c in &answer.candidates {
            browser.add_remote_candidate(str0m::Candidate::from_sdp_string(c).unwrap());
        }
        for c in &browser.local_candidates() {
            agent.add_remote_candidate(str0m::Candidate::from_sdp_string(c).unwrap());
        }

        // 握手到 Connected（内存转发）。
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut agent, &mut browser, &sa, &sb);
            if agent.state() == WebRtcState::Connected && browser.state() == WebRtcState::Connected {
                break;
            }
            if Instant::now() > deadline {
                panic!("peers did not reach Connected");
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // 让驱动任务接管 agent（缩短看门狗窗口：500ms），不再 pump——浏览器
        // "死亡"。agent 的 SCTP 写入再无人 ACK，写缓冲（128KB）填满后持续
        // Backpressure → last_write_ok 陈旧 → 看门狗拆链。
        let state = P2pState::default();
        let (ctrl_tx, _ctrl_rx) = mpsc::channel::<String>(8);
        let id = state.next_id.fetch_add(1, Ordering::Relaxed);
        let peer = Arc::new(Mutex::new(agent));
        let driver = tokio::spawn({
            let slot = state.inner.clone();
            let video_tx = state.video_tx.clone();
            let last_init = state.last_init.clone();
            let peer = peer.clone();
            let ctrl_tx = ctrl_tx.clone();
            async move {
                run_driver(
                    slot,
                    video_tx,
                    last_init,
                    "s-p2p-dead",
                    &ctrl_tx,
                    id,
                    peer,
                    HANDSHAKE_TIMEOUT,
                    Duration::from_millis(500),
                )
                .await;
            }
        });

        // 等驱动任务建连并设置 video_tx（投递口就绪）。
        let d1 = Instant::now() + Duration::from_secs(5);
        loop {
            if state.video_tx.read().unwrap().is_some() {
                break;
            }
            if Instant::now() > d1 {
                panic!("driver never reached Connected (video_tx not set)");
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // 死链下持续灌帧（模拟 30-60fps 生产者）：消费循环写不进 DataChannel，
        // 队列保持有界；看门狗（500ms）应主动拆链、清 video_tx。
        let mut pushed = 0usize;
        let d2 = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(guard) = state.video_tx.read() {
                if let Some(q) = guard.as_ref() {
                    q.push(vec![0xabu8; 16 * 1024]);
                    pushed += 1;
                } else {
                    break; // 已置 None = 拆链完成。
                }
            } else {
                break;
            }
            if Instant::now() > d2 {
                panic!("video_tx not cleared after dead-peer watchdog window (pushed {pushed})");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert!(
            state.video_tx.read().unwrap().is_none(),
            "dead-peer watchdog must clear video_tx within bounded time"
        );
        // 驱动任务应已自行退出（Failed 拆链路径）。
        driver.abort();
    }
}
