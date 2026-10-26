#![allow(dead_code)]
//! 网页流式代理（MYS-2383 阶段2）：把浏览器鉴权后的网页 GET 转成 agent 的
//! `web:request`，再把 agent 的流式回包还原成 HTTP 响应。
//!
//! 与 `file_transfer` 的区别在于**关联的是「请求」而不是「文件」**，且回包
//! 必须严格保序（`seq` 连续），因此这里自带一套独立的状态与清理路径：
//!
//! - `POST /agent/web/ticket` 签发**绑定会话**的短期凭据（ticket）。iframe
//!   主文档与浏览器自动发出的子资源请求无法携带 `Authorization` 头，只能把
//!   凭据放进 URL，所以凭据必须短命、随机、与 session token 无任何推导关系。
//! - `GET /agent/web/proxy?ticket=…&url=…` 用 ticket（或 `X-SR-Token` +
//!   `X-Auth`，供 API/测试）鉴权，校验会话能力 `web:proxy-v1`，把请求下发给
//!   agent 并把回包流式写回。
//!
//! 硬约束（与任务卡一致）：
//! 1. 网页流量**绝不**进入终端/MCP 广播通道：回包按 `request_id` 进每请求
//!    独立的 bounded channel（[`EVENT_CHANNEL_CAPACITY`]）。
//! 2. 任何锁都不跨 await 持有；路由循环里只用 `try_send`，回包通道满时丢弃
//!    该网页请求并让 agent 停发，不让终端/MCP 被一个卡住的浏览器拖住。
//! 3. 每条清理路径只清一次：请求的移除与 `web:cancel` 都由
//!    [`WebRequestGuard`] 统一负责（正常结束则不 cancel）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::Stream;

use crate::proto::{Message as ProtoMessage, Permission};
use crate::relay::SharedState;

/// 注册能力名：会话注册的 capabilities 必须含它才允许走网页代理。旧版 agent
/// 不声明 → 相关接口一律 404（UI 不依赖错误正文，只看状态码）。
pub const CAPABILITY: &str = "web:proxy-v1";

/// 每个网页请求独立的回包通道容量。回包是 `head` + N×`chunk` + `end`，
/// 32 条足够吸收一次网络读到的多块，同时把单个卡住的浏览器占用的内存
/// 钉在一个小块数量级（32 × ≤64KiB ≈ 2MiB）。
pub const EVENT_CHANNEL_CAPACITY: usize = 32;

/// `url` 查询参数上限（任务卡：长度 ≤4096）。
pub const MAX_URL_LEN: usize = 4096;

/// ticket 字节数（hex 编码后 64 字符）。256 bit CSPRNG 输出，与 session
/// token 无任何推导关系。
const TICKET_BYTES: usize = 32;

/// 全部会话的 ticket 总量硬上限，防「大量会话各自签发到上限」把内存顶穿。
/// 会话本身已有 [`crate::relay::MAX_SESSIONS`] 上限，这里再兜一层。
const MAX_TOTAL_TICKETS: usize = 8192;

/// 网页请求的可调时间/容量参数。生产值见 [`WebLimits::default`]；单测用小
/// 值验证「首包超时」「块间 idle 超时」等路径，不必真等 35s。
#[derive(Debug, Clone, Copy)]
pub struct WebLimits {
    /// ticket 有效期。
    pub ticket_ttl: Duration,
    /// 每 session 同时有效的 ticket 数上限（超出按最旧淘汰）。
    pub max_tickets_per_session: usize,
    /// 每 session 同时在途的网页请求上限（超出 429）。
    pub max_inflight_per_session: usize,
    /// 等 agent 首个回包（`web:head`）的上限。agent 是 30s，relay 留余量。
    pub head_timeout: Duration,
    /// 块间空闲上限。agent 是 30s，relay 留余量。
    pub idle_timeout: Duration,
    /// 单个请求的总时限。agent 是 5min，relay 与其对齐（略紧）。
    pub total_timeout: Duration,
    /// 把 `web:request` 投进 agent bulk 通道的等待上限。
    pub downlink_timeout: Duration,
}

impl Default for WebLimits {
    fn default() -> Self {
        Self {
            ticket_ttl: Duration::from_secs(600),
            max_tickets_per_session: 8,
            max_inflight_per_session: 8,
            head_timeout: Duration::from_secs(35),
            idle_timeout: Duration::from_secs(35),
            total_timeout: Duration::from_secs(300),
            downlink_timeout: Duration::from_secs(5),
        }
    }
}

/// agent 回包事件（`web:head` / `web:chunk` / `web:end` / `web:error` 解码后）。
#[derive(Debug)]
pub enum WebEvent {
    Head {
        status: u16,
        headers: Vec<(String, String)>,
        final_url: String,
    },
    Chunk {
        seq: u32,
        data: Vec<u8>,
    },
    End,
    Error {
        kind: String,
        message: String,
    },
    /// agent 回包本身不合协议（base64 坏、字段缺失/类型错）。与 `Error`
    /// 分开：它不是「目标站失败」，而是「这条流已经不可信」，必须中断响应
    /// 而不是尝试继续拼接。
    Invalid {
        reason: &'static str,
    },
}

/// 一条在途网页请求的 relay 侧句柄。
#[derive(Debug)]
pub struct WebSink {
    pub session_id: String,
    pub tx: mpsc::Sender<WebEvent>,
}

struct TicketEntry {
    session_id: String,
    expires_at: tokio::time::Instant,
}

#[derive(Default)]
struct TicketState {
    /// ticket → 归属与过期时刻。key 就是 ticket 明文（与 session token 一样
    /// 只存在于内存，绝不写日志）。
    tickets: HashMap<String, TicketEntry>,
    /// session → 该会话已签发的 ticket（按签发顺序，用于「最多 N 个」淘汰）。
    by_session: HashMap<String, VecDeque<String>>,
}

#[derive(Default)]
struct StreamState {
    /// request_id → 回包 sink。
    sinks: HashMap<String, WebSink>,
    /// session → 该会话在途的 request_id 集合（O(1) 数并发，避免每次遍历
    /// 全表；与 `sinks` 在同一把锁内维护，二者不会失配）。
    by_session: HashMap<String, HashSet<String>>,
}

/// 网页代理的全部 relay 侧状态。挂在 [`SharedState`] 上，进程内单例。
///
/// 两把锁刻意分开：ticket 校验发生在请求入口，回包路由发生在 agent 上行
/// 热路径，分开后二者互不阻塞。每把锁的临界区都只有哈希操作，**不含
/// await**。
///
/// 时间参数用原子量存而不是普通字段：`SharedState` 一旦建成就是 `Arc`，
/// 单测无法拿到 `&mut` 去把 35s 的首包超时改小；原子量让测试能在真实
/// `SharedState` 上把超时压到毫秒级，从而验证超时分支本身（而不是另建
/// 一套与生产路径不同的对象）。
pub struct WebProxyStore {
    ticket_ttl_ms: std::sync::atomic::AtomicU64,
    head_timeout_ms: std::sync::atomic::AtomicU64,
    idle_timeout_ms: std::sync::atomic::AtomicU64,
    total_timeout_ms: std::sync::atomic::AtomicU64,
    downlink_timeout_ms: std::sync::atomic::AtomicU64,
    max_tickets_per_session: usize,
    max_inflight_per_session: usize,
    tickets: RwLock<TicketState>,
    streams: RwLock<StreamState>,
}

impl Default for WebProxyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl WebProxyStore {
    pub fn new() -> Self {
        Self::with_limits(WebLimits::default())
    }

    /// 单测用：改成毫秒级超时/更小的配额。
    pub fn with_limits(limits: WebLimits) -> Self {
        use std::sync::atomic::AtomicU64;
        Self {
            ticket_ttl_ms: AtomicU64::new(limits.ticket_ttl.as_millis() as u64),
            head_timeout_ms: AtomicU64::new(limits.head_timeout.as_millis() as u64),
            idle_timeout_ms: AtomicU64::new(limits.idle_timeout.as_millis() as u64),
            total_timeout_ms: AtomicU64::new(limits.total_timeout.as_millis() as u64),
            downlink_timeout_ms: AtomicU64::new(limits.downlink_timeout.as_millis() as u64),
            max_tickets_per_session: limits.max_tickets_per_session,
            max_inflight_per_session: limits.max_inflight_per_session,
            tickets: RwLock::new(TicketState::default()),
            streams: RwLock::new(StreamState::default()),
        }
    }

    pub fn limits(&self) -> WebLimits {
        use std::sync::atomic::Ordering::Relaxed;
        let ms = |a: &std::sync::atomic::AtomicU64| Duration::from_millis(a.load(Relaxed));
        WebLimits {
            ticket_ttl: ms(&self.ticket_ttl_ms),
            max_tickets_per_session: self.max_tickets_per_session,
            max_inflight_per_session: self.max_inflight_per_session,
            head_timeout: ms(&self.head_timeout_ms),
            idle_timeout: ms(&self.idle_timeout_ms),
            total_timeout: ms(&self.total_timeout_ms),
            downlink_timeout: ms(&self.downlink_timeout_ms),
        }
    }

