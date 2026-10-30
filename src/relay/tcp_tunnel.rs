//! relay 侧纯 TCP 字节管道（MYS-2383 阶段6，协议见 `src/agent/tcp_tunnel.rs`）。
//!
//! agent 只做字节搬运；本模块把一条 agent 上的 TCP 连接包装成实现了
//! [`AsyncRead`] + [`AsyncWrite`] 的 [`TunnelStream`]，好让 `web_map.rs` 在上面
//! 直接跑 `hyper` 的 HTTP/1.1 客户端（含 WebSocket 升级）与 rustls。
//!
//! # 与 agent 的约定（照抄协议，不重新设计）
//!
//! relay → agent：`tcp:open` / `tcp:data` / `tcp:ack` / `tcp:eof` / `tcp:close`；
//! agent → relay：`tcp:opened` / `tcp:data` / `tcp:ack` / `tcp:eof` /
//! `tcp:closed` / `tcp:error`。
//!
//! # 两条约束决定了这里的形状
//!
//! 1. **路由热路径不能 await**：`route_agent_message` 跑在该 agent 唯一的上行
//!    任务上，任何 await（哪怕拿一把 tokio 异步锁）都会把终端输出与 MCP 回包
//!    堵在后面。因此连接表用 `std::sync::Mutex`（临界区里全是哈希操作与
//!    `try_send`），下行 ack/close 走 [`send_nonblocking`]。
//! 2. **内存有界**：relay→agent 方向遵守 512 KiB「已发未确认」窗口（超了就
//!    `Poll::Pending`，`tcp:ack` 唤醒）；agent→relay 方向读者**整条消息消费完**
//!    才回 `tcp:ack` —— 读者不读，agent 的窗口就满，背压一路传回目标站。
//!    agent 的窗口按**字节**计，小消息可以很多条（WebSocket 小帧、SSE），所以
//!    读队列按字节记账：排队字节超过窗口（或条数超过 [`MAX_QUEUED_MSGS`]）才
//!    视为 agent 违约并中止连接。
//!
//! # 终态
//!
//! 每条连接只结束一次：正常 `closed`、agent `error`、本侧违约、或会话清理。
//! 之后 [`Drop`] **不再**发 `tcp:close` —— 只是把记录摘掉、写任务 abort。

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};

use crate::proto::Message as ProtoMessage;
use crate::relay::SharedState;

/// 能力名：agent 注册时必须声明它，`/agent/web/map` 才允许建映射。必须与
/// agent 侧 `src/agent/tcp_tunnel.rs::CAPABILITY` 一致。
pub const CAPABILITY: &str = "net:tcp-v1";

/// 单条 `tcp:data` 的原始字节上限（base64 前）。与 agent 侧一致。
pub const MAX_CHUNK_BYTES: usize = 48 * 1024;

/// relay→agent 方向「已发未确认」字节上限。与 agent 侧 `WINDOW_BYTES` 一致。
pub const WINDOW_BYTES: usize = 512 * 1024;

/// 每连接 agent→relay 方向排队消息条数的硬上限（防御违约 agent 用海量空/小
/// 消息撑爆内存；守约的 agent 受 [`WINDOW_BYTES`] 字节窗口约束）。
pub const MAX_QUEUED_MSGS: usize = 8192;

/// 每会话同时存在的管道上限。agent 上限 32，relay 留余量到 24：一个页面的
/// HTTP 连接池（≤6）加若干 WebSocket 足够用，又不会把 agent 的名额顶满。
pub const MAX_TUNNELS_PER_SESSION: usize = 24;

/// 建连（`tcp:open` → `tcp:opened`）的默认超时。
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// 下发 `tcp:open` 等控制消息时等 bulk 空位的上限。
const DOWNLINK_TIMEOUT: Duration = Duration::from_secs(5);

/// 每连接待写队列容量（条）。
const WRITE_QUEUE: usize = 8;

/// 打开管道失败的原因。`web_map` 据此映射 HTTP 状态码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// 该会话没有在线的 agent（bulk 下行通道不存在/已关闭）。
    Offline,
    /// 该会话的管道数已达上限。
    Busy,
    /// agent 明确报错，带 agent 的 kind 与说明。
    Agent { kind: String, message: String },
    /// `tcp:opened` 未在超时内到达。
    Timeout,
    /// 下行通道忙（投递 `tcp:open` 超时）。
    Downlink,
}

#[derive(Debug, Clone)]
struct ErrInfo {
    kind: io::ErrorKind,
    message: String,
}

