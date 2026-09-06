//! WebRTC P2P peer wrapper (str0m 0.23, sans-I/O) — SDP/ICE/DataChannel.
//!
//! 阶段1（Task 1）：`WebRtcPeer` 封装 str0m，提供 SDP offer/answer JSON 交换、
//! DataChannel 二进制收发、ICE candidate 导出/注入，以及一个可注入的报文传输缝
//! （生产：真实 UDP socket 驱动任务；测试：内存转发）。
//!
//! 驱动循环（sans-I/O）：调用方持有 peer 并周期调用 [`WebRtcPeer::drive`]：
//! 喂入收到的 UDP 报文 + 时间片，排干 str0m 输出（Transmit → 发出 / Event → 状态
//! 机与回调）。生产环境在 tokio 任务里 select socket recv 与 rtc 超时即可，见
//! `agent/p2p.rs` 的 `desktop:p2p-*` 信令接线（Task 2）。

use std::net::SocketAddr;
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

/// Peer 级 WebRTC 连接状态（计划接口：Connecting | Connected | Failed）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebRtcState {
    /// 建连中（ICE New/Checking）。
    Connecting,
    /// ICE + DTLS + SCTP 已打通。
    Connected,
    /// 不可恢复：驱动错误、ICE Disconnected、DataChannel 被远端关闭。
    Failed,
}

/// 报文传输缝：生产用真实 UDP socket（std/tokio `try_recv`/`try_send_to`）驱动，
/// 测试注入内存转发，不碰 socket。保持单缝，不过度抽象（YAGNI）。
///
/// `Send` 是必需的：agent 侧 P2P 驱动任务在 tokio 里 `spawn` 持有 peer
/// （`Arc<Mutex<WebRtcPeer>>`），非 Send 的传输会导致 future 无法跨线程。
/// `UdpTransport`（std socket）与测试用内存传输（Arc<Mutex>）均满足。
pub trait Transport: Send {
    /// 发一个 UDP 报文到 `dest`。
    fn send(&mut self, dest: SocketAddr, data: &[u8]);
    /// 非阻塞取一个收到的报文（source, data）；无则为 `None`。
    fn recv(&mut self) -> Option<(SocketAddr, Vec<u8>)>;
}

/// 生产用 UDP 传输（Task 2）：std 同步非阻塞 socket，天然适配 [`Transport`]
/// 的 `send/recv` 形状。绑定地址由调用方决定（agent 侧 `0.0.0.0:0`）。
pub struct UdpTransport {
    socket: std::net::UdpSocket,
    local: SocketAddr,
}

impl UdpTransport {
    /// 绑定监听地址（通常 `"0.0.0.0:0"`），设非阻塞 + 50ms 读超时上限。
    pub fn bind(addr: &str) -> Result<Self, std::io::Error> {
        let socket = std::net::UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?; // recv 无包用 WouldBlock 表示
        socket.set_read_timeout(Some(std::time::Duration::from_millis(50)))?;
        let local = socket.local_addr()?;
        Ok(Self { socket, local })
    }

    /// 实际绑定地址（生产场景为 `0.0.0.0:port`；测试可回环）。
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// 实际绑定的端口（`local.port()` 简写，agent 建 host candidate 用）。
    pub fn local_port(&self) -> u16 {
        self.local.port()
    }
}

impl Transport for UdpTransport {
    fn send(&mut self, dest: SocketAddr, data: &[u8]) {
        let _ = self.socket.send_to(data, dest);
    }
    fn recv(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        let mut buf = [0u8; 65536];
        match self.socket.recv_from(&mut buf) {
            Ok((n, src)) => Some((src, buf[..n].to_vec())),
            Err(_) => None, // WouldBlock / 超时 / 无数据 → 无包
        }
    }
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

/// SDP 载体形态：str0m JSON（工具链/测试）或 RFC 4566 文本（真实浏览器）。
#[derive(Clone, Copy)]
enum SdpFormat {
    Json,
    Rfc,
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
    ///
    /// Task 6 起固定为**不可靠 + 无序**（`MaxRetransmits{0}` + `ordered:false`）：
    /// fMP4 丢旧保新、帧时效优先——丢包不重传、不阻塞后续消息（有序通道的
    /// 丢包会造成 head-of-line 卡顿，重传队列还会拖慢拥塞窗口）。浏览器侧
    /// `createDataChannel('desktop', {ordered:false, maxRetransmits:0})` 同步
    /// 该配置（真实建连中浏览器是 channel 发起方，参数以浏览器为准）。
    pub fn make_offer(&mut self) -> Result<String, WebrtcError> {
        self.make_offer_with_config(ChannelConfig {
            label: "desktop".to_string(),
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            ..Default::default()
        })
    }

