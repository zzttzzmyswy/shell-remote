//! 同源网页映射 v2（MYS-2383 阶段6）：在 `/agent/web/m/<map_id>/…` 下通过 agent
//! 的纯 TCP 管道（`net:tcp-v1`）对远端网页做 HTTP/1.1 透明反代（含 WebSocket）。
//!
//! # 三层各自的位置
//!
//! - agent（`src/agent/tcp_tunnel.rs`）：只搬字节，不解析 HTTP、不做 TLS。
//! - 本模块：映射管理 + 反代。**不解析、不改写响应体**——只改请求/响应头与
//!   `Location` / `Set-Cookie`，body 一律流式透传。
//! - 浏览器（下一阶段）：Service Worker 把映射页面的请求改写到
//!   `/agent/web/m/<map_id>/…`。本卡不改 `web/**`。
//!
//! v0.58 的 `web:proxy-v1`（`web_proxy.rs`）保持原样不动，供旧 agent 回退。
//!
//! # 同源带来的两个约束
//!
//! 用户拒绝独立域名/端口，映射页面与 shell-remote 同源。远端脚本因此与本页
//! 脚本同源运行（用户已接受该风险），此外：
//!
//! - 必须**禁止**远端页面借映射路径注册 Service Worker（`Service-Worker:
//!   script` → 403），否则它能拿到整个源的请求代理权，包括 shell-remote 自己的。
//! - 响应头里必须剥掉会作用到本站源或挡住同源嵌入的那些（CSP、HSTS、
//!   `Clear-Site-Data`、XFO、COEP/COOP/CORP、`Alt-Svc`、`Service-Worker-Allowed`）。
//!
//! # 改写规则（唯一需要理解的部分）
//!
//! 请求：`Host` → 目标 authority；剥 hop-by-hop 与本站凭据头；`Origin` 等于本站
//! 源时换成 `target_origin`；本站源的 `Referer` 去映射前缀后拼 `target_origin`。
//! 响应：剥 hop-by-hop 与上段那组头；`Location` 加映射前缀；`Set-Cookie` 去
//! `Domain`、`Path` 加映射前缀。
//!
//! 这些规则都写成了纯函数并逐一单测（见本文件 `tests`）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
use axum::http::{Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use hyper::body::{Bytes, Frame, Incoming, SizeHint};
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};

use crate::proto::Permission;
use crate::relay::tcp_tunnel::{self, OpenError, TunnelStream};
use crate::relay::SharedState;

/// 映射页面的路径前缀（浏览器 Service Worker 阶段复用）。
pub const PATH_PREFIX: &str = "/agent/web/m";

/// `map_id` 的随机字节数（hex 编码后 64 字符）。它**就是访问凭据**，日志里
/// 不得打印。
const MAP_ID_BYTES: usize = 32;

/// 目标 URL 长度上限（任务卡：≤4096）。
pub const MAX_URL_LEN: usize = 4096;

/// 空闲 TTL：30 min 内没被访问就回收。
pub const IDLE_TTL: Duration = Duration::from_secs(1800);

/// 每会话最多同时存在的映射数（超出淘汰最久未用的）。
pub const MAX_MAPS_PER_SESSION: usize = 8;

/// 全局映射数上限。
pub const MAX_TOTAL_MAPS: usize = 4096;

/// 每个映射的空闲 HTTP 连接池大小。
pub const POOL_SIZE: usize = 6;

/// 池中空闲连接的空闲上限。
pub const POOL_IDLE: Duration = Duration::from_secs(30);

/// 每个映射同时在途的请求上限（含 WebSocket）。超出排队。
pub const MAX_INFLIGHT: usize = 16;

/// 排队等名额的上限（超出 503）。
pub const QUEUE_WAIT: Duration = Duration::from_secs(15);

/// 等目标响应头的上限。响应 body 与 WebSocket **不设**总时长上限。
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(60);

/// 请求头里必须剥掉的 hop-by-hop 名（小写）。
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// 响应头里额外剥掉的名字：会作用到**本站源**或挡住同源嵌入。
const RESPONSE_STRIP: &[&str] = &[
    "content-security-policy",
    "content-security-policy-report-only",
    "x-frame-options",
    "strict-transport-security",
    "clear-site-data",
    "alt-svc",
    "cross-origin-opener-policy",
    "cross-origin-embedder-policy",
    "cross-origin-resource-policy",
    "service-worker-allowed",
];

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ── 映射表 ──────────────────────────────────────────────────────────

/// 一条映射。建好之后不再变化（运行时状态在 [`MapRuntime`]）。
#[derive(Debug, Clone)]
pub struct MapEntry {
    pub map_id: String,
    pub session_id: String,
    /// 目标源，形如 `http://192.168.0.1` / `https://host:8443`（无尾斜杠）。
    pub target_origin: String,
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// `Host` 头该带的值：非默认端口带 `:port`。
    pub authority: String,
    /// 最近一次访问时刻（TTL 按它算）。
    last_used: tokio::time::Instant,
}

#[derive(Default)]
struct MapState {
    maps: HashMap<String, MapEntry>,
    /// session → 该会话的 map_id（按最近使用排序，最久未用在前）。
    by_session: HashMap<String, Vec<String>>,
    /// `(session_id, target_origin)` → map_id，用于「同目标复用同一 map_id」。
    by_target: HashMap<(String, String), String>,
}

/// 全进程映射表。挂在 [`SharedState::web_maps`]。
///
/// 锁用 `std::sync::Mutex`：临界区全是哈希操作。
pub struct MapStore {
    state: Mutex<MapState>,
    idle_ttl_ms: AtomicU64,
    max_per_session: AtomicUsize,
    max_total: AtomicUsize,
}