impl ErrInfo {
    fn new(kind: io::ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn to_io(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

/// agent 报的 `kind` → `io::ErrorKind`。
fn kind_to_io_kind(kind: &str) -> io::ErrorKind {
    match kind {
        "timeout" => io::ErrorKind::TimedOut,
        "protocol" => io::ErrorKind::InvalidData,
        "io" => io::ErrorKind::ConnectionReset,
        "invalid" => io::ErrorKind::InvalidInput,
        "busy" => io::ErrorKind::WouldBlock,
        _ => io::ErrorKind::Other,
    }
}

/// agent → relay 的一条回包（解码后）。
#[derive(Debug)]
enum ReadEvent {
    Data(Vec<u8>),
    Eof,
    Closed,
    Error(ErrInfo),
}

/// relay → agent 的写指令。`Eof` 与 `Data` 同队列，保证 eof 排在所有 data 之后。
#[derive(Debug)]
enum WriteCmd {
    Data { seq: u64, data: Vec<u8> },
    Eof,
}

/// 一条连接两端共享的状态。锁内只有标量操作，没有 await。
#[derive(Default)]
struct Shared {
    /// relay→agent 的写 seq（从 0 连续）。
    write_seq: u64,
    /// 已发给 agent 但尚未被 `tcp:ack` 确认的字节数。
    unacked: usize,
    /// `poll_shutdown` 已调用。
    write_closed: bool,
    write_error: Option<ErrInfo>,
    read_error: Option<ErrInfo>,
    /// 已进入终态：`Drop` 不再发 `tcp:close`。
    terminal: bool,
    /// 写路径被窗口/队列挡住时挂在这里；`tcp:ack` 或写任务腾出一格后唤醒。
    waker: Option<Waker>,
    /// agent→relay 读队列里尚未被读者取走的字节数 / 条数（违约判定用）。
    queued_bytes: usize,
    queued_msgs: usize,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn wake(sh: &mut Shared) {
    if let Some(w) = sh.waker.take() {
        w.wake();
    }
}

/// 连接表里的一条记录。路由与清理只碰这里，不碰 [`TunnelStream`]（后者在
/// 浏览器侧，随时可能被 drop）。
struct Entry {
    session_id: String,
    read_tx: mpsc::UnboundedSender<ReadEvent>,
    /// 下行通道：路由路径要发 `tcp:ack` / `tcp:close`，不能为它 await
    /// `agent_broadcast` 的异步锁。
    bulk: mpsc::Sender<String>,
    shared: Arc<Mutex<Shared>>,
    /// agent→relay 方向期望的下一条 `tcp:data` 的 seq。
    read_seq: Arc<AtomicU64>,
    /// `tcp:opened` / 首条 `tcp:error` 的投递口（`open` 只取一次）。
    opened: Option<oneshot::Sender<Result<(), OpenError>>>,
    /// 写方向任务的 abort 句柄。
    writer: tokio::task::AbortHandle,
}

/// 全进程的管道表（挂在 `SharedState::tcp_tunnels`）。
///
/// 用 `std::sync::Mutex` 而不是 tokio 异步锁：访问点既有热路径（agent 上行
/// 路由）又有 `Drop`（不能 await），临界区全是哈希操作。
pub struct TunnelStore {
    conns: Mutex<HashMap<String, Entry>>,
    by_session: Mutex<HashMap<String, usize>>,
    open_timeout_ms: AtomicU64,
    max_per_session: AtomicUsize,
}

impl Default for TunnelStore {
    fn default() -> Self {
        Self::new()
    }
}

/// `(session_id, conn_id)` → 表 key。conn_id 本身已是 128 bit 随机；分隔符让
/// 「同一 conn_id 属于另一个会话」也不会撞到同一条记录。
fn key_of(session_id: &str, conn_id: &str) -> String {
    format!("{session_id}\u{1}{conn_id}")
}

impl TunnelStore {
    pub fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            by_session: Mutex::new(HashMap::new()),
            open_timeout_ms: AtomicU64::new(OPEN_TIMEOUT.as_millis() as u64),
            max_per_session: AtomicUsize::new(MAX_TUNNELS_PER_SESSION),
        }
    }

    /// 建连超时（单测压到毫秒级）。
    pub fn open_timeout(&self) -> Duration {
        Duration::from_millis(self.open_timeout_ms.load(Ordering::Relaxed))
    }

    pub fn set_open_timeout(&self, d: Duration) {
        self.open_timeout_ms
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }

    pub fn max_per_session(&self) -> usize {
        self.max_per_session.load(Ordering::Relaxed)
    }

    pub fn set_max_per_session(&self, n: usize) {
        self.max_per_session.store(n, Ordering::Relaxed);
    }

    /// 某会话当前管道数（观测/测试用）。
    pub fn count_session(&self, session_id: &str) -> usize {
        lock(&self.by_session).get(session_id).copied().unwrap_or(0)
    }

    /// 全进程管道数（观测用）。
    pub fn count_all(&self) -> usize {
        lock(&self.conns).len()
    }

    fn remove(&self, session_id: &str, conn_id: &str) -> Option<Entry> {
        let entry = lock(&self.conns).remove(&key_of(session_id, conn_id));
        if entry.is_some() {
            let mut by = lock(&self.by_session);
            if let Some(n) = by.get_mut(session_id) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    by.remove(session_id);
                }
            }
        }
        entry
    }

    /// 会话消失（agent 断线 / kick / 空闲回收）：该会话全部管道读写报错并被
    /// 移除。返回处理的管道数。
    pub fn close_session(&self, session_id: &str) -> usize {
        let keys: Vec<String> = lock(&self.conns)
            .iter()
            .filter(|(_, e)| e.session_id == session_id)
            .map(|(k, _)| k.clone())
            .collect();
        let mut n = 0;
        for key in keys {
            if let Some(entry) = lock(&self.conns).remove(&key) {
                n += 1;
                fail_entry(&entry);
            }
        }
        lock(&self.by_session).remove(session_id);
        n
    }

    /// 登记一条连接；该会话已到上限时返回 `Err(Busy)`。
    fn insert(&self, entry: Entry, conn_id: &str, session_id: &str) -> Result<(), OpenError> {
        let max = self.max_per_session();
        {
            let mut by = lock(&self.by_session);
            let cur = by.get(session_id).copied().unwrap_or(0);
            if cur >= max {
                return Err(OpenError::Busy);
            }
            by.insert(session_id.to_string(), cur + 1);
        }
        lock(&self.conns).insert(key_of(session_id, conn_id), entry);
        Ok(())
    }
}

/// 让一条记录的读写都报错并停掉写任务。用于会话清理与本侧违约。
///
/// 顺序有意如此：先置错误（读者/写者下一次 poll 立刻看到），再摘记录
/// （drop `read_tx` → 挂起的读者被 `poll_recv` 唤醒为 `Ready(None)`），
/// 最后 abort 写任务。错误先于唤醒落地，所以读者拿到的是 `Err` 而不是 EOF。
fn fail_entry(entry: &Entry) {
    {
        let mut sh = lock(&entry.shared);
        sh.terminal = true;
        let err = ErrInfo::new(io::ErrorKind::ConnectionAborted, "tcp tunnel closed");
        sh.read_error.get_or_insert(err.clone());
        sh.write_error.get_or_insert(err);
        wake(&mut sh);
    }
    entry.writer.abort();
}

/// 非阻塞下发一条下行消息（路由与 `Drop` 路径专用）。
///
/// `try_send` 成功就返回；通道满时交给短命任务去等 —— 这两条路径分别在 agent
/// 唯一的上行任务里和 `Drop` 里，**绝不等候**。
fn send_nonblocking(bulk: &mpsc::Sender<String>, msg: String) {
    if let Err(mpsc::error::TrySendError::Full(msg)) = bulk.try_send(msg) {
        let bulk = bulk.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                DOWNLINK_TIMEOUT,
                crate::relay::file_transfer::deliver_bulk(&bulk, msg),
            )
            .await;
        });
    }
}

/// 取该会话的 bulk 下行通道；没有 = agent 离线。
async fn bulk_of(state: &SharedState, session_id: &str) -> Option<mpsc::Sender<String>> {
    state
        .agent_broadcast
        .read()
        .await
        .get(session_id)
        .and_then(|cm| cm.agent_bulk.clone())
        .filter(|tx| !tx.is_closed())
}