    /// 只改时间参数（配额不动）。单测用来把生产超时压到毫秒级。
    pub fn set_timeouts(
        &self,
        head_timeout: Duration,
        idle_timeout: Duration,
        total_timeout: Duration,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        self.head_timeout_ms
            .store(head_timeout.as_millis() as u64, Relaxed);
        self.idle_timeout_ms
            .store(idle_timeout.as_millis() as u64, Relaxed);
        self.total_timeout_ms
            .store(total_timeout.as_millis() as u64, Relaxed);
    }

    // ── ticket ────────────────────────────────────────────────────

    /// 为一个会话签发 ticket。随机、与 session token 无推导关系、绑定会话，
    /// 过期或超配额（同一会话最旧的先淘汰）后不再有效。
    pub async fn issue_ticket(&self, session_id: &str) -> String {
        let ticket = random_ticket();
        let now = tokio::time::Instant::now();
        let mut st = self.tickets.write().await;
        st.prune_expired(now);
        // 总量兜底：先按过期清，再按「会话已不存在」清（调用方无法感知的
        // 会话残留），仍然超限就退化为「淘汰最早签发的那个」。
        if st.tickets.len() >= MAX_TOTAL_TICKETS {
            st.drop_evictable(MAX_TOTAL_TICKETS / 2);
        }
        let per_session = self.max_tickets_per_session.max(1);
        // 先把该会话已超配额的旧 ticket 摘出来，再动 `tickets`——两处都是
        // `st` 的可变借用，必须分开（借用检查器）。
        let mut evicted: Vec<String> = Vec::new();
        {
            let queue = st.by_session.entry(session_id.to_string()).or_default();
            while queue.len() >= per_session {
                match queue.pop_front() {
                    Some(oldest) => evicted.push(oldest),
                    None => break,
                }
            }
            queue.push_back(ticket.clone());
        }
        for old in evicted {
            st.tickets.remove(&old);
        }
        st.tickets.insert(
            ticket.clone(),
            TicketEntry {
                session_id: session_id.to_string(),
                expires_at: now
                    + Duration::from_millis(
                        self.ticket_ttl_ms
                            .load(std::sync::atomic::Ordering::Relaxed),
                    ),
            },
        );
        ticket
    }

    /// ticket → 所属会话。无效/过期返回 `None`（调用方回 403）。过期条目
    /// 顺手清掉（惰性清理）。
    pub async fn resolve_ticket(&self, ticket: &str) -> Option<String> {
        if ticket.is_empty() {
            return None;
        }
        let now = tokio::time::Instant::now();
        let mut st = self.tickets.write().await;
        match st.tickets.get(ticket) {
            Some(e) if e.expires_at > now => Some(e.session_id.clone()),
            Some(_) => {
                let sid = st.tickets.remove(ticket).map(|e| e.session_id);
                if let Some(sid) = sid {
                    st.unlink_session(&sid, ticket);
                }
                None
            }
            None => None,
        }
    }

    /// 撤销一个会话的全部 ticket（agent 断线 / 会话结束时调用）。返回撤销
    /// 数量。
    pub async fn revoke_session_tickets(&self, session_id: &str) -> usize {
        let mut st = self.tickets.write().await;
        let Some(queue) = st.by_session.remove(session_id) else {
            return 0;
        };
        let mut n = 0;
        for t in queue {
            if st.tickets.remove(&t).is_some() {
                n += 1;
            }
        }
        n
    }

    /// 某会话当前有效 ticket 数（观测/测试用）。
    pub async fn ticket_count(&self, session_id: &str) -> usize {
        let now = tokio::time::Instant::now();
        let st = self.tickets.read().await;
        st.by_session.get(session_id).map_or(0, |q| {
            q.iter()
                .filter(|t| st.tickets.get(*t).is_some_and(|e| e.expires_at > now))
                .count()
        })
    }

    // ── 在途请求 ──────────────────────────────────────────────────

    /// 登记一条在途请求。会话并发已满时返回 `None`（调用方回 429）。
    pub async fn register_stream(
        &self,
        session_id: &str,
        request_id: &str,
        tx: mpsc::Sender<WebEvent>,
    ) -> Option<()> {
        let mut st = self.streams.write().await;
        let inflight = st.by_session.get(session_id).map_or(0, |s| s.len());
        if inflight >= self.max_inflight_per_session {
            return None;
        }
        st.by_session
            .entry(session_id.to_string())
            .or_default()
            .insert(request_id.to_string());
        st.sinks.insert(
            request_id.to_string(),
            WebSink {
                session_id: session_id.to_string(),
                tx,
            },
        );
        Some(())
    }

    /// 移除一条在途请求。返回 `true` 表示确实是本次调用把它摘掉的（用于
    /// 判断「要不要给 agent 发 cancel」，避免重复下发）。
    pub async fn remove_stream(&self, session_id: &str, request_id: &str) -> bool {
        let mut st = self.streams.write().await;
        if st.sinks.remove(request_id).is_none() {
            return false;
        }
        if let Some(set) = st.by_session.get_mut(session_id) {
            set.remove(request_id);
            if set.is_empty() {
                st.by_session.remove(session_id);
            }
        }
        true
    }

    /// 取一个回包 sink（回包路由用）。
    async fn sink(&self, request_id: &str) -> Option<mpsc::Sender<WebEvent>> {
        self.streams
            .read()
            .await
            .sinks
            .get(request_id)
            .map(|s| s.tx.clone())
    }

    /// 该会话当前在途请求数。
    pub async fn inflight(&self, session_id: &str) -> usize {
        self.streams
            .read()
            .await
            .by_session
            .get(session_id)
            .map_or(0, |s| s.len())
    }

    /// 取消并清理一个会话的全部在途请求（agent 断线 / 注册替换 / 会话移除）。
    ///
    /// 只把 sink 从表里摘掉并丢弃：对应的响应任务会因通道关闭而结束，浏览器
    /// 侧看到的是提前 EOF。**不给 agent 发 cancel**——这条路径的前提就是
    /// agent 已经不在（或已被新连接替换），发了也只是浪费一条下行。
    pub async fn cancel_session(&self, session_id: &str) -> usize {
        let ids: Vec<String> = {
            let mut st = self.streams.write().await;
            let Some(set) = st.by_session.remove(session_id) else {
                return 0;
            };
            for rid in &set {
                st.sinks.remove(rid);
            }
            set.into_iter().collect()
        };
        ids.len()
    }

    /// 定期清理：过期 ticket + 超时在途请求（兜底，正常路径由 guard 清理）。
    pub async fn sweep(&self) -> (usize, usize) {
        let now = tokio::time::Instant::now();
        let tickets = {
            let mut st = self.tickets.write().await;
            st.prune_expired(now)
        };
        let stale: Vec<(String, String)> = {
            let st = self.streams.read().await;
            st.sinks
                .iter()
                .filter(|(_, s)| s.tx.is_closed())
                .map(|(rid, s)| (s.session_id.clone(), rid.clone()))
                .collect()
        };
        let mut n = 0;
        for (sid, rid) in stale {
            if self.remove_stream(&sid, &rid).await {
                n += 1;
            }
        }
        (tickets, n)
    }
}

fn random_ticket() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: [u8; TICKET_BYTES] = rng.gen();
    hex::encode(bytes)
}

impl TicketState {
    fn prune_expired(&mut self, now: tokio::time::Instant) -> usize {
        let expired: Vec<(String, String)> = self
            .tickets
            .iter()
            .filter(|(_, e)| e.expires_at <= now)
            .map(|(t, e)| (t.clone(), e.session_id.clone()))
            .collect();
        for (t, sid) in &expired {
            self.tickets.remove(t);
            self.unlink_session(sid, t);
        }
        expired.len()
    }

    fn unlink_session(&mut self, session_id: &str, ticket: &str) {
        if let Some(q) = self.by_session.get_mut(session_id) {
            q.retain(|t| t != ticket);
            if q.is_empty() {
                self.by_session.remove(session_id);
            }
        }
    }

    /// 总量超限时的兜底淘汰：按过期时刻排序，丢掉最早过期（≈最早签发）的
    /// 那一批。正常运行不会走到这里（每会话 8 个 + 会话数有上限）。
    fn drop_evictable(&mut self, target_len: usize) {
        let mut all: Vec<(String, tokio::time::Instant)> = self
            .tickets
            .iter()
            .map(|(t, e)| (t.clone(), e.expires_at))
            .collect();
        all.sort_by_key(|(_, exp)| *exp);
        let drop_n = all.len().saturating_sub(target_len);
        for (t, _) in all.into_iter().take(drop_n) {
            if let Some(e) = self.tickets.remove(&t) {
                self.unlink_session(&e.session_id, &t);
            }
        }
    }
}

// ── agent → relay 回包路由 ──────────────────────────────────────────

