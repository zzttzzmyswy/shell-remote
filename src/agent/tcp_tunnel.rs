//! agent 侧纯 TCP 字节管道（MYS-2383 阶段2）。
//!
//! relay 通过 bulk 下行通道下发 `tcp:*` 指令，本模块按指令连到目标 `host:port`
//! 并双向转发**原始字节**。agent 不解析 HTTP、不做 TLS、不改写任何内容——
//! 复杂 SPA / WebSocket / Cookie 登录全部由浏览器与 relay 负责，agent 只是一根
//! 带流控的管子。
//!
//! # 消息
//!
//! relay → agent：
//! - `tcp:open {conn_id, host, port}` 建连；
//! - `tcp:data {conn_id, seq, data}` 写 socket（`data` 为 base64，`seq` 从 0 连续）；
//! - `tcp:ack  {conn_id, bytes}` 归还 agent→relay 方向的发送窗口；
//! - `tcp:eof  {conn_id}` relay 不再写：对 socket 做写半关闭，读方向继续；
//! - `tcp:close{conn_id}` 立即中止，**不再产生任何消息**。
//!
//! agent → relay：
//! - `tcp:opened {conn_id}`、`tcp:data {conn_id, seq, data}`、
//!   `tcp:ack {conn_id, bytes}`、`tcp:eof {conn_id}`、`tcp:closed {conn_id}`、
//!   `tcp:error {conn_id, kind, message}`。
//!
//! # 不变式
//!
//! 除 `tcp:close` 外，每个被接受的 `tcp:open`（以及因 `invalid`/`busy` 被拒的）
//! **恰好一条**终态消息：`tcp:closed` 或 `tcp:error`。终态之后记录从表中移除、
//! 并发名额归还。
//!
//! # 流控
//!
//! agent→relay 每连接最多 [`WINDOW_BYTES`] 字节"已发未确认"。窗口用满就停止读
//! socket（让 TCP 把背压自然传给目标），收到 `tcp:ack` 后继续。已建立的连接不设
//! 空闲超时（WebSocket 可能长时间静默），只在 `tcp:close`、出错、会话
//! `shutdown()` 时结束。
//!
//! # 上行通道
//!
//! 字节流**不走** control 通道（`task_control_tx` 单条逐个 POST，会把终端与 MCP
//! 的回包挤在后面）。这里另起一条专用上行 task：有界队列 → 每轮取一批
//! （≤[`UPLINK_BATCH_MSGS`] 条 / ≤[`UPLINK_BATCH_BYTES`] 字节）→ 拼成 JSON 数组
//! 一次 POST 到 `/agent/send`（relay 已支持数组批量）。单 task 顺序 POST 保证同一
//! 连接的消息不乱序。POST 失败即中止全部连接——字节流已不可恢复。
//!
//! # `tcp:close` 的严格语义
//!
//! `tcp:close` 到达之后，这个 conn_id **一条消息都不会再发出去**：派发侧先给
//! conn_id 立墓碑（[`Out::closed`]），上行 task 在 POST 前按墓碑把已入队的消息
//! 剔掉，连接 task 本身被 abort 从源头断掉。conn_id 被重新 open 时墓碑清除。
//!
//! 唯一残留的时序：relay 违反 relay→agent 窗口把写队列灌满时，派发侧回
//! `tcp:error kind=protocol` 并 abort 该连接；极窄的窗口内该连接自己的终态消息
//! 可能已经入队，relay 会先收到一条 `tcp:error` 再收到它自己的终态。属 relay
//! 违约路径，不影响正常语义。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use crate::proto::Message;

/// 注册能力名：relay 据此判断该 agent 支持纯 TCP 字节管道（阶段2 的 HTTP 反代
/// 与 WebSocket 透传都建立在它之上）。lean 构建同样声明。
pub const CAPABILITY: &str = "net:tcp-v1";

/// 每会话并发连接上限。
pub const MAX_CONNS: usize = 32;

/// 单条 `tcp:data` 承载的原始字节上限（base64 前）。
pub const MAX_CHUNK_BYTES: usize = 48 * 1024;

/// 每连接 agent→relay 方向"已发未确认"字节上限。
pub const WINDOW_BYTES: usize = 512 * 1024;

/// 建连超时。生产值见 [`Limits::default`]；测试注入极小值验证 `kind=timeout`。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 专用上行队列容量（条）。
pub const UPLINK_QUEUE: usize = 256;
/// 单次批量 POST 最多合并的消息条数。
pub const UPLINK_BATCH_MSGS: usize = 64;
/// 单次批量 POST 的累计字节上限。
pub const UPLINK_BATCH_BYTES: usize = 1024 * 1024;

/// `conn_id` 长度上限（字符）。
const MAX_CONN_ID_CHARS: usize = 64;
/// `host` 长度上限（字节，DNS 名上限 253）。
const MAX_HOST_LEN: usize = 253;
/// 每连接的待写队列容量。relay 遵守窗口时远不会打满，打满即视为违约。
const WRITE_QUEUE: usize = 64;

/// 可注入的限额。生产用 [`Limits::default`]；单测注入小值，避免真的等 10s、
/// 也避免几 MiB 的用例跑得慢。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// 建连超时。
    pub connect: Duration,
    /// 每会话并发连接数。
    pub max_conns: usize,
    /// 每连接 agent→relay 方向已发未确认字节上限。
    pub window: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            max_conns: MAX_CONNS,
            window: WINDOW_BYTES,
        }
    }
}

/// 取消令牌：一次连接一个。`notify_one` 会存一张许可，因此先取消后等待也不会
/// 错过（`notify_waiters` 会，故不用它）。
#[derive(Default)]
struct CancelFlag {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelFlag {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

/// agent→relay 方向的发送窗口：已发未确认字节数 + 窗口释放通知。
#[derive(Default)]
struct Unacked {
    bytes: AtomicUsize,
    notify: tokio::sync::Notify,
}

impl Unacked {
    fn add(&self, n: usize) {
        self.bytes.fetch_add(n, Ordering::SeqCst);
    }

    fn level(&self) -> usize {
        self.bytes.load(Ordering::SeqCst)
    }