/// 打开一条到 `host:port` 的管道。
///
/// 成功返回的 [`TunnelStream`] 已经收到 `tcp:opened`（目标 TCP 连接已建立）。
pub async fn open(
    state: &Arc<SharedState>,
    session_id: &str,
    host: &str,
    port: u16,
) -> Result<TunnelStream, OpenError> {
    let store = state.tcp_tunnels.clone();
    let Some(bulk) = bulk_of(state, session_id).await else {
        return Err(OpenError::Offline);
    };

    let conn_id = random_conn_id();
    let (read_tx, read_rx) = mpsc::unbounded_channel::<ReadEvent>();
    let (data_tx, data_rx) = mpsc::channel::<WriteCmd>(WRITE_QUEUE);
    let (opened_tx, opened_rx) = oneshot::channel::<Result<(), OpenError>>();
    let shared = Arc::new(Mutex::new(Shared::default()));
    let read_seq = Arc::new(AtomicU64::new(0));

    // 写方向任务：data/eof → bulk（带背压）。它自己 await，所以上行路由与
    // 浏览器侧都不用等。
    let writer = tokio::spawn(writer_loop(
        bulk.clone(),
        session_id.to_string(),
        conn_id.clone(),
        data_rx,
        shared.clone(),
    ));

    let entry = Entry {
        session_id: session_id.to_string(),
        read_tx,
        bulk: bulk.clone(),
        shared: shared.clone(),
        read_seq,
        opened: Some(opened_tx),
        writer: writer.abort_handle(),
    };
    if let Err(e) = store.insert(entry, &conn_id, session_id) {
        writer.abort();
        return Err(e);
    }

    let mut stream = TunnelStream {
        conn_id: conn_id.clone(),
        session_id: session_id.to_string(),
        store: store.clone(),
        bulk: bulk.clone(),
        shared,
        data_tx,
        read_rx,
        pending: Vec::new(),
        pending_pos: 0,
        pending_ack: 0,
        read_eof: false,
        finished: false,
        _writer: writer,
    };

    // 下发 `tcp:open`。
    let proto = json!({
        "type": "tcp:open",
        "session_id": session_id,
        "payload": { "conn_id": &conn_id, "host": host, "port": port }
    })
    .to_string();
    if tokio::time::timeout(
        DOWNLINK_TIMEOUT,
        crate::relay::file_transfer::deliver_bulk(&bulk, proto),
    )
    .await
    .is_err()
    {
        if let Some(e) = store.remove(session_id, &conn_id) {
            fail_entry(&e);
        }
        stream.finished = true;
        return Err(OpenError::Downlink);
    }

    // 等 `tcp:opened` / `tcp:error`。
    match tokio::time::timeout(store.open_timeout(), opened_rx).await {
        Ok(Ok(Ok(()))) => Ok(stream),
        // agent 明确拒绝：记录已由路由侧摘除并置终态。
        Ok(Ok(Err(e))) => Err(e),
        // oneshot 被 drop：记录被清理（agent 断线 / 会话清理）。
        Ok(Err(_)) => Err(OpenError::Offline),
        Err(_) => {
            // 超时：主动 `tcp:close`，让 agent 把可能正在建立的连接掐掉。
            if let Some(e) = store.remove(session_id, &conn_id) {
                send_nonblocking(
                    &e.bulk,
                    json!({
                        "type": "tcp:close",
                        "session_id": session_id,
                        "payload": { "conn_id": &conn_id }
                    })
                    .to_string(),
                );
                fail_entry(&e);
            }
            stream.finished = true;
            Err(OpenError::Timeout)
        }
    }
}

fn random_conn_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: [u8; 16] = rng.gen();
    hex::encode(bytes)
}

/// 写方向任务：`WriteCmd` → bulk 下行（`tcp:data` / `tcp:eof`）。
///
/// 这里可以 await（不在路由路径上）：bulk 通道自身的背压就是我们要的节流。
async fn writer_loop(
    bulk: mpsc::Sender<String>,
    session_id: String,
    conn_id: String,
    mut rx: mpsc::Receiver<WriteCmd>,
    shared: Arc<Mutex<Shared>>,
) {
    while let Some(cmd) = rx.recv().await {
        let msg = match cmd {
            WriteCmd::Data { seq, data } => json!({
                "type": "tcp:data",
                "session_id": &session_id,
                "payload": {
                    "conn_id": &conn_id,
                    "seq": seq,
                    "data": B64.encode(&data),
                }
            }),
            WriteCmd::Eof => json!({
                "type": "tcp:eof",
                "session_id": &session_id,
                "payload": { "conn_id": &conn_id }
            }),
        }
        .to_string();
        crate::relay::file_transfer::deliver_bulk(&bulk, msg).await;
        // 队列腾出一格：唤醒可能被 `try_send` Full 挡住的写者。
        let mut sh = lock(&shared);
        wake(&mut sh);
    }
}

// ── agent → relay 路由 ──────────────────────────────────────────────