    /// 按给定 channel config 产 offer（吞吐对比/单测用；生产固定走
    /// [`Self::make_offer`] 的不可靠+无序）。
    pub fn make_offer_with_config(&mut self, config: ChannelConfig) -> Result<String, WebrtcError> {
        let mut change = self.rtc.sdp_api();
        let cid = change.add_channel_with_config(config);
        self.channel = Some(cid);
        let (offer, pending) = change.apply().ok_or(WebrtcError::Sdp)?;
        self.pending_offer = Some(pending);
        Ok(serde_json::to_string(&offer)?)
    }

    /// 应答方向：接受远端 offer，产 SDP answer。兼容两种 SDP 形态：
    /// - str0m JSON（Task 1/2 的 `make_offer` 产物，工具链/测试自洽）；
    /// - RFC 4566 文本（真实浏览器 `pc.createOffer` 产物，Task 3 主线）。
    /// 应答格式跟随请求格式：浏览器收 RFC 文本才 `setRemoteDescription` 可解。
    pub fn answer_offer(&mut self, offer_sdp: &str) -> Result<String, WebrtcError> {
        let (offer, fmt) = match serde_json::from_str::<SdpOffer>(offer_sdp) {
            Ok(o) => (o, SdpFormat::Json),
            Err(_) => (
                SdpOffer::from_sdp_string(offer_sdp).map_err(|e| {
                    tracing::warn!("RFC SDP offer parse failed: {e}");
                    WebrtcError::Sdp
                })?,
                SdpFormat::Rfc,
            ),
        };
        let answer = self.rtc.sdp_api().accept_offer(offer)?;
        match fmt {
            SdpFormat::Json => Ok(serde_json::to_string(&answer)?),
            SdpFormat::Rfc => Ok(answer.to_sdp_string()),
        }
    }

    /// 发起方：接受 SDP answer（同样兼容 JSON 与 RFC 文本，见 [`Self::answer_offer`]）。
    pub fn handle_answer(&mut self, answer_sdp: &str) -> Result<(), WebrtcError> {
        let answer = match serde_json::from_str::<SdpAnswer>(answer_sdp) {
            Ok(a) => a,
            Err(_) => SdpAnswer::from_sdp_string(answer_sdp).map_err(|e| {
                tracing::warn!("RFC SDP answer parse failed: {e}");
                WebrtcError::Sdp
            })?,
        };
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
        // 单次 drive 限批喂入：底层 dimpl DTLS 的接收队列上限（max_queue_rx=30
        // 条记录），且**只在 poll_output 排空时才消费**。若把一次 recv 积累的
        // 整个 socket 背压（数十条 SCTP SACK/DTLS 记录）一次性塞入，会在本轮
        // 排空前就撞满上限 → ReceiveQueueFull → 整个 peer 判定 Failed。限批 +
        // 每次 drive 后的 poll_output 天然形成「插一批→排一批」的节奏。
        const MAX_FEED_PER_DRIVE: usize = 8;
        let mut fed = 0usize;
        while let Some((source, data)) = self.transport.recv() {
            if fed >= MAX_FEED_PER_DRIVE {
                break; // 余量留在 socket buffer，下次 drive 再喂（≤5ms）
            }
            fed += 1;
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
                    // New / Checking：正常建连中，保持 Connecting。
                    IceConnectionState::New | IceConnectionState::Checking => {
                        WebRtcState::Connecting
                    }
                    // Disconnected（is-0.11 无 Failed 变体，Disconnected 即终态）：
                    // final-review #1 复审判定必须拆链——死链若长期停在 Connecting，
                    // 驱动循环的 15s 握手超时被 `announced_connected` 门控关掉，
                    // 配合无界 fMP4 镜像队列会在长活 agent 上 OOM。P2P 属
                    // best-effort，弱链路本就该回退 relay 矩阵。
                    IceConnectionState::Disconnected => WebRtcState::Failed,
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
            // 浏览器关页/主动关闭 DataChannel：建连后通道关闭即会话终结。
            // 此前被 `_ => {}` 吞掉，死链永不 teardown（#1 复审）。
            Event::ChannelClose(_) => {
                tracing::debug!("p2p datachannel closed by remote, marking failed");
                self.mark_failed();
            }
            _ => {}
        }
    }