impl Default for MapStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MapStore {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MapState::default()),
            idle_ttl_ms: AtomicU64::new(IDLE_TTL.as_millis() as u64),
            max_per_session: AtomicUsize::new(MAX_MAPS_PER_SESSION),
            max_total: AtomicUsize::new(MAX_TOTAL_MAPS),
        }
    }

    pub fn idle_ttl(&self) -> Duration {
        Duration::from_millis(self.idle_ttl_ms.load(Ordering::Relaxed))
    }

    /// 单测用：把 TTL 压到毫秒级。
    pub fn set_idle_ttl(&self, d: Duration) {
        self.idle_ttl_ms
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// 单测用：改配额。
    pub fn set_limits(&self, per_session: usize, total: usize) {
        self.max_per_session.store(per_session, Ordering::Relaxed);
        self.max_total.store(total, Ordering::Relaxed);
    }

    pub fn count(&self) -> usize {
        lock(&self.state).maps.len()
    }

    pub fn count_session(&self, session_id: &str) -> usize {
        lock(&self.state)
            .by_session
            .get(session_id)
            .map_or(0, |v| v.len())
    }

    /// 建一条映射，或复用同会话同目标的现存映射（Cookie 因此能跨多次打开保持
    /// 登录态）。返回实际生效的条目。
    pub fn upsert(&self, entry: MapEntry) -> MapEntry {
        let now = tokio::time::Instant::now();
        let mut st = lock(&self.state);
        st.prune_expired(now, self.idle_ttl());
        let key = (entry.session_id.clone(), entry.target_origin.clone());
        let existing_id = st.by_target.get(&key).cloned();
        if let Some(id) = existing_id {
            if let Some(existing) = st.maps.get_mut(&id) {
                existing.last_used = now;
                let out = existing.clone();
                st.touch(&out.session_id, &out.map_id);
                return out;
            }
        }
        // 全局配额：仍超限就淘汰最久未用的。
        let max_total = self.max_total.load(Ordering::Relaxed).max(1);
        while st.maps.len() >= max_total && st.evict_lru_one() {}
        // 每会话配额。
        let per_session = self.max_per_session.load(Ordering::Relaxed).max(1);
        while st.by_session.get(&entry.session_id).map_or(0, |v| v.len()) >= per_session
            && st.evict_lru_session(&entry.session_id)
        {}
        st.by_session
            .entry(entry.session_id.clone())
            .or_default()
            .push(entry.map_id.clone());
        st.by_target.insert(key, entry.map_id.clone());
        st.maps.insert(entry.map_id.clone(), entry.clone());
        entry
    }

    /// 查一条映射并刷新「最近使用」。无效或过期返回 `None`。
    pub fn get(&self, map_id: &str) -> Option<MapEntry> {
        if map_id.is_empty() {
            return None;
        }
        let now = tokio::time::Instant::now();
        let ttl = self.idle_ttl();
        let mut st = lock(&self.state);
        if st
            .maps
            .get(map_id)
            .is_some_and(|e| now.duration_since(e.last_used) >= ttl)
        {
            st.remove(map_id);
            return None;
        }
        let entry = st.maps.get_mut(map_id)?;
        entry.last_used = now;
        let out = entry.clone();
        st.touch(&out.session_id, map_id);
        Some(out)
    }

    /// 只查存在性，**不刷新** TTL，也不淘汰。周期清理用它判断运行时是否还
    /// 有归属，绝不能顺手刷新 `last_used`（否则映射永远不会过期）。
    pub fn contains(&self, map_id: &str) -> bool {
        lock(&self.state).maps.contains_key(map_id)
    }

    /// 撤销一个会话的全部映射（agent 断线 / 会话移除）。返回撤销数量。
    pub fn revoke_session(&self, session_id: &str) -> Vec<String> {
        let mut st = lock(&self.state);
        let Some(ids) = st.by_session.remove(session_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for id in ids {
            if let Some(e) = st.maps.remove(&id) {
                st.by_target
                    .remove(&(e.session_id.clone(), e.target_origin.clone()));
                out.push(id);
            }
        }
        out
    }

    /// 周期清扫过期映射，返回清除数量。
    pub fn sweep(&self) -> usize {
        let now = tokio::time::Instant::now();
        lock(&self.state).prune_expired(now, self.idle_ttl())
    }
}

impl MapState {
    fn touch(&mut self, session_id: &str, map_id: &str) {
        if let Some(v) = self.by_session.get_mut(session_id) {
            v.retain(|id| id != map_id);
            v.push(map_id.to_string());
        }
    }

    fn remove(&mut self, map_id: &str) -> Option<MapEntry> {
        let e = self.maps.remove(map_id)?;
        self.by_target
            .remove(&(e.session_id.clone(), e.target_origin.clone()));
        if let Some(v) = self.by_session.get_mut(&e.session_id) {
            v.retain(|id| id != map_id);
            if v.is_empty() {
                self.by_session.remove(&e.session_id);
            }
        }
        Some(e)
    }

    fn evict_lru_session(&mut self, session_id: &str) -> bool {
        match self
            .by_session
            .get(session_id)
            .and_then(|v| v.first().cloned())
        {
            Some(id) => self.remove(&id).is_some(),
            None => false,
        }
    }

    fn evict_lru_one(&mut self) -> bool {
        match self
            .by_session
            .values()
            .filter_map(|v| v.first().cloned())
            .next()
        {
            Some(id) => self.remove(&id).is_some(),
            None => false,
        }
    }

    fn prune_expired(&mut self, now: tokio::time::Instant, ttl: Duration) -> usize {
        let expired: Vec<String> = self
            .maps
            .iter()
            .filter(|(_, e)| now.duration_since(e.last_used) >= ttl)
            .map(|(id, _)| id.clone())
            .collect();
        let n = expired.len();
        for id in expired {
            self.remove(&id);
        }
        n
    }
}

fn random_map_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: [u8; MAP_ID_BYTES] = rng.gen();
    hex::encode(bytes)
}

// ── 目标 URL 解析 ───────────────────────────────────────────────────

/// 解析后的目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// `scheme://host[:port]`，无尾斜杠，默认端口不带 `:port`。
    pub origin: String,
    /// `Host` 头的值：非默认端口带 `:port`。
    pub authority: String,
}

fn default_port(tls: bool) -> u16 {
    if tls {
        443
    } else {
        80
    }
}

/// 解析目标 URL。只做形状校验——是否可达、是否内网都交给 agent（访问设备
/// 自身 `127.0.0.1` 正是本功能目标）。
pub fn parse_target(url: &str) -> Result<Target, &'static str> {
    if url.is_empty() {
        return Err("missing url");
    }
    if url.len() > MAX_URL_LEN {
        return Err("url too long");
    }
    if url.chars().any(|c| c.is_control()) {
        return Err("url contains control characters");
    }
    let lower = url.to_ascii_lowercase();
    let (tls, rest) = if let Some(r) = lower.strip_prefix("http://") {
        (false, r)
    } else if let Some(r) = lower.strip_prefix("https://") {
        (true, r)
    } else {
        return Err("url must be an absolute http/https URL");
    };
    // authority[:port][/path][?q][#f]
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() {
        return Err("url must include a host");
    }
    if authority.contains('@') {
        return Err("url must not contain userinfo");
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            let port: u16 = p.parse().map_err(|_| "invalid port")?;
            if port == 0 {
                return Err("invalid port");
            }
            (h, port)
        }
        Some((h, "")) => (h, default_port(tls)),
        Some(_) => return Err("invalid port"),
        None => (authority, default_port(tls)),
    };
    if host.is_empty() {
        return Err("url must include a host");
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return Err("invalid host");
    }
    let scheme = if tls { "https" } else { "http" };
    let default = default_port(tls);
    Ok(Target {
        tls,
        host: host.to_string(),
        port,
        origin: if port == default {
            format!("{scheme}://{host}")
        } else {
            format!("{scheme}://{host}:{port}")
        },
        authority: if port == default {
            host.to_string()
        } else {
            format!("{host}:{port}")
        },
    })
}

// ── 请求/响应头改写（纯函数） ───────────────────────────────────────

/// 反代一条请求需要的最小上下文。
#[derive(Debug, Clone)]
pub struct RequestContext<'a> {
    pub map: &'a MapEntry,
    /// relay 的源（`scheme://host`），由请求头算出。
    pub relay_origin: String,
}

/// 本请求是否是一次 WebSocket 升级（`Upgrade: websocket`，大小写不敏感）。
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// 由请求头算出 relay 的源，规则与 `install_script_handler` 一致：
/// `X-Forwarded-Proto` 为 `https` 时 https，否则 http；host 取 `Host`。
pub fn relay_origin(headers: &HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|v| *v == "https")
        .map(|_| "https")
        .unwrap_or("http");
    format!("{proto}://{host}")
}

/// `referer` 属于 relay 源时返回去掉源之后的部分（以 `/` 开头）。
fn referer_path(relay_origin: &str, referer: &str) -> Option<String> {
    let rest = referer.strip_prefix(relay_origin)?;
    if rest.is_empty() {
        Some("/".to_string())
    } else if rest.starts_with('/') {
        Some(rest.to_string())
    } else {
        // `https://relay.example.com.evil/` 这种前缀伪装不算同源。
        None
    }
}

fn ensure_leading_slash(rest: &str) -> String {
    if rest.starts_with('/') {
        rest.to_string()
    } else {
        format!("/{rest}")
    }
}

/// 把 `Location` 改写成映射前缀下的地址。返回 `None` 表示原样透传。
///
/// - 目标源的绝对 URL（`http://192.168.0.1/a?b#c`）→ `/agent/web/m/<id>/a?b#c`；
/// - 以 `/` 开头的路径（`/login?x=1`）→ 加前缀；
/// - 相对路径与其他源 → 原样。
pub fn rewrite_location(map_id: &str, target_origin: &str, location: &str) -> Option<String> {
    let prefix = format!("{PATH_PREFIX}/{map_id}");
    if let Some(rest) = location.strip_prefix(target_origin) {
        // 必须紧跟 `/` `?` `#` 或结束，否则 `http://a` 会错误匹配 `http://ab/`。
        if rest.is_empty() {
            return Some(format!("{prefix}/"));
        }
        if rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#') {
            return Some(format!("{prefix}{}", ensure_leading_slash(rest)));
        }
        return None;
    }
    if location.starts_with('/') {
        return Some(format!("{prefix}{location}"));
    }
    None
}