/// 路由侧对一条回包的判定。
enum Routed {
    /// 正常处理（或未知连接，静默丢弃）。
    Handled,
    /// agent 违约：调用方要发 `tcp:close` 并让读写报错。
    Violation(&'static str),
}

/// `ws::route_agent_message` 的管道分支。
///
/// 返回 `true` 表示这条消息属于 TCP 管道（调用方不再走广播/录制/MCP）。
/// 未知连接静默丢弃：迟到回包（浏览器早已断开）是正常现象。
pub async fn route_agent_message(
    state: &Arc<SharedState>,
    session_id: &str,
    msg: &ProtoMessage,
    raw_len: usize,
) -> bool {
    let msg_type = msg.msg_type.as_str();
    if !matches!(
        msg_type,
        "tcp:opened" | "tcp:data" | "tcp:ack" | "tcp:eof" | "tcp:closed" | "tcp:error"
    ) {
        return false;
    }
    // 上行带宽记账（与终端/MCP 同一口径）：管道的大流量同样占上行。用调用方
    // 传进来的**原始**文本长度，避免在这条热路径上把 payload 再序列化一遍。
    state.bandwidth.up.consume(raw_len);

    let conn_id = msg
        .payload
        .get("conn_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if conn_id.is_empty() || conn_id.len() > 64 {
        tracing::debug!(
            session = %session_id,
            msg_type,
            "tcp tunnel: agent message without usable conn_id — dropped"
        );
        return true;
    }

    let key = key_of(session_id, conn_id);
    let (routed, bulk) = {
        let mut conns = lock(&state.tcp_tunnels.conns);
        let Some(entry) = conns.get_mut(&key) else {
            tracing::trace!(
                session = %session_id,
                conn_id,
                msg_type,
                "tcp tunnel: message for unknown connection dropped"
            );
            return true;
        };
        let bulk = entry.bulk.clone();
        let mut sh = lock(&entry.shared);
        let routed = match msg_type {
            "tcp:opened" => {
                if let Some(tx) = entry.opened.take() {
                    let _ = tx.send(Ok(()));
                }
                Routed::Handled
            }
            "tcp:data" => match parse_data(&msg.payload) {
                None => Routed::Violation("malformed tcp:data"),
                Some((seq, data)) => {
                    let want = entry.read_seq.load(Ordering::Relaxed);
                    let len = data.len();
                    if seq != want {
                        Routed::Violation("tcp:data seq out of order")
                    } else if sh.queued_bytes + len > WINDOW_BYTES
                        || sh.queued_msgs >= MAX_QUEUED_MSGS
                    {
                        // 守约的 agent 已发未确认 ≤ 窗口，排队字节不可能超过它。
                        Routed::Violation("read queue exceeds the agent window")
                    } else if entry.read_tx.send(ReadEvent::Data(data)).is_err() {
                        // 读者已不在（TunnelStream 正在 drop）：静默丢弃。
                        Routed::Handled
                    } else {
                        sh.queued_bytes += len;
                        sh.queued_msgs += 1;
                        entry.read_seq.store(want + 1, Ordering::Relaxed);
                        Routed::Handled
                    }
                }
            },
            "tcp:ack" => {
                let bytes = msg
                    .payload
                    .get("bytes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                sh.unacked = sh.unacked.saturating_sub(bytes);
                wake(&mut sh);
                Routed::Handled
            }
            "tcp:eof" => {
                let _ = entry.read_tx.send(ReadEvent::Eof);
                Routed::Handled
            }
            "tcp:closed" => {
                sh.terminal = true;
                sh.write_error.get_or_insert(ErrInfo::new(
                    io::ErrorKind::BrokenPipe,
                    "tunnel closed by agent",
                ));
                let _ = entry.read_tx.send(ReadEvent::Closed);
                wake(&mut sh);
                Routed::Handled
            }
            _ => {
                // tcp:error
                let kind = msg
                    .payload
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("io")
                    .to_string();
                let message = msg
                    .payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tunnel error")
                    .to_string();
                let err = ErrInfo::new(kind_to_io_kind(&kind), message.clone());
                sh.terminal = true;
                sh.read_error.get_or_insert(err.clone());
                sh.write_error.get_or_insert(err.clone());
                if let Some(tx) = entry.opened.take() {
                    // open 还在等：直接以 agent 的错误结束，不再等超时。
                    let _ = tx.send(Err(OpenError::Agent { kind, message }));
                }
                let _ = entry.read_tx.send(ReadEvent::Error(err));
                wake(&mut sh);
                Routed::Handled
            }
        };
        (routed, bulk)
    };

    if let Routed::Violation(reason) = routed {
        tracing::warn!(
            session = %session_id,
            conn_id,
            reason,
            "tcp tunnel: agent violates the protocol — connection aborted"
        );
        if let Some(entry) = state.tcp_tunnels.remove(session_id, conn_id) {
            send_nonblocking(
                &bulk,
                json!({
                    "type": "tcp:close",
                    "session_id": session_id,
                    "payload": { "conn_id": conn_id }
                })
                .to_string(),
            );
            fail_entry(&entry);
        }
    }
    true
}

/// 严格解析 `tcp:data`：`seq` 必须是 u64、`data` 必须是合法 base64。
fn parse_data(payload: &serde_json::Value) -> Option<(u64, Vec<u8>)> {
    let seq = payload.get("seq")?.as_u64()?;
    let data = payload.get("data")?.as_str()?;
    let bytes = B64.decode(data).ok()?;
    Some((seq, bytes))
}

// ── TunnelStream ────────────────────────────────────────────────────

/// 一条管道在 relay 侧的读写句柄：`AsyncRead` + `AsyncWrite`，可直接交给
/// `hyper::client::conn::http1` 或 rustls。
pub struct TunnelStream {
    conn_id: String,
    session_id: String,
    store: Arc<TunnelStore>,
    /// 该会话的下行 bulk 通道：`Drop` 里发 `tcp:close` 要用它。
    bulk: mpsc::Sender<String>,
    shared: Arc<Mutex<Shared>>,
    data_tx: mpsc::Sender<WriteCmd>,
    read_rx: mpsc::UnboundedReceiver<ReadEvent>,
    /// 一条 `tcp:data` 的全部字节。**整条消费完**才回 ack（流控）。
    pending: Vec<u8>,
    pending_pos: usize,
    pending_ack: usize,
    /// 已读到 EOF：后续读一律返回 0（`poll_recv` 不会再醒来）。
    read_eof: bool,
    /// `open` 的失败路径已自行收尾，`Drop` 不再补 `tcp:close`。
    finished: bool,
    _writer: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for TunnelStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelStream")
            .field("conn_id", &self.conn_id)
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl TunnelStream {
    pub fn conn_id(&self) -> &str {
        &self.conn_id
    }

    /// 非阻塞下发（ack/close）。
    fn send_ctrl(&self, msg_type: &str, payload: serde_json::Value) {
        send_nonblocking(
            &self.bulk,
            json!({
                "type": msg_type,
                "session_id": &self.session_id,
                "payload": payload,
            })
            .to_string(),
        );
    }
}

impl Drop for TunnelStream {
    fn drop(&mut self) {
        let terminal = {
            let sh = lock(&self.shared);
            sh.terminal
        };
        // 先摘记录：路由侧的迟到回包会静默丢弃，不会往一个已死的队列里塞。
        self.store.remove(&self.session_id, &self.conn_id);
        if !terminal && !self.finished {
            // 未正常结束 → 主动通知 agent 释放目标连接。
            self.send_ctrl("tcp:close", json!({ "conn_id": &self.conn_id }));
        }
        // abort 之后 writer_loop 立刻结束，不会留在 bulk 的 await 上。
        self._writer.abort();
    }
}

impl AsyncRead for TunnelStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // 终态错误优先：清理/agent 报错之后，缓冲里剩下的半条消息不该再交给
        // hyper（那会被当成一段合法但残缺的响应体）。
        {
            let sh = lock(&this.shared);
            if let Some(err) = &sh.read_error {
                this.finished = true;
                return Poll::Ready(Err(err.to_io()));
            }
        }
        if this.read_eof {
            return Poll::Ready(Ok(()));
        }
        loop {
            if this.pending_pos < this.pending.len() {
                let n = buf.remaining().min(this.pending.len() - this.pending_pos);
                buf.put_slice(&this.pending[this.pending_pos..this.pending_pos + n]);
                this.pending_pos += n;
                if this.pending_pos >= this.pending.len() {
                    // 整条消息消费完 → 归还 agent 的发送窗口。
                    let acked = this.pending_ack;
                    this.pending.clear();
                    this.pending_pos = 0;
                    this.pending_ack = 0;
                    if acked > 0 {
                        this.send_ctrl(
                            "tcp:ack",
                            json!({ "conn_id": &this.conn_id, "bytes": acked }),
                        );
                    }
                }
                return Poll::Ready(Ok(()));
            }
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            match this.read_rx.poll_recv(cx) {
                Poll::Ready(Some(ReadEvent::Data(data))) => {
                    {
                        let mut sh = lock(&this.shared);
                        sh.queued_bytes = sh.queued_bytes.saturating_sub(data.len());
                        sh.queued_msgs = sh.queued_msgs.saturating_sub(1);
                    }
                    this.pending_ack = data.len();
                    this.pending = data;
                    this.pending_pos = 0;
                }
                Poll::Ready(Some(ReadEvent::Eof)) | Poll::Ready(Some(ReadEvent::Closed)) => {
                    this.read_eof = true;
                    this.finished = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(ReadEvent::Error(err))) => {
                    this.finished = true;
                    return Poll::Ready(Err(err.to_io()));
                }
                Poll::Ready(None) => {
                    // 记录被摘除（会话清理/违约）。错误优先于 EOF。
                    this.read_eof = true;
                    this.finished = true;
                    let sh = lock(&this.shared);
                    return match &sh.read_error {
                        Some(err) => Poll::Ready(Err(err.to_io())),
                        None => Poll::Ready(Ok(())),
                    };
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for TunnelStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut sh = lock(&this.shared);
        if let Some(err) = &sh.write_error {
            return Poll::Ready(Err(err.to_io()));
        }
        if sh.write_closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "tunnel write side already shut down",
            )));
        }
        let avail = WINDOW_BYTES.saturating_sub(sh.unacked);
        if avail == 0 {
            sh.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = buf.len().min(MAX_CHUNK_BYTES).min(avail);
        let seq = sh.write_seq;
        match this.data_tx.try_send(WriteCmd::Data {
            seq,
            data: buf[..n].to_vec(),
        }) {
            Ok(()) => {
                sh.write_seq += 1;
                sh.unacked += n;
                Poll::Ready(Ok(n))
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                sh.waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                let err = ErrInfo::new(io::ErrorKind::BrokenPipe, "tunnel writer is gone");
                sh.write_error = Some(err.clone());
                Poll::Ready(Err(err.to_io()))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // 数据进了写队列就算已提交，真正的搬运由 bulk 通道负责。hyper 不依赖
        // flush 的返回时机（它只要求 poll_write 之后能读回包）。
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let tx = {
            let mut sh = lock(&this.shared);
            if sh.write_closed || sh.write_error.is_some() {
                return Poll::Ready(Ok(()));
            }
            sh.write_closed = true;
            this.data_tx.clone()
        };
        // `tcp:eof` 与 data 同队列 → 顺序天然成立。队列满时交给短命任务
        // （`poll_shutdown` 不能 await）。
        if let Err(mpsc::error::TrySendError::Full(cmd)) = tx.try_send(WriteCmd::Eof) {
            tokio::spawn(async move {
                let _ = tx.send(cmd).await;
            });
        }
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Permission;
    use crate::relay::ChannelMap;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn make_state() -> Arc<SharedState> {
        Arc::new(SharedState::new(
            String::new(),
            100 * 1024 * 1024,
            None,
            String::new(),
            String::new(),
            None,
        ))
    }

    /// 注册会话 + 能力 + bulk 下行通道；返回 (sid, bulk_rx)。
    async fn setup(state: &Arc<SharedState>) -> (String, mpsc::Receiver<String>) {
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        assert!(matches!(r.tokens[0].1, Permission::ReadWrite));
        state
            .sessions
            .update_capabilities(&sid, vec![CAPABILITY.to_string()])
            .await;
        let (bulk_tx, bulk_rx) = mpsc::channel::<String>(64);
        let mut cm = ChannelMap::new();
        cm.agent_bulk = Some(bulk_tx);
        state.agent_broadcast.write().await.insert(sid.clone(), cm);
        (sid, bulk_rx)
    }

    fn msg(sid: &str, ty: &str, payload: Value) -> String {
        json!({"type": ty, "session_id": sid, "payload": payload}).to_string()
    }

    async fn feed(state: &Arc<SharedState>, sid: &str, text: &str) {
        crate::relay::ws::route_agent_message(state, sid, text).await;
    }

    /// 从 bulk 通道取一条消息，返回解析后的 JSON。
    async fn next_bulk(rx: &mut mpsc::Receiver<String>) -> Value {
        let text = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("bulk 通道 5s 内没有下行消息")
            .expect("bulk 通道关闭");
        serde_json::from_str(&text).expect("下行消息不是 JSON")
    }

    /// 打开一条管道：后台跑 `open`，从 bulk 读到 `tcp:open`，再按 `reply`
    /// 回一条消息。返回 (open 结果, conn_id)。
    async fn open_with(
        state: &Arc<SharedState>,
        sid: &str,
        rx: &mut mpsc::Receiver<String>,
        reply: impl FnOnce(&str) -> Option<String>,
    ) -> (Result<TunnelStream, OpenError>, String) {
        let st = state.clone();
        let sid_owned = sid.to_string();
        let task = tokio::spawn(async move { open(&st, &sid_owned, "127.0.0.1", 80).await });
        let v = next_bulk(rx).await;
        assert_eq!(v["type"], "tcp:open", "首条下行必须是 tcp:open: {v}");
        let conn_id = v["payload"]["conn_id"].as_str().unwrap().to_string();
        assert_eq!(v["payload"]["host"], "127.0.0.1");
        assert_eq!(v["payload"]["port"], 80);
        if let Some(m) = reply(&conn_id) {
            feed(state, sid, &m).await;
        }
        let r = task.await.unwrap();
        (r, conn_id)
    }

    async fn open_ok(
        state: &Arc<SharedState>,
        sid: &str,
        rx: &mut mpsc::Receiver<String>,
    ) -> (TunnelStream, String) {
        let (r, conn_id) = open_with(state, sid, rx, |id| {
            Some(msg(sid, "tcp:opened", json!({ "conn_id": id })))
        })
        .await;
        (r.expect("open 应成功"), conn_id)
    }

    #[tokio::test]
    async fn test_open_success_and_close_on_drop() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (stream, conn_id) = open_ok(&state, &sid, &mut rx).await;
        assert_eq!(state.tcp_tunnels.count_session(&sid), 1);
        assert_eq!(stream.conn_id(), conn_id);
        drop(stream);
        // Drop 必须发 tcp:close（否则 agent 侧目标连接泄漏）。
        let v = next_bulk(&mut rx).await;
        assert_eq!(v["type"], "tcp:close", "{v}");
        assert_eq!(v["payload"]["conn_id"], conn_id.as_str());
        assert_eq!(state.tcp_tunnels.count_session(&sid), 0);
    }

    #[tokio::test]
    async fn test_open_rejected_by_agent() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (r, _) = open_with(&state, &sid, &mut rx, |id| {
            Some(msg(
                &sid,
                "tcp:error",
                json!({"conn_id": id, "kind": "io", "message": "connection refused"}),
            ))
        })
        .await;
        match r {
            Err(OpenError::Agent { kind, message }) => {
                assert_eq!(kind, "io");
                assert!(message.contains("refused"));
            }
            other => panic!("应返回 Agent 错误，实际 {other:?}"),
        }
        assert_eq!(state.tcp_tunnels.count_session(&sid), 0);
    }

    #[tokio::test]
    async fn test_open_timeout_sends_close() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        state
            .tcp_tunnels
            .set_open_timeout(Duration::from_millis(50));
        let (r, conn_id) = open_with(&state, &sid, &mut rx, |_| None).await;
        assert_eq!(r.err(), Some(OpenError::Timeout));
        let v = next_bulk(&mut rx).await;
        assert_eq!(v["type"], "tcp:close", "超时必须发 tcp:close: {v}");
        assert_eq!(v["payload"]["conn_id"], conn_id.as_str());
        assert_eq!(state.tcp_tunnels.count_session(&sid), 0);
    }

    #[tokio::test]
    async fn test_open_offline_without_bulk_channel() {
        let state = make_state();
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let err = open(&state, &r.session_id, "127.0.0.1", 80)
            .await
            .err()
            .expect("没有 bulk 通道必须失败");
        assert_eq!(err, OpenError::Offline);
    }

    #[tokio::test]
    async fn test_open_busy_at_per_session_limit() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        state.tcp_tunnels.set_max_per_session(1);
        let (_s1, _id1) = open_ok(&state, &sid, &mut rx).await;
        // 超配额时在下发 `tcp:open` **之前**就拒绝，所以 bulk 上不该有任何消息。
        let r = open(&state, &sid, "127.0.0.1", 80).await;
        assert_eq!(r.err(), Some(OpenError::Busy));
        assert!(
            tokio::time::timeout(Duration::from_millis(150), rx.recv())
                .await
                .is_err(),
            "超配额不应下发 tcp:open"
        );
    }

