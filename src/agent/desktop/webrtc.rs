//! WebRTC P2P peer wrapper (str0m 0.23, sans-I/O) — SDP/ICE/DataChannel.
//!
//! 阶段1（Task 1）：`WebRtcPeer` 封装 str0m，提供 SDP offer/answer JSON 交换、
//! DataChannel 二进制收发、ICE candidate 导出/注入，以及一个可注入的报文传输缝
//! （生产：真实 UDP socket 驱动任务；测试：内存转发）。
//!
//! 驱动循环（sans-I/O）：调用方持有 peer 并周期调用 [`WebRtcPeer::drive`]：
//! 喂入收到的 UDP 报文 + 时间片，排干 str0m 输出（Transmit → 发出 / Event → 状态
//! 机与回调）。生产环境在 tokio 任务里 select socket recv 与 rtc 超时即可，见
//! `desktop/mod.rs` 的 `desktop:p2p-*` 信令接线（Task 2）。

use std::net::SocketAddr;
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

/// Peer 级 WebRTC 连接状态（计划接口：Connecting | Connected | Failed）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebRtcState {
    /// 建连中（ICE New/Checking，或暂时断开待恢复）。
    Connecting,
    /// ICE + DTLS + SCTP 已打通。
    Connected,
    /// 内部驱动错误（poll/handle 失败）——不可恢复。
    Failed,
}

/// 报文传输缝：生产用真实 UDP socket（std/tokio `try_recv`/`try_send_to`）驱动，
/// 测试注入内存转发，不碰 socket。保持单缝，不过度抽象（YAGNI）。
pub trait Transport {
    /// 发一个 UDP 报文到 `dest`。
    fn send(&mut self, dest: SocketAddr, data: &[u8]);
    /// 非阻塞取一个收到的报文（source, data）；无则为 `None`。
    fn recv(&mut self) -> Option<(SocketAddr, Vec<u8>)>;
}

/// 本模块错误。
#[derive(Debug, thiserror::Error)]
pub enum WebrtcError {
    #[error("str0m error: {0}")]
    Rtc(#[from] str0m::RtcError),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no pending local offer — call make_offer() first")]
    NoPendingOffer,
    #[error("could not generate SDP offer/answer")]
    Sdp,
    #[error("data channel not open yet")]
    ChannelNotOpen,
    #[error("data channel write rejected by backpressure")]
    Backpressure,
}

/// 默认空传输（`new()` 构造用，生产由 `with_transport` 注入真实 socket 传输）。
struct NoopTransport;

impl Transport for NoopTransport {
    fn send(&mut self, _dest: SocketAddr, _data: &[u8]) {}
    fn recv(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        None
    }
}

/// Agent 侧 WebRTC peer 封装（str0m 0.23）。
///
/// 构造后先 [`make_offer`](WebRtcPeer::make_offer) 产出 offer，交给远端；
/// 远端用 [`answer_offer`](WebRtcPeer::answer_offer) 应答；发起来
/// [`handle_answer`](WebRtcPeer::handle_answer) 接受。随后调用方按驱动循环
/// [`drive`](WebRtcPeer::drive) 推进握手，`on_data` 在 DataChannel 收到远端
/// 二进制时触发（fMP4 下行回调）。
pub struct WebRtcPeer {
    rtc: Rtc,
    local_addr: SocketAddr,
    local_candidate: Candidate,
    channel: Option<ChannelId>,
    pending_offer: Option<SdpPendingOffer>,
    on_data: Option<Box<dyn Fn(Vec<u8>) + Send + Sync>>,
    state: WebRtcState,
    failed: bool,
    transport: Box<dyn Transport>,
}

impl WebRtcPeer {
    /// 以本地 socket 地址创建 peer，并加入 host candidate（UDP）。
    /// `local_addr` 必须是真实绑定的 socket 地址（生产）；测试可传任意回环地址，
    /// 因为传输是注入的。
    pub fn new(local_addr: SocketAddr) -> Self {
        Self::with_transport(local_addr, Box::new(NoopTransport))
    }