/// 改写一条 `Set-Cookie`。
///
/// - 去掉 `Domain=`（目标站的域作用不到 relay 源，浏览器会直接丢弃整条）；
/// - `Path` 缺省或为 `/` → 显式 `Path=/`；
/// - 其他 `Path=/p` → `Path=/agent/web/m/<id>/p`。
pub fn rewrite_set_cookie(map_id: &str, cookie: &str) -> String {
    let prefix = format!("{PATH_PREFIX}/{map_id}");
    let mut parts: Vec<String> = Vec::new();
    let mut saw_path = false;
    for (i, part) in cookie.split(';').enumerate() {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        if i > 0 {
            let lower = trimmed.to_ascii_lowercase();
            if lower.starts_with("domain=") {
                continue;
            }
            if let Some(path) = attr_value(trimmed, "path") {
                saw_path = true;
                parts.push(format!("Path={}", rewrite_cookie_path(&prefix, path)));
                continue;
            }
        }
        parts.push(trimmed.to_string());
    }
    if !saw_path && !parts.is_empty() {
        parts.push("Path=/".to_string());
    }
    parts.join("; ")
}

/// 取 `name=value` 属性里的值（名大小写不敏感）。
fn attr_value<'a>(part: &'a str, name: &str) -> Option<&'a str> {
    let (k, v) = part.split_once('=')?;
    if k.trim().eq_ignore_ascii_case(name) {
        Some(v.trim())
    } else {
        None
    }
}

fn rewrite_cookie_path(prefix: &str, path: &str) -> String {
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else if path.starts_with('/') {
        format!("{prefix}{path}")
    } else {
        format!("{prefix}/{path}")
    }
}

/// `origin` 是否等于本站源（大小写不敏感、忽略尾斜杠）。
fn origin_same(origin: &str, relay_origin: &str) -> bool {
    origin
        .trim_end_matches('/')
        .eq_ignore_ascii_case(relay_origin)
}

/// 去掉 `/agent/web/m/<map_id>` 前缀；不匹配时原样返回。
pub fn strip_map_prefix(path: &str, map_id: &str) -> String {
    let prefix = format!("{PATH_PREFIX}/{map_id}");
    match path.strip_prefix(&prefix) {
        Some("") => "/".to_string(),
        Some(rest) if rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#') => {
            ensure_leading_slash(rest)
        }
        _ => path.to_string(),
    }
}

/// 构造发给目标的请求头。`ws` 为真时保留 `Connection: Upgrade` / `Upgrade`。
pub fn build_upstream_headers(
    ctx: &RequestContext<'_>,
    headers: &HeaderMap,
    ws: bool,
) -> HeaderMap {
    let map = ctx.map;
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        let lname = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lname.as_str())
            && !(ws && (lname == "connection" || lname == "upgrade"))
        {
            continue;
        }
        // 本站凭据与内部头，绝不外发。
        if lname == "x-sr-token" || lname == "x-auth" || lname.starts_with("x-sr-") {
            continue;
        }
        let Ok(n) = HeaderName::from_bytes(name.as_str().as_bytes()) else {
            continue;
        };
        let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) else {
            continue;
        };
        out.append(n, v);
    }
    // WebSocket：`Connection` 统一写成 `Upgrade`（原值可能带额外 token）。
    if ws {
        out.insert(
            axum::http::header::CONNECTION,
            HeaderValue::from_static("Upgrade"),
        );
        out.insert(
            axum::http::header::UPGRADE,
            HeaderValue::from_static("websocket"),
        );
    }
    // Host 必须是目标 authority。
    if let Ok(v) = HeaderValue::from_str(&map.authority) {
        out.insert(axum::http::header::HOST, v);
    }
    // Origin：本站源 → target_origin，否则保留循环里复制过来的原值。
    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if origin_same(origin, &ctx.relay_origin) {
            if let Ok(v) = HeaderValue::from_str(&map.target_origin) {
                out.insert(axum::http::header::ORIGIN, v);
            }
        }
    }
    // Referer：本站源 → 去映射前缀后拼 target_origin；其他源原样。
    if let Some(referer) = headers
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(path) = referer_path(&ctx.relay_origin, referer) {
            let stripped = strip_map_prefix(&path, &map.map_id);
            if let Ok(v) = HeaderValue::from_str(&format!("{}{stripped}", map.target_origin)) {
                out.insert(axum::http::header::REFERER, v);
            }
        }
    }
    out
}

/// 构造发给浏览器的响应头（body 不动）。
///
/// `upgrade` 为真表示这是一条 101：此时 `Connection` / `Upgrade` **必须**保留
/// （它们是升级握手的协议要素，剥掉浏览器会直接判定握手失败），其余 hop-by-hop
/// 仍然剥掉。
pub fn build_downstream_headers(map: &MapEntry, headers: &HeaderMap, upgrade: bool) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        let lname = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lname.as_str()) {
            if !(upgrade && (lname == "connection" || lname == "upgrade")) {
                continue;
            }
        }
        if RESPONSE_STRIP.contains(&lname.as_str()) {
            continue;
        }
        if lname == "location" {
            let loc = value.to_str().unwrap_or("");
            let new = rewrite_location(&map.map_id, &map.target_origin, loc)
                .unwrap_or_else(|| loc.to_string());
            if let Ok(v) = HeaderValue::from_str(&new) {
                out.append(HeaderName::from_static("location"), v);
            }
            continue;
        }
        if lname == "set-cookie" {
            let c = value.to_str().unwrap_or("");
            if let Ok(v) = HeaderValue::from_str(&rewrite_set_cookie(&map.map_id, c)) {
                out.append(HeaderName::from_static("set-cookie"), v);
            }
            continue;
        }
        let Ok(n) = HeaderName::from_bytes(name.as_str().as_bytes()) else {
            continue;
        };
        let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) else {
            continue;
        };
        out.append(n, v);
    }
    if upgrade {
        // 统一写成 `Upgrade: websocket`，避免把目标的 `Connection` 里的额外
        // token（`keep-alive, Upgrade`）带到响应上。
        out.insert(
            axum::http::header::CONNECTION,
            HeaderValue::from_static("Upgrade"),
        );
        out.insert(
            axum::http::header::UPGRADE,
            HeaderValue::from_static("websocket"),
        );
    }
    out
}

// ── 每映射运行时（连接池 + 在途配额） ───────────────────────────────

/// 池里的一条上游连接。`SendRequest` 只在没有在途请求时进池，因此每条连接
/// 同一时刻只承载一个请求（HTTP/1.1 的天然语义）。
struct PooledConn {
    sender: hyper::client::conn::http1::SendRequest<Body>,
    idle: tokio::time::Instant,
    /// 连接的驱动任务（`Drop` detach 后自己结束）。
    task: tokio::task::JoinHandle<()>,
}

impl PooledConn {
    fn usable(&self) -> bool {
        !self.sender.is_closed() && self.idle.elapsed() < POOL_IDLE
    }
}

/// 一个映射的运行时状态。
pub struct MapRuntime {
    pool: Mutex<Vec<PooledConn>>,
    /// 在途请求配额（≤[`MAX_INFLIGHT`]），排队等它。
    permits: Arc<Semaphore>,
    pool_size: AtomicUsize,
}

impl MapRuntime {
    pub fn new() -> Self {
        Self {
            pool: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(MAX_INFLIGHT)),
            pool_size: AtomicUsize::new(POOL_SIZE),
        }
    }

    pub fn pool_size(&self) -> usize {
        self.pool_size.load(Ordering::Relaxed)
    }

    /// 单测用：改池大小。
    pub fn set_pool_size(&self, n: usize) {
        self.pool_size.store(n.max(1), Ordering::Relaxed);
    }

    fn take_idle(&self) -> Option<PooledConn> {
        let mut pool = lock(&self.pool);
        while let Some(c) = pool.pop() {
            if c.usable() {
                return Some(c);
            }
        }
        None
    }

    fn put_idle(&self, conn: PooledConn) {
        let max = self.pool_size();
        let mut pool = lock(&self.pool);
        pool.retain(|c| c.usable());
        if pool.len() < max && conn.usable() {
            pool.push(conn);
        }
    }
}

impl Default for MapRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// 每映射运行时的表（与映射表同生命周期）。
#[derive(Default)]
pub struct RuntimeStore {
    maps: Mutex<HashMap<String, Arc<MapRuntime>>>,
}

impl RuntimeStore {
    pub fn get_or_create(&self, map_id: &str) -> Arc<MapRuntime> {
        lock(&self.maps)
            .entry(map_id.to_string())
            .or_insert_with(|| Arc::new(MapRuntime::new()))
            .clone()
    }