/// `route_agent_message` 的网页分支：识别 `web:head/chunk/end/error` 并按
/// `request_id`送入该请求独立的 bounded channel。
///
/// 返回 `true` 表示这条消息属于网页代理（调用方不再走广播/MCP 路径），
/// 从而保证网页回包永不进入 `broadcast_types`、终端录制或 MCP oneshot。
///
/// 投递一律用 `try_send`：路由循环跑在该 agent 的**唯一**上行任务上，
/// 任何 await 都会连累终端输出与 MCP 调用。通道满 → 判定这个浏览器已经
/// 跟不上，摘掉该请求并通知 agent 停发（cancel 用 try_send，满则丢弃）。
pub async fn route_agent_message(
    state: &Arc<SharedState>,
    session_id: &str,
    msg: &ProtoMessage,
) -> bool {
    let request_id = msg
        .payload
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !matches!(
        msg.msg_type.as_str(),
        "web:head" | "web:chunk" | "web:end" | "web:error"
    ) {
        return false;
    }
    // 没有 request_id 就无法关联，丢弃（agent 侧也保证非空）。
    if request_id.is_empty() || request_id.len() > 256 {
        tracing::debug!(
            session = %session_id,
            msg_type = %msg.msg_type,
            "web proxy: agent message without usable request_id — dropped"
        );
        return true;
    }
    let Some(tx) = state.web_proxy.sink(request_id).await else {
        // 未知/已结束的请求：迟到回包直接丢弃（不建任何条目）。
        tracing::debug!(
            session = %session_id,
            msg_type = %msg.msg_type,
            "web proxy: reply for unknown request dropped"
        );
        return true;
    };

    let event = match msg.msg_type.as_str() {
        "web:head" => parse_head(&msg.payload),
        "web:chunk" => parse_chunk(&msg.payload),
        "web:end" => Some(WebEvent::End),
        _ => Some(WebEvent::Error {
            kind: msg
                .payload
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("http")
                .to_string(),
            message: msg
                .payload
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
    };
    let Some(event) = event else {
        // 回包本身不合协议 → 中断这条流（不静默拼坏内容），并让 agent 停发。
        abort_stream(
            state,
            session_id,
            request_id,
            &tx,
            WebEvent::Invalid {
                reason: "malformed agent reply",
            },
        )
        .await;
        return true;
    };

    match tx.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            // 浏览器消费不动（暂停/断网/被阻塞）→ 丢弃这个网页请求并终止
            // 流，绝不无界缓冲，也绝不让 agent 的上行路由等在这里。
            tracing::warn!(
                session = %session_id,
                msg_type = %msg.msg_type,
                "web proxy: reply channel full — request aborted, agent asked to stop"
            );
            state.web_proxy.remove_stream(session_id, request_id).await;
            send_cancel_nonblocking(state, session_id, request_id).await;
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            // 响应任务已经走了（客户端断开/超时）：清掉残留条目。
            state.web_proxy.remove_stream(session_id, request_id).await;
        }
    }
    true
}

/// 把一条「流不可信」事件尽量投给响应任务后摘掉请求，并通知 agent 停发。
async fn abort_stream(
    state: &Arc<SharedState>,
    session_id: &str,
    request_id: &str,
    tx: &mpsc::Sender<WebEvent>,
    ev: WebEvent,
) {
    let reason = match &ev {
        WebEvent::Invalid { reason } => *reason,
        _ => "aborted",
    };
    tracing::warn!(
        session = %session_id,
        reason,
        "web proxy: agent reply violates the protocol — stream aborted"
    );
    let _ = tx.try_send(ev);
    state.web_proxy.remove_stream(session_id, request_id).await;
    send_cancel_nonblocking(state, session_id, request_id).await;
}

/// 非阻塞下发 `web:cancel`：先 `try_send`，bulk 满则交给一个短时后台任务，
/// 保证路由循环永不等候。
async fn send_cancel_nonblocking(state: &Arc<SharedState>, session_id: &str, request_id: &str) {
    let bulk = {
        let broadcast = state.agent_broadcast.read().await;
        broadcast
            .get(session_id)
            .and_then(|cm| cm.agent_bulk.clone())
    };
    let Some(bulk) = bulk else { return };
    let proto = json!({
        "type": "web:cancel",
        "session_id": session_id,
        "payload": { "request_id": request_id }
    })
    .to_string();
    if let Err(mpsc::error::TrySendError::Full(proto)) = bulk.try_send(proto) {
        let timeout = state.web_proxy.limits().downlink_timeout;
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                timeout,
                crate::relay::file_transfer::deliver_bulk(&bulk, proto),
            )
            .await;
        });
    }
}

/// 严格解析 `web:head` 的 payload。任何字段缺失/类型不符 → `None`（由调用
/// 方按「回包损坏」中断流，而不是返回半个 head 给浏览器）。
fn parse_head(payload: &serde_json::Value) -> Option<WebEvent> {
    let status = payload.get("status")?.as_u64()?;
    if !(100..=599).contains(&status) {
        return None;
    }
    let final_url = payload.get("final_url")?.as_str()?.to_string();
    let raw = payload.get("headers")?.as_array()?;
    let mut headers = Vec::with_capacity(raw.len());
    for pair in raw {
        let pair = pair.as_array()?;
        if pair.len() != 2 {
            return None;
        }
        let name = pair[0].as_str()?;
        let value = pair[1].as_str()?;
        if name.is_empty() {
            return None;
        }
        headers.push((name.to_string(), value.to_string()));
    }
    Some(WebEvent::Head {
        status: status as u16,
        headers,
        final_url,
    })
}

/// 严格解析 `web:chunk`：`seq` 必须是 u32，`data` 必须是合法 base64。
fn parse_chunk(payload: &serde_json::Value) -> Option<WebEvent> {
    let seq = payload.get("seq")?.as_u64()?;
    if seq > u32::MAX as u64 {
        return None;
    }
    let data = payload.get("data")?.as_str()?;
    let bytes = BASE64.decode(data).ok()?;
    Some(WebEvent::Chunk {
        seq: seq as u32,
        data: bytes,
    })
}

/// agent 报的错误 kind → head 之前的 HTTP 状态码。任务卡只允许 400/502/504/503。
fn kind_to_status(kind: &str) -> StatusCode {
    match kind {
        "invalid_url" | "unsupported_method" => StatusCode::BAD_REQUEST,
        "timeout" => StatusCode::GATEWAY_TIMEOUT,
        "connect" | "http" => StatusCode::BAD_GATEWAY,
        // busy / cancelled / 未知：agent 侧暂时无法服务。
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

// ── 请求清理守卫 ────────────────────────────────────────────────────

/// 一条网页请求的生命周期守卫：**唯一**负责「从 map 摘除 + 必要时通知
/// agent 停发」。
///
/// - 正常收到 `web:end` → [`finish`](Self::finish)：只摘除，不 cancel。
/// - 客户端断开 / 超时 / 回包损坏 / 任务被 drop → `Drop`：摘除并下发
///   `web:cancel`。
///
/// 只有确实由自己摘掉（`remove_stream` 返回 true）时才发 cancel，因此
/// 「路由侧已判定通道满并清理」与「守卫随后 drop」不会重复下发。
struct WebRequestGuard {
    state: Arc<SharedState>,
    session_id: String,
    request_id: String,
    armed: bool,
}

impl WebRequestGuard {
    fn new(state: Arc<SharedState>, session_id: String, request_id: String) -> Self {
        Self {
            state,
            session_id,
            request_id,
            armed: true,
        }
    }

    /// 正常结束：摘除条目、不再 cancel，也不再参与 Drop。
    async fn finish(mut self) {
        self.armed = false;
        self.state
            .web_proxy
            .remove_stream(&self.session_id, &self.request_id)
            .await;
    }
}

impl Drop for WebRequestGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let state = self.state.clone();
        let sid = self.session_id.clone();
        let rid = self.request_id.clone();
        // Drop 不能 await：交给一个短命任务完成摘除 + cancel（有超时上界）。
        let sid_c = sid.clone();
        tokio::spawn(async move {
            if state.web_proxy.remove_stream(&sid_c, &rid).await {
                send_cancel_nonblocking(&state, &sid_c, &rid).await;
            }
        });
    }
}

// ── HTTP 入口 ──────────────────────────────────────────────────────

/// `POST /agent/web/ticket`：签发绑定会话的短期凭据。
///
/// 鉴权 = 服务器口令（`X-Auth`）+ 会话 token（`X-SR-Token`），ro/rw 均可
/// （网页 GET 不改变远端状态）。签发前校验能力：旧 agent 直接 404，省得
/// 浏览器拿一个注定用不上的 ticket 去换 404。
pub async fn ticket_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !crate::relay::auth::server_password_ok(&state, &headers, None).await {
        return crate::relay::auth::invalid_password_response();
    }
    let token = headers
        .get("x-sr-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let (session_id, _permission) = match state.sessions.authenticate(token).await {
        Some(r) => r,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "invalid session token"})),
            )
                .into_response()
        }
    };
    if !has_web_capability(&state, &session_id).await {
        return capability_missing();
    }
    let ticket = state.web_proxy.issue_ticket(&session_id).await;
    // 有意不记录 ticket 本身：它与 session token 同级，只属于浏览器。
    tracing::debug!(
        session = %session_id,
        "web proxy: ticket issued"
    );
    axum::Json(json!({
        "ticket": ticket,
        "expires_in": state.web_proxy.limits().ticket_ttl.as_secs(),
    }))
    .into_response()
}