    #[tokio::test]
    async fn test_write_window_is_bounded() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (mut stream, _id) = open_ok(&state, &sid, &mut rx).await;

        // 持续排空 bulk，否则写任务被 bulk 背压挡住，测到的就不是窗口。
        let drain = tokio::spawn(async move {
            let mut n = 0usize;
            while let Some(text) = rx.recv().await {
                let v: Value = serde_json::from_str(&text).unwrap();
                if v["type"] == "tcp:data" {
                    n += B64
                        .decode(v["payload"]["data"].as_str().unwrap())
                        .unwrap()
                        .len();
                }
                if n >= WINDOW_BYTES {
                    break;
                }
            }
            n
        });

        let chunk = vec![0x41u8; MAX_CHUNK_BYTES];
        let mut written = 0usize;
        loop {
            match tokio::time::timeout(Duration::from_millis(200), stream.write(&chunk)).await {
                Ok(Ok(n)) => written += n,
                Ok(Err(e)) => panic!("写失败: {e}"),
                Err(_) => break, // Pending：窗口已满
            }
            assert!(written <= WINDOW_BYTES, "写入超过窗口上限 {written}");
        }
        assert!(
            written > WINDOW_BYTES - MAX_CHUNK_BYTES && written <= WINDOW_BYTES,
            "窗口应恰好用满，实际 {written}"
        );