    fn mark_failed(&mut self) {
        self.failed = true;
        self.state = WebRtcState::Failed;
    }
}

/// P2P 信令应答载荷（`desktop:p2p-answer`）：本地 SDP answer JSON + host
/// candidate 列表（`desktop:p2p-candidate` 同源导出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct P2pAnswerPayload {
    pub sdp: String,
    pub candidates: Vec<String>,
}

/// Answerer 侧纯函数：接受一条 `desktop:p2p-offer`（sdp + candidates 数组），
/// 产出 `(peer, P2pAnswerPayload)`。传输注入（测试内存转发 / 生产 UDP socket），
/// 不依赖 agent 消息循环，是可单测的"处理一条 offer 消息"单元。
///
/// `local_addr` 是 host candidate 与 ICE `Receive::new` 用的本地地址：生产
/// 场景传"对外广播 IP + 实际绑定端口"（`is` crate 用 `Receive.destination`
/// 匹配本地候选，二者必须一致，见 `task-2-context`）。
pub fn begin_answer(
    local_addr: SocketAddr,
    transport: Box<dyn Transport>,
    offer_sdp: &str,
    offer_candidates: &[String],
) -> Result<(WebRtcPeer, P2pAnswerPayload), WebrtcError> {
    let mut peer = WebRtcPeer::with_transport(local_addr, transport);
    // Task 3 的 fMP4 下行占位：DataChannel 收到远端二进制先记日志。
    peer.on_data(Box::new(|d| tracing::debug!("p2p datachannel rx {} bytes", d.len())));
    let sdp = peer.answer_offer(offer_sdp)?;
    for c in offer_candidates {
        match Candidate::from_sdp_string(c) {
            Ok(cand) => peer.add_remote_candidate(cand),
            Err(e) => tracing::warn!("desktop:p2p-offer bad remote candidate dropped ({e}): {c}"),
        }
    }
    let answer = P2pAnswerPayload {
        sdp,
        candidates: peer.local_candidates(),
    };
    Ok((peer, answer))
}

#[cfg(test)]
pub(crate) mod testutil {
    //! 内存传输测试设施：双 peer 无 socket 握手/收发（`agent/p2p.rs` 的
    //! 镜像投递测试也复用）。Arc<Mutex<>>（而非 Rc<RefCell>）：`Transport: Send`，
    //! 内存传输也必须 Send（P2P 驱动任务在 tokio::spawn 里持有 peer）。

    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::{Transport, WebRtcPeer};

    #[derive(Default)]
    pub(crate) struct MemState {
        /// (source, dest, data)
        pub(crate) outbox: VecDeque<(SocketAddr, SocketAddr, Vec<u8>)>,
        /// (source, data)
        pub(crate) inbox: VecDeque<(SocketAddr, Vec<u8>)>,
    }

    #[derive(Clone)]
    pub(crate) struct MemTransport {
        pub(crate) state: Arc<Mutex<MemState>>,
        pub(crate) local: SocketAddr,
    }

    impl Transport for MemTransport {
        fn send(&mut self, dest: SocketAddr, data: &[u8]) {
            self.state
                .lock()
                .unwrap()
                .outbox
                .push_back((self.local, dest, data.to_vec()));
        }
        fn recv(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
            self.state.lock().unwrap().inbox.pop_front()
        }
    }