    /// 创建 peer 并注入报文传输（测试用内存转发 / 生产用真实 UDP 传输）。
    pub fn with_transport(local_addr: SocketAddr, transport: Box<dyn Transport>) -> Self {
        let rtc = RtcConfig::new().build(Instant::now());
        let candidate = Candidate::host(local_addr, "udp").expect("udp host candidate");
        let mut this = Self {
            rtc,
            local_addr,
            local_candidate: candidate.clone(),
            channel: None,
            pending_offer: None,
            on_data: None,
            state: WebRtcState::Connecting,
            failed: false,
            transport,
        };
        this.rtc.add_local_candidate(candidate);
        this
    }

    /// 发起方：建 DataChannel（label "desktop"）并产 SDP offer（JSON 字符串）。
    pub fn make_offer(&mut self) -> Result<String, WebrtcError> {
        let mut change = self.rtc.sdp_api();
        let cid = change.add_channel("desktop".to_string());
        self.channel = Some(cid);
        let (offer, pending) = change.apply().ok_or(WebrtcError::Sdp)?;
        self.pending_offer = Some(pending);
        Ok(serde_json::to_string(&offer)?)
    }

    /// 应答方向：接受远端 offer，产 SDP answer（JSON 字符串）。
    pub fn answer_offer(&mut self, offer_json: &str) -> Result<String, WebrtcError> {
        let offer: SdpOffer = serde_json::from_str(offer_json)?;
        let answer = self.rtc.sdp_api().accept_offer(offer)?;
        Ok(serde_json::to_string(&answer)?)
    }

    /// 发起方：接受 SDP answer。
    pub fn handle_answer(&mut self, answer_json: &str) -> Result<(), WebrtcError> {
        let answer: SdpAnswer = serde_json::from_str(answer_json)?;
        let pending = self
            .pending_offer
            .take()
            .ok_or(WebrtcError::NoPendingOffer)?;
        self.rtc.sdp_api().accept_answer(pending, answer)?;
        Ok(())
    }

    /// 设置 DataChannel 收到远端二进制时的回调（fMP4 下行）。
    pub fn on_data(&mut self, cb: Box<dyn Fn(Vec<u8>) + Send + Sync>) {
        self.on_data = Some(cb);
    }

    /// DataChannel 已打开后发送二进制（write 失败/未打开返回错误）。
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), WebrtcError> {
        let cid = self.channel.ok_or(WebrtcError::ChannelNotOpen)?;
        let mut chan = self.rtc.channel(cid).ok_or(WebrtcError::ChannelNotOpen)?;
        let accepted = chan.write(true, bytes)?;
        if !accepted {
            return Err(WebrtcError::Backpressure);
        }
        Ok(())
    }

    /// 当前连接状态。
    pub fn state(&self) -> WebRtcState {
        if self.failed {
            WebRtcState::Failed
        } else {
            self.state
        }
    }

    /// 本端候选导出为标准 `candidate:...` 字符串（Task 2 `desktop:p2p-candidate`）。
    pub fn local_candidates(&self) -> Vec<String> {
        vec![self.local_candidate.to_sdp_string()]
    }

    /// 单独喂入远端 ICE candidate（trickle/补充候选）。
    pub fn add_remote_candidate(&mut self, cand: Candidate) {
        self.rtc.add_remote_candidate(cand);
    }