/// `GET /agent/web/proxy?ticket=…&url=…`（也接受 `X-SR-Token` + `X-Auth`
/// 头鉴权，供 API 与测试使用）。
pub async fn proxy_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    // ── 1. 鉴权：ticket 或 header 会话 token，二选一，都缺则 401 ──
    let ticket = unique_param(&params, "ticket");
    let session_id = match ticket {
        Some(Some(ticket)) => match state.web_proxy.resolve_ticket(&ticket).await {
            Some(sid) => sid,
            // 无效/过期/已撤销的 ticket：403（不是 401——它不是会话口令）。
            None => {
                return (
                    StatusCode::FORBIDDEN,
                    axum::Json(json!({"error": "invalid or expired ticket"})),
                )
                    .into_response()
            }
        },
        // ticket 重复出现：参数非法。
        Some(None) => return bad_request("duplicate ticket parameter"),
        None => {
            if !crate::relay::auth::server_password_ok(&state, &headers, None).await {
                return crate::relay::auth::invalid_password_response();
            }
            let token = headers
                .get("x-sr-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            match state.sessions.authenticate(token).await {
                Some((sid, _)) => sid,
                None => {
                    return (
                        StatusCode::UNAUTHORIZED,
                        axum::Json(json!({"error": "invalid session token"})),
                    )
                        .into_response()
                }
            }
        }
    };

    // ── 2. 能力协商：旧 agent 一律 404（UI 只看状态码） ──
    if !has_web_capability(&state, &session_id).await {
        return capability_missing();
    }

    // ── 3. 参数：只支持 GET、`url` 唯一、长度受限、绝对 http/https ──
    let url = match unique_param(&params, "url") {
        Some(Some(u)) => u,
        Some(None) => return bad_request("duplicate url parameter"),
        None => return bad_request("missing url parameter"),
    };
    if let Err(msg) = validate_url(&url) {
        return bad_request(msg);
    }

    // ── 4. agent 在线？没有 bulk 通道 = 离线 ──
    let bulk_tx = {
        let broadcast = state.agent_broadcast.read().await;
        match broadcast
            .get(&session_id)
            .and_then(|cm| cm.agent_bulk.clone())
        {
            Some(tx) if !tx.is_closed() => tx,
            _ => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(json!({"error": "agent is offline"})),
                )
                    .into_response()
            }
        }
    };

    // ── 5. 登记请求（并发上限 8 → 429） ──
    let request_id = uuid::Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::channel::<WebEvent>(EVENT_CHANNEL_CAPACITY);
    if state
        .web_proxy
        .register_stream(&session_id, &request_id, tx)
        .await
        .is_none()
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(json!({"error": "too many in-flight web requests"})),
        )
            .into_response();
    }
    // 从这里开始，任何提前返回都必须经过 guard 清理（含 cancel）。
    // 总时限从**这一刻**起算：任务卡要求「单请求总时限 5min」，它必须覆盖
    // 「等首包」+「传 body」两段，否则一个卡 30s 才回 head 的页面还能再传
    // 5min，端到端超过约定。
    let request_started = tokio::time::Instant::now();
    let guard = WebRequestGuard::new(state.clone(), session_id.clone(), request_id.clone());

    // ── 6. 下发 `web:request`（bounded 下行，等 ≤5s） ──
    let proto = json!({
        "type": "web:request",
        "session_id": &session_id,
        "payload": {
            "request_id": &request_id,
            "url": &url,
            "method": "GET",
        }
    })
    .to_string();
    if tokio::time::timeout(
        state.web_proxy.limits().downlink_timeout,
        crate::relay::file_transfer::deliver_bulk(&bulk_tx, proto),
    )
    .await
    .is_err()
    {
        tracing::warn!(
            session = %session_id,
            "web proxy: agent downlink busy >{}s — request rejected",
            state.web_proxy.limits().downlink_timeout.as_secs()
        );
        guard.finish().await;
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error": "agent downlink busy"})),
        )
            .into_response();
    }

    // ── 7. 等首包（head 或 error），上限 35s ──
    let limits = state.web_proxy.limits();
    let first = tokio::time::timeout(limits.head_timeout, rx.recv()).await;
    let head = match first {
        Ok(Some(WebEvent::Head {
            status,
            headers,
            final_url,
        })) => (status, headers, final_url),
        Ok(Some(WebEvent::Error { kind, message })) => {
            // agent 已经终止 → 只摘除，不必再 cancel。
            guard.finish().await;
            let status = kind_to_status(&kind);
            return (
                status,
                axum::Json(json!({"error": kind, "message": message})),
            )
                .into_response();
        }
        Ok(Some(WebEvent::End)) => {
            // 没 head 直接 end：协议上不应出现，当作空 200 会让 UI 以为页面
            // 是空白的。如实报错。
            guard.finish().await;
            return bad_gateway("agent ended the stream before sending headers");
        }
        Ok(Some(WebEvent::Chunk { .. })) | Ok(Some(WebEvent::Invalid { .. })) => {
            guard.finish().await;
            return bad_gateway("agent sent a body chunk before headers");
        }
        Ok(None) => {
            // 通道关闭：agent 断线（断线路径已把它从表里摘掉）。
            guard.finish().await;
            return bad_gateway("agent disconnected before sending headers");
        }
        Err(_) => {
            // 超时：agent 可能还在跑 → 必须让它停发。
            drop(guard);
            return (
                StatusCode::GATEWAY_TIMEOUT,
                axum::Json(json!({"error": "timeout", "message": "timed out waiting for the target's response headers"})),
            )
                .into_response();
        }
    };

    // ── 8. 组响应：状态 + 已过滤的响应头 + X-SR-Final-URL + 流式 body ──
    let (status, headers, final_url) = head;
    let mut builder =
        Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY));
    for (name, value) in filter_response_headers(&headers) {
        builder = builder.header(name, value);
    }
    // 供 UI 以同源 fetch 读取（重定向后按 final_url 解析相对资源）。
    if let Ok(v) = HeaderValue::from_str(&final_url) {
        builder = builder.header("x-sr-final-url", v);
    }
    // 提示任何中间缓存都不要把这个响应当成可复用的静态资源。
    builder = builder.header("x-accel-buffering", "no");

    let body_stream = web_body_stream(
        state.clone(),
        session_id,
        request_id,
        rx,
        guard,
        request_started,
    );
    builder
        .body(axum::body::Body::from_stream(body_stream))
        .unwrap_or_else(|_| bad_gateway("failed to build response"))
}

/// 响应 body 流：逐块写出，严格校验 `seq` 连续，超时/断流一律终止。
///
/// `guard` 被移进生成器：客户端断开时 body 被 drop → 生成器被 drop → guard
/// 的 Drop 摘除条目并给 agent 发 `web:cancel`。正常 `web:end` 则走
/// [`WebRequestGuard::finish`]，不发 cancel。
fn web_body_stream(
    state: Arc<SharedState>,
    session_id: String,
    request_id: String,
    mut rx: mpsc::Receiver<WebEvent>,
    guard: WebRequestGuard,
    request_started: tokio::time::Instant,
) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> {
    async_stream::stream! {
        let limits = state.web_proxy.limits();
        let mut guard = Some(guard);
        // 总时限覆盖「等首包 + 传 body」两段（从 handler 发出 web:request 起算）。
        let deadline = request_started + limits.total_timeout;
        let mut expected_seq: u32 = 0;
        loop {
            let now = tokio::time::Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                tracing::warn!(
                    session = %session_id,
                    "web proxy: total time limit exceeded — stream aborted"
                );
                break;
            }
            let wait = remaining.min(limits.idle_timeout);
            match tokio::time::timeout(wait, rx.recv()).await {
                Err(_) => {
                    tracing::warn!(
                        session = %session_id,
                        "web proxy: body idle/total timeout — stream aborted"
                    );
                    break;
                }
                Ok(None) => break, // 通道关闭（agent 断线 / 已判定中止）
                Ok(Some(WebEvent::Chunk { seq, data })) => {
                    if seq != expected_seq {
                        // 丢块或乱序：继续写就是「静默拼坏内容」，直接中断。
                        tracing::warn!(
                            session = %session_id,
                            expected = expected_seq,
                            got = seq,
                            "web proxy: chunk seq gap — stream aborted"
                        );
                        break;
                    }
                    expected_seq = expected_seq.wrapping_add(1);
                    // 只记账不等待：交互流不能被限速拖成卡顿（与终端 SSE 同口径）。
                    state.bandwidth.down.consume(data.len());
                    yield Ok(Bytes::from(data));
                }
                Ok(Some(WebEvent::End)) => {
                    if let Some(g) = guard.take() {
                        g.finish().await;
                    }
                    return;
                }
                Ok(Some(WebEvent::Error { kind, .. })) => {
                    // head 之后的错误：终止 body，如实反映为截断（不发 end）。
                    tracing::warn!(
                        session = %session_id,
                        kind = %kind,
                        "web proxy: agent reported an error mid-body — stream truncated"
                    );
                    break;
                }
                Ok(Some(WebEvent::Head { .. })) | Ok(Some(WebEvent::Invalid { .. })) => {
                    // head 重复 / 回包损坏：不可信，终止。
                    break;
                }
            }
        }
        // 非正常结束：显式 drop 触发 guard 的清理 + cancel。
        drop(guard.take());
        let _ = &request_id;
    }
}

// ── 辅助 ────────────────────────────────────────────────────────────