    pub fn forget_many<'a>(&self, ids: impl Iterator<Item = &'a str>) {
        let mut m = lock(&self.maps);
        for id in ids {
            m.remove(id);
        }
    }

    /// 清掉所有不再有对应映射的运行时（周期清理兜底）。
    pub fn retain_only(&self, alive: impl Fn(&str) -> bool) {
        lock(&self.maps).retain(|id, _| alive(id));
    }

    pub fn count(&self) -> usize {
        lock(&self.maps).len()
    }
}

// ── HTTP 入口：建映射 ───────────────────────────────────────────────

/// `POST /agent/web/map`：为一个目标源建（或复用）映射。
///
/// 鉴权 = `X-Auth` + `X-SR-Token`，且**只允许 rw**：完整映射能提交表单、改设备
/// 配置，ro 继续用 v0.58 的静态只读模式。
pub async fn create_map_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !crate::relay::auth::server_password_ok(&state, &headers, None).await {
        return crate::relay::auth::invalid_password_response();
    }
    let token = headers
        .get("x-sr-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let session_id = match state.sessions.authenticate(token).await {
        Some((sid, Permission::ReadWrite)) => sid,
        Some(_) => {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(json!({"error": "read-only token"})),
            )
                .into_response()
        }
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "invalid session token"})),
            )
                .into_response()
        }
    };

    // 能力协商：旧 agent 一律 404（UI 只看状态码）。
    if !has_capability(&state, &session_id).await {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "unsupported"})),
        )
            .into_response();
    }
    if !agent_online(&state, &session_id).await {
        return agent_offline();
    }

    let url = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => match v.get("url").and_then(|u| u.as_str()) {
            Some(u) => u.to_string(),
            None => return bad_request("missing url"),
        },
        Err(_) => return bad_request("invalid json body"),
    };
    let target = match parse_target(&url) {
        Ok(t) => t,
        Err(msg) => return bad_request(msg),
    };

    let entry = state.web_maps.upsert(MapEntry {
        map_id: random_map_id(),
        session_id: session_id.clone(),
        target_origin: target.origin.clone(),
        tls: target.tls,
        host: target.host,
        port: target.port,
        authority: target.authority,
        last_used: tokio::time::Instant::now(),
    });

    // 有意不记录 map_id 本身：它就是访问凭据，只属于浏览器。
    tracing::debug!(
        session = %session_id,
        target = %target.origin,
        "web map: mapping created or reused"
    );
    axum::Json(json!({
        "map_id": entry.map_id,
        "base": format!("{PATH_PREFIX}/{}/", entry.map_id),
        "target_origin": entry.target_origin,
        "expires_in": state.web_maps.idle_ttl().as_secs(),
    }))
    .into_response()
}

async fn has_capability(state: &SharedState, session_id: &str) -> bool {
    state
        .sessions
        .get_capabilities(session_id)
        .await
        .iter()
        .any(|c| c == tcp_tunnel::CAPABILITY)
}

async fn agent_online(state: &SharedState, session_id: &str) -> bool {
    state
        .agent_broadcast
        .read()
        .await
        .get(session_id)
        .and_then(|cm| cm.agent_bulk.clone())
        .is_some_and(|tx| !tx.is_closed())
}

fn bad_request(msg: &'static str) -> Response {
    (StatusCode::BAD_REQUEST, axum::Json(json!({"error": msg}))).into_response()
}

fn agent_offline() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(json!({"error": "agent is offline"})),
    )
        .into_response()
}

// ── HTTP 入口：反代 ─────────────────────────────────────────────────

/// `… /agent/web/m/:map_id/*rest`：反代主体。
///
/// 直接收 `Request<Body>`（不用 Json 提取器），因为上传必须流式、不受 2 MiB
/// 体积上限约束。
pub async fn proxy_handler(
    State(state): State<Arc<SharedState>>,
    Path((map_id, rest)): Path<(String, String)>,
    req: Request<Body>,
) -> Response {
    proxy_inner(state, map_id, rest, req).await
}

/// `… /agent/web/m/:map_id`（无尾随部分）的反代入口。
pub async fn proxy_root_handler(
    State(state): State<Arc<SharedState>>,
    Path(map_id): Path<String>,
    req: Request<Body>,
) -> Response {
    proxy_inner(state, map_id, String::new(), req).await
}

async fn proxy_inner(
    state: Arc<SharedState>,
    map_id: String,
    rest: String,
    req: Request<Body>,
) -> Response {
    let (parts, body) = req.into_parts();
    let headers = parts.headers.clone();
    let mut body_slot = body;

    // ── 1. 映射必须有效（无效/过期 → 404） ──
    let Some(map) = state.web_maps.get(&map_id) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "unknown map"})),
        )
            .into_response();
    };
    // ── 2. 禁止远端页面借映射路径注册 Service Worker ──
    if headers
        .get("service-worker")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("script"))
    {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": "service worker registration is not allowed"})),
        )
            .into_response();
    }
    // ── 3. agent 必须仍在线（断线后立刻失效，不等映射被撤） ──
    if !agent_online(&state, &map.session_id).await {
        return agent_offline();
    }

    let ws = is_websocket_upgrade(&headers);
    let runtime = state.web_map_runtimes.get_or_create(&map.map_id);

    // ── 4. 在途配额：超出排队，最多 15s ──
    let permit =
        match tokio::time::timeout(QUEUE_WAIT, runtime.permits.clone().acquire_owned()).await {
            Ok(Ok(p)) => p,
            Ok(Err(_)) => return busy(),
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(json!({"error": "too many concurrent requests for this map"})),
                )
                    .into_response()
            }
        };

    let ctx = RequestContext {
        map: &map,
        relay_origin: relay_origin(&headers),
    };
    let upstream_headers = build_upstream_headers(&ctx, &headers, ws);
    let path = build_upstream_path(&rest, parts.uri.query());

    let uri: Uri = match path.parse() {
        Ok(u) => u,
        Err(_) => return bad_request("invalid request target"),
    };
    let has_body = request_has_body(&headers);
    // body 只能取一次，重试只对无 body 的请求开放（这是重试的主要场景，也是
    // `Body` 不可 Clone 带来的必然约束）。
    let can_retry = !has_body && !ws;

    // ── 5-6. 取连接 + 发请求 ──
    //
    // 重试一次：池里的 keep-alive 连接可能已被目标悄悄关掉（目标空闲超时、
    // 设备重启），`is_closed()` 的检查有竞态窗口。此时请求根本没被目标处理过
    // （hyper 会把它判为 `canceled`/`incomplete`），换一条新管道重发是安全的。
    let mut attempt = 0;
    let (mut sender, conn_task, resp) = loop {
        let reused = if ws { None } else { runtime.take_idle() };
        let was_reused = reused.is_some();
        let (mut sender, conn_task) = match reused {
            Some(c) => (c.sender, Some(c.task)),
            None => match open_upstream(&state, &map).await {
                Ok(v) => v,
                Err(resp) => return resp,
            },
        };
        let mut builder = Request::builder()
            .method(parts.method.clone())
            .uri(uri.clone());
        {
            let h = builder.headers_mut().unwrap();
            *h = upstream_headers.clone();
        }
        // 无 body 时用空 body 重发，有 body 时把原 body 交出去（只能一次）。
        let (body_done_tx, body_done_rx) = oneshot::channel::<()>();
        let upstream_body = if has_body {
            Body::new(BodySignal {
                inner: std::mem::replace(&mut body_slot, Body::empty()),
                done: Some(body_done_tx),
            })
        } else {
            drop(body_done_tx);
            Body::empty()
        };
        let upstream_req = match builder.body(upstream_body) {
            Ok(r) => r,
            Err(_) => return bad_request("invalid request"),
        };
        match send_with_head_timeout(&mut sender, upstream_req, has_body, body_done_rx).await {
            Ok(r) => break (sender, conn_task, r),
            // 复用的连接失败且请求无 body → 换新连接重试一次。
            Err(SendError::Hyper(e)) if was_reused && can_retry && attempt == 0 => {
                attempt += 1;
                tracing::debug!(error = %e, "web map: pooled connection failed — retrying on a fresh tunnel");
                if let Some(t) = conn_task {
                    t.abort();
                }
                continue;
            }
            Err(e) => return send_error_response(e),
        }
    };

    let status = resp.status();
    let upgrading = status == StatusCode::SWITCHING_PROTOCOLS;
    let down_headers = build_downstream_headers(&map, resp.headers(), upgrading);

    // ── 7. 101：双向原始字节透传 ──
    if status == StatusCode::SWITCHING_PROTOCOLS {
        let on_browser = match parts.extensions.get::<hyper::upgrade::OnUpgrade>() {
            Some(u) => u.clone(),
            None => {
                return (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(json!({"error": "bad_gateway", "message": "browser connection is not upgradeable"})),
                )
                    .into_response()
            }
        };
        let on_upstream = hyper::upgrade::on(resp);
        let mut out = Response::builder().status(status);
        {
            let h = out.headers_mut().unwrap();
            *h = down_headers;
        }
        let response = out.body(Body::empty()).unwrap();
        // 101 必须先回到浏览器，`on_browser` 才能完成，所以透传在独立任务里。
        tokio::spawn(async move {
            let (Ok(up_browser), Ok(up_target)) = (on_browser.await, on_upstream.await) else {
                return;
            };
            let mut a = TokioIo::new(up_browser);
            let mut b = TokioIo::new(up_target);
            // 原始字节，不解析 WS 帧。
            let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
            // 名额随任务结束释放；WebSocket 连接不入池，由 agent 侧关闭后回收。
            drop(permit);
        });
        return response;
    }

    // ── 8. 普通响应：头已改写，body 流式透传 ──
    let guard = ConnGuard {
        runtime: runtime.clone(),
        sender: Some(sender),
        conn_task,
        permit: Some(permit),
        pool: !ws,
        armed: true,
    };
    let mut out = Response::builder().status(status);
    {
        let h = out.headers_mut().unwrap();
        *h = down_headers;
    }
    out.body(Body::new(StreamWithGuard {
        inner: resp.into_body(),
        guard: Some(guard),
    }))
    .unwrap()
}

