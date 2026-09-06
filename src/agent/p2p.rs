//! Agent 侧 WebRTC P2P 信令处理（Task 2，agent ↔ relay ↔ 浏览器）。
//!
//! 生命周期（浏览器是 offerer，agent 是 answerer，方向见 task-2-context）：
//! - `desktop:p2p-offer {sdp, candidates[]}` → 建 UDP socket、应答、spawn 驱动任务；
//! - `desktop:p2p-candidate {candidate}` → 喂共享 peer（trickle）；
//! - 建连 → 打日志 + 广播 `desktop:p2p-state {state:"connected"}` + 开启 fMP4
//!   下行投递口（Task 3）：`desktop:video` 的字节镜像进 DataChannel；
//! - 15s 握手超时 / `Failed` → 广播 `desktop:p2p-state {state:"failed"}` + 清理；
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
#[derive(Clone)]
pub(crate) struct P2pState {
    inner: Arc<Mutex<Option<P2pSession>>>,
    next_id: Arc<AtomicU64>,
    /// 已建连后 agent 侧 fMP4 下行投递口（Task 3）：None = 未连接/未建。
    /// 驱动任务在 Connected 时设为 `Some(UnboundedSender<Vec<u8>>)`，post_fn
    /// 把 `desktop:video` 的 fMP4 字节（base64 解码后）镜像投递进来，消费循环
    /// 逐箱写入 DataChannel（relay 路径保底不中断）。
    pub(crate) video_tx:
        Arc<std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>,
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

/// abort 当前会话的驱动任务并置 None（同时清 fMP4 投递口：drop sender →
/// 消费循环自然退出；驱动任务被 abort 时不会跑到自己的清理代码，这里兜底）。
async fn abort(state: &P2pState) {
    let mut guard = state.inner.lock().await;
    if let Some(s) = guard.take() {
        if let Some(d) = s.driver {
            d.abort();
        }
        tracing::debug!("desktop p2p session torn down");
    }
    drop(guard);
    *state.video_tx.write().unwrap() = None;
}

/// spawn P2P 驱动任务：循环 drive → 状态机 → 广播。Until Connected：15s
/// 超时判 failed（清理）；Connected：开启 fMP4 镜像投递口（Task 3）并保持
/// 运行推进 SCTP/ICE keepalive。
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
        let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
        let mut announced_connected = false;
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
                        // 开启 fMP4 下行投递口：post_fn（relay 镜像）经 unbounded
                        // channel 把 desktop:video 字节投进来，独立消费循环写
                        // DataChannel。置 None（drop sender）时消费循环自然退出。
                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                        *video_tx.write().unwrap() = Some(tx);
                        // 参数集必达：独立任务直接写 peer（不走会被 frag 积压的
                        // 通道），路径（SCTP 通道 open / cwnd）未就绪时小步重试，
                        // 直到成功或会话结束。浏览器通常在 init 发出后才建连，
                        // 缺参数集则后续 frag 全被 demux 丢弃、画面永不出现。
                        tokio::spawn(init_datachannel_delivery(
                            peer.clone(),
                            slot.clone(),
                            id,
                            last_init.clone(),
                        ));
                        tokio::spawn(drain_video_to_channel(rx, peer.clone()));
                        send_state(&ctrl, &sid, "connected").await;
                    }
                    // 已建连：无握手超时，持续驱动推进 SCTP/ICE keepalive。
                }
                WebRtcState::Failed => {
                    tracing::warn!("desktop p2p failed");
                    send_state(&ctrl, &sid, "failed").await;
                    break;
                }
                WebRtcState::Connecting => {
                    if !announced_connected && tokio::time::Instant::now() >= deadline {
                        tracing::warn!(
                            timeout_ms = HANDSHAKE_TIMEOUT.as_millis(),
                            "desktop p2p handshake timeout, tearing down"
                        );
                        send_state(&ctrl, &sid, "failed").await;
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
        // 退出时清投递口（drop sender → 消费循环退出）；仅当自己仍是当前会话
        // 才清位与投递口（防覆盖后旧任务误清新会话的状态）。
        let mut guard = slot.lock().await;
        if let Some(s) = guard.as_ref() {
            if s.id == id {
                *guard = None;
                drop(guard);
                *video_tx.write().unwrap() = None;
            }
        }
    })
}

/// 保证参数集（init=ftyp+moov）必达 DataChannel 的独立任务。
///
/// 不走 frag 通道：通道会被积压的 frag 占满，排后的 init 会被饿死。这里直接
/// 锁 peer 写（与消费循环串行，无并发问题）；路径未就绪（通道未 open /
/// SCTP 背压）时小步重试。退出条件：① 写入成功；② 会话被覆盖或回收
/// （`slot` 中已不是本会话 id）；③ 非路径类错误。
async fn init_datachannel_delivery(
    peer: Arc<Mutex<WebRtcPeer>>,
    slot: Arc<Mutex<Option<P2pSession>>>,
    id: u64,
    last_init: Arc<std::sync::RwLock<Option<Vec<u8>>>>,
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

/// 消费 fMP4 投递口的字节并写入 DataChannel。sender 全 drop（video_tx 置
/// None）后 `recv` 返回 None，本循环自然退出。
///
/// 有界重试：SCTP 初始拥塞窗口小、通道可能未 open，`send` 会临时返回
/// ChannelNotOpen / Backpressure——同一字节重试到路径可用（上限 ~2s，
/// 对齐 str0m SCTP cwnd 起跳），超过即丢（fMP4 丢旧保新，relay 路径保底）。
/// init 的必达由 [`init_datachannel_delivery`] 独立保证，本循环只处理实时
/// frag（窗口让路时保住提升吞吐，窗口持续关闭时按旧丢新）。
async fn drain_video_to_channel(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    peer: Arc<Mutex<WebRtcPeer>>,
) {
    // ~2s：覆盖 SCTP 通道 open + cwnd 起跳（初始 cwnd≈4×MTU，需 SACK 驱动
    // 增长；LAN 上首个 ACK 通常在 <100ms，但端到端链路可能更慢）。超过即丢
    // （fMP4 丢旧保新，relay 路径保底）。
    const MAX_RETRY_BUDGET_MS: u64 = 2000;
    const RETRY_INTERVAL: Duration = Duration::from_millis(5);
    while let Some(bytes) = rx.recv().await {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(MAX_RETRY_BUDGET_MS);
        let mut sent = false;
        loop {
            let result = {
                let mut guard = peer.lock().await;
                guard.send(&bytes)
            };
            match result {
                Ok(()) => {
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

    /// 镜像投递端到端：unbounded channel（post_fn 写入侧）→
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
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let drain_handle = tokio::spawn(drain_video_to_channel(rx, peer.clone()));
        tx.send(b"init-bytes".to_vec()).unwrap();
        tx.send(vec![1u8, 2, 3, 4, 5]).unwrap();

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
        drop(tx);
        drain_handle.abort();

        assert_eq!(
            got.lock().unwrap().as_slice(),
            b"init-bytes\x01\x02\x03\x04\x05",
            "datachannel must deliver the mirrored fMP4 bytes in order"
        );
    }
}