//! Agent 侧 WebRTC P2P 信令处理（Task 2，agent ↔ relay ↔ 浏览器）。
//!
//! 生命周期（浏览器是 offerer，agent 是 answerer，方向见 task-2-context）：
//! - `desktop:p2p-offer {sdp, candidates[]}` → 建 UDP socket、应答、spawn 驱动任务；
//! - `desktop:p2p-candidate {candidate}` → 喂共享 peer（trickle）；
//! - 建连 → 打日志 + 广播 `desktop:p2p-state {state:"connected"}`；
//! - 15s 握手超时 / `Failed` → 广播 `desktop:p2p-state {state:"failed"}` + 清理；
//! - `desktop:stop` 或二次 offer（last-wins 覆盖）→ abort 驱动任务、置 None。
//!
//! 键鼠上行 / fMP4 下行走 DataChannel 是 Task 3/6，本模块只到信令与状态广播。

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::agent::desktop::webrtc::{
    begin_answer, UdpTransport, WebRtcPeer, WebRtcState,
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
#[derive(Clone, Default)]
pub(crate) struct P2pState {
    inner: Arc<Mutex<Option<P2pSession>>>,
    next_id: Arc<AtomicU64>,
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
    abort(&state.inner).await;

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

/// `desktop:stop` / 会话结束：中止驱动任务并清理会话。
pub(crate) async fn shutdown(state: &P2pState) {
    abort(&state.inner).await;
}

/// abort 当前会话的驱动任务并置 None。
async fn abort(slot: &Mutex<Option<P2pSession>>) {
    let mut guard = slot.lock().await;
    if let Some(s) = guard.take() {
        if let Some(d) = s.driver {
            d.abort();
        }
        tracing::debug!("desktop p2p session torn down");
    }
}

/// spawn P2P 驱动任务：循环 drive → 状态机 → 广播。Until Connected：15s
/// 超时判 failed（清理）；Connected：保持运行伺候 DataChannel（Task 3）。
fn spawn_driver(
    state: &P2pState,
    sid: String,
    ctrl: tokio::sync::mpsc::Sender<String>,
    id: u64,
    peer: Arc<Mutex<WebRtcPeer>>,
) -> tokio::task::JoinHandle<()> {
    let slot = state.inner.clone();
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
        // 退出时仅当自己仍是当前会话才清位（防覆盖后误清新 peer）。
        let mut guard = slot.lock().await;
        if let Some(s) = guard.as_ref() {
            if s.id == id {
                *guard = None;
            }
        }
    })
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
fn pick_advertised_ip() -> std::net::Ipv4Addr {
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
}