    /// relay 归还窗口。超额 ack（relay 算错）按 0 处理：不 panic、不产生"负欠账"
    /// 让后续数据白送。
    fn ack(&self, n: usize) {
        let mut cur = self.bytes.load(Ordering::SeqCst);
        loop {
            let next = cur.saturating_sub(n);
            match self
                .bytes
                .compare_exchange(cur, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        self.notify.notify_one();
    }

    async fn wait_below(&self, limit: usize) {
        while self.level() >= limit {
            self.notify.notified().await;
        }
    }
}

/// 回包出口：把 `Message` 序列化后推进**专用**上行队列。
#[derive(Clone)]
struct Out {
    tx: mpsc::Sender<String>,
    session_id: String,
    /// 上行已废（POST 失败 / 会话结束）：不再产生新消息。
    dead: Arc<AtomicBool>,
    /// 已被 `tcp:close` 关掉的 conn_id（墓碑）。上行 task 发批次前必须把属于这些
    /// 连接的消息剔掉 —— 它们可能在 close 到达前就已经入队。连接被重新 open 时
    /// 把 id 移出集合（名额已释放，id 可复用）。见 [`ClosedSet`]。
    closed: Arc<Mutex<ClosedSet>>,
}

/// `tcp:close` 墓碑集合：有序 + 有上限。
///
/// 为什么要有上限：长会话里 conn_id 可以一直换新，无界集合就是内存泄漏。
///
/// 为什么按上限淘汰最旧的那条是安全的：墓碑只用来拦"已经躺在上行队列里、
/// 还没 POST 出去"的消息，而那条队列只有 [`UPLINK_QUEUE`]=256 格。一个墓碑
/// 被淘汰，意味着它之后又来了 [`MAX_TOMBSTONES`] 个 close —— 期间上行 task
/// 早已把那一批（最多 256 条）取走。因此被淘汰的 id 对应不到任何仍在队列里的
/// 消息。即便如此，[`Out::send`] 的入口检查仍是第一道防线：正常情况下新消息
/// 根本进不了队列，这里的过滤只负责那个极窄的竞态窗口。
struct ClosedSet {
    set: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
}

/// 墓碑集合上限。远大于上行队列容量，保证淘汰时队列里不可能还有对应消息。
const MAX_TOMBSTONES: usize = 4096;

impl ClosedSet {
    fn new() -> Self {
        Self {
            set: std::collections::HashSet::new(),
            order: std::collections::VecDeque::new(),
        }
    }

    fn insert(&mut self, conn_id: String) {
        if self.set.insert(conn_id.clone()) {
            self.order.push_back(conn_id);
            while self.order.len() > MAX_TOMBSTONES {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }

    fn remove(&mut self, conn_id: &str) {
        if self.set.remove(conn_id) {
            self.order.retain(|id| id != conn_id);
        }
    }

    fn contains(&self, conn_id: &str) -> bool {
        self.set.contains(conn_id)
    }

    fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

impl Out {
    /// 返回 false = 这条没送出去（通道关闭或上行已废），调用方应停止发送。
    async fn send(&self, msg_type: &str, payload: Value) -> bool {
        if self.dead.load(Ordering::SeqCst) {
            return false;
        }
        // 已经 close 的连接，消息连队列都别进（第一道防线）。
        if let Some(id) = payload.get("conn_id").and_then(|v| v.as_str()) {
            if self.closed.lock().unwrap().contains(id) {
                return true;
            }
        }
        let msg = Message {
            msg_type: msg_type.to_string(),
            session_id: self.session_id.clone(),
            payload,
        };
        match serde_json::to_string(&msg) {
            Ok(text) => self.tx.send(text).await.is_ok(),
            Err(e) => {
                tracing::warn!(msg_type, "tcp tunnel: failed to serialize message: {e}");
                false
            }
        }
    }
}

/// relay→agent 方向的待写指令（FIFO，保证 socket 上的写入顺序与 seq 顺序一致）。
enum WriteCmd {
    Data { seq: u64, data: Vec<u8> },
    Eof,
}

/// 在途连接记录：取消令牌 + 任务句柄（会话结束 abort）+ 写队列 + 窗口 + 并发票。
///
/// 并发票由这条记录持有而不是由 task 持有：记录被移除（终态、`tcp:close`、
/// 会话结束）时票立刻归还，"当前连接数"与 map 大小因此严格一致。
struct Conn {
    cancel: Arc<CancelFlag>,
    abort: tokio::task::AbortHandle,
    write_tx: mpsc::Sender<WriteCmd>,
    unacked: Arc<Unacked>,
    #[allow(dead_code)]
    permit: OwnedSemaphorePermit,
}

struct Inner {
    out: Out,
    limits: Limits,
    permits: Arc<Semaphore>,
    conns: Mutex<HashMap<String, Conn>>,
}

/// 会话级 TCP 管道句柄（Clone 只是 Arc 复制）。
#[derive(Clone)]
pub struct TcpTunnel {
    inner: Arc<Inner>,
}

impl TcpTunnel {
    /// `uplink_tx` 必须是**专用**上行队列的发送端（不是 control 通道）。
    pub fn new(session_id: String, uplink_tx: mpsc::Sender<String>) -> Self {
        Self::with_limits(session_id, uplink_tx, Limits::default())
    }

    /// 同 [`TcpTunnel::new`]，但可指定限额（单测用）。
    pub fn with_limits(
        session_id: String,
        uplink_tx: mpsc::Sender<String>,
        limits: Limits,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                out: Out {
                    tx: uplink_tx,
                    session_id,
                    dead: Arc::new(AtomicBool::new(false)),
                    closed: Arc::new(Mutex::new(ClosedSet::new())),
                },
                limits,
                permits: Arc::new(Semaphore::new(limits.max_conns)),
                conns: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// 处理 `tcp:open`。只做校验/配额/派发，建连在独立 task 里。
    pub async fn handle_open(&self, payload: &Value) {
        let conn_id = payload
            .get("conn_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let host = payload.get("host").and_then(|v| v.as_str()).unwrap_or("");
        let port = payload.get("port").and_then(|v| v.as_u64());

        let port = match validate_open(conn_id, host, port) {
            Ok(p) => p,
            Err((kind, message)) => {
                tracing::debug!(conn_id, %message, "tcp:open rejected");
                self.emit_error(conn_id, kind, message).await;
                return;
            }
        };

        // 重复 conn_id：明确失败，不顶替旧连接（旧连接继续跑，relay 自己决定
        // 是关旧的还是放弃这次 open）。
        if self.inner.conns.lock().unwrap().contains_key(conn_id) {
            tracing::debug!(conn_id, "tcp:open rejected: conn_id already in use");
            self.emit_error(conn_id, "invalid", "conn_id already in use".to_string())
                .await;
            return;
        }

        // 配额与插入之间没有 await，派发又串行，所以不存在并发抢名额的窗口。
        let permit = match self.inner.permits.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::debug!(conn_id, "tcp:open rejected: concurrency limit");
                self.emit_error(
                    conn_id,
                    "busy",
                    format!(
                        "more than {} concurrent connections",
                        self.inner.limits.max_conns
                    ),
                )
                .await;
                return;
            }
        };

        let cancel = Arc::new(CancelFlag::default());
        let unacked = Arc::new(Unacked::default());
        let (write_tx, write_rx) = mpsc::channel::<WriteCmd>(WRITE_QUEUE);
        let inner = self.inner.clone();
        let conn_id_owned = conn_id.to_string();
        let host_owned = host.to_string();
        let cancel_task = cancel.clone();
        let unacked_task = unacked.clone();
        // 任务先等我们把记录写进 map：否则极快的失败路径会在插入之前跑完自清理，
        // 留下一条永不回收的陈旧记录（与 web_proxy 同样的处理）。
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = ready_rx.await;
            run_conn(
                inner,
                conn_id_owned,
                host_owned,
                port,
                cancel_task,
                unacked_task,
                write_rx,
            )
            .await;
        });
        self.inner.conns.lock().unwrap().insert(
            conn_id.to_string(),
            Conn {
                cancel,
                abort: task.abort_handle(),
                write_tx,
                unacked,
                permit,
            },
        );
        // id 可能被复用（上一个拥有者已 close 并释放名额）：清掉墓碑，新连接
        // 的消息必须照常发出去。
        self.inner.out.closed.lock().unwrap().remove(conn_id);
        let _ = ready_tx.send(());
    }

    /// 处理 `tcp:data`：base64 解码后转交给该连接的写队列（不 await socket I/O）。
    pub async fn handle_data(&self, payload: &Value) {
        let conn_id = payload
            .get("conn_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if conn_id.is_empty() {
            return;
        }
        // 未知 conn_id 静默忽略：连接可能刚关闭，relay 手里还有在途数据。
        let write_tx = {
            let map = self.inner.conns.lock().unwrap();
            match map.get(conn_id) {
                Some(c) => c.write_tx.clone(),
                None => {
                    tracing::trace!(conn_id, "tcp:data for unknown connection ignored");
                    return;
                }
            }
        };
        let seq = match payload.get("seq").and_then(|v| v.as_u64()) {
            Some(s) => s,
            None => {
                self.fail(conn_id, "protocol", "tcp:data missing seq").await;
                return;
            }
        };
        let data_b64 = payload.get("data").and_then(|v| v.as_str()).unwrap_or("");
        let data = match B64.decode(data_b64) {
            Ok(d) => d,
            Err(_) => {
                self.fail(conn_id, "protocol", "tcp:data has invalid base64")
                    .await;
                return;
            }
        };
        match write_tx.try_send(WriteCmd::Data { seq, data }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // 连接任务已结束（终态已发或已 abort），无需再表示什么。
                tracing::trace!(conn_id, "tcp:data for a finished connection ignored");
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                // relay 违反了 relay→agent 方向的窗口，字节流已无法保序/恢复。
                self.fail(
                    conn_id,
                    "protocol",
                    "relay exceeded the write window (write queue full)",
                )
                .await;
            }
        }
    }

    /// 处理 `tcp:ack`：归还 agent→relay 方向的发送窗口。
    pub fn handle_ack(&self, payload: &Value) {
        let conn_id = payload
            .get("conn_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if conn_id.is_empty() {
            return;
        }
        let bytes = payload.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let map = self.inner.conns.lock().unwrap();
        match map.get(conn_id) {
            Some(c) => {
                if bytes > 0 {
                    c.unacked.ack(bytes);
                } else {
                    // bytes=0 的 ack 没有语义，但仍唤醒一次等待者无意义，直接忽略。
                }
            }
            None => tracing::trace!(conn_id, "tcp:ack for unknown connection ignored"),
        }
    }

    /// 处理 `tcp:eof`：relay 侧不再写，转交给连接任务做写半关闭。
    pub fn handle_eof(&self, payload: &Value) {
        let conn_id = payload
            .get("conn_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if conn_id.is_empty() {
            return;
        }
        let write_tx = {
            let map = self.inner.conns.lock().unwrap();
            match map.get(conn_id) {
                Some(c) => c.write_tx.clone(),
                None => {
                    tracing::trace!(conn_id, "tcp:eof for unknown connection ignored");
                    return;
                }
            }
        };
        // 队列满也不能丢 eof（丢了就是泄漏半开连接）：改成直接 abort，
        // relay 会看到连接消失。实际走到这里的概率接近 0。
        if write_tx.try_send(WriteCmd::Eof).is_err() {
            let mut map = self.inner.conns.lock().unwrap();
            if let Some(c) = map.remove(conn_id) {
                c.cancel.cancel();
                c.abort.abort();
            }
        }
    }

    /// 处理 `tcp:close`：立即中止，**不再发送任何消息**（包括已经入队但还没
    /// POST 出去的那几条 —— 排空侧会按墓碑把它们剔掉）。
    pub fn handle_close(&self, payload: &Value) {
        let conn_id = payload
            .get("conn_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if conn_id.is_empty() {
            return;
        }
        let conn = self.inner.conns.lock().unwrap().remove(conn_id);
        // 先立墓碑再 abort：即使任务此刻正把一条消息推进队列，上行 task 在发
        // 批次前也会看到墓碑把它筛掉。
        self.inner
            .out
            .closed
            .lock()
            .unwrap()
            .insert(conn_id.to_string());
        match conn {
            Some(c) => {
                tracing::debug!(conn_id, "tcp:close: aborting connection");
                // 取消 + abort 双保险：任务若停在 await 上，abort 立即生效。
                c.cancel.cancel();
                c.abort.abort();
            }
            None => tracing::trace!(conn_id, "tcp:close for unknown connection ignored"),
        }
    }

    /// 会话结束：中止全部连接，且不再发送任何消息。
    pub fn shutdown(&self) {
        self.inner.out.dead.store(true, Ordering::SeqCst);
        let drained: Vec<Conn> = {
            let mut map = self.inner.conns.lock().unwrap();
            map.drain().map(|(_, v)| v).collect()
        };
        for c in drained {
            c.cancel.cancel();
            c.abort.abort();
        }
    }

    /// 上行已废（批量 POST 失败）：字节流不可恢复，中止全部的连接。
    pub fn fail_all(&self) {
        tracing::warn!("tcp tunnel: uplink POST failed — aborting all connections");
        self.shutdown();
    }

    /// 当前在途连接数（测试与观测用）。
    pub fn active_count(&self) -> usize {
        self.inner.conns.lock().unwrap().len()
    }

    async fn emit_error(&self, conn_id: &str, kind: &str, message: String) {
        let payload = json!({
            "conn_id": conn_id,
            "kind": kind,
            "message": message,
        });
        if !self.inner.out.send("tcp:error", payload).await {
            tracing::debug!(conn_id, "tcp:error not delivered (uplink closing)");
        }
    }

    /// 派发侧判定某连接已不可用：摘掉记录、abort 任务、回一条 `tcp:error`。
    async fn fail(&self, conn_id: &str, kind: &str, message: &str) {
        let removed = self.inner.conns.lock().unwrap().remove(conn_id);
        match removed {
            Some(c) => {
                c.cancel.cancel();
                c.abort.abort();
                self.emit_error(conn_id, kind, message.to_string()).await;
            }
            // 任务已自己结束并摘了记录（终态已发）：别再补一条。
            None => {
                tracing::trace!(conn_id, "fail() for a finished connection ignored");
            }
        }
    }
}

/// 校验 `tcp:open` 参数。目标地址不受限制（loopback/内网都允许）——访问设备
/// 自身网络正是这个功能的用途。
fn validate_open(
    conn_id: &str,
    host: &str,
    port: Option<u64>,
) -> Result<u16, (&'static str, String)> {
    if conn_id.is_empty() || conn_id.chars().count() > MAX_CONN_ID_CHARS {
        return Err((
            "invalid",
            format!("conn_id must be 1..={MAX_CONN_ID_CHARS} characters"),
        ));
    }
    if host.is_empty() || host.len() > MAX_HOST_LEN {
        return Err((
            "invalid",
            format!("host must be non-empty and at most {MAX_HOST_LEN} bytes"),
        ));
    }
    match port {
        Some(p) if (1..=65535).contains(&p) => Ok(p as u16),
        _ => Err(("invalid", "port must be 1..=65535".to_string())),
    }
}

/// 连接 task 的结局，决定发哪条终态消息（或不发）。
enum Outcome {
    /// `tcp:close` / 会话结束：不留任何消息。
    Aborted,
    /// 两个方向都已结束。
    Closed,
    /// 终态错误 `(kind, message)`。
    Failed(&'static str, String),
}

/// 单连接的执行体（独立 task）。负责 `tcp:opened` → 双向转发 → 终态。
async fn run_conn(
    inner: Arc<Inner>,
    conn_id: String,
    host: String,
    port: u16,
    cancel: Arc<CancelFlag>,
    unacked: Arc<Unacked>,
    write_rx: mpsc::Receiver<WriteCmd>,
) {
    let outcome = drive(&inner, &conn_id, &host, port, &cancel, &unacked, write_rx).await;

    // 先发终态，再摘记录：摘掉之后同 id 的新 open 才能进来，避免终态落到
    // 新连接头上。
    match outcome {
        Outcome::Aborted => {}
        Outcome::Closed => {
            let _ = inner
                .out
                .send("tcp:closed", json!({ "conn_id": conn_id }))
                .await;
        }
        Outcome::Failed(kind, message) => {
            let payload = json!({
                "conn_id": conn_id,
                "kind": kind,
                "message": message,
            });
            let _ = inner.out.send("tcp:error", payload).await;
        }
    }

    // 自清理：只有 map 里还是自己那条记录时才移除（`tcp:close` 可能已经摘掉，
    // 或同 id 的新连接已经插进来）。
    let mut map = inner.conns.lock().unwrap();
    let is_current = map
        .get(&conn_id)
        .is_some_and(|cur| Arc::ptr_eq(&cur.cancel, &cancel));
    if is_current {
        map.remove(&conn_id);
    }
}

async fn drive(
    inner: &Inner,
    conn_id: &str,
    host: &str,
    port: u16,
    cancel: &CancelFlag,
    unacked: &Unacked,
    mut write_rx: mpsc::Receiver<WriteCmd>,
) -> Outcome {
    // ── 建连 ────────────────────────────────────────────────────────────
    let stream = match tokio::time::timeout(inner.limits.connect, TcpStream::connect((host, port)))
        .await
    {
        Err(_) => return Outcome::Failed("timeout", format!("connect to {host}:{port} timed out")),
        Ok(Err(e)) => {
            tracing::debug!(conn_id, %host, port, error = %e, "tcp tunnel: connect failed");
            return Outcome::Failed("connect", format!("connect to {host}:{port} failed: {e}"));
        }
        Ok(Ok(s)) => s,
    };
    if cancel.is_cancelled() {
        return Outcome::Aborted;
    }
    // 交互式（WebSocket/SSE）流量对 Nagle 的合并几乎只有坏处。
    if let Err(e) = stream.set_nodelay(true) {
        tracing::debug!(conn_id, error = %e, "tcp tunnel: set_nodelay failed");
    }
    if !inner
        .out
        .send("tcp:opened", json!({ "conn_id": conn_id }))
        .await
    {
        return Outcome::Aborted;
    }

    let (mut rd, mut wr) = stream.into_split();
    let window = inner.limits.window.max(1);
    let mut buf = vec![0u8; MAX_CHUNK_BYTES.min(window)];
    // relay→agent 方向的期望 seq（从 0 连续）。
    let mut in_seq: u64 = 0;
    // agent→relay 方向的写出 seq（从 0 连续）。
    let mut out_seq: u64 = 0;
    // 目标关闭了读方向（我们已发 tcp:eof）。
    let mut read_done = false;
    // relay 发来 tcp:eof（我们已做写半关闭）。
    let mut relay_eof = false;

    loop {
        if read_done && relay_eof {
            return Outcome::Closed;
        }

        // 窗口用满就不读 socket，让 TCP 把背压传给目标。
        let avail = window.saturating_sub(unacked.level());
        let can_read = !read_done && avail > 0;
        let want = avail.min(buf.len());
        // 只有「读方向还活着、但窗口满了」才需要等窗口释放。读方向已结束时也
        // 去等就是空转（`wait_below` 会立刻返回），会把写队列饿死。
        let wait_window = !read_done && avail == 0;

        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Outcome::Aborted,

            cmd = write_rx.recv() => match cmd {
                // 写队列发送端被丢弃 = 隧道正在销毁。
                None => return Outcome::Aborted,
                Some(WriteCmd::Data { seq, data }) => {
                    if seq != in_seq {
                        return Outcome::Failed(
                            "protocol",
                            format!("tcp:data seq {seq} out of order (expected {in_seq})"),
                        );
                    }
                    in_seq += 1;
                    if let Err(e) = wr.write_all(&data).await {
                        return Outcome::Failed("io", format!("write to target failed: {e}"));
                    }
                    let ack = json!({ "conn_id": conn_id, "bytes": data.len() });
                    if !inner.out.send("tcp:ack", ack).await {
                        return Outcome::Aborted;
                    }
                }
                Some(WriteCmd::Eof) => {
                    relay_eof = true;
                    if let Err(e) = wr.shutdown().await {
                        return Outcome::Failed("io", format!("shutdown(write) failed: {e}"));
                    }
                }
            },

            // 窗口释放后再回到循环顶部重算 avail。
            _ = unacked.wait_below(window), if wait_window => continue,

            r = rd.read(&mut buf[..want]), if can_read => match r {
                Err(e) => return Outcome::Failed("io", format!("read from target failed: {e}")),
                Ok(0) => {
                    read_done = true;
                    if !inner.out.send("tcp:eof", json!({ "conn_id": conn_id })).await {
                        return Outcome::Aborted;
                    }
                }
                Ok(n) => {
                    unacked.add(n);
                    let payload = json!({
                        "conn_id": conn_id,
                        "seq": out_seq,
                        "data": B64.encode(&buf[..n]),
                    });
                    out_seq += 1;
                    if !inner.out.send("tcp:data", payload).await {
                        return Outcome::Aborted;
                    }
                }
            },
        }
    }
}

/// 从队列里取一批待上行的消息。
///
/// 先阻塞等第一条（`None`=所有发送端已 drop），之后非阻塞地把队列里已有的
/// 消息尽量装进这一批，受 `max_msgs` 与 `max_bytes` 双重约束。
///
/// `carry` 承载"已经取出但这批装不下"的那一条，下一轮优先发出——tokio 的
/// mpsc 没有 peek，要靠它才能既守住字节上限又不丢消息。
pub async fn drain_batch(
    rx: &mut mpsc::Receiver<String>,
    carry: &mut Option<String>,
    max_msgs: usize,
    max_bytes: usize,
) -> Vec<String> {
    let mut batch: Vec<String> = Vec::new();
    let mut bytes = 0usize;

    let first = match carry.take() {
        Some(m) => Some(m),
        None => rx.recv().await,
    };
    let Some(first) = first else {
        return batch;
    };
    bytes += first.len();
    batch.push(first);

    while batch.len() < max_msgs {
        if let Some(m) = carry.take() {
            // carry 只会在上一轮因超限留下一条，并且当时已确认它装得下
            // 单独一条的额度。
            if bytes + m.len() > max_bytes {
                *carry = Some(m);
                break;
            }
            bytes += m.len();
            batch.push(m);
            continue;
        }
        match rx.try_recv() {
            Ok(m) => {
                if bytes + m.len() > max_bytes {
                    *carry = Some(m);
                    break;
                }
                bytes += m.len();
                batch.push(m);
            }
            Err(_) => break,
        }
    }
    batch
}

/// 从一批已序列化的消息里剔掉属于 `closed` 中 conn_id 的那些。
///
/// `tcp:close` 承诺"此后一条消息都不发"，而中止瞬间可能已经有几条消息躺在
/// 队列里等待 POST。这里在发批次前按 `conn_id` 过滤掉它们（反序列化失败的行
/// 原样保留——自身就是坏消息，交给 relay 的既有逻辑处理）。
fn drop_closed_conns(batch: Vec<String>, closed: &std::sync::Mutex<ClosedSet>) -> Vec<String> {
    let closed = closed.lock().unwrap();
    if closed.is_empty() {
        return batch;
    }
    batch
        .into_iter()
        .filter(|text| match serde_json::from_str::<Message>(text) {
            Ok(m) => !m
                .payload
                .get("conn_id")
                .and_then(|v| v.as_str())
                .is_some_and(|id| closed.contains(id)),
            Err(_) => true,
        })
        .collect()
}

/// 专用上行 task：批量取消息 → 拼 JSON 数组 → 一次 POST 到 `/agent/send`。
///
/// 单 task 顺序 POST，同一连接的消息天然保序。POST 失败即 `fail_all()`：
/// 丢掉的字节流不可恢复，继续跑只会让 relay 收到半截数据。
pub async fn uplink_loop(
    client: reqwest::Client,
    send_url: String,
    agent_token: String,
    mut rx: mpsc::Receiver<String>,
    tunnel: TcpTunnel,
) {
    let mut carry: Option<String> = None;
    loop {
        let batch = drain_batch(&mut rx, &mut carry, UPLINK_BATCH_MSGS, UPLINK_BATCH_BYTES).await;
        if batch.is_empty() {
            // 所有发送端都没了：会话结束。
            tracing::debug!("tcp tunnel uplink: all senders dropped, exiting");
            return;
        }
        // 已经 `tcp:close` 的连接，它的消息一条都不能再出去。
        let batch = drop_closed_conns(batch, &tunnel.inner.out.closed);
        if batch.is_empty() {
            continue;
        }
        // relay 的 batch 通道（`src/relay/ws.rs` 的 `agent_send_handler`）接受
        // JSON 数组，逐条按各自的 `session_id` 路由。
        let body = format!("[{}]", batch.join(","));
        if super::post_raw(&client, &send_url, &agent_token, &body).await {
            continue;
        }
        tracing::warn!(
            dropped = batch.len(),
            "tcp tunnel uplink: batch POST failed — aborting all connections"
        );
        tunnel.fail_all();
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Instant;
    use tokio::net::TcpListener;

    // ── 测试脚手架 ──────────────────────────────────────────────────────

    /// 本地目标服务（等同于设备侧 127.0.0.1 上的某个监听端口）。
    struct Target {
        addr: SocketAddr,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for Target {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    /// 起一个只接受一条连接的本地目标，把 `TcpStream` 交给 `f`。
    async fn target<F, Fut>(f: F) -> Target
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            if let Ok((s, _)) = listener.accept().await {
                f(s).await;
            }
        });
        Target { addr, handle }
    }

    /// 目标接受连接后既不写也不关：让连接保持打开，方便单独观察其他方向的
    /// 行为（注意 `TcpStream` 必须被 async 块持有，否则一 drop 就是 EOF）。
    async fn idle_target() -> Target {
        target(|s| async move {
            let _keep = s;
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
        .await
    }

    /// 测试用管道 + 上行接收端。
    fn tunnel(limits: Limits) -> (TcpTunnel, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel::<String>(UPLINK_QUEUE);
        (
            TcpTunnel::with_limits("sess-test".to_string(), tx, limits),
            rx,
        )
    }

    fn small_limits() -> Limits {
        Limits {
            connect: Duration::from_secs(5),
            max_conns: MAX_CONNS,
            window: WINDOW_BYTES,
        }
    }

    /// 收一条上行消息，返回 (type, payload)。
    async fn recv(rx: &mut mpsc::Receiver<String>) -> (String, Value) {
        let raw = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for an agent→relay message")
            .expect("uplink channel closed");
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["session_id"], "sess-test");
        (
            v["type"].as_str().unwrap().to_string(),
            v["payload"].clone(),
        )
    }

    /// 断言一段时间内不再有任何消息到达。
    async fn assert_quiet(rx: &mut mpsc::Receiver<String>, what: &str) {
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Err(_) => {}
            Ok(Some(raw)) => panic!("{what}: unexpected extra message {raw}"),
            Ok(None) => {}
        }
    }

    fn open_payload(conn_id: &str, addr: SocketAddr) -> Value {
        json!({ "conn_id": conn_id, "host": "127.0.0.1", "port": addr.port() })
    }

    // ── 纯函数 ──────────────────────────────────────────────────────────

    #[test]
    fn test_validate_open_accepts_plain_targets() {
        assert_eq!(validate_open("c1", "127.0.0.1", Some(80)).unwrap(), 80);
        assert_eq!(validate_open("c1", "10.0.0.5", Some(65535)).unwrap(), 65535);
        assert_eq!(validate_open("c1", "example.lan", Some(1)).unwrap(), 1);
        // 64 字符的 conn_id 仍在范围内。
        let long = "a".repeat(64);
        assert!(validate_open(&long, "h", Some(1)).is_ok());
    }

    #[test]
    fn test_validate_open_rejects_bad_fields() {
        // conn_id
        assert_eq!(validate_open("", "h", Some(1)).unwrap_err().0, "invalid");
        let too_long = "a".repeat(65);
        assert_eq!(
            validate_open(&too_long, "h", Some(1)).unwrap_err().0,
            "invalid"
        );
        // host
        assert_eq!(validate_open("c", "", Some(1)).unwrap_err().0, "invalid");
        let long_host = "a".repeat(254);
        assert_eq!(
            validate_open("c", &long_host, Some(1)).unwrap_err().0,
            "invalid"
        );
        // port
        for port in [None, Some(0), Some(65536), Some(u64::MAX)] {
            assert_eq!(
                validate_open("c", "h", port).unwrap_err().0,
                "invalid",
                "port {port:?} 必须被拒绝"
            );
        }
    }

    #[test]
    fn test_readonly_sessions_must_not_open_tunnels() {
        // 完整映射能改设备配置（往局域网设备写字节），因此只开给 rw 会话：
        // `tcp:*` 不在只读白名单里，`requires_write` 的 fail-closed 结论是"写"。
        for t in ["tcp:open", "tcp:data", "tcp:ack", "tcp:eof", "tcp:close"] {
            assert!(crate::proto::requires_write(t), "{t} 必须要求写权限");
            assert!(
                !crate::agent::web_proxy::readonly_allowed(t),
                "{t} 不能进只读白名单"
            );
        }
    }

    #[test]
    fn test_capabilities_declare_tcp_tunnel_and_keep_web_proxy() {
        let caps = crate::agent::client::capabilities_for_test();
        assert!(caps.iter().any(|c| c == "net:tcp-v1"), "缺少 net:tcp-v1");
        assert!(
            caps.iter().any(|c| c == "web:proxy-v1"),
            "web:proxy-v1 被误删"
        );
    }

    #[tokio::test]
    async fn test_drain_batch_respects_msg_and_byte_limits() {
        // 条数上限。
        let (tx, mut rx) = mpsc::channel::<String>(256);
        for i in 0..100 {
            tx.send(format!("m{i}")).await.unwrap();
        }
        drop(tx);
        let mut carry = None;
        let batch = drain_batch(&mut rx, &mut carry, 64, UPLINK_BATCH_BYTES).await;
        assert_eq!(batch.len(), 64, "一批最多 64 条");
        assert!(carry.is_none());
        let rest = drain_batch(&mut rx, &mut carry, 64, UPLINK_BATCH_BYTES).await;
        assert_eq!(rest.len(), 36, "剩下的 36 条在下一批");
        assert!(!rest[0].is_empty());

        // 字节上限：20 条 60KiB = 1.17MiB，一批只能装 1MiB。
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let payload = "x".repeat(60 * 1024);
        for _ in 0..20 {
            tx.send(payload.clone()).await.unwrap();
        }
        // 发送端必须先 drop：否则最后几轮 drain_batch 会阻塞在「等新消息」上。
        drop(tx);
        let mut carry = None;
        let first = drain_batch(&mut rx, &mut carry, UPLINK_BATCH_MSGS, UPLINK_BATCH_BYTES).await;
        let first_bytes: usize = first.iter().map(|m| m.len()).sum();
        assert!(first_bytes <= UPLINK_BATCH_BYTES, "一批不得超过 1 MiB");
        assert_eq!(first.len(), 17, "1MiB / 60KiB = 17 条");
        assert!(carry.is_some(), "装不下的那条必须留到下一轮");
        // 剩下的（含 carry）一条不少。
        let mut all = first;
        loop {
            let more =
                drain_batch(&mut rx, &mut carry, UPLINK_BATCH_MSGS, UPLINK_BATCH_BYTES).await;
            if more.is_empty() {
                break;
            }
            all.extend(more);
        }
        assert_eq!(all.len(), 20, "carry 机制不能让消息丢失");
        assert!(all.iter().all(|m| m.len() == 60 * 1024));
    }

    #[tokio::test]
    async fn test_drain_batch_returns_empty_when_all_senders_dropped() {
        let (tx, mut rx) = mpsc::channel::<String>(4);
        drop(tx);
        let mut carry = None;
        let batch = drain_batch(&mut rx, &mut carry, 64, UPLINK_BATCH_BYTES).await;
        assert!(batch.is_empty());
    }

    #[test]
    fn test_closed_set_is_bounded_and_reusable() {
        // 长会话里 conn_id 会一直换新：墓碑集合必须有上限，否则就是内存泄漏。
        let mut cs = ClosedSet::new();
        for i in 0..(MAX_TOMBSTONES + 100) {
            cs.insert(format!("c{i}"));
        }
        assert_eq!(cs.set.len(), MAX_TOMBSTONES, "墓碑数必须被上限压住");
        assert_eq!(cs.order.len(), MAX_TOMBSTONES);
        // 最旧的被淘汰，最新的还在。
        assert!(!cs.contains("c0"), "最旧的墓碑应已淘汰");
        assert!(cs.contains(&format!("c{}", MAX_TOMBSTONES + 99)));
        // 上限远大于上行队列容量 —— 淘汰时队列里不可能还有对应消息。
        assert!(MAX_TOMBSTONES > UPLINK_QUEUE * 8);

        // 重复 insert 不重复占位。
        let before = cs.order.len();
        let newest = format!("c{}", MAX_TOMBSTONES + 99);
        cs.insert(newest.clone());
        assert_eq!(cs.order.len(), before, "重复立墓碑不得增长");
        assert!(cs.contains(&newest));

        // conn_id 复用（重新 open）时 remove 必须真的清掉。
        cs.remove(&newest);
        assert!(!cs.contains(&newest));
        assert_eq!(cs.order.len(), before - 1);
        // 重复 remove 是安全的 no-op。
        cs.remove(&newest);
        assert_eq!(cs.order.len(), before - 1);
    }

    // ── 建连与双向转发 ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_open_then_echo_roundtrip() {
        // echo 目标：读到什么写回什么。
        let t = target(|mut s| async move {
            let mut buf = vec![0u8; 4096];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .await;
        let (tun, mut rx) = tunnel(small_limits());

        tun.handle_open(&open_payload("c1", t.addr)).await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:opened");
        assert_eq!(p["conn_id"], "c1");

        // relay → 目标。
        let hello = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        tun.handle_data(&json!({
            "conn_id": "c1",
            "seq": 0,
            "data": B64.encode(hello),
        }))
        .await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:ack", "写完必须回 ack");
        assert_eq!(p["conn_id"], "c1");
        assert_eq!(p["bytes"].as_u64().unwrap() as usize, hello.len());

        // 目标 → relay（echo 回来的同一串字节），seq 从 0 开始。
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:data");
        assert_eq!(p["seq"].as_u64().unwrap(), 0);
        assert_eq!(B64.decode(p["data"].as_str().unwrap()).unwrap(), hello);

        // 第二条数据 seq=1，连续。
        tun.handle_data(&json!({
            "conn_id": "c1",
            "seq": 1,
            "data": B64.encode(b"second"),
        }))
        .await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:ack");
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:data");
        assert_eq!(p["seq"].as_u64().unwrap(), 1);
        assert_eq!(B64.decode(p["data"].as_str().unwrap()).unwrap(), b"second");

        assert_eq!(tun.active_count(), 1);
        tun.shutdown();
        assert_eq!(tun.active_count(), 0);
    }

    #[tokio::test]
    async fn test_out_of_order_seq_is_protocol_error() {
        let t = idle_target().await;
        let (tun, mut rx) = tunnel(small_limits());
        tun.handle_open(&open_payload("c1", t.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");

        // 第一条就发 seq=1（期望 0）。
        tun.handle_data(&json!({
            "conn_id": "c1",
            "seq": 1,
            "data": B64.encode(b"x"),
        }))
        .await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:error");
        assert_eq!(p["kind"], "protocol");
        assert_eq!(tun.active_count(), 0, "违约连接必须立刻摘除");

        // 重复 seq 同样违约（新连接上重放）。
        let t2 = idle_target().await;
        tun.handle_open(&open_payload("c2", t2.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");
        tun.handle_data(&json!({"conn_id":"c2","seq":0,"data":B64.encode(b"a")}))
            .await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:ack");
        tun.handle_data(&json!({"conn_id":"c2","seq":0,"data":B64.encode(b"b")}))
            .await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:error");
        assert_eq!(p["kind"], "protocol");
        tun.shutdown();
    }

    #[tokio::test]
    async fn test_invalid_base64_is_protocol_error() {
        let t = idle_target().await;
        let (tun, mut rx) = tunnel(small_limits());
        tun.handle_open(&open_payload("c1", t.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");
        tun.handle_data(&json!({"conn_id":"c1","seq":0,"data":"!!!not-base64!!!"}))
            .await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:error");
        assert_eq!(p["kind"], "protocol");
        tun.shutdown();
    }

    #[tokio::test]
    async fn test_tcp_data_for_unknown_connection_is_ignored() {
        let (tun, mut rx) = tunnel(small_limits());
        // 未知 conn_id：静默忽略，不能产生任何消息，更不能 panic。
        tun.handle_data(&json!({"conn_id":"ghost","seq":0,"data":B64.encode(b"x")}))
            .await;
        tun.handle_ack(&json!({"conn_id":"ghost","bytes":10}));
        tun.handle_eof(&json!({"conn_id":"ghost"}));
        tun.handle_close(&json!({"conn_id":"ghost"}));
        assert_quiet(&mut rx, "unknown conn_id").await;
    }

    // ── 流控 ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_window_limits_unacked_bytes_then_resumes() {
        const TOTAL: usize = 2 * 1024 * 1024;
        const WIN: usize = 64 * 1024;
        // 目标一次写 2 MiB，然后关掉写方向（让 agent 收到 EOF）。
        let t = target(|mut s| async move {
            let data: Vec<u8> = (0..TOTAL).map(|i| (i % 251) as u8).collect();
            let _ = s.write_all(&data).await;
            let _ = s.shutdown().await;
            // 等 agent 把数据都拉完再退出。
            tokio::time::sleep(Duration::from_secs(10)).await;
        })
        .await;
        let (tun, mut rx) = tunnel(Limits {
            connect: Duration::from_secs(5),
            max_conns: MAX_CONNS,
            window: WIN,
        });
        tun.handle_open(&open_payload("c1", t.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");

        // 不 ack：已发未确认字节数必须停在 WINDOW 以内。
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut unacked = 0usize;
        let mut received = 0usize;
        let mut next_seq = 0u64;
        let mut got: Vec<u8> = Vec::with_capacity(TOTAL);
        while unacked < WIN {
            let (ty, p) = tokio::time::timeout_at(deadline.into(), async { recv(&mut rx).await })
                .await
                .expect("window should fill up");
            assert_eq!(ty, "tcp:data");
            assert_eq!(p["seq"].as_u64().unwrap(), next_seq);
            next_seq += 1;
            let bytes = B64.decode(p["data"].as_str().unwrap()).unwrap();
            assert!(bytes.len() <= MAX_CHUNK_BYTES);
            unacked += bytes.len();
            received += bytes.len();
            got.extend_from_slice(&bytes);
        }
        assert!(unacked <= WIN, "已发未确认 {unacked} 超过窗口 {WIN}");
        // 窗口满之后不再有数据到达。
        assert_quiet(&mut rx, "window full").await;

        // 补发 ack：剩余数据必须全部到达且内容一致。
        let mut acked = 0usize;
        while received < TOTAL {
            // 把当前未确认的全部确认掉。
            tun.handle_ack(&json!({"conn_id":"c1","bytes":unacked}));
            acked += unacked;
            unacked = 0;
            let (ty, p) =
                tokio::time::timeout(Duration::from_secs(10), async { recv(&mut rx).await })
                    .await
                    .expect("acked window should let more data through");
            if ty == "tcp:eof" {
                break;
            }
            assert_eq!(ty, "tcp:data");
            assert_eq!(p["seq"].as_u64().unwrap(), next_seq);
            next_seq += 1;
            let bytes = B64.decode(p["data"].as_str().unwrap()).unwrap();
            unacked += bytes.len();
            received += bytes.len();
            got.extend_from_slice(&bytes);
        }
        assert_eq!(received, TOTAL, "ack 之后剩余数据必须全部到达");
        assert_eq!(got.len(), TOTAL);
        let expect: Vec<u8> = (0..TOTAL).map(|i| (i % 251) as u8).collect();
        assert_eq!(got, expect, "字节内容必须与目标写入的完全一致");
        assert_eq!(acked + unacked, TOTAL, "ack 与未确认字节数之和应等于总量");
        // 目标关写方向 → eof 已被我们收到过（循环里 break 出了）。
        tun.shutdown();
    }

    // ── EOF / 终态 ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_target_close_emits_eof_then_closed_on_both_sides() {
        // 目标立刻关闭：agent 读方向 0 字节 → tcp:eof。
        let t = target(|s| async move { drop(s) }).await;
        let (tun, mut rx) = tunnel(small_limits());
        tun.handle_open(&open_payload("c1", t.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:eof", "目标先关必须先看到 eof");
        assert_eq!(p["conn_id"], "c1");

        // 此刻还没有终态（relay 还没发 eof）。
        assert_quiet(&mut rx, "waiting for relay eof").await;

        // relay 侧 eof → 两侧都结束 → 恰好一条 tcp:closed。
        tun.handle_eof(&json!({ "conn_id": "c1" }));
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:closed");
        assert_eq!(p["conn_id"], "c1");
        assert_quiet(&mut rx, "after terminal").await;
        assert_eq!(tun.active_count(), 0, "终态后必须释放名额");
    }

    #[tokio::test]
    async fn test_relay_eof_half_closes_write_and_keeps_reading() {
        // 目标读到 EOF 后回一段数据再关闭：验证 tcp:eof 只关写方向。
        let t = target(|mut s| async move {
            let mut buf = vec![0u8; 1024];
            // 一直读到 EOF（relay 的 tcp:eof 触发的 shutdown）。
            loop {
                match s.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
            let _ = s.write_all(b"after-eof").await;
            let _ = s.shutdown().await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        })
        .await;
        let (tun, mut rx) = tunnel(small_limits());
        tun.handle_open(&open_payload("c1", t.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");

        tun.handle_data(&json!({"conn_id":"c1","seq":0,"data":B64.encode(b"bye")}))
            .await;
        assert_eq!(recv(&mut rx).await.0, "tcp:ack");

        tun.handle_eof(&json!({ "conn_id": "c1" }));
        // 写方向关闭后目标才写出 after-eof，我们仍能读到。
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:data");
        assert_eq!(
            B64.decode(p["data"].as_str().unwrap()).unwrap(),
            b"after-eof"
        );

        let (ty, _) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:eof");
        let (ty, _) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:closed", "两侧都结束后恰好一条 closed");
        assert_quiet(&mut rx, "after closed").await;
        tun.shutdown();
    }

    #[tokio::test]
    async fn test_close_produces_no_further_messages() {
        let t = target(|mut s| async move {
            // 慢目标：一直不回数据，让连接保持打开。
            let mut buf = vec![0u8; 16];
            while s.read(&mut buf).await.unwrap_or(0) > 0 {}
        })
        .await;
        let (tun, mut rx) = tunnel(small_limits());
        tun.handle_open(&open_payload("c1", t.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");

        tun.handle_close(&json!({ "conn_id": "c1" }));
        assert_eq!(tun.active_count(), 0);
        assert_quiet(&mut rx, "after tcp:close").await;

        // 关闭之后再来数据/ack/eof 都是静默忽略。
        tun.handle_data(&json!({"conn_id":"c1","seq":0,"data":B64.encode(b"x")}))
            .await;
        tun.handle_ack(&json!({"conn_id":"c1","bytes":1}));
        tun.handle_eof(&json!({ "conn_id": "c1" }));
        assert_quiet(&mut rx, "after tcp:close (late traffic)").await;
    }

    #[tokio::test]
    async fn test_close_drops_already_queued_messages_for_that_conn() {
        // `tcp:close` 的严格语义：之后一条消息都不能再发，包括中止瞬间已经
        // 躺在队列里等 POST 的那些。分两层验证：排空侧的墓碑过滤 + `Out::send`
        // 源头拦截。
        let (tun, mut rx) = tunnel(small_limits());
        tun.handle_close(&json!({ "conn_id": "dead" }));

        let stale = serde_json::to_string(&Message {
            msg_type: "tcp:data".to_string(),
            session_id: "sess-test".to_string(),
            payload: json!({"conn_id":"dead","seq":0,"data":B64.encode(b"stale")}),
        })
        .unwrap();
        let live = serde_json::to_string(&Message {
            msg_type: "tcp:opened".to_string(),
            session_id: "sess-test".to_string(),
            payload: json!({"conn_id":"live"}),
        })
        .unwrap();

        // 排空侧：已 close 连接的那条被剔掉，活连接的那条原样保留。
        let kept = drop_closed_conns(vec![stale, live.clone()], &tun.inner.out.closed);
        assert_eq!(kept, vec![live], "已 close 连接的消息必须被丢弃");

        // 源头：墓碑 id 连队列都进不去（send 返回 true = 通道本身健康）。
        assert!(
            tun.inner
                .out
                .send("tcp:data", json!({"conn_id":"dead","seq":1,"data":""}))
                .await,
            "被墓碑拦下不等于通道坏掉"
        );

        // 重新 open 同一个 id 之后墓碑必须清除（名额已释放，id 可复用）。
        let t = idle_target().await;
        tun.handle_open(&open_payload("dead", t.addr)).await;
        let (ty, _) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:opened", "复用 conn_id 的新连接必须能发消息");
        assert_quiet(&mut rx, "after reuse").await;
        tun.shutdown();
    }

    #[tokio::test]
    async fn test_shutdown_aborts_all_connections_silently() {
        let t1 = idle_target().await;
        let t2 = idle_target().await;
        let (tun, mut rx) = tunnel(small_limits());
        tun.handle_open(&open_payload("c1", t1.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");
        tun.handle_open(&open_payload("c2", t2.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");
        assert_eq!(tun.active_count(), 2);

        tun.shutdown();
        assert_eq!(tun.active_count(), 0);
        // shutdown 之后不再有任何消息（包括终态）。
        assert_quiet(&mut rx, "after shutdown").await;
        // 名额已全部归还：可以再开满 32 个新连接（这里只验证 2 个能重新进来）。
        let t3 = idle_target().await;
        tun.handle_open(&open_payload("c3", t3.addr)).await;
        // 上行已死，不会再有消息；这句只是确认派发链路不 panic。
        assert_quiet(&mut rx, "after shutdown (new open)").await;
    }

    #[tokio::test]
    async fn test_each_accepted_open_yields_exactly_one_terminal() {
        // 目标接受后立刻关闭 → opened + eof + (relay eof 后) closed。
        let t = target(|s| async move { drop(s) }).await;
        let (tun, mut rx) = tunnel(small_limits());
        tun.handle_open(&open_payload("c1", t.addr)).await;

        let mut terminal = 0;
        let mut saw_opened = false;
        let mut sent_eof = false;
        loop {
            let (ty, _) = tokio::time::timeout(Duration::from_secs(10), recv(&mut rx))
                .await
                .expect("should reach a terminal state");
            match ty.as_str() {
                "tcp:opened" => saw_opened = true,
                "tcp:eof" => {
                    sent_eof = true;
                    tun.handle_eof(&json!({ "conn_id": "c1" }));
                }
                "tcp:closed" | "tcp:error" => {
                    terminal += 1;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_opened);
        assert!(sent_eof);
        assert_eq!(terminal, 1);
        assert_quiet(&mut rx, "after the single terminal").await;
    }

    // ── 建连失败 ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_connect_refused_is_connect_error() {
        // 用一个确定没有监听者的端口触发 ECONNREFUSED。
        //
        // 不要用"bind(0) 拿到端口再 drop"的写法：drop 之后那个端口回到内核的
        // 临时端口池，同进程里并行跑的其他测试（本文件每个 target() 都 bind(0)）
        // 有机会正好拿到它，connect 就会成功 —— 那是真正的 flaky 来源。
        // 端口 1 在 Linux 临时端口范围（32768-60999）之外，`bind(0)` 永远不会
        // 分配它，非 root 也绑不上，因此恒为"无监听者"。
        const CLOSED_PORT: u16 = 1;

        let (tun, mut rx) = tunnel(small_limits());
        tun.handle_open(&json!({
            "conn_id": "c1",
            "host": "127.0.0.1",
            "port": CLOSED_PORT,
        }))
        .await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:error");
        assert_eq!(p["kind"], "connect");
        assert_eq!(p["conn_id"], "c1");
        assert_quiet(&mut rx, "after connect error").await;
        assert_eq!(tun.active_count(), 0, "失败必须归还名额");
    }

    #[tokio::test]
    async fn test_connect_timeout_is_timeout_error() {
        let (tun, mut rx) = tunnel(Limits {
            // 极小超时 + 不可达的 TEST-NET 地址。
            connect: Duration::from_millis(50),
            max_conns: MAX_CONNS,
            window: WINDOW_BYTES,
        });
        tun.handle_open(&json!({
            "conn_id": "c1",
            "host": "192.0.2.1",
            "port": 9,
        }))
        .await;
        let (ty, p) = tokio::time::timeout(Duration::from_secs(10), recv(&mut rx))
            .await
            .expect("timeout path must terminate");
        assert_eq!(ty, "tcp:error");
        assert_eq!(p["kind"], "timeout");
        assert_eq!(tun.active_count(), 0);
    }

    // ── 配额与参数校验 ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_busy_when_max_conns_reached() {
        let t = idle_target().await;
        let (tun, mut rx) = tunnel(Limits {
            connect: Duration::from_secs(5),
            max_conns: 1,
            window: WINDOW_BYTES,
        });
        tun.handle_open(&open_payload("c1", t.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");

        tun.handle_open(&open_payload("c2", t.addr)).await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:error");
        assert_eq!(p["kind"], "busy");
        assert_eq!(p["conn_id"], "c2");
        assert_eq!(tun.active_count(), 1, "busy 不得占用名额");
        tun.shutdown();
    }

    #[tokio::test]
    async fn test_duplicate_conn_id_is_invalid_and_keeps_old_connection() {
        let t = target(|mut s| async move {
            let mut buf = vec![0u8; 64];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // echo，用来证明旧连接还活着。
                        if s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .await;
        let (tun, mut rx) = tunnel(small_limits());
        tun.handle_open(&open_payload("c1", t.addr)).await;
        assert_eq!(recv(&mut rx).await.0, "tcp:opened");

        tun.handle_open(&open_payload("c1", t.addr)).await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:error");
        assert_eq!(p["kind"], "invalid");
        assert_eq!(p["conn_id"], "c1");
        assert_eq!(tun.active_count(), 1, "旧连接不能被顶替");

        // 旧连接仍然可用。
        tun.handle_data(&json!({"conn_id":"c1","seq":0,"data":B64.encode(b"still-here")}))
            .await;
        assert_eq!(recv(&mut rx).await.0, "tcp:ack");
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:data");
        assert_eq!(
            B64.decode(p["data"].as_str().unwrap()).unwrap(),
            b"still-here"
        );
        tun.shutdown();
    }

    #[tokio::test]
    async fn test_invalid_port_and_conn_id_are_rejected() {
        let (tun, mut rx) = tunnel(small_limits());
        // 非法 port。
        tun.handle_open(&json!({"conn_id":"c1","host":"127.0.0.1","port":0}))
            .await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:error");
        assert_eq!(p["kind"], "invalid");

        // 非法 conn_id（空）。
        tun.handle_open(&json!({"conn_id":"","host":"127.0.0.1","port":80}))
            .await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:error");
        assert_eq!(p["kind"], "invalid");

        // 非法 conn_id（超长）。
        let long = "a".repeat(65);
        tun.handle_open(&json!({"conn_id":long,"host":"127.0.0.1","port":80}))
            .await;
        let (ty, p) = recv(&mut rx).await;
        assert_eq!(ty, "tcp:error");
        assert_eq!(p["kind"], "invalid");
        assert_eq!(tun.active_count(), 0);
    }
}