fn busy() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(json!({"error": "too many concurrent requests for this map"})),
    )
        .into_response()
}

/// 请求是否带 body（`Content-Length: 0` 与无 `Transfer-Encoding` 视为无）。
fn request_has_body(headers: &HeaderMap) -> bool {
    if headers.get(axum::http::header::TRANSFER_ENCODING).is_some() {
        return true;
    }
    headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > 0)
}

/// 发请求时可能的失败。
enum SendError {
    /// 上游/TCP 层失败。
    Hyper(hyper::Error),
    /// 响应头等超时。
    Timeout,
}

/// 发请求并等响应头，超时 [`HEAD_TIMEOUT`]。
///
/// 带 body 的请求（上传）在 body 传完之前**不**计时——否则慢链路下 10 MiB 上传
/// 必然 504，与「body 不设总时长上限」相悖。超时从 body 传完那一刻开始算。
async fn send_with_head_timeout(
    sender: &mut hyper::client::conn::http1::SendRequest<Body>,
    req: Request<Body>,
    has_body: bool,
    body_done: oneshot::Receiver<()>,
) -> Result<hyper::Response<Incoming>, SendError> {
    let fut = sender.send_request(req);
    tokio::pin!(fut);
    if !has_body {
        return match tokio::time::timeout(HEAD_TIMEOUT, fut).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(SendError::Hyper(e)),
            Err(_) => Err(SendError::Timeout),
        };
    }
    let mut body_done = body_done;
    loop {
        tokio::select! {
            r = &mut fut => {
                return match r {
                    Ok(resp) => Ok(resp),
                    Err(e) => Err(SendError::Hyper(e)),
                };
            }
            // body 传完（或发送端被 drop）→ 之后开始计响应头超时。
            _ = &mut body_done => break,
        }
    }
    match tokio::time::timeout(HEAD_TIMEOUT, fut).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => Err(SendError::Hyper(e)),
        Err(_) => Err(SendError::Timeout),
    }
}

/// 上游请求的 path（origin-form）：`/` + rest，保留原 query。
fn build_upstream_path(rest: &str, query: Option<&str>) -> String {
    let path = if rest.is_empty() {
        "/".to_string()
    } else if rest.starts_with('/') {
        rest.to_string()
    } else {
        format!("/{rest}")
    };
    match query {
        Some(q) => format!("{path}?{q}"),
        None => path,
    }
}

/// 建一条新的上游连接（https 目标先做 TLS）。
async fn open_upstream(
    state: &Arc<SharedState>,
    map: &MapEntry,
) -> Result<
    (
        hyper::client::conn::http1::SendRequest<Body>,
        Option<tokio::task::JoinHandle<()>>,
    ),
    Response,
> {
    let tunnel = match tcp_tunnel::open(state, &map.session_id, &map.host, map.port).await {
        Ok(t) => t,
        Err(e) => return Err(open_error_response(&e)),
    };
    let io: Box<dyn UpstreamIo> = if map.tls {
        Box::new(tls_wrap(tunnel, &map.host).await?)
    } else {
        Box::new(tunnel)
    };
    let (sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(io)).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "web map: http1 handshake with target failed");
            return Err((
                StatusCode::BAD_GATEWAY,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; charset=utf-8",
                )],
                "target is not speaking HTTP/1.1".to_string(),
            )
                .into_response());
        }
    };
    // 连接必须被驱动；`SendRequest` drop 后它自然结束。`.with_upgrades()` 是
    // WebSocket 透传的前提。
    let task = tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    Ok((sender, Some(task)))
}

/// 上游 IO 的两种形态（裸 TCP 管道 / TLS over 管道）。
trait UpstreamIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> UpstreamIo for T {}

/// 在管道上做 TLS 握手（接受自签证书，内网设备基本都是自签）。
async fn tls_wrap(
    tunnel: TunnelStream,
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<TunnelStream>, Response> {
    use rustls::pki_types::ServerName;
    let mut cfg = (*crate::tlsutil::no_verify_client_config()).clone();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let server_name = match ServerName::try_from(host.to_string()) {
        Ok(n) => n,
        Err(_) => return Err(bad_gateway_text("invalid TLS server name")),
    };
    match tokio::time::timeout(HEAD_TIMEOUT, connector.connect(server_name, tunnel)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "web map: TLS handshake with target failed");
            Err(bad_gateway_text("TLS handshake with target failed"))
        }
        Err(_) => Err((
            StatusCode::GATEWAY_TIMEOUT,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            "TLS handshake with target timed out".to_string(),
        )
            .into_response()),
    }
}

fn bad_gateway_text(msg: &str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        msg.to_string(),
    )
        .into_response()
}

/// `OpenError` → HTTP 状态码（任务卡 C 段）。
fn open_error_response(e: &OpenError) -> Response {
    let (status, msg) = match e {
        OpenError::Offline => (StatusCode::SERVICE_UNAVAILABLE, "agent is offline"),
        OpenError::Busy => (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many tunnels open on this agent",
        ),
        OpenError::Downlink => (StatusCode::SERVICE_UNAVAILABLE, "agent downlink busy"),
        OpenError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "connect to target timed out"),
        OpenError::Agent { kind, .. } => match kind.as_str() {
            "timeout" => (StatusCode::GATEWAY_TIMEOUT, "connect to target timed out"),
            "busy" => (
                StatusCode::SERVICE_UNAVAILABLE,
                "too many tunnels open on this agent",
            ),
            _ => (StatusCode::BAD_GATEWAY, "cannot connect to target"),
        },
    };
    (
        status,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        msg.to_string(),
    )
        .into_response()
}

/// 发送失败 → 状态码。超时 504，其余（管道断开、目标提前关闭）502。
fn send_error_response(e: SendError) -> Response {
    match e {
        SendError::Timeout => (
            StatusCode::GATEWAY_TIMEOUT,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            "timed out waiting for the target's response headers".to_string(),
        )
            .into_response(),
        SendError::Hyper(e) => {
            tracing::debug!(error = %e, "web map: upstream request failed");
            gateway_error(&e)
        }
    }
}

/// hyper 客户端错误 → 502（管道断开、目标提前关闭、对端不说 HTTP/1.1）。
fn gateway_error(e: &hyper::Error) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        format!("upstream request failed: {e}"),
    )
        .into_response()
}