        // ack 全部字节后应能继续写。
        let _ = tokio::time::timeout(Duration::from_secs(5), drain).await;
        feed(
            &state,
            &sid,
            &msg(
                &sid,
                "tcp:ack",
                json!({"conn_id": stream.conn_id(), "bytes": written}),
            ),
        )
        .await;
        let n = tokio::time::timeout(Duration::from_secs(2), stream.write(&chunk))
            .await
            .expect("ack 之后窗口应重新可用")
            .expect("写不应失败");
        assert_eq!(n, chunk.len());
    }

    #[tokio::test]
    async fn test_read_acks_only_after_full_consumption() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (mut stream, conn_id) = open_ok(&state, &sid, &mut rx).await;
        feed(
            &state,
            &sid,
            &msg(
                &sid,
                "tcp:data",
                json!({"conn_id": &conn_id, "seq": 0, "data": B64.encode(b"hello")}),
            ),
        )
        .await;

        // 只读 2 字节：不得 ack。
        let mut small = [0u8; 2];
        let n = stream.read(&mut small).await.unwrap();
        assert_eq!(&small[..n], b"he");
        assert!(
            tokio::time::timeout(Duration::from_millis(150), rx.recv())
                .await
                .is_err(),
            "未消费完整条消息前不得 ack"
        );

        // 读完整条 → 收到 ack(bytes=5)。
        let mut rest = [0u8; 8];
        let n = stream.read(&mut rest).await.unwrap();
        assert_eq!(&rest[..n], b"llo");
        let v = next_bulk(&mut rx).await;
        assert_eq!(v["type"], "tcp:ack", "{v}");
        assert_eq!(v["payload"]["bytes"], 5);
        assert_eq!(v["payload"]["conn_id"], conn_id.as_str());
    }

    #[tokio::test]
    async fn test_read_seq_gap_aborts_connection() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (mut stream, conn_id) = open_ok(&state, &sid, &mut rx).await;
        // 第一条就给 seq=1（期望 0）→ 违约。
        feed(
            &state,
            &sid,
            &msg(
                &sid,
                "tcp:data",
                json!({"conn_id": &conn_id, "seq": 1, "data": B64.encode(b"x")}),
            ),
        )
        .await;
        let mut buf = [0u8; 16];
        let r = stream.read(&mut buf).await;
        assert!(r.is_err(), "seq 乱序必须让读返回错误，实际 {r:?}");
        // 且必须给 agent 发 tcp:close。
        let v = next_bulk(&mut rx).await;
        assert_eq!(v["type"], "tcp:close", "{v}");
        assert_eq!(state.tcp_tunnels.count_session(&sid), 0);
    }

    #[tokio::test]
    async fn test_agent_eof_makes_read_return_zero() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (mut stream, conn_id) = open_ok(&state, &sid, &mut rx).await;
        feed(
            &state,
            &sid,
            &msg(&sid, "tcp:eof", json!({"conn_id": &conn_id})),
        )
        .await;
        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "agent eof 之后读必须返回 0");
        // 再读一次仍是 0（不是 Pending）。
        let n = stream.read(&mut buf).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn test_session_cleanup_fails_both_directions() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (mut stream, _id) = open_ok(&state, &sid, &mut rx).await;
        assert_eq!(state.tcp_tunnels.close_session(&sid), 1);

        let mut buf = [0u8; 8];
        let r = stream.read(&mut buf).await;
        assert!(r.is_err(), "会话清理后读必须报错，实际 {r:?}");
        let w = stream.write(b"x").await;
        assert!(w.is_err(), "会话清理后写必须报错，实际 {w:?}");
        // 已进入终态：Drop 不再补 tcp:close。
        drop(stream);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), rx.recv())
                .await
                .is_err(),
            "终态之后 Drop 不应再发 tcp:close"
        );
    }

    #[tokio::test]
    async fn test_shutdown_sends_eof_after_data() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (mut stream, conn_id) = open_ok(&state, &sid, &mut rx).await;
        stream.write_all(b"abc").await.unwrap();
        stream.shutdown().await.unwrap();

        let first = next_bulk(&mut rx).await;
        assert_eq!(first["type"], "tcp:data", "{first}");
        assert_eq!(first["payload"]["seq"], 0);
        assert_eq!(
            B64.decode(first["payload"]["data"].as_str().unwrap())
                .unwrap(),
            b"abc"
        );
        let second = next_bulk(&mut rx).await;
        assert_eq!(
            second["type"], "tcp:eof",
            "eof 必须排在 data 之后: {second}"
        );
        assert_eq!(second["payload"]["conn_id"], conn_id.as_str());
        // shutdown 之后再写必须报错。
        assert!(stream.write(b"x").await.is_err());
    }

    #[tokio::test]
    async fn test_unknown_conn_messages_are_dropped() {
        let state = make_state();
        let (sid, _rx) = setup(&state).await;
        feed(
            &state,
            &sid,
            &msg(
                &sid,
                "tcp:data",
                json!({"conn_id": "deadbeef", "seq": 0, "data": ""}),
            ),
        )
        .await;
        assert_eq!(state.tcp_tunnels.count_all(), 0);
    }

    #[tokio::test]
    async fn test_tcp_messages_never_enter_broadcast() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (stream, conn_id) = open_ok(&state, &sid, &mut rx).await;
        // route 的返回值必须为 true（= 已被管道消费，不进广播/MCP/录制）。
        let p: ProtoMessage = serde_json::from_str(&msg(
            &sid,
            "tcp:data",
            json!({"conn_id": &conn_id, "seq": 0, "data": B64.encode(b"z")}),
        ))
        .unwrap();
        assert!(
            route_agent_message(&state, &sid, &p, 0).await,
            "tcp:* 必须由管道分支消费"
        );
        assert!(
            state.agent_event_buffers.read().await.get(&sid).is_none(),
            "事件缓冲不应因 tcp:* 增长"
        );
        drop(stream);
    }

    #[test]
    fn test_kind_mapping() {
        assert_eq!(kind_to_io_kind("timeout"), io::ErrorKind::TimedOut);
        assert_eq!(kind_to_io_kind("protocol"), io::ErrorKind::InvalidData);
        assert_eq!(kind_to_io_kind("io"), io::ErrorKind::ConnectionReset);
        assert_eq!(kind_to_io_kind("invalid"), io::ErrorKind::InvalidInput);
        assert_eq!(kind_to_io_kind("busy"), io::ErrorKind::WouldBlock);
        assert_eq!(kind_to_io_kind("whatever"), io::ErrorKind::Other);
    }

    /// agent 的窗口按字节计：一批里大量小 `tcp:data`（WebSocket 小帧、SSE）
    /// 只要总字节在窗口内就必须正常收下（回归：曾按条数 32 判违约）。
    #[tokio::test]
    async fn test_many_small_messages_within_byte_window() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (mut stream, conn_id) = open_ok(&state, &sid, &mut rx).await;
        for seq in 0..64u64 {
            let m = msg(
                &sid,
                "tcp:data",
                json!({"conn_id": &conn_id, "seq": seq, "data": B64.encode(b"0123456789")}),
            );
            feed(&state, &sid, &m).await;
        }
        let mut got = vec![0u8; 640];
        tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut got))
            .await
            .expect("读取超时")
            .expect("64 条小消息（640 字节）不应中止管道");
        assert_eq!(&got[..10], b"0123456789");
        assert_eq!(state.tcp_tunnels.count_session(&sid), 1);
    }

    /// 回归（MYS-2495）：目标关掉写方向（agent 发 `tcp:eof`）之后，relay 侧丢
    /// 弃管道时**必须**仍然下发 `tcp:close`。
    ///
    /// `tcp:eof` 只表示 agent 的**读**方向结束（目标不再发数据），agent 的写
    /// 方向还在等 relay 的指令：没有 `tcp:close` / `tcp:eof`，那条连接记录与
    /// 目标 socket 会永久留在 agent 上（现场表现为一堆 CLOSE-WAIT），攒到 agent
    /// 的 32 条上限后所有新的网页映射请求都被拒成 503。
    #[tokio::test]
    async fn test_drop_after_agent_eof_still_closes_agent_connection() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (mut stream, conn_id) = open_ok(&state, &sid, &mut rx).await;

        // 目标写完响应就关掉写方向 → agent 回 `tcp:eof`（写方向仍然开着）。
        feed(
            &state,
            &sid,
            &msg(&sid, "tcp:eof", json!({ "conn_id": &conn_id })),
        )
        .await;
        let mut buf = [0u8; 16];
        assert_eq!(stream.read(&mut buf).await.unwrap(), 0, "eof 后读必须返回 0");

        // 浏览器侧收完响应，hyper 丢掉管道。
        drop(stream);

        let v = next_bulk(&mut rx).await;
        assert_eq!(
            v["type"], "tcp:close",
            "agent 半关闭之后 drop 仍必须发 tcp:close（否则 agent 侧连接永久泄漏）: {v}"
        );
        assert_eq!(v["payload"]["conn_id"], conn_id.as_str());
        assert_eq!(state.tcp_tunnels.count_session(&sid), 0);
    }

    /// 回归（MYS-2495）：端到端复现现场 503 —— 目标每次都在写完响应后关连接，
    /// 浏览器每个请求都新建并丢弃一条管道。
    ///
    /// 假 agent 严格按协议行事：`tcp:open` → `tcp:opened` → 目标数据 →
    /// `tcp:eof`；只在收到 `tcp:close`（或 relay 的 `tcp:eof`）时才释放连接，
    /// 并发上限 32（= `src/agent/tcp_tunnel.rs::MAX_CONNS`）。基线在目标半关闭
    /// 之后不再下发 `tcp:close`，连接一条条攒着，第 33 个请求起被拒成 `busy`。
    #[tokio::test]
    async fn test_repeated_target_close_does_not_leak_agent_quota() {
        use std::collections::HashSet;

        /// 与 `src/agent/tcp_tunnel.rs::MAX_CONNS` 一致：agent 侧并发上限。
        const AGENT_MAX_CONNS: usize = 32;
        const REQUESTS: usize = AGENT_MAX_CONNS + 8;

        let state = make_state();
        let (sid, rx) = setup(&state).await;
        let live: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        // 假 agent：消费 relay 的下行指令，按上面那套协议回包。
        let agent = {
            let state = state.clone();
            let sid = sid.clone();
            let live = live.clone();
            let mut rx = rx;
            tokio::spawn(async move {
                while let Some(text) = rx.recv().await {
                    let v: Value = serde_json::from_str(&text).unwrap();
                    let conn_id = v["payload"]["conn_id"].as_str().unwrap_or("").to_string();
                    let reply = |ty: &'static str, payload: Value| {
                        let state = state.clone();
                        let sid = sid.clone();
                        async move {
                            feed(&state, &sid, &msg(&sid, ty, payload)).await;
                        }
                    };
                    match v["type"].as_str().unwrap_or("") {
                        "tcp:open" => {
                            // 配额判定与插入之间不能有 await（与真实 agent 一致）。
                            let admitted = {
                                let mut l = live.lock().unwrap();
                                if l.len() >= AGENT_MAX_CONNS {
                                    false
                                } else {
                                    l.insert(conn_id.clone());
                                    true
                                }
                            };
                            if !admitted {
                                reply(
                                    "tcp:error",
                                    json!({
                                        "conn_id": conn_id,
                                        "kind": "busy",
                                        "message": format!(
                                            "more than {AGENT_MAX_CONNS} concurrent connections"
                                        ),
                                    }),
                                )
                                .await;
                                continue;
                            }
                            reply("tcp:opened", json!({ "conn_id": conn_id })).await;
                            // 目标写出响应体就关掉写方向（CLOSE-WAIT 的现场形态）。
                            reply(
                                "tcp:data",
                                json!({
                                    "conn_id": conn_id,
                                    "seq": 0,
                                    "data": B64.encode(b"HTTP/1.1 200 OK\r\n\r\nhi"),
                                }),
                            )
                            .await;
                            reply("tcp:eof", json!({ "conn_id": conn_id })).await;
                        }
                        // relay 不再需要这条管道 → 释放。
                        "tcp:close" => {
                            live.lock().unwrap().remove(&conn_id);
                        }
                        // relay 写半关闭：读方向早已结束，agent 两侧都收摊。
                        "tcp:eof" => {
                            if live.lock().unwrap().remove(&conn_id) {
                                reply("tcp:closed", json!({ "conn_id": conn_id })).await;
                            }
                        }
                        _ => {}
                    }
                }
            })
        };

        for i in 0..REQUESTS {
            let stream = tokio::time::timeout(
                Duration::from_secs(10),
                open(&state, &sid, "127.0.0.1", 80),
            )
            .await
            .expect("open 不应挂住")
            .unwrap_or_else(|e| panic!("第 {} 个请求被拒: {e:?}（agent 侧配额泄漏）", i + 1));
            // 浏览器读完响应体（读到 eof）后丢弃管道。
            let mut stream = stream;
            let mut buf = [0u8; 64];
            loop {
                let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                    .await
                    .expect("读不应挂住")
                    .expect("读失败");
                if n == 0 {
                    break;
                }
            }
            drop(stream);
            // 给假 agent 一点时间处理 drop 产生的 tcp:close。
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(
                state.tcp_tunnels.count_session(&sid),
                0,
                "第 {} 个请求结束后 relay 侧配额必须已归还",
                i + 1
            );
        }

        // 每个请求的管道都必须被 agent 释放，否则配额已经泄漏。
        let leaked = live.lock().unwrap().len();
        assert_eq!(leaked, 0, "{REQUESTS} 个请求后 agent 侧仍残留 {leaked} 条连接");
        agent.abort();
    }

    /// 排队字节超过窗口才算违约（agent 无视 ack 狂发）。
    #[tokio::test]
    async fn test_read_queue_beyond_window_is_violation() {
        let state = make_state();
        let (sid, mut rx) = setup(&state).await;
        let (mut stream, conn_id) = open_ok(&state, &sid, &mut rx).await;
        let chunk = B64.encode(vec![7u8; MAX_CHUNK_BYTES]);
        let n = WINDOW_BYTES / MAX_CHUNK_BYTES + 1;
        for seq in 0..=n as u64 {
            let m = msg(
                &sid,
                "tcp:data",
                json!({"conn_id": &conn_id, "seq": seq, "data": &chunk}),
            );
            feed(&state, &sid, &m).await;
        }
        assert_eq!(state.tcp_tunnels.count_session(&sid), 0, "超窗应中止连接");
        let mut buf = [0u8; 16];
        assert!(stream.read(&mut buf).await.is_err());
    }
}