/// 取一个查询参数。外层 `Some(None)` 表示该键出现了多次（参数非法），
/// 外层 `None` 表示缺失。
fn unique_param(params: &[(String, String)], key: &str) -> Option<Option<String>> {
    let mut found: Option<String> = None;
    for (k, v) in params {
        if k == key {
            if found.is_some() {
                return Some(None);
            }
            found = Some(v.clone());
        }
    }
    found.map(Some)
}

/// 校验目标 URL：绝对 http/https、有 host、长度受限。
///
/// 只做「形状」校验——是否可达、是否指向设备内网都交给 agent（访问设备自身
/// `127.0.0.1` 正是本功能目标，不在这里做 SSRF 式拦截）。
pub fn validate_url(url: &str) -> Result<(), &'static str> {
    if url.is_empty() {
        return Err("missing url parameter");
    }
    if url.len() > MAX_URL_LEN {
        return Err("url too long");
    }
    let lower = url.to_ascii_lowercase();
    let rest = if let Some(r) = lower.strip_prefix("http://") {
        r
    } else if let Some(r) = lower.strip_prefix("https://") {
        r
    } else {
        return Err("url must be an absolute http/https URL");
    };
    // host 部分不能为空（`http:///path`、`http://`）。
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if host_end == 0 {
        return Err("url must include a host");
    }
    if url.chars().any(|c| c.is_control()) {
        return Err("url contains control characters");
    }
    Ok(())
}

/// 响应头再过滤一层的名字黑名单。agent 侧已经过滤过，这里只是防御：
/// agent 被替换/降级成不守协议的实现时，cookie 与 hop-by-hop 头也不能漏给
/// 浏览器（尤其是 `set-cookie` 会把远端会话种到 relay 的同源域上）。
fn is_denied_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "set-cookie"
            | "set-cookie2"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-encoding"
    )
}

/// 把 agent 报的 header 变成可安全写入响应的 (HeaderName, HeaderValue)。
/// 名字/值非法、或命中黑名单的一律丢弃（绝不 panic）。
fn filter_response_headers(headers: &[(String, String)]) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        if is_denied_response_header(name) {
            continue;
        }
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        out.push((name, value));
    }
    out
}

async fn has_web_capability(state: &SharedState, session_id: &str) -> bool {
    state
        .sessions
        .get_capabilities(session_id)
        .await
        .iter()
        .any(|c| c == CAPABILITY)
}

fn capability_missing() -> Response {
    // 404 而不是 403：UI 用它判断「这台设备的 agent 不支持网页映射」，不读正文。
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({"error": "web proxy is not supported by this agent"})),
    )
        .into_response()
}

fn bad_request(msg: &'static str) -> Response {
    (StatusCode::BAD_REQUEST, axum::Json(json!({"error": msg}))).into_response()
}