/// 响应体结束（或浏览器断开）后归还连接、释放在途名额。
struct ConnGuard {
    runtime: Arc<MapRuntime>,
    sender: Option<hyper::client::conn::http1::SendRequest<Body>>,
    conn_task: Option<tokio::task::JoinHandle<()>>,
    permit: Option<OwnedSemaphorePermit>,
    /// 是否允许把连接放回池（WebSocket 与出错路径不放）。
    pool: bool,
    armed: bool,
}

impl ConnGuard {
    /// 正常读完响应：连接可复用则入池。
    fn finish(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let Some(permit) = self.permit.take() else {
            return;
        };
        drop(permit);
        if self.pool {
            if let (Some(sender), Some(task)) = (self.sender.take(), self.conn_task.take()) {
                self.runtime.put_idle(PooledConn {
                    sender,
                    idle: tokio::time::Instant::now(),
                    task,
                });
                return;
            }
        }
        // 不入池：drop `SendRequest` 让连接任务自己结束。
        self.sender.take();
        if let Some(t) = self.conn_task.take() {
            t.abort();
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        // body 未读完就 drop（浏览器断开）：半截连接绝不能回池。
        drop(self.permit.take());
        self.sender.take();
        if let Some(t) = self.conn_task.take() {
            t.abort();
        }
    }
}

/// 把 hyper 的响应体包一层，读完/出错时触发 [`ConnGuard::finish`]（把上游连接
/// 放回池、释放在途名额）。
///
/// **必须同时看 `is_end_stream`**：`Content-Length` 定长的 body 在最后一个
/// frame 之后 `size_hint` 就归零、`is_end_stream()` 变为 true，服务端不会再
/// poll 一次拿 `Ready(None)`。只看 `Ready(None)` 的话，所有定长响应都会走
/// [`ConnGuard::drop`] 把连接直接 abort —— 连接池等于不存在，每个请求都新开
/// 一条 TCP 管道（实测 10 个顺序请求开了 10 条）。
struct StreamWithGuard {
    inner: Incoming,
    guard: Option<ConnGuard>,
}

impl StreamWithGuard {
    /// 结束并归还连接（幂等：`guard` 取走后不再重复）。
    fn finish_guard(&mut self) {
        if let Some(mut g) = self.guard.take() {
            g.finish();
        }
    }
}

impl hyper::body::Body for StreamWithGuard {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        // 空 body（204/304、`Content-Length: 0`）：一开始就结束，可能再也不会
        // 被 poll，这里直接收尾。
        if this.inner.is_end_stream() {
            this.finish_guard();
            return Poll::Ready(None);
        }
        let r = std::pin::Pin::new(&mut this.inner).poll_frame(cx);
        match &r {
            Poll::Ready(None) | Poll::Ready(Some(Err(_))) => this.finish_guard(),
            Poll::Ready(Some(Ok(_))) => {
                if this.inner.is_end_stream() {
                    this.finish_guard();
                }
            }
            Poll::Pending => {}
        }
        r
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// 请求体包一层：body 传完时通知 [`send_with_head_timeout`]，用于「有 body 的
/// 请求从 body 结束才开始计响应头超时」。
struct BodySignal {
    inner: Body,
    done: Option<oneshot::Sender<()>>,
}

impl hyper::body::Body for BodySignal {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let r = std::pin::Pin::new(&mut this.inner).poll_frame(cx);
        if matches!(&r, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            if let Some(tx) = this.done.take() {
                let _ = tx.send(());
            }
        }
        r.map(|o| {
            o.map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))
        })
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::body::Body as _;

    // ── parse_target ───────────────────────────────────────────────

    #[test]
    fn test_parse_target_defaults_and_origins() {
        let t = parse_target("http://192.168.0.1/").unwrap();
        assert_eq!(t.origin, "http://192.168.0.1");
        assert_eq!(t.authority, "192.168.0.1");
        assert_eq!(t.port, 80);
        assert!(!t.tls);

        let t = parse_target("http://192.168.0.1:8080/a/b?c=d#e").unwrap();
        assert_eq!(t.origin, "http://192.168.0.1:8080");
        assert_eq!(t.authority, "192.168.0.1:8080");
        assert_eq!(t.port, 8080);

        let t = parse_target("https://example.com/x").unwrap();
        assert_eq!(t.origin, "https://example.com");
        assert_eq!(t.authority, "example.com");
        assert_eq!(t.port, 443);
        assert!(t.tls);

        // 显式写默认端口时 origin 不带 `:80`。
        let t = parse_target("http://host:80/").unwrap();
        assert_eq!(t.origin, "http://host");
        assert_eq!(t.authority, "host");

        let t = parse_target("http://127.0.0.1:3000/").unwrap();
        assert_eq!(t.origin, "http://127.0.0.1:3000");
    }

    #[test]
    fn test_parse_target_rejects_bad_forms() {
        for bad in [
            "",
            "/index.html",
            "example.com/x",
            "ftp://example.com/x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "//example.com/x",
            "http://",
            "http:///path",
            "https://?q=1",
            "http://user:pass@host/",
            "http://host:0/",
            "http://host:99999/",
            "http://exa mple.com/",
            "http://exa\nmple.com/",
        ] {
            assert!(parse_target(bad).is_err(), "应拒绝 {bad:?}");
        }
    }

    #[test]
    fn test_parse_target_length_limit() {
        let base = "http://127.0.0.1/";
        let ok = format!("{base}{}", "a".repeat(MAX_URL_LEN - base.len()));
        assert_eq!(ok.len(), MAX_URL_LEN);
        assert!(parse_target(&ok).is_ok(), "恰好 4096 应通过");
        let long = format!("{base}{}", "a".repeat(MAX_URL_LEN));
        assert!(parse_target(&long).is_err(), "超过 4096 应拒绝");
    }

    // ── Location 改写 ──────────────────────────────────────────────

    #[test]
    fn test_rewrite_location_absolute_same_origin() {
        let id = "abc123";
        assert_eq!(
            rewrite_location(id, "http://192.168.0.1", "http://192.168.0.1/admin?a=1#top").unwrap(),
            "/agent/web/m/abc123/admin?a=1#top"
        );
        assert_eq!(
            rewrite_location(id, "http://192.168.0.1", "http://192.168.0.1").unwrap(),
            "/agent/web/m/abc123/"
        );
        assert_eq!(
            rewrite_location(id, "http://192.168.0.1", "http://192.168.0.1?x=1").unwrap(),
            "/agent/web/m/abc123/?x=1"
        );
    }

    #[test]
    fn test_rewrite_location_root_path() {
        assert_eq!(
            rewrite_location("abc123", "http://192.168.0.1", "/login?next=%2F").unwrap(),
            "/agent/web/m/abc123/login?next=%2F"
        );
    }

    #[test]
    fn test_rewrite_location_relative_and_other_origin_untouched() {
        // 相对路径：浏览器按映射 URL 自己解析，不用改。
        assert_eq!(rewrite_location("abc123", "http://t", "next/page"), None);
        // 其他源：原样。
        assert_eq!(
            rewrite_location("abc123", "http://t", "https://cdn.example/x.js"),
            None
        );
        // 前缀伪装（`http://t.evil/`）不算同源。
        assert_eq!(
            rewrite_location("abc123", "http://t", "http://t.evil/x"),
            None
        );
    }

    // ── Set-Cookie 改写 ────────────────────────────────────────────

    #[test]
    fn test_rewrite_set_cookie_strips_domain() {
        let out = rewrite_set_cookie("m1", "sid=abc; Domain=192.168.0.1; Path=/; HttpOnly");
        assert!(!out.to_ascii_lowercase().contains("domain="), "{out}");
        assert!(out.contains("sid=abc"));
        assert!(out.contains("Path=/"));
        assert!(out.contains("HttpOnly"));
    }