    /// 推进引擎一步：喂入待处理报文 + `now` 时间片，排干输出（Transmit→发出，
    /// Event→状态机/回调），返回下次应唤醒的时间或 `None`。生产在 socket 驱动
    /// 任务里周期调用；测试用其推进内存转发握手。
    pub fn drive(&mut self, now: Instant) -> Option<Instant> {
        // 1. 喂入待处理的外部报文。
        while let Some((source, data)) = self.transport.recv() {
            if let Ok(recv) = Receive::new(Protocol::Udp, source, self.local_addr, &data) {
                let input = Input::Receive(now, recv);
                if self.rtc.accepts(&input) {
                    if let Err(e) = self.rtc.handle_input(input) {
                        self.mark_failed();
                        tracing::warn!("webrtc handle_input: {e}");
                        return None;
                    }
                }
            }
        }
        // 2. 喂时间片。
        if let Err(e) = self.rtc.handle_input(Input::Timeout(now)) {
            self.mark_failed();
            tracing::warn!("webrtc timeout tick: {e}");
            return None;
        }
        // 3. 排干输出：Transmit → 发出，Event → 状态机/回调；遇 Timeout 记下次唤醒。
        let mut next: Option<Instant> = None;
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    self.transport.send(t.destination, &t.contents);
                }
                Ok(Output::Timeout(until)) => {
                    next = Some(until);
                    break;
                }
                Ok(Output::Event(ev)) => self.handle_event(ev),
                Err(e) => {
                    self.mark_failed();
                    tracing::warn!("webrtc poll_output: {e}");
                    break;
                }
            }
        }
        next
    }

    /// 处理 str0m 输出事件：ICE 状态 → `state`，DataChannel 打开/数据 → 回调。
    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Connected => self.state = WebRtcState::Connected,
            Event::IceConnectionStateChange(s) => {
                self.state = match s {
                    IceConnectionState::Connected | IceConnectionState::Completed => {
                        WebRtcState::Connected
                    }
                    // New / Checking / Disconnected（暂且视为可恢复的建连中）。
                    _ => WebRtcState::Connecting,
                };
            }
            Event::ChannelOpen(cid, _label) => {
                self.channel.get_or_insert(cid);
            }
            Event::ChannelData(d) => {
                if let Some(cb) = &self.on_data {
                    cb(d.data);
                }
            }
            _ => {}
        }
    }

    fn mark_failed(&mut self) {
        self.failed = true;
        self.state = WebRtcState::Failed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 内存传输核心：两个 peer 各自持有一个核的句柄，测试侧也保留句柄做转发。
    #[derive(Default)]
    struct MemState {
        /// (source, dest, data)
        outbox: VecDeque<(SocketAddr, SocketAddr, Vec<u8>)>,
        /// (source, data)
        inbox: VecDeque<(SocketAddr, Vec<u8>)>,
    }

    struct MemTransport {
        state: Rc<RefCell<MemState>>,
        local: SocketAddr,
    }

    impl Transport for MemTransport {
        fn send(&mut self, dest: SocketAddr, data: &[u8]) {
            self.state
                .borrow_mut()
                .outbox
                .push_back((self.local, dest, data.to_vec()));
        }
        fn recv(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
            self.state.borrow_mut().inbox.pop_front()
        }
    }

    /// 把所有未发报文搬到对端 inbox；返回是否搬动了东西。
    fn move_packets(sa: &Rc<RefCell<MemState>>, sb: &Rc<RefCell<MemState>>) -> bool {
        let mut moved = false;
        {
            let mut a = sa.borrow_mut();
            let mut b = sb.borrow_mut();
            while let Some((src, _dst, data)) = a.outbox.pop_front() {
                b.inbox.push_back((src, data));
                moved = true;
            }
            while let Some((src, _dst, data)) = b.outbox.pop_front() {
                a.inbox.push_back((src, data));
                moved = true;
            }
        }
        moved
    }

    /// 推进双 peer 一步：驱动引擎并把产生的报文转发到对端 inbox。
    fn pump(
        a: &mut WebRtcPeer,
        b: &mut WebRtcPeer,
        sa: &Rc<RefCell<MemState>>,
        sb: &Rc<RefCell<MemState>>,
    ) {
        let now = Instant::now();
        a.drive(now);
        b.drive(now);
        loop {
            let moved = move_packets(sa, sb);
            if !moved {
                break;
            }
            a.drive(now);
            b.drive(now);
        }
    }

    fn test_peer(ip: u8, port: u16, state: &Rc<RefCell<MemState>>) -> WebRtcPeer {
        let addr: SocketAddr = format!("147.147.147.{ip}:{port}").parse().unwrap();
        let t = MemTransport {
            state: state.clone(),
            local: addr,
        };
        WebRtcPeer::with_transport(addr, Box::new(t))
    }

    /// 计划 Step 2 原测试：offer/answer JSON 往返 + 状态机存活。
    /// 0.23 下接 answer 后未 pump 前 ICE 停在 New（映射为 Connecting），
    /// 与计划断言 `state() == Connecting` 一致，不改意图。
    #[test]
    fn peer_creates_offer_and_parses_answer() {
        let sa = Rc::new(RefCell::new(MemState::default()));
        let sb = Rc::new(RefCell::new(MemState::default()));
        let mut a = test_peer(1, 50000, &sa);
        let mut b = test_peer(2, 50001, &sb);

        let offer = a.make_offer().unwrap();
        let answer = b.answer_offer(&offer).unwrap();
        a.handle_answer(&answer).unwrap();

        // offer/answer 往返成功，状态机接 answer 后处于建连中（未失败，也未连接）。
        assert_eq!(a.state(), WebRtcState::Connecting);
        assert_eq!(b.state(), WebRtcState::Connecting);
        // 本地候选可导出（后续 desktop:p2p-candidate 用）。
        let cands = a.local_candidates();
        assert_eq!(cands.len(), 1);
        assert!(cands[0].starts_with("candidate:"), "got {cands:?}");
    }

    /// 双 peer 内存互通：无 socket，握手连上后 DataChannel 双向收发字节。
    #[test]
    fn two_peers_connect_and_exchange_data() {
        let sa = Rc::new(RefCell::new(MemState::default()));
        let sb = Rc::new(RefCell::new(MemState::default()));
        let mut a = test_peer(1, 50000, &sa);
        let mut b = test_peer(2, 50001, &sb);

        let offer = a.make_offer().unwrap();
        let answer = b.answer_offer(&offer).unwrap();
        a.handle_answer(&answer).unwrap();

        let got_a = Arc::new(Mutex::new(Vec::new()));
        let got_b = Arc::new(Mutex::new(Vec::new()));
        let cb_a = got_a.clone();
        a.on_data(Box::new(move |d| {
            cb_a.lock().unwrap().extend_from_slice(&d)
        }));
        let cb_b = got_b.clone();
        b.on_data(Box::new(move |d| {
            cb_b.lock().unwrap().extend_from_slice(&d)
        }));

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut a_sent = false;
        let mut b_sent = false;
        loop {
            if Instant::now() > deadline {
                break;
            }
            if !a_sent {
                a_sent = a.send(&[9, 8, 7]).is_ok();
            }
            if !b_sent {
                b_sent = b.send(&[0, 1, 2, 3]).is_ok();
            }
            pump(&mut a, &mut b, &sa, &sb);
            let (ca_len, cb_len) = { (got_a.lock().unwrap().len(), got_b.lock().unwrap().len()) };
            if a.state() == WebRtcState::Connected
                && b.state() == WebRtcState::Connected
                && ca_len >= 4
                && cb_len >= 3
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(
            a.state(),
            WebRtcState::Connected,
            "peer A failed to connect"
        );
        assert_eq!(
            b.state(),
            WebRtcState::Connected,
            "peer B failed to connect"
        );
        assert_eq!(
            got_a.lock().unwrap().as_slice(),
            &[0u8, 1, 2, 3],
            "A should receive B's bytes"
        );
        assert_eq!(
            got_b.lock().unwrap().as_slice(),
            &[9u8, 8, 7],
            "B should receive A's bytes"
        );
    }
}