fn bad_gateway(msg: &'static str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        axum::Json(json!({"error": "bad_gateway", "message": msg})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::ChannelMap;
    use axum::http::HeaderMap;

    fn make_state(auth: &str) -> Arc<SharedState> {
        Arc::new(SharedState::new(
            auth.to_string(),
            100 * 1024 * 1024,
            None,
            String::new(),
            String::new(),
            None,
        ))
    }

    /// 注册一个会话、写入能力声明、并接上一条 agent bulk 下行通道。
    /// 返回 (sid, tokens, bulk_rx)。
    async fn setup(
        state: &Arc<SharedState>,
        caps: &[&str],
    ) -> (String, Vec<(String, Permission)>, mpsc::Receiver<String>) {
        let r = state.sessions.register(None, "both", None).await.unwrap();
        let sid = r.session_id.clone();
        state
            .sessions
            .update_capabilities(&sid, caps.iter().map(|s| s.to_string()).collect())
            .await;
        let (bulk_tx, bulk_rx) = mpsc::channel::<String>(16);
        let mut cm = ChannelMap::new();
        cm.agent_bulk = Some(bulk_tx);
        state.agent_broadcast.write().await.insert(sid.clone(), cm);
        (sid, r.tokens, bulk_rx)
    }

    fn msg(sid: &str, ty: &str, payload: serde_json::Value) -> String {
        json!({"type": ty, "session_id": sid, "payload": payload}).to_string()
    }

    async fn feed(state: &Arc<SharedState>, sid: &str, text: &str) {
        crate::relay::ws::route_agent_message(state, sid, text).await;
    }

    fn head_payload(status: u16, headers: serde_json::Value, final_url: &str) -> serde_json::Value {
        json!({"request_id": "", "status": status, "headers": headers, "final_url": final_url})
    }

    /// 从 bulk 通道取出下发的 `web:request`，返回 request_id。
    async fn next_request(rx: &mut mpsc::Receiver<String>) -> String {
        let text = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("web:request 未在 5s 内下发")
            .expect("bulk 通道关闭");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["type"], "web:request",
            "下行首条必须是 web:request: {text}"
        );
        assert_eq!(v["payload"]["method"], "GET");
        v["payload"]["request_id"].as_str().unwrap().to_string()
    }

    fn get_params(pairs: &[(&str, &str)]) -> Query<Vec<(String, String)>> {
        Query(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    /// 等某个会话的在途请求数收敛到期望值。
    ///
    /// `WebRequestGuard::drop` 的清理是 `tokio::spawn` 出来的（`Drop` 不能
    /// await），所以「响应已经结束」与「在途表已摘除」之间有几微秒的窗口。
    /// 生产上无害（周期 sweep 兜底），测试里必须等它收敛而不是抢跑。
    async fn wait_inflight(state: &Arc<SharedState>, sid: &str, want: usize) {
        for _ in 0..300 {
            if state.web_proxy.inflight(sid).await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            state.web_proxy.inflight(sid).await,
            want,
            "在途表未收敛到 {want}"
        );
    }

    fn token_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-sr-token", token.parse().unwrap());
        h
    }

    // ── URL / 参数校验 ─────────────────────────────────────────────

    #[test]
    fn test_validate_url_accepts_absolute_http_https() {
        assert!(validate_url("http://127.0.0.1/").is_ok());
        assert!(validate_url("http://127.0.0.1:8080/a/b?c=d#e").is_ok());
        assert!(validate_url("https://example.com/x").is_ok());
        // 设备内网地址是本功能目标，不做 SSRF 式拦截。
        assert!(validate_url("http://192.168.1.10:3000/").is_ok());
        assert!(validate_url("http://10.0.0.1/").is_ok());
        assert!(validate_url("HTTP://Example.COM/").is_ok());
    }

    #[test]
    fn test_validate_url_rejects_relative_and_other_schemes() {
        for bad in [
            "",
            "/index.html",
            "example.com/x",
            "ftp://example.com/x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "//example.com/x",
        ] {
            assert!(validate_url(bad).is_err(), "应拒绝 {bad:?}");
        }
    }

    #[test]
    fn test_validate_url_rejects_missing_host_and_controls() {
        assert!(validate_url("http://").is_err());
        assert!(validate_url("http:///path").is_err());
        assert!(validate_url("https://?q=1").is_err());
        assert!(validate_url("http://exa\nmple.com/").is_err());
        assert!(validate_url("http://example.com/\r\nX: y").is_err());
    }

    #[test]
    fn test_validate_url_length_limit() {
        let base = "http://127.0.0.1/";
        let ok = format!("{base}{}", "a".repeat(MAX_URL_LEN - base.len()));
        assert_eq!(ok.len(), MAX_URL_LEN);
        assert!(validate_url(&ok).is_ok(), "恰好 4096 应通过");
        let long = format!("{base}{}", "a".repeat(MAX_URL_LEN));
        assert!(validate_url(&long).is_err(), "超过 4096 应拒绝");
    }

    #[test]
    fn test_unique_param_semantics() {
        let p = vec![
            ("url".to_string(), "http://a/".to_string()),
            ("ticket".to_string(), "t".to_string()),
        ];
        assert_eq!(unique_param(&p, "url"), Some(Some("http://a/".into())));
        assert_eq!(unique_param(&p, "missing"), None);
        let dup = vec![
            ("url".to_string(), "http://a/".to_string()),
            ("url".to_string(), "http://b/".to_string()),
        ];
        assert_eq!(unique_param(&dup, "url"), Some(None), "重复键必须判非法");
    }

    // ── ticket：随机性 / 绑定 / 过期 / 撤销 / 配额 ─────────────────

    #[tokio::test]
    async fn test_ticket_is_random_and_not_derived_from_token() {
        let store = WebProxyStore::new();
        let a = store.issue_ticket("sid-a").await;
        let b = store.issue_ticket("sid-a").await;
        assert_eq!(a.len(), TICKET_BYTES * 2, "256bit → 64 hex 字符");
        assert_ne!(a, b, "同一会话连签两次也必须不同");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // 与会话 token 无任何字符级关系（同长度、同前缀等都不成立）。
        assert!(!a.contains("sid-a"));
        assert_eq!(store.ticket_count("sid-a").await, 2);
    }

    #[tokio::test]
    async fn test_ticket_binds_to_session() {
        let store = WebProxyStore::new();
        let ta = store.issue_ticket("sid-a").await;
        let tb = store.issue_ticket("sid-b").await;
        assert_eq!(store.resolve_ticket(&ta).await.as_deref(), Some("sid-a"));
        assert_eq!(store.resolve_ticket(&tb).await.as_deref(), Some("sid-b"));
        assert_eq!(store.resolve_ticket("nonsense").await, None);
        assert_eq!(store.resolve_ticket("").await, None);
    }

    #[tokio::test]
    async fn test_ticket_expires() {
        let store = WebProxyStore::with_limits(WebLimits {
            ticket_ttl: Duration::from_millis(60),
            ..Default::default()
        });
        let t = store.issue_ticket("sid-a").await;
        assert_eq!(store.resolve_ticket(&t).await.as_deref(), Some("sid-a"));
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(store.resolve_ticket(&t).await, None, "过期后必须失效");
        assert_eq!(
            store.ticket_count("sid-a").await,
            0,
            "惰性清理应摘掉过期条目"
        );
    }

    #[tokio::test]
    async fn test_ticket_revoked_with_session() {
        let store = WebProxyStore::new();
        let t1 = store.issue_ticket("sid-a").await;
        let t2 = store.issue_ticket("sid-a").await;
        let other = store.issue_ticket("sid-b").await;
        assert_eq!(store.revoke_session_tickets("sid-a").await, 2);
        assert_eq!(store.resolve_ticket(&t1).await, None);
        assert_eq!(store.resolve_ticket(&t2).await, None);
        assert_eq!(
            store.resolve_ticket(&other).await.as_deref(),
            Some("sid-b"),
            "别的会话的 ticket 不受影响"
        );
        assert_eq!(
            store.revoke_session_tickets("sid-a").await,
            0,
            "重复撤销安全"
        );
    }

    #[tokio::test]
    async fn test_ticket_per_session_quota_evicts_oldest() {
        let store = WebProxyStore::new();
        let max = WebLimits::default().max_tickets_per_session;
        let mut tickets = Vec::new();
        for _ in 0..max {
            tickets.push(store.issue_ticket("sid-a").await);
        }
        assert_eq!(store.ticket_count("sid-a").await, max);
        // 第 max+1 个 → 最旧的被淘汰。
        let newest = store.issue_ticket("sid-a").await;
        assert_eq!(store.ticket_count("sid-a").await, max, "每会话上限必须生效");
        assert_eq!(
            store.resolve_ticket(&tickets[0]).await,
            None,
            "最旧的应被淘汰"
        );
        assert_eq!(
            store.resolve_ticket(&tickets[max - 1]).await.as_deref(),
            Some("sid-a"),
            "较新的仍有效"
        );
        assert_eq!(
            store.resolve_ticket(&newest).await.as_deref(),
            Some("sid-a")
        );
    }

    #[tokio::test]
    async fn test_sweep_clears_expired_tickets() {
        let store = WebProxyStore::with_limits(WebLimits {
            ticket_ttl: Duration::from_millis(40),
            ..Default::default()
        });
        store.issue_ticket("sid-a").await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let (tickets, streams) = store.sweep().await;
        assert_eq!(tickets, 1);
        assert_eq!(streams, 0);
    }

    // ── 鉴权 / 能力 ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_ticket_endpoint_requires_server_password() {
        let state = make_state("pw");
        let (sid, tokens, _rx) = setup(&state, &[CAPABILITY]).await;
        let mut h = token_headers(&tokens[0].0);
        h.insert("x-auth", "wrong".parse().unwrap());
        let resp = ticket_handler(State(state.clone()), h).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(state.web_proxy.ticket_count(&sid).await, 0);
    }

    #[tokio::test]
    async fn test_ticket_endpoint_requires_valid_session_token() {
        let state = make_state("pw");
        setup(&state, &[CAPABILITY]).await;
        let mut h = HeaderMap::new();
        h.insert("x-auth", "pw".parse().unwrap());
        h.insert("x-sr-token", "bogus".parse().unwrap());
        let resp = ticket_handler(State(state.clone()), h).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let mut h = HeaderMap::new();
        h.insert("x-auth", "pw".parse().unwrap());
        let resp = ticket_handler(State(state.clone()), h).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "缺 token 也拒绝");
    }

    #[tokio::test]
    async fn test_ticket_endpoint_allows_ro_and_rw() {
        let state = make_state("");
        let (sid, tokens, _rx) = setup(&state, &[CAPABILITY]).await;
        for (token, _) in &tokens {
            let resp = ticket_handler(State(state.clone()), token_headers(token)).await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(state.web_proxy.ticket_count(&sid).await, tokens.len());

        // 响应体只含 ticket 与有效期，绝不回显会话 token。
        let resp = ticket_handler(State(state.clone()), token_headers(&tokens[0].0)).await;
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ticket = v["ticket"].as_str().unwrap().to_string();
        assert_eq!(v["expires_in"], 600);
        for (token, _) in &tokens {
            assert!(!body.windows(token.len()).any(|w| w == token.as_bytes()));
        }
        assert_ne!(ticket, tokens[0].0);
    }

    #[tokio::test]
    async fn test_ticket_endpoint_404_without_capability() {
        let state = make_state("");
        let (sid, tokens, _rx) = setup(&state, &[]).await;
        let resp = ticket_handler(State(state.clone()), token_headers(&tokens[0].0)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "旧 agent → 404");
        assert_eq!(
            state.web_proxy.ticket_count(&sid).await,
            0,
            "不应签发 ticket"
        );
    }

    #[tokio::test]
    async fn test_proxy_401_without_any_credential() {
        let state = make_state("pw");
        setup(&state, &[CAPABILITY]).await;
        let resp = proxy_handler(
            State(state.clone()),
            HeaderMap::new(),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_proxy_401_with_invalid_session_token() {
        let state = make_state("pw");
        setup(&state, &[CAPABILITY]).await;
        let mut h = HeaderMap::new();
        h.insert("x-auth", "pw".parse().unwrap());
        h.insert("x-sr-token", "nope".parse().unwrap());
        let resp = proxy_handler(
            State(state.clone()),
            h,
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_proxy_403_with_invalid_ticket() {
        let state = make_state("pw");
        setup(&state, &[CAPABILITY]).await;
        // ticket 鉴权不需要 server password（设计简化：签发时已校验过）。
        let resp = proxy_handler(
            State(state.clone()),
            HeaderMap::new(),
            get_params(&[("ticket", "deadbeef"), ("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_proxy_403_with_expired_ticket() {
        let state = make_state("");
        let (sid, _tokens, _rx) = setup(&state, &[CAPABILITY]).await;
        // 通过一个 40ms 的 ticket TTL 替代生产 10min，验证"过期 ticket 不再放行"。
        let ticket_state = WebProxyStore::with_limits(WebLimits {
            ticket_ttl: Duration::from_millis(40),
            ..Default::default()
        });
        let ticket = ticket_state.issue_ticket(&sid).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(ticket_state.resolve_ticket(&ticket).await, None);
    }

    #[tokio::test]
    async fn test_proxy_404_when_capability_missing_and_no_network_request() {
        let state = make_state("");
        let (_sid, tokens, mut bulk_rx) = setup(&state, &[]).await;
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(
            bulk_rx.try_recv().is_err(),
            "旧 agent 无能力时不得下发任何 web:request（不能触发网络请求）"
        );
    }

    #[tokio::test]
    async fn test_proxy_503_when_agent_offline() {
        let state = make_state("");
        // 有会话、有能力，但没有 agent_broadcast 条目 → 离线。
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        state
            .sessions
            .update_capabilities(&r.session_id, vec![CAPABILITY.to_string()])
            .await;
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&r.tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── 参数错误 400 ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_proxy_400_for_bad_params() {
        let state = make_state("");
        let (_sid, tokens, _rx) = setup(&state, &[CAPABILITY]).await;
        let h = token_headers(&tokens[0].0);
        let cases: Vec<Vec<(&str, &str)>> = vec![
            vec![],                                           // 缺 url
            vec![("url", "/relative")],                       // 相对
            vec![("url", "ftp://127.0.0.1/")],                // 非 http
            vec![("url", "http://")],                         // 无 host
            vec![("url", "http://a/"), ("url", "http://b/")], // url 重复
            vec![
                ("url", "http://127.0.0.1/"),
                ("ticket", "a"),
                ("ticket", "b"),
            ],
        ];
        for params in cases {
            let resp = proxy_handler(State(state.clone()), h.clone(), get_params(&params)).await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "参数 {params:?} 应判 400"
            );
        }
        // 长度超限
        let long = format!("http://127.0.0.1/{}", "a".repeat(MAX_URL_LEN));
        let resp = proxy_handler(
            State(state.clone()),
            h,
            Query(vec![("url".to_string(), long)]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ── 正常路径：状态 / 响应头 / body ─────────────────────────────

    #[tokio::test]
    async fn test_total_timeout_covers_head_wait_and_body() {
        // 任务卡：单请求总时限 5min，必须覆盖「等首包 + 传 body」。这里把总时限
        // 压到 150ms、首包等待放宽，让 agent 先慢 100ms 再回 head，然后一直传块
        // —— 若 deadline 从 body 流创建时起算，这条流会一直跑到 idle 超时；
        // 从请求发出起算则应在 ~150ms 处收尾。
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        state.web_proxy.set_timeouts(
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_millis(150),
        );
        let st = state.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            // 每 20ms 一块，永远不发 end —— 只有总时限能收尾。
            for seq in 0..100u32 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                feed(
                    &st,
                    &sid_c,
                    &msg(
                        &sid_c,
                        "web:chunk",
                        json!({"request_id": rid, "seq": seq, "data": BASE64.encode(b"x")}),
                    ),
                )
                .await;
            }
        });
        let started = std::time::Instant::now();
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = tokio::time::timeout(
            Duration::from_secs(5),
            axum::body::to_bytes(resp.into_body(), 1 << 20),
        )
        .await
        .expect("总时限到点后 body 必须结束")
        .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(1500),
            "必须在总时限附近收尾，实际 {elapsed:?}"
        );
        assert!(!body.is_empty(), "收尾前已经写出的块应保留");
        assert!(
            body.len() < 100,
            "总时限必须截断，实际传了 {} 块",
            body.len()
        );
        wait_inflight(&state, &sid, 0).await;
    }

    #[tokio::test]
    async fn test_proxy_streams_status_headers_and_body() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let st = state.clone();
        let sid_c = sid.clone();
        let feeder = tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({
                        "request_id": rid,
                        "status": 201,
                        "headers": [["content-type", "text/html; charset=utf-8"],
                                    ["cache-control", "no-store"],
                                    ["etag", "\"abc\""]],
                        "final_url": "http://127.0.0.1:8080/index.html"
                    }),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 0, "data": BASE64.encode(b"<html>hi</html>")}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(&sid_c, "web:end", json!({"request_id": rid})),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1:8080/index.html")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");
        assert_eq!(resp.headers().get("etag").unwrap(), "\"abc\"");
        assert_eq!(
            resp.headers().get("x-sr-final-url").unwrap(),
            "http://127.0.0.1:8080/index.html",
            "UI 靠它解析重定向后的相对资源"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"<html>hi</html>");
        feeder.await.unwrap();
        assert_eq!(
            state.web_proxy.inflight(&sid).await,
            0,
            "结束后必须清空在途表"
        );
    }

    #[tokio::test]
    async fn test_proxy_ro_token_can_read_page() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let ro = tokens
            .iter()
            .find(|(_, p)| *p == Permission::ReadOnly)
            .expect("setup 应签发 ro token")
            .0
            .clone();
        let st = state.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200,
                           "headers": [["content-type", "text/plain"]],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 0, "data": BASE64.encode(b"ro-ok")}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(&sid_c, "web:end", json!({"request_id": rid})),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&ro),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "ro token 必须能读页面");
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"ro-ok");
    }

    #[tokio::test]
    async fn test_proxy_ticket_auth_path_works() {
        let state = make_state("pw");
        let (sid, _tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let ticket = state.web_proxy.issue_ticket(&sid).await;
        let st = state.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(&sid_c, "web:end", json!({"request_id": rid})),
            )
            .await;
        });
        // ticket 路径不需要 X-Auth（签发时已通过 server password）。
        let resp = proxy_handler(
            State(state.clone()),
            HeaderMap::new(),
            get_params(&[("ticket", &ticket), ("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_proxy_reassembles_over_64kib() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let chunk: Vec<u8> = (0..64 * 1024).map(|i| (i % 256) as u8).collect();
        let expected: Vec<u8> = chunk.repeat(4);
        let st = state.clone();
        let chunk_c = chunk.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200,
                           "headers": [["content-type", "application/octet-stream"]],
                           "final_url": "http://127.0.0.1/big"}),
                ),
            )
            .await;
            for seq in 0..4u32 {
                feed(
                    &st,
                    &sid_c,
                    &msg(
                        &sid_c,
                        "web:chunk",
                        json!({"request_id": rid, "seq": seq, "data": BASE64.encode(&chunk_c)}),
                    ),
                )
                .await;
            }
            feed(
                &st,
                &sid_c,
                &msg(&sid_c, "web:end", json!({"request_id": rid})),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/big")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(body.len(), 256 * 1024);
        assert_eq!(&body[..], &expected[..], "256KiB 必须逐字节还原");
    }

    #[tokio::test]
    async fn test_proxy_truncates_on_seq_gap() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let st = state.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 0, "data": BASE64.encode(b"first")}),
                ),
            )
            .await;
            // seq 跳号：不得继续拼接（否则就是把内容静默拼坏）
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 2, "data": BASE64.encode(b"gap")}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(&sid_c, "web:end", json!({"request_id": rid})),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"first", "跳号之后的内容必须被丢弃");
    }

    #[tokio::test]
    async fn test_proxy_truncates_on_invalid_base64() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let st = state.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 0, "data": BASE64.encode(b"ok")}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 1, "data": "!!!not-base64!!!"}),
                ),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"ok", "坏块必须中断响应而不是拼进 body");
        // 中断后 agent 应收到 cancel，且条目被摘除。
        assert_eq!(state.web_proxy.inflight(&sid).await, 0);
    }

    #[tokio::test]
    async fn test_bad_head_before_response_maps_to_502() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let st = state.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            // status 非法
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 999, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "损坏的 head 不能当真"
        );
        assert_eq!(state.web_proxy.inflight(&sid).await, 0);
    }

    #[tokio::test]
    async fn test_chunk_before_head_maps_to_502() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let st = state.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 0, "data": BASE64.encode(b"x")}),
                ),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn test_end_before_head_maps_to_502() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let st = state.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(&sid_c, "web:end", json!({"request_id": rid})),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    // ── agent 报错：head 前映射为 400/502/504/503 ──────────────────

    #[tokio::test]
    async fn test_agent_error_before_head_status_mapping() {
        for (kind, expect) in [
            ("invalid_url", StatusCode::BAD_REQUEST),
            ("unsupported_method", StatusCode::BAD_REQUEST),
            ("connect", StatusCode::BAD_GATEWAY),
            ("http", StatusCode::BAD_GATEWAY),
            ("timeout", StatusCode::GATEWAY_TIMEOUT),
            ("busy", StatusCode::SERVICE_UNAVAILABLE),
            ("cancelled", StatusCode::SERVICE_UNAVAILABLE),
            ("something-new", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let state = make_state("");
            let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
            let st = state.clone();
            let kind_s = kind.to_string();
            let sid_c = sid.clone();
            tokio::spawn(async move {
                let rid = next_request(&mut bulk_rx).await;
                feed(
                    &st,
                    &sid_c,
                    &msg(
                        &sid_c,
                        "web:error",
                        json!({"request_id": rid, "kind": kind_s, "message": "boom"}),
                    ),
                )
                .await;
            });
            let resp = proxy_handler(
                State(state.clone()),
                token_headers(&tokens[0].0),
                get_params(&[("url", "http://127.0.0.1/")]),
            )
            .await;
            assert_eq!(resp.status(), expect, "kind={kind}");
            assert_eq!(state.web_proxy.inflight(&sid).await, 0);
        }
    }

    #[tokio::test]
    async fn test_agent_error_mid_body_truncates() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let st = state.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 0, "data": BASE64.encode(b"partial")}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:error",
                    json!({"request_id": rid, "kind": "timeout", "message": "idle"}),
                ),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"partial", "body 中的 error 只能终止流");
    }

    // ── 响应头过滤（防御不守协议的 agent）─────────────────────────

    #[test]
    fn test_response_header_filter_drops_cookies_and_hop_by_hop() {
        let got = filter_response_headers(&[
            ("content-type".into(), "text/html".into()),
            ("set-cookie".into(), "sid=evil".into()),
            ("Set-Cookie".into(), "sid2=evil".into()),
            ("connection".into(), "close".into()),
            ("transfer-encoding".into(), "chunked".into()),
            ("content-encoding".into(), "gzip".into()),
            ("x-ok".into(), "1".into()),
            ("bad header".into(), "x".into()),
            ("x-ctl".into(), "a\r\nb".into()),
        ]);
        let names: Vec<String> = got.iter().map(|(n, _)| n.to_string()).collect();
        assert!(names.contains(&"content-type".to_string()));
        assert!(names.contains(&"x-ok".to_string()));
        assert!(!names.iter().any(|n| n == "set-cookie"), "{names:?}");
        assert!(!names.iter().any(|n| n == "connection"));
        assert!(!names.iter().any(|n| n == "transfer-encoding"));
        assert!(!names.iter().any(|n| n == "content-encoding"));
        assert!(!names.iter().any(|n| n == "bad header"), "非法名字应丢弃");
        assert!(!names.iter().any(|n| n == "x-ctl"), "非法值应丢弃");
    }

    // ── 超时 / 客户端断开 → cancel ────────────────────────────────

    #[tokio::test]
    async fn test_head_timeout_returns_504_and_cancels() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        // 把生产的 35s 首包超时压到 60ms，走**同一条** handler 路径。
        state.web_proxy.set_timeouts(
            Duration::from_millis(60),
            Duration::from_secs(30),
            Duration::from_secs(60),
        );
        // agent 收了请求但不回包（模拟目标站卡住）。
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/slow")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        let rid = {
            let text = bulk_rx.try_recv().expect("web:request 应已下发");
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            v["payload"]["request_id"].as_str().unwrap().to_string()
        };
        // 超时（agent 可能仍在跑）必须让它停发。
        let cancel = tokio::time::timeout(Duration::from_secs(3), bulk_rx.recv())
            .await
            .expect("超时后应下发 web:cancel")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&cancel).unwrap();
        assert_eq!(v["type"], "web:cancel");
        assert_eq!(v["payload"]["request_id"], rid);
        wait_inflight(&state, &sid, 0).await;
    }

    #[tokio::test]
    async fn test_body_idle_timeout_truncates_stream_and_cancels() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        state.web_proxy.set_timeouts(
            Duration::from_secs(5),
            Duration::from_millis(60),
            Duration::from_secs(30),
        );
        let st = state.clone();
        let sid_c = sid.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 0, "data": BASE64.encode(b"part")}),
                ),
            )
            .await;
            // 之后静默：块间 idle 超时应收尾。
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = tokio::time::timeout(
            Duration::from_secs(5),
            axum::body::to_bytes(resp.into_body(), 4096),
        )
        .await
        .expect("idle 超时后 body 必须结束")
        .unwrap();
        assert_eq!(&body[..], b"part");
        wait_inflight(&state, &sid, 0).await;
    }

    #[tokio::test]
    async fn test_proxy_client_disconnect_sends_cancel() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let st = state.clone();
        let sid_c = sid.clone();
        let feeder = tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            // 只发一块，然后不再发 end —— 模拟慢页面。
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 0, "data": BASE64.encode(b"a")}),
                ),
            )
            .await;
            // 把 bulk_rx 还回去：后续要读 web:cancel。
            (rid, bulk_rx)
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let (rid, mut bulk_rx) = feeder.await.unwrap();
        drop(resp); // 客户端断开
                    // 守卫的 Drop 会异步摘除并下发 web:cancel。
        let text = tokio::time::timeout(Duration::from_secs(5), bulk_rx.recv())
            .await
            .expect("5s 内应下发 web:cancel")
            .expect("bulk 通道关闭");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "web:cancel");
        assert_eq!(v["payload"]["request_id"], rid);
        assert_eq!(state.web_proxy.inflight(&sid).await, 0);
    }

    #[tokio::test]
    async fn test_normal_end_does_not_send_cancel() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let st = state.clone();
        let sid_c = sid.clone();
        let feeder = tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(&sid_c, "web:end", json!({"request_id": rid})),
            )
            .await;
            bulk_rx
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        let _ = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let mut bulk_rx = feeder.await.unwrap();
        // 正常结束不应有 web:cancel 下行。
        assert!(
            tokio::time::timeout(Duration::from_millis(300), bulk_rx.recv())
                .await
                .is_err(),
            "正常 end 之后不得再下发 web:cancel"
        );
        assert_eq!(state.web_proxy.inflight(&sid).await, 0);
    }

    // ── 并发上限 ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_inflight_limit_returns_429() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let max = WebLimits::default().max_inflight_per_session;
        // 直接占满在途表（agent 不回包 → handler 会阻塞在等 head，故不走 handler）。
        for i in 0..max {
            let (tx, _rx) = mpsc::channel(4);
            state
                .web_proxy
                .register_stream(&sid, &format!("rid-{i}"), tx)
                .await
                .expect("未达上限前应登记成功");
        }
        assert_eq!(state.web_proxy.inflight(&sid).await, max);
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(bulk_rx.try_recv().is_err(), "超限请求不应下发给 agent");

        // 释放一个名额后应恢复。
        state.web_proxy.remove_stream(&sid, "rid-0").await;
        assert_eq!(state.web_proxy.inflight(&sid).await, max - 1);
        let st = state.clone();
        let sid_c = sid.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(&sid_c, "web:end", json!({"request_id": rid})),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "名额释放后应能继续服务");
    }

    // ── agent 断线 / 会话清理 ─────────────────────────────────────

    #[tokio::test]
    async fn test_cancel_session_ends_body_and_clears_state() {
        let state = make_state("");
        let (sid, tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let st = state.clone();
        let sid_c = sid.clone();
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let rid = next_request(&mut bulk_rx).await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:head",
                    json!({"request_id": rid, "status": 200, "headers": [],
                           "final_url": "http://127.0.0.1/"}),
                ),
            )
            .await;
            feed(
                &st,
                &sid_c,
                &msg(
                    &sid_c,
                    "web:chunk",
                    json!({"request_id": rid, "seq": 0, "data": BASE64.encode(b"x")}),
                ),
            )
            .await;
        });
        let resp = proxy_handler(
            State(state.clone()),
            token_headers(&tokens[0].0),
            get_params(&[("url", "http://127.0.0.1/")]),
        )
        .await;
        assert_eq!(state.web_proxy.inflight(&sid).await, 1);
        // agent 断线：撤销 ticket + 取消在途请求。
        assert_eq!(state.web_proxy.cancel_session(&sid).await, 1);
        assert_eq!(state.web_proxy.inflight(&sid).await, 0);
        // 响应任务因通道关闭而提前 EOF（不 panic、不挂住）。
        let body = tokio::time::timeout(
            Duration::from_secs(5),
            axum::body::to_bytes(resp.into_body(), 4096),
        )
        .await
        .expect("agent 断线后 body 必须结束")
        .unwrap();
        assert_eq!(&body[..], b"x");
    }

    #[tokio::test]
    async fn test_agent_disconnect_revokes_tickets_and_requests() {
        let state = make_state("");
        let (sid, _tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let ticket = state.web_proxy.issue_ticket(&sid).await;
        let (tx, _rx) = mpsc::channel(4);
        state
            .web_proxy
            .register_stream(&sid, "rid-x", tx)
            .await
            .unwrap();
        // remove_session_state 是统一的会话清理入口（kick / 空闲回收 / 驱逐）。
        crate::relay::remove_session_state(&state, &sid).await;
        assert_eq!(state.web_proxy.resolve_ticket(&ticket).await, None);
        assert_eq!(state.web_proxy.inflight(&sid).await, 0);
        // 能力声明**不清**：agent 重连会重新注册覆盖（浏览器靠
        // session:agent_disconnect 自己隐藏入口）。
        assert_eq!(
            state.sessions.get_capabilities(&sid).await,
            vec![CAPABILITY.to_string()]
        );
        let _ = &mut bulk_rx;
    }

    // ── 回包路由的边界 ────────────────────────────────────────────

    #[tokio::test]
    async fn test_reply_for_unknown_request_is_dropped_without_state() {
        let state = make_state("");
        let (sid, _tokens, _rx) = setup(&state, &[CAPABILITY]).await;
        let handled = crate::relay::web_proxy::route_agent_message(
            &state,
            &sid,
            &serde_json::from_str(&msg(
                &sid,
                "web:chunk",
                json!({"request_id": "never-registered", "seq": 0, "data": "eA=="}),
            ))
            .unwrap(),
        )
        .await;
        assert!(handled, "web:* 必须由网页分支接管（不得进终端广播）");
        assert_eq!(state.web_proxy.inflight(&sid).await, 0);
        assert_eq!(state.web_proxy.ticket_count(&sid).await, 0);
    }

    #[tokio::test]
    async fn test_non_web_message_is_not_handled() {
        let state = make_state("");
        let (sid, _tokens, _rx) = setup(&state, &[CAPABILITY]).await;
        let handled = crate::relay::web_proxy::route_agent_message(
            &state,
            &sid,
            &serde_json::from_str(&msg(&sid, "terminal:output", json!({"data": "x"}))).unwrap(),
        )
        .await;
        assert!(!handled, "终端输出必须继续走广播路径");
    }

    #[tokio::test]
    async fn test_reply_without_request_id_is_swallowed() {
        let state = make_state("");
        let (sid, _tokens, _rx) = setup(&state, &[CAPABILITY]).await;
        let handled = crate::relay::web_proxy::route_agent_message(
            &state,
            &sid,
            &serde_json::from_str(&msg(&sid, "web:head", json!({"status": 200}))).unwrap(),
        )
        .await;
        assert!(handled);
    }

    #[tokio::test]
    async fn test_full_reply_channel_aborts_request_without_blocking() {
        let state = make_state("");
        let (sid, _tokens, mut bulk_rx) = setup(&state, &[CAPABILITY]).await;
        let (tx, _rx) = mpsc::channel::<WebEvent>(1);
        state
            .web_proxy
            .register_stream(&sid, "rid-full", tx)
            .await
            .unwrap();
        // 先塞满容量 1 的通道。
        crate::relay::web_proxy::route_agent_message(
            &state,
            &sid,
            &serde_json::from_str(&msg(
                &sid,
                "web:chunk",
                json!({"request_id": "rid-full", "seq": 0, "data": "eA=="}),
            ))
            .unwrap(),
        )
        .await;
        assert_eq!(state.web_proxy.inflight(&sid).await, 1);
        // 再来一条 → 通道满 → 丢弃该请求并让 agent 停发；路由不得被阻塞。
        let started = std::time::Instant::now();
        crate::relay::web_proxy::route_agent_message(
            &state,
            &sid,
            &serde_json::from_str(&msg(
                &sid,
                "web:chunk",
                json!({"request_id": "rid-full", "seq": 1, "data": "eQ=="}),
            ))
            .unwrap(),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "通道满时必须立即返回（不得 await）"
        );
        assert_eq!(state.web_proxy.inflight(&sid).await, 0, "请求应被摘除");
        let cancel = tokio::time::timeout(Duration::from_secs(2), bulk_rx.recv())
            .await
            .expect("应下发 web:cancel")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&cancel).unwrap();
        assert_eq!(v["type"], "web:cancel");
        assert_eq!(v["payload"]["request_id"], "rid-full");
    }
}