    #[test]
    fn test_rewrite_set_cookie_path_cases() {
        // Path 缺省 → 显式 Path=/。
        assert_eq!(
            rewrite_set_cookie("m1", "a=1; HttpOnly"),
            "a=1; HttpOnly; Path=/"
        );
        // Path=/ → 保持 /。
        assert_eq!(rewrite_set_cookie("m1", "a=1; Path=/"), "a=1; Path=/");
        // Path=/p → 加映射前缀。
        assert_eq!(
            rewrite_set_cookie("m1", "a=1; Path=/admin/x; Secure"),
            "a=1; Path=/agent/web/m/m1/admin/x; Secure"
        );
        // 属性名大小写不敏感。
        assert_eq!(
            rewrite_set_cookie("m1", "a=1; path=/p"),
            "a=1; Path=/agent/web/m/m1/p"
        );
    }

    // ── Origin / Referer ───────────────────────────────────────────

    fn map_entry(id: &str, target: &str) -> MapEntry {
        let t = parse_target(target).unwrap();
        MapEntry {
            map_id: id.to_string(),
            session_id: "sid".to_string(),
            target_origin: t.origin,
            tls: t.tls,
            host: t.host,
            port: t.port,
            authority: t.authority,
            last_used: tokio::time::Instant::now(),
        }
    }