    /// 把所有未发报文搬到对端 inbox；返回是否搬动了东西。
    pub(crate) fn move_packets(sa: &Arc<Mutex<MemState>>, sb: &Arc<Mutex<MemState>>) -> bool {
        let mut moved = false;
        {
            let mut a = sa.lock().unwrap();
            let mut b = sb.lock().unwrap();
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
    pub(crate) fn pump(
        a: &mut WebRtcPeer,
        b: &mut WebRtcPeer,
        sa: &Arc<Mutex<MemState>>,
        sb: &Arc<Mutex<MemState>>,
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

    pub(crate) fn test_peer(ip: u8, port: u16, state: &Arc<Mutex<MemState>>) -> WebRtcPeer {
        let addr: SocketAddr = format!("147.147.147.{ip}:{port}").parse().unwrap();
        let t = MemTransport {
            state: state.clone(),
            local: addr,
        };
        WebRtcPeer::with_transport(addr, Box::new(t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::testutil::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 计划 Step 2 原测试：offer/answer JSON 往返 + 状态机存活。
    /// 0.23 下接 answer 后未 pump 前 ICE 停在 New（映射为 Connecting），
    /// 与计划断言 `state() == Connecting` 一致，不改意图。
    #[test]
    fn peer_creates_offer_and_parses_answer() {
        let sa = Arc::new(Mutex::new(MemState::default()));
        let sb = Arc::new(Mutex::new(MemState::default()));
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
        let sa = Arc::new(Mutex::new(MemState::default()));
        let sb = Arc::new(Mutex::new(MemState::default()));
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

    // ── Task 2：P2P 信令通道（agent 应答侧）─────────────────────
    // TDD RED→GREEN：先有这些断言，再实现 `begin_answer`（纯函数）。

    /// agent 收到 `desktop:p2p-offer {sdp, candidates[]}` → 产出
    /// `desktop:p2p-answer {sdp, candidates[]}`（含本地 host candidate）。
    #[test]
    fn p2p_agent_answers_offer_with_sdp_and_candidates() {
        // 浏览器侧 offerer peer（Task 3 形态）：真实 str0m make_offer。
        let sb = Arc::new(Mutex::new(MemState::default()));
        let mut browser = test_peer(1, 53000, &sb);
        let offer = browser.make_offer().unwrap();

        // agent 侧：经纯函数 `begin_answer` 应答（注入内存传输，无 socket）。
        let sa = Arc::new(Mutex::new(MemState::default()));
        let agent_addr: SocketAddr = "147.147.147.2:53001".parse().unwrap();
        let t = MemTransport {
            state: sa.clone(),
            local: agent_addr,
        };
        let (mut agent, answer) = begin_answer(agent_addr, Box::new(t), &offer, &[]).unwrap();

        assert!(
            !answer.sdp.is_empty(),
            "answer sdp must not be empty"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&answer.sdp).is_ok(),
            "answer sdp must be serializable JSON"
        );
        assert_eq!(
            answer.candidates.len(),
            1,
            "agent must advertise exactly one host candidate"
        );
        assert!(
            answer.candidates[0].starts_with("candidate:"),
            "candidate must be a standard candidate:... string, got {:?}",
            answer.candidates
        );
        // 接 answer 后处于建连中（未失败），远端候选可注入。
        assert_eq!(agent.state(), WebRtcState::Connecting);
        let browser_cands = browser.local_candidates();
        for c in &browser_cands {
            let cand = Candidate::from_sdp_string(c).expect("browser candidate must parse");
            agent.add_remote_candidate(cand);
        }
        assert_eq!(agent.state(), WebRtcState::Connecting);
    }

    // ── Task 3：真实浏览器 RFC SDP 互操作 ──────────────────────
    // RED→GREEN：浏览器 `pc.createOffer()` 产 RFC 4566 文本（非 str0m JSON），
    // agent 必须能解析并以同格式应答（JSON-only 会让真实浏览器协商失败）。

    /// 浏览器式 RFC offer → begin_answer → RFC 文本 answer（浏览器
    /// setRemoteDescription 可直接解），且状态机进入建连中。
    #[test]
    fn p2p_rfc_browser_offer_is_answered_in_rfc_format() {
        let sb = Arc::new(Mutex::new(MemState::default()));
        let mut browser = test_peer(1, 53300, &sb);
        let offer_json = browser.make_offer().unwrap();
        // 浏览器不会发出 str0m JSON——把 JSON 转成 RFC 文本再走应答侧。
        let offer_obj: SdpOffer = serde_json::from_str(&offer_json).unwrap();
        let offer_rfc = offer_obj.to_sdp_string();
        assert!(offer_rfc.starts_with("v=0"), "RFC offer must be plain SDP text");

        let sa = Arc::new(Mutex::new(MemState::default()));
        let agent_addr: SocketAddr = "147.147.147.2:53301".parse().unwrap();
        let t = MemTransport { state: sa.clone(), local: agent_addr };
        let (mut agent, answer) = begin_answer(agent_addr, Box::new(t), &offer_rfc, &[]).unwrap();

        assert!(
            answer.sdp.starts_with("v=0"),
            "answer to a browser RFC offer must be RFC text, got {}",
            &answer.sdp[..answer.sdp.len().min(40)]
        );
        assert_eq!(answer.candidates.len(), 1, "host candidate still advertised");
        assert_eq!(agent.state(), WebRtcState::Connecting);

        // 浏览器侧可接受该 RFC answer（handle_answer 走 RFC 解析）。
        browser.handle_answer(&answer.sdp).unwrap();
        assert_eq!(browser.state(), WebRtcState::Connecting);
    }

    /// Offer 生产侧：str0m JSON 产物转出 RFC 文本应可再解析（保证测试夹具
    /// 与真实浏览器形态的转录不破）。
    #[test]
    fn p2p_sdp_json_roundtrips_to_rfc_text() {
        let sb = Arc::new(Mutex::new(MemState::default()));
        let mut browser = test_peer(1, 53350, &sb);
        let offer_json = browser.make_offer().unwrap();
        let offer: SdpOffer = serde_json::from_str(&offer_json).unwrap();
        let rfc = offer.to_sdp_string();
        let reparsed = SdpOffer::from_sdp_string(&rfc);
        assert!(reparsed.is_ok(), "RFC text must round-trip through the parser");
    }

    /// offer 内嵌候选注入不炸：坏候选被跳过不影响应答。
    #[test]
    fn p2p_offer_with_bad_candidates_still_answers() {
        let sb = Arc::new(Mutex::new(MemState::default()));
        let mut browser = test_peer(1, 53100, &sb);
        let offer = browser.make_offer().unwrap();

        let sa = Arc::new(Mutex::new(MemState::default()));
        let agent_addr: SocketAddr = "147.147.147.2:53101".parse().unwrap();
        let t = MemTransport {
            state: sa.clone(),
            local: agent_addr,
        };
        let bad = vec!["this is not a candidate".to_string()];
        let (_agent, answer) = begin_answer(agent_addr, Box::new(t), &offer, &bad).unwrap();
        assert!(!answer.sdp.is_empty(), "answer sdp must not be empty");
    }

    /// 纯函数产出的 answer 可被浏览器式 peer 接受，并全流程握手至 Connected
    /// （agent 侧 peer 由 begin_answer 创建，走真实信令路径）。
    #[test]
    fn p2p_full_handshake_via_begin_answer() {
        let sa = Arc::new(Mutex::new(MemState::default()));
        let sb = Arc::new(Mutex::new(MemState::default()));
        let mut browser = test_peer(1, 53200, &sb);
        let offer = browser.make_offer().unwrap();

        let agent_addr: SocketAddr = "147.147.147.2:53201".parse().unwrap();
        let t = MemTransport {
            state: sa.clone(),
            local: agent_addr,
        };
        let (mut agent, answer) = begin_answer(agent_addr, Box::new(t), &offer, &[]).unwrap();

        // 浏览器接受 answer + agent 的 host candidate。
        browser.handle_answer(&answer.sdp).unwrap();
        for c in &answer.candidates {
            browser.add_remote_candidate(Candidate::from_sdp_string(c).unwrap());
        }
        // agent 侧浏览器候选：offer sdp 内联候选由 accept_offer 解析，这里
        // 再按信令协议显式补喂（浏览器 local_candidates 与 sdp 内联一致）。
        let browser_cands = browser.local_candidates();
        for c in &browser_cands {
            agent.add_remote_candidate(Candidate::from_sdp_string(c).unwrap());
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut agent, &mut browser, &sa, &sb);
            if agent.state() == WebRtcState::Connected && browser.state() == WebRtcState::Connected {
                break;
            }
            if Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            agent.state(),
            WebRtcState::Connected,
            "agent peer must reach Connected via begin_answer flow"
        );
        assert_eq!(
            browser.state(),
            WebRtcState::Connected,
            "browser peer must reach Connected"
        );
    }

    /// 生产 UDP 传输：本地绑一个 0.0.0.0:0 socket 可收发（回环自聊）。
    #[test]
    fn p2p_udp_transport_binds_and_loops() {
        let mut a = UdpTransport::bind("0.0.0.0:0").expect("bind");
        let mut b = UdpTransport::bind("0.0.0.0:0").expect("bind");
        let a_addr = a.local_addr();
        let b_addr = b.local_addr();
        assert_ne!(a_addr.port(), 0, "must get a real ephemeral port");
        assert_ne!(a_addr.port(), b_addr.port(), "two binds must differ");

        a.send(b_addr, b"ping");
        b.send(a_addr, b"pong");
        // recv 是非阻塞轮询：先发后收，回环最迟几 ms 内到达。源 IP 由内核按
        // 路由挑（0.0.0.0 绑定发包的实际源 IP），只校验端口与载荷。
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some((src, data)) = b.recv() {
                assert_eq!(src.port(), a_addr.port(), "source port must be a's port");
                assert_eq!(data, b"ping");
                break;
            }
            if Instant::now() > deadline {
                panic!("udp loopback recv timed out");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        loop {
            if let Some((src, data)) = a.recv() {
                assert_eq!(src.port(), b_addr.port(), "source port must be b's port");
                assert_eq!(data, b"pong");
                break;
            }
            if Instant::now() > deadline {
                panic!("udp loopback recv timed out");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // ── Task 6：真实 UDP socket 吞吐专项 ──────────────────────
    // 内存传输（mem-transport）看不到网络延迟：SACK 即时回环 → cwnd 爆发式增长，
    // 测得 57MB/s（controller /tmp/str0m-gate 实验）。本组测试用**真实 UDP 回环
    // socket** 走完整 ICE/DTLS/SCTP 栈，并可通过 `drive_delay` 注入链路延迟
    // （模拟真实网络的 RTT），诚实复现 cwnd 慢启动对延迟的敏感。

    /// 真实 UDP 回环 socket 双 peer 握手到 Connected。`reliable=true` 用默认
    /// 可靠有序通道；false 用 `MaxRetransmits{0} + unordered`（与生产 make_offer
    /// 一致）。`drive_delay` 于每次 drive 之间注入，模拟链路 RTT。
    fn socket_pair_connected(
        reliable: bool,
        drive_delay: Duration,
    ) -> (WebRtcPeer, WebRtcPeer) {
        let t_a = UdpTransport::bind("127.0.0.1:0").expect("bind A");
        let a_addr = t_a.local_addr();
        let mut a = WebRtcPeer::with_transport(a_addr, Box::new(t_a));
        let config = if reliable {
            ChannelConfig {
                label: "desktop".to_string(),
                ..Default::default()
            }
        } else {
            ChannelConfig {
                label: "desktop".to_string(),
                ordered: false,
                reliability: Reliability::MaxRetransmits { retransmits: 0 },
                ..Default::default()
            }
        };
        let offer = a.make_offer_with_config(config).expect("make offer");

        let t_b = UdpTransport::bind("127.0.0.1:0").expect("bind B");
        let b_addr = t_b.local_addr();
        let mut b = WebRtcPeer::with_transport(b_addr, Box::new(t_b));
        let answer = b.answer_offer(&offer).expect("answer offer");
        a.handle_answer(&answer).expect("handle answer");
        // 显式互喂 host candidate（offer sdp 内联 + 信令候选双保险）。
        for c in b.local_candidates() {
            a.add_remote_candidate(str0m::Candidate::from_sdp_string(&c).expect("B cand"));
        }
        for c in a.local_candidates() {
            b.add_remote_candidate(str0m::Candidate::from_sdp_string(&c).expect("A cand"));
        }

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            a.drive(Instant::now());
            b.drive(Instant::now());
            if a.state() == WebRtcState::Connected && b.state() == WebRtcState::Connected {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "socket pair handshake timed out (reliable={reliable})"
            );
            std::thread::sleep(drive_delay);
        }
        (a, b)
    }

    /// 跑一次吞吐窗口：`window` 秒内持续把 16KiB 消息塞给 sender（SCTP 背压时
    /// 退避重试），统计接收端实际收到的总字节数。返回 (发送字节, 接收字节, MB/s)。
    fn socket_throughput_once(
        reliable: bool,
        drive_delay: Duration,
        window: Duration,
    ) -> (usize, usize, f64) {
        let (mut sender, mut receiver) = socket_pair_connected(reliable, drive_delay);
        let received = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let cb = received.clone();
        receiver.on_data(Box::new(move |d| *cb.lock().unwrap() += d.len()));

        const CHUNK: usize = 16 * 1024;
        let start = Instant::now();
        let mut sent: usize = 0;
        let mut stalls: u32 = 0;
        while Instant::now() - start < window {
            let now = Instant::now();
            sender.drive(now);
            receiver.drive(now);
            // 连续推入多条直到写缓冲满（对齐生产 drain_video_to_channel 的
            // 持续发送语义；capped 防单轮无限占用）。
            for _ in 0..256 {
                match sender.send(&[0u8; CHUNK]) {
                    Ok(()) => {
                        sent += CHUNK;
                        stalls = 0;
                    }
                    Err(_) => {
                        // SCTP 背压（cwnd/写缓冲满）：退避，避免忙轮询占满 CPU。
                        stalls += 1;
                        if stalls % 8 == 0 {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        break;
                    }
                }
            }
            if !drive_delay.is_zero() {
                std::thread::sleep(drive_delay);
            }
        }
        let elapsed = Instant::now().saturating_duration_since(start);
        let recv = *received.lock().unwrap();
        let mbps = recv as f64 / 1024.0 / 1024.0 / elapsed.as_secs_f64().max(1e-9);
        (sent, recv, mbps)
    }

    /// 真实 socket 吞吐复测（主要交付物）：`--ignored --nocapture` 运行。
    /// 回环 RTT≈0 → 数据通路/库层上限；注入 drive 延迟模拟 WAN RTT → 可见
    /// cwnd 慢启动对延迟敏感（可靠/不可靠都被拖低——SCTP cwnd 增长依赖 SACK
    /// 往返，与 DCEP 可靠参数无关）。结果用于报告，不做硬阈值断言。
    #[test]
    #[ignore = "perf smoke: run with cargo test -- --ignored --nocapture"]
    fn socket_throughput_reliable_vs_unreliable() {
        let cases: &[(bool, Duration, &str)] = &[
            (false, Duration::from_millis(1), "unreliable 1ms (loop近零RTT)"),
            (true, Duration::from_millis(1), "reliable   1ms"),
            (false, Duration::from_millis(5), "unreliable 5ms (生产 drive tick)"),
            (true, Duration::from_millis(5), "reliable   5ms"),
            (false, Duration::from_millis(50), "unreliable 50ms (模拟 WAN RTT)"),
            (true, Duration::from_millis(50), "reliable   50ms"),
        ];
        let window = Duration::from_secs(4);
        println!("=== real-socket DataChannel throughput (loopback, 16KiB msgs, {}s window) ===", window.as_secs());
        for (reliable, delay, name) in cases {
            let (sent, recv, mbps) = socket_throughput_once(*reliable, *delay, window);
            println!("{name:<40} sent={} recv={} {mbps:.2} MB/s", sent, recv);
        }
    }

    /// 真实 socket 发送语义无回归（普通测试，随套件跑）：回环上不可靠+无序
    /// 通道投递小载荷（模拟 init 段 + 若干帧），字节必须完整到达且内容一致。
    #[test]
    fn socket_pair_send_path_unreliable_delivers_bytes() {
        let (mut sender, mut receiver) = socket_pair_connected(false, Duration::from_millis(1));
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cb = got.clone();
        receiver.on_data(Box::new(move |d| cb.lock().unwrap().extend_from_slice(&d)));

        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut sent_ok = false;
        loop {
            if !sent_ok {
                sent_ok = sender.send(&payload).is_ok();
            }
            sender.drive(Instant::now());
            receiver.drive(Instant::now());
            if sent_ok && got.lock().unwrap().len() >= payload.len() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "unreliable send path did not deliver 4KiB in time"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let g = got.lock().unwrap();
        assert_eq!(
            g.as_slice(),
            payload.as_slice(),
            "delivered bytes must equal payload on the unreliable channel"
        );
    }
}