    fn hdrs(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn test_relay_origin_from_headers() {
        assert_eq!(
            relay_origin(&hdrs(&[("host", "a.example")])),
            "http://a.example"
        );
        assert_eq!(
            relay_origin(&hdrs(&[
                ("host", "a.example"),
                ("x-forwarded-proto", "https")
            ])),
            "https://a.example"
        );
        assert_eq!(
            relay_origin(&hdrs(&[
                ("host", "a.example"),
                ("x-forwarded-proto", "http")
            ])),
            "http://a.example"
        );
    }

    #[test]
    fn test_origin_rewrite_only_for_relay_origin() {
        let map = map_entry("m1", "http://192.168.0.1:8080/");
        let ctx = RequestContext {
            map: &map,
            relay_origin: "https://relay.example".to_string(),
        };
        let out = build_upstream_headers(
            &ctx,
            &hdrs(&[
                ("origin", "https://relay.example"),
                ("host", "relay.example"),
            ]),
            false,
        );
        assert_eq!(out["origin"], "http://192.168.0.1:8080");
        let out = build_upstream_headers(
            &ctx,
            &hdrs(&[
                ("origin", "https://other.example"),
                ("host", "relay.example"),
            ]),
            false,
        );
        assert_eq!(out["origin"], "https://other.example");
    }

    #[test]
    fn test_referer_rewrite() {
        let map = map_entry("m1", "http://192.168.0.1/");
        let ctx = RequestContext {
            map: &map,
            relay_origin: "https://relay.example".to_string(),
        };
        let out = build_upstream_headers(
            &ctx,
            &hdrs(&[
                ("host", "relay.example"),
                ("referer", "https://relay.example/agent/web/m/m1/a/b?q=1"),
            ]),
            false,
        );
        assert_eq!(out["referer"], "http://192.168.0.1/a/b?q=1");
        let out = build_upstream_headers(
            &ctx,
            &hdrs(&[
                ("host", "relay.example"),
                ("referer", "https://relay.example/session"),
            ]),
            false,
        );
        assert_eq!(out["referer"], "http://192.168.0.1/session");
        let out = build_upstream_headers(
            &ctx,
            &hdrs(&[
                ("host", "relay.example"),
                ("referer", "https://other.example/x"),
            ]),
            false,
        );
        assert_eq!(out["referer"], "https://other.example/x");
    }

    #[test]
    fn test_request_header_strip_list() {
        let map = map_entry("m1", "http://192.168.0.1/");
        let ctx = RequestContext {
            map: &map,
            relay_origin: "https://relay.example".to_string(),
        };
        let out = build_upstream_headers(
            &ctx,
            &hdrs(&[
                ("host", "relay.example"),
                ("connection", "keep-alive, X-Custom"),
                ("keep-alive", "timeout=5"),
                ("proxy-authenticate", "Basic"),
                ("proxy-authorization", "Basic xx"),
                ("te", "trailers"),
                ("trailer", "X"),
                ("transfer-encoding", "chunked"),
                ("upgrade", "h2c"),
                ("x-sr-token", "secret"),
                ("x-auth", "password"),
                ("x-sr-agent-token", "t"),
                ("cookie", "a=b"),
                ("authorization", "Bearer z"),
                ("accept-encoding", "gzip, br"),
            ]),
            false,
        );
        for gone in [
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "x-sr-token",
            "x-auth",
            "x-sr-agent-token",
        ] {
            assert!(out.get(gone).is_none(), "{gone} 应被剥掉: {out:?}");
        }
        assert_eq!(out["cookie"], "a=b");
        assert_eq!(out["authorization"], "Bearer z");
        assert_eq!(out["accept-encoding"], "gzip, br");
        assert_eq!(out["host"], "192.168.0.1");
    }

    #[test]
    fn test_websocket_request_keeps_upgrade_headers() {
        let map = map_entry("m1", "http://192.168.0.1/");
        let ctx = RequestContext {
            map: &map,
            relay_origin: "https://relay.example".to_string(),
        };
        let out = build_upstream_headers(
            &ctx,
            &hdrs(&[
                ("host", "relay.example"),
                ("connection", "keep-alive, Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("sec-websocket-version", "13"),
                ("x-sr-token", "secret"),
            ]),
            true,
        );
        assert_eq!(out["connection"], "Upgrade");
        assert_eq!(out["upgrade"], "websocket");
        assert_eq!(out["sec-websocket-version"], "13");
        assert!(out.get("sec-websocket-key").is_some());
        assert!(out.get("x-sr-token").is_none());
    }

    #[test]
    fn test_is_websocket_upgrade_case_insensitive() {
        assert!(is_websocket_upgrade(&hdrs(&[("upgrade", "WebSocket")])));
        assert!(is_websocket_upgrade(&hdrs(&[("upgrade", "websocket")])));
        assert!(!is_websocket_upgrade(&hdrs(&[("upgrade", "h2c")])));
        assert!(!is_websocket_upgrade(&HeaderMap::new()));
    }

    #[test]
    fn test_response_header_strip_list() {
        let map = map_entry("m1", "http://192.168.0.1/");
        let out = build_downstream_headers(
            &map,
            &hdrs(&[
                ("content-type", "text/html"),
                ("content-length", "10"),
                ("connection", "keep-alive"),
                ("keep-alive", "timeout=5"),
                ("transfer-encoding", "chunked"),
                ("content-security-policy", "default-src 'none'"),
                ("content-security-policy-report-only", "default-src 'none'"),
                ("x-frame-options", "DENY"),
                ("strict-transport-security", "max-age=31536000"),
                ("clear-site-data", "\"storage\""),
                ("alt-svc", "h3=\":443\""),
                ("cross-origin-opener-policy", "same-origin"),
                ("cross-origin-embedder-policy", "require-corp"),
                ("cross-origin-resource-policy", "same-origin"),
                ("service-worker-allowed", "/"),
                ("set-cookie", "a=1"),
                ("location", "/next"),
            ]),
            false,
        );
        for gone in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "content-security-policy",
            "content-security-policy-report-only",
            "x-frame-options",
            "strict-transport-security",
            "clear-site-data",
            "alt-svc",
            "cross-origin-opener-policy",
            "cross-origin-embedder-policy",
            "cross-origin-resource-policy",
            "service-worker-allowed",
        ] {
            assert!(out.get(gone).is_none(), "{gone} 应被剥掉");
        }
        assert_eq!(out["content-type"], "text/html");
        assert_eq!(out["content-length"], "10");
        // Location 与 Set-Cookie 是改写而非剥离。
        assert_eq!(out["location"], "/agent/web/m/m1/next");
        assert_eq!(out["set-cookie"], "a=1; Path=/");
    }

    #[test]
    fn test_response_keeps_upgrade_headers_on_101() {
        let map = map_entry("m1", "http://192.168.0.1/");
        let out = build_downstream_headers(
            &map,
            &hdrs(&[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-accept", "abc"),
                ("sec-websocket-protocol", "chat"),
                ("sec-websocket-extensions", "permessage-deflate"),
                ("transfer-encoding", "chunked"),
            ]),
            true,
        );
        // 101 必须带 Connection/Upgrade，否则浏览器判定握手失败。
        assert_eq!(out["connection"], "Upgrade");
        assert_eq!(out["upgrade"], "websocket");
        // Sec-WebSocket-* 原样透传。
        assert_eq!(out["sec-websocket-accept"], "abc");
        assert_eq!(out["sec-websocket-protocol"], "chat");
        assert_eq!(out["sec-websocket-extensions"], "permessage-deflate");
        // 其余 hop-by-hop 仍然剥掉。
        assert!(out.get("transfer-encoding").is_none());
    }

    #[test]
    fn test_build_upstream_path() {
        assert_eq!(build_upstream_path("", None), "/");
        assert_eq!(build_upstream_path("a/b", None), "/a/b");
        assert_eq!(build_upstream_path("a/b", Some("x=1")), "/a/b?x=1");
        assert_eq!(build_upstream_path("", Some("x=1")), "/?x=1");
        assert_eq!(build_upstream_path("a", Some("")), "/a?");
    }

    #[test]
    fn test_strip_map_prefix() {
        assert_eq!(strip_map_prefix("/agent/web/m/m1/a", "m1"), "/a");
        assert_eq!(strip_map_prefix("/agent/web/m/m1", "m1"), "/");
        assert_eq!(strip_map_prefix("/agent/web/m/m1?x=1", "m1"), "/?x=1");
        assert_eq!(
            strip_map_prefix("/agent/web/m/m2/a", "m1"),
            "/agent/web/m/m2/a"
        );
        assert_eq!(strip_map_prefix("/session", "m1"), "/session");
    }

    #[test]
    fn test_request_has_body() {
        assert!(!request_has_body(&HeaderMap::new()));
        assert!(!request_has_body(&hdrs(&[("content-length", "0")])));
        assert!(request_has_body(&hdrs(&[("content-length", "10")])));
        assert!(request_has_body(&hdrs(&[("transfer-encoding", "chunked")])));
    }

    // ── 映射表 ─────────────────────────────────────────────────────

    fn entry_for(sid: &str, target: &str) -> MapEntry {
        let t = parse_target(target).unwrap();
        MapEntry {
            map_id: random_map_id(),
            session_id: sid.to_string(),
            target_origin: t.origin,
            tls: t.tls,
            host: t.host,
            port: t.port,
            authority: t.authority,
            last_used: tokio::time::Instant::now(),
        }
    }

    #[test]
    fn test_map_store_reuses_same_target() {
        let store = MapStore::new();
        let a = store.upsert(entry_for("s1", "http://192.168.0.1/"));
        let b = store.upsert(entry_for("s1", "http://192.168.0.1/"));
        assert_eq!(a.map_id, b.map_id, "同会话同目标必须复用同一 map_id");
        assert_eq!(store.count(), 1);
        let c = store.upsert(entry_for("s1", "http://192.168.0.2/"));
        assert_ne!(c.map_id, a.map_id);
        assert_eq!(store.count(), 2);
        // 另一会话同目标 → 各自独立。
        let d = store.upsert(entry_for("s2", "http://192.168.0.1/"));
        assert_ne!(d.map_id, a.map_id);
    }

    #[test]
    fn test_map_store_per_session_limit_evicts_lru() {
        let store = MapStore::new();
        store.set_limits(2, 100);
        let a = store.upsert(entry_for("s1", "http://a/"));
        let b = store.upsert(entry_for("s1", "http://b/"));
        // 访问 a 使它成为最近使用；再建 c 应淘汰 b。
        assert!(store.get(&a.map_id).is_some());
        let c = store.upsert(entry_for("s1", "http://c/"));
        assert_eq!(store.count_session("s1"), 2);
        assert!(store.get(&a.map_id).is_some(), "a 最近用过，不该被淘汰");
        assert!(store.get(&b.map_id).is_none(), "b 最久未用，应被淘汰");
        assert!(store.get(&c.map_id).is_some());
    }

    #[test]
    fn test_map_store_total_limit() {
        let store = MapStore::new();
        store.set_limits(8, 2);
        let _a = store.upsert(entry_for("s1", "http://a/"));
        let _b = store.upsert(entry_for("s2", "http://b/"));
        let _c = store.upsert(entry_for("s3", "http://c/"));
        assert_eq!(store.count(), 2, "全局上限必须生效");
    }

    #[test]
    fn test_contains_does_not_refresh_ttl() {
        let store = MapStore::new();
        store.set_idle_ttl(Duration::from_millis(60));
        let a = store.upsert(entry_for("s1", "http://a/"));
        // contains 只查存在，不刷新 last_used。
        assert!(store.contains(&a.map_id));
        std::thread::sleep(Duration::from_millis(90));
        assert!(
            store.get(&a.map_id).is_none(),
            "contains 不得刷新 TTL（否则映射永不过期）"
        );
        assert!(!store.contains(&a.map_id));
    }

    #[test]
    fn test_map_store_idle_expiry() {
        let store = MapStore::new();
        store.set_idle_ttl(Duration::from_millis(0));
        let a = store.upsert(entry_for("s1", "http://a/"));
        assert!(store.get(&a.map_id).is_none(), "过期映射必须失效");
    }

    #[test]
    fn test_map_store_revokes_whole_session() {
        let store = MapStore::new();
        let a = store.upsert(entry_for("s1", "http://a/"));
        let b = store.upsert(entry_for("s1", "http://b/"));
        let c = store.upsert(entry_for("s2", "http://c/"));
        let revoked = store.revoke_session("s1");
        assert_eq!(revoked.len(), 2);
        assert!(store.get(&a.map_id).is_none());
        assert!(store.get(&b.map_id).is_none());
        assert!(store.get(&c.map_id).is_some(), "别会话的映射不受影响");
        assert!(store.revoke_session("nope").is_empty());
    }

    #[test]
    fn test_map_id_is_64_hex_chars() {
        let id = random_map_id();
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(id, random_map_id());
    }

    // ── 响应体守卫 ─────────────────────────────────────────────────

    /// 定长 body 的响应读完必须归还连接，而不是走 `Drop` 把连接 abort。
    ///
    /// 这是 E2E 实测到的真 bug：只认 `Ready(None)` 时，`Content-Length` 响应在
    /// 最后一个 frame 之后 `is_end_stream()` 就为 true、服务端不再 poll，于是
    /// 连接全被 abort，连接池形同虚设（10 个顺序请求开了 10 条 TCP 管道）。
    #[tokio::test]
    async fn test_stream_guard_returns_connection_for_fixed_length_body() {
        // 用一对 duplex 冒充上游：手写一条带 Content-Length 的 HTTP/1.1 响应，
        // 再让 hyper 客户端握手拿到真实的 `Incoming`（`Incoming::empty()` 是
        // 私有的，只能这样构造）。
        let (client_io, mut server_io) = tokio::io::duplex(8192);
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut req = vec![0u8; 1024];
            let _ = server_io.read(&mut req).await;
            let _ = server_io
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                .await;
            // 写完不关闭连接（keep-alive），模拟真实的目标站。
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(client_io))
            .await
            .expect("handshake");
        let conn_task = tokio::spawn(async move {
            let _ = conn.await;
        });

        let resp = sender
            .send_request(
                Request::builder()
                    .uri("/x")
                    .header("host", "target")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("响应");
        assert_eq!(resp.status(), StatusCode::OK);

        let runtime = Arc::new(MapRuntime::new());
        let mut body = StreamWithGuard {
            inner: resp.into_body(),
            guard: Some(ConnGuard {
                runtime: runtime.clone(),
                sender: None,
                conn_task: Some(conn_task),
                permit: None,
                pool: true,
                armed: true,
            }),
        };
        // 模拟 hyper 服务端：逐 frame poll，`is_end_stream()` 一旦为 true 就
        // 停手（真实服务端不会再多 poll 一次去拿 `Ready(None)`）。
        let mut got = Vec::new();
        loop {
            let frame =
                std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await;
            match frame {
                Some(Ok(f)) => {
                    if let Some(d) = f.data_ref() {
                        got.extend_from_slice(d.as_ref());
                    }
                }
                Some(Err(e)) => panic!("响应体出错: {e}"),
                None => break,
            }
            if body.is_end_stream() {
                break;
            }
        }
        assert_eq!(got, b"hello");
        assert!(
            body.guard.is_none(),
            "定长 body 读完后 guard 必须已被收走（否则连接会被 abort，连接池失效）"
        );
    }
}
