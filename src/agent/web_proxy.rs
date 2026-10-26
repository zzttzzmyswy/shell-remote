//! agent 侧 Web 网页代理（MYS-2383 阶段 1）。
//!
//! relay 通过 bulk 下行通道下发 `web:request`，本模块把一次 `GET` 抓取变成
//! 有界、可随时取消的流式回包：`web:head` → 0..N 个 `web:chunk` → `web:end`，
//! 任一失败只回一个终态 `web:error`。回包走既有的 control 上行（与
//! `mcp:result` / `fs:result` 同一条通道），因此不需要新增任何传输设施。
//!
//! 设计要点：
//! - **目标地址是远端设备自己的网络**：`127.0.0.1`、RFC1918、链路本地地址
//!   全部合法 —— 访问设备本机页面正是这个功能的目的，这里不做 SSRF 过滤。
//!   也不对 URL 做任何解析后重写，原样交给 reqwest。
//! - **有界**：每会话并发 8、块 ≤64KiB、重定向 ≤5、连接 10s、块间 idle 30s、
//!   单请求总时限 5min。
//! - **流式**：每次只持有一个网络块，绝不聚合整个响应体。
//! - **不阻塞主循环**：`handle_request` 只做校验/配额/派发，抓取在独立 task，
//!   主循环（终端输入、MCP 调用）不会因慢页面卡住。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::proto::Message;

/// 注册能力名：relay 据此判断该 agent 支持网页代理（旧 agent 无此能力，
/// Web 页面不显示入口）。
pub const CAPABILITY: &str = "web:proxy-v1";

/// 单请求的时间上限。生产值见 [`Limits::default`]；测试用小值验证"超时 →
/// 恰好一个 web:error"这条路径，而不必真的等 30s。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub idle: Duration,
    pub total: Duration,
    pub max_redirects: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            idle: IDLE_TIMEOUT,
            total: TOTAL_TIMEOUT,
            max_redirects: MAX_REDIRECTS,
        }
    }
}

/// 每会话同时在途请求上限，超出立刻回 `busy`（不排队，避免慢页面堆积）。
pub const MAX_CONCURRENT: usize = 8;

/// 单个 `web:chunk` 承载的原始字节上限（base64 前）。
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;

/// TCP/TLS 连接建立上限。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 首字节与块间空闲上限：对面不发数据超过这个时间就判定超时。
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// 单请求总时限（含慢速持续传输）。
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(300);
/// 重定向最多跟几次。
const MAX_REDIRECTS: usize = 5;
/// request_id 长度上限（协议要求非空；这里再设一个上限防止 map 被撑爆）。
const MAX_REQUEST_ID_LEN: usize = 256;

/// 允许回传给 relay 的响应头白名单（小写比较）。只保留端到端渲染所需的
/// 内容元数据；`Set-Cookie`、CORS、CSP 以及全部 hop-by-hop 头都不在内。
pub const FORWARDED_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "cache-control",
    "etag",
    "last-modified",
    "content-disposition",
];

/// `web:request` / `web:cancel` 是只读会话也允许的控制消息：页面 GET 不改
/// 远端状态（`crate::proto::requires_write` 的默认结论是"写"，这里显式放行）。
pub fn readonly_allowed(msg_type: &str) -> bool {
    matches!(msg_type, "web:request" | "web:cancel")
}

/// 取消令牌：一次请求一个。`notify_one` 会存一张许可，因此先取消后等待也
/// 不会错过（`Notify::notify_waiters` 会，故不用它）。
#[derive(Default)]
struct CancelFlag {
    cancelled: AtomicBool,
    /// 静默取消：任务不发终态消息（被同 id 的新请求顶替时用）。
    silent: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelFlag {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn cancel_silently(&self) {
        self.silent.store(true, Ordering::SeqCst);
        self.cancel();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn is_silent(&self) -> bool {
        self.silent.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

/// 回包出口：把 `Message` 序列化后推进既有的 control 上行通道。
#[derive(Clone)]
struct Out {
    tx: mpsc::Sender<String>,
    session_id: String,
}

impl Out {
    /// 返回 false = 通道已关闭（会话正在结束），调用方应停止后续发送。
    async fn send(&self, msg_type: &str, payload: Value) -> bool {
        let msg = Message {
            msg_type: msg_type.to_string(),
            session_id: self.session_id.clone(),
            payload,
        };
        match serde_json::to_string(&msg) {
            Ok(text) => self.tx.send(text).await.is_ok(),
            Err(e) => {
                tracing::warn!(msg_type, "web proxy: failed to serialize message: {e}");
                false
            }
        }
    }
}

/// 在途请求记录：取消令牌 + 任务句柄（会话结束时 abort）+ 并发票。
///
/// 并发票由这条记录持有而不是由 task 持有：记录被移除（正常完成、取消、
/// 同 id 顶替、会话结束）时票立刻归还，"当前在途数"与 map 大小因此严格
/// 一致，不会出现「已取消但仍占配额」的窗口。
struct Active {
    cancel: Arc<CancelFlag>,
    abort: tokio::task::AbortHandle,
    #[allow(dead_code)]
    permit: tokio::sync::OwnedSemaphorePermit,
}

struct Inner {
    out: Out,
    /// 会话级 HTTP client；构建失败时为 None（每个请求回 connect 错误）。
    client: Option<reqwest::Client>,
    limits: Limits,
    /// 并发配额（`MAX_CONCURRENT` 张票，try_acquire 失败即 busy）。
    permits: Arc<tokio::sync::Semaphore>,
    active: Mutex<HashMap<String, Active>>,
    /// 会话已结束：任务不再发终态消息。
    closed: AtomicBool,
}

/// 会话级网页代理句柄（Clone 只是 Arc 复制）。
#[derive(Clone)]
pub struct WebProxy {
    inner: Arc<Inner>,
}

impl WebProxy {
    /// `control_tx` 是 `run_session` 里的控制上行发送端（与 `Out::control`
    /// 同一个通道）。
    pub fn new(session_id: String, control_tx: mpsc::Sender<String>) -> Self {
        Self::with_limits(session_id, control_tx, Limits::default())
    }

    /// 同 [`WebProxy::new`]，但可指定时间上限（单测用）。
    pub fn with_limits(
        session_id: String,
        control_tx: mpsc::Sender<String>,
        limits: Limits,
    ) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(limits.max_redirects))
            // 设备内网页面不该被环境变量里的 HTTP_PROXY 接管：显式直连，
            // 保证「访问设备自身 127.0.0.1」的语义可预测。
            .no_proxy()
            .user_agent(concat!("shell-remote-agent/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| tracing::warn!("web proxy: HTTP client build failed: {e}"))
            .ok();
        Self {
            inner: Arc::new(Inner {
                out: Out {
                    tx: control_tx,
                    session_id,
                },
                client,
                limits,
                permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)),
                active: Mutex::new(HashMap::new()),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// 处理一条 `web:request`。只做校验/配额/派发，立即返回。
    pub async fn handle_request(&self, payload: &Value) {
        let request_id = payload
            .get("request_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if request_id.is_empty() || request_id.len() > MAX_REQUEST_ID_LEN {
            // 没有可用的 request_id 就无法关联回包，只能丢弃。
            tracing::warn!(
                len = request_id.len(),
                "web:request with missing/oversized request_id dropped"
            );
            return;
        }
        let rid = request_id.to_string();

        let method = payload.get("method").and_then(|v| v.as_str()).unwrap_or("");
        if !method.eq_ignore_ascii_case("GET") {
            tracing::debug!(request_id = %rid, method, "web:request rejected: unsupported method");
            self.emit_error(&rid, "unsupported_method", "only GET is supported")
                .await;
            return;
        }

        let raw_url = payload.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let url = match validate_url(raw_url) {
            Ok(u) => u,
            Err(reason) => {
                tracing::debug!(request_id = %rid, reason, "web:request rejected: invalid url");
                self.emit_error(&rid, "invalid_url", reason).await;
                return;
            }
        };

        // 同一 request_id 重复下发：按 last-wins 处理 —— 旧任务静默取消（不
        // 回终态，避免 relay 先收到 cancelled 再把新回包当成迟到数据）。
        if let Some(prev) = self.inner.active.lock().unwrap().remove(&rid) {
            tracing::warn!(request_id = %rid, "duplicate web:request id — replacing in-flight request");
            prev.cancel.cancel_silently();
            prev.abort.abort();
        }

        let permit = match self.inner.permits.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::debug!(request_id = %rid, "web:request rejected: concurrency limit");
                self.emit_error(
                    &rid,
                    "busy",
                    &format!("more than {MAX_CONCURRENT} concurrent web requests"),
                )
                .await;
                return;
            }
        };

        let cancel = Arc::new(CancelFlag::default());
        let inner = self.inner.clone();
        let rid_task = rid.clone();
        let cancel_task = cancel.clone();
        // 任务先等我方把记录写进 map：否则极快的失败路径会在插入之前跑完
        // 自清理，留下一条永不回收的陈旧记录。
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = ready_rx.await;
            run_request(inner, rid_task, url, cancel_task).await;
        });
        self.inner.active.lock().unwrap().insert(
            rid,
            Active {
                cancel,
                abort: task.abort_handle(),
                permit,
            },
        );
        let _ = ready_tx.send(());
    }

    /// 处理 `web:cancel`：取消并清理。未知 id / 重复 cancel 安全忽略。
    pub fn handle_cancel(&self, payload: &Value) {
        let request_id = payload
            .get("request_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if request_id.is_empty() {
            return;
        }
        let active = self.inner.active.lock().unwrap().remove(request_id);
        match active {
            Some(a) => {
                tracing::debug!(request_id, "web:cancel: aborting in-flight request");
                a.cancel.cancel();
            }
            None => {
                tracing::debug!(request_id, "web:cancel for unknown request ignored");
            }
        }
    }

    /// 会话结束：取消并 abort 全部在途任务，且不再发任何终态消息。
    pub fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        let drained: Vec<Active> = {
            let mut map = self.inner.active.lock().unwrap();
            map.drain().map(|(_, v)| v).collect()
        };
        for a in drained {
            a.cancel.cancel_silently();
            a.abort.abort();
        }
    }

    /// 当前在途请求数（测试与观测用）。
    pub fn active_count(&self) -> usize {
        self.inner.active.lock().unwrap().len()
    }

    async fn emit_error(&self, request_id: &str, kind: &str, message: &str) {
        let payload = json!({
            "request_id": request_id,
            "kind": kind,
            "message": message,
        });
        if !self.inner.out.send("web:error", payload).await {
            tracing::debug!(request_id, "web:error not delivered (session closing)");
        }
    }
}

/// 校验目标 URL：必须是绝对的 http/https 且有 host。私网/回环地址**合法**
/// （访问设备本机页面正是功能目标），相对地址、其他 scheme 一律拒绝。
fn validate_url(raw: &str) -> Result<reqwest::Url, &'static str> {
    if raw.is_empty() {
        return Err("url is empty");
    }
    let url = reqwest::Url::parse(raw).map_err(|_| "url is not an absolute URL")?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err("only http/https URLs are supported"),
    }
    if url.host_str().map_or(true, |h| h.is_empty()) {
        return Err("url has no host");
    }
    Ok(url)
}

/// 过滤响应头：只保留 [`FORWARDED_HEADERS`] 内的端到端头，同名重复只留第一个。
/// 值按 UTF-8 有损解码（HTTP 头允许 obs-text，不能让一个坏头毁掉整条响应）。
fn filter_headers<'a, I>(headers: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        if !FORWARDED_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if out.iter().any(|(existing, _)| *existing == name) {
            continue;
        }
        out.push((name, String::from_utf8_lossy(value).into_owned()));
    }
    out
}

/// 把一次网络读到的字节切成 ≤[`MAX_CHUNK_BYTES`] 的块并 base64 编码，
/// `seq` 从 `seq_start` 连续递增。
fn encode_chunks(seq_start: u32, bytes: &[u8]) -> Vec<(u32, String)> {
    bytes
        .chunks(MAX_CHUNK_BYTES)
        .enumerate()
        .map(|(i, part)| (seq_start + i as u32, B64.encode(part)))
        .collect()
}

/// 一次抓取的终态：只有它决定发 `web:end` 还是 `web:error`。
enum Failure {
    /// 参数/配额类失败（已在派发前发出，不会走到这里）。
    Cancelled,
    Timeout(&'static str),
    Connect(&'static str),
    Http(&'static str),
}

impl Failure {
    fn kind(&self) -> &'static str {
        match self {
            Failure::Cancelled => "cancelled",
            Failure::Timeout(_) => "timeout",
            Failure::Connect(_) => "connect",
            Failure::Http(_) => "http",
        }
    }

    fn message(&self) -> &'static str {
        match self {
            Failure::Cancelled => "request cancelled",
            Failure::Timeout(m) | Failure::Connect(m) | Failure::Http(m) => m,
        }
    }
}

/// reqwest 传输层错误分类。错误正文**不**回传给 relay（避免把 URL/凭据带进
/// 回包），只在本地日志里记录完整错误。
fn classify(e: &reqwest::Error) -> Failure {
    if e.is_timeout() {
        Failure::Timeout("request timed out")
    } else if e.is_connect() {
        Failure::Connect("failed to connect to the target")
    } else if e.is_redirect() {
        Failure::Http("too many redirects")
    } else {
        Failure::Http("request failed")
    }
}

/// 单次请求的执行体（独立 task）。负责发出 head / chunk / end / error。
async fn run_request(
    inner: Arc<Inner>,
    request_id: String,
    url: reqwest::Url,
    cancel: Arc<CancelFlag>,
) {
    let outcome = fetch_and_stream(&inner, &request_id, url, &cancel).await;
    // 自清理：只有 map 里还是自己那条记录时才移除（被同 id 新请求顶替时
    // 记录已属于新任务，不能删）。
    {
        let mut map = inner.active.lock().unwrap();
        let is_current = map
            .get(&request_id)
            .is_some_and(|cur| Arc::ptr_eq(&cur.cancel, &cancel));
        if is_current {
            map.remove(&request_id);
        }
    }
    if let Err(failure) = outcome {
        if inner.closed.load(Ordering::SeqCst) || cancel.is_silent() {
            return;
        }
        tracing::debug!(
            request_id = %request_id,
            kind = failure.kind(),
            "web request failed"
        );
        let payload = json!({
            "request_id": request_id,
            "kind": failure.kind(),
            "message": failure.message(),
        });
        let _ = inner.out.send("web:error", payload).await;
    }
}

async fn fetch_and_stream(
    inner: &Inner,
    request_id: &str,
    url: reqwest::Url,
    cancel: &CancelFlag,
) -> Result<(), Failure> {
    let client = inner
        .client
        .as_ref()
        .ok_or(Failure::Connect("web proxy HTTP client unavailable"))?;

    // 总时限从发起请求起算（含等首字节），否则"30s 首字节 + 5min 传输"会超过
    // 协议约定的单请求 5min 上限。
    let deadline = tokio::time::Instant::now() + inner.limits.total;

    // ── 等待响应头（首字节）─
    let resp = {
        let fut = client.get(url).send();
        tokio::pin!(fut);
        let wait = inner
            .limits
            .idle
            .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        tokio::select! {
            _ = cancel.cancelled() => return Err(Failure::Cancelled),
            r = tokio::time::timeout(wait, &mut fut) => match r {
                Err(_) => return Err(Failure::Timeout("timed out waiting for response headers")),
                Ok(Err(e)) => {
                    tracing::debug!(request_id, error = %e, "web proxy: request error");
                    return Err(classify(&e));
                }
                Ok(Ok(resp)) => resp,
            }
        }
    };

    let headers: Vec<Vec<String>> = filter_headers(
        resp.headers()
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_bytes())),
    )
    .into_iter()
    .map(|(name, value)| vec![name, value])
    .collect();
    let head = json!({
        "request_id": request_id,
        "status": resp.status().as_u16(),
        "headers": headers,
        "final_url": resp.url().as_str(),
    });
    if !inner.out.send("web:head", head).await {
        return Err(Failure::Cancelled);
    }

    // ── 响应体：逐块流式转发 ────────────────────────────────────────────
    let mut resp = resp;
    let mut seq: u32 = 0;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Failure::Timeout("total request time limit exceeded"));
        }
        let wait = remaining.min(inner.limits.idle);
        let next = {
            let fut = resp.chunk();
            tokio::pin!(fut);
            tokio::select! {
                _ = cancel.cancelled() => return Err(Failure::Cancelled),
                r = tokio::time::timeout(wait, &mut fut) => match r {
                    Err(_) => {
                        return Err(if remaining <= inner.limits.idle {
                            Failure::Timeout("total request time limit exceeded")
                        } else {
                            Failure::Timeout("response body idle timeout")
                        });
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(request_id, error = %e, "web proxy: body stream error");
                        return Err(classify(&e));
                    }
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(bytes))) => bytes,
                }
            }
        };
        for (chunk_seq, data) in encode_chunks(seq, &next) {
            let payload = json!({
                "request_id": request_id,
                "seq": chunk_seq,
                "data": data,
            });
            if !inner.out.send("web:chunk", payload).await {
                return Err(Failure::Cancelled);
            }
            seq = chunk_seq + 1;
        }
    }

    if !inner
        .out
        .send("web:end", json!({ "request_id": request_id }))
        .await
    {
        return Err(Failure::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use std::time::Instant;

    /// 本地临时 HTTP 服务（等同于设备侧 127.0.0.1 上的页面服务）。
    struct TestServer {
        base: String,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn serve(app: Router) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        TestServer {
            base: format!("http://127.0.0.1:{port}"),
            handle,
        }
    }

    /// 测试用代理句柄 + 回包接收端。
    fn proxy(limits: Limits) -> (WebProxy, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel::<String>(256);
        (
            WebProxy::with_limits("sess-test".to_string(), tx, limits),
            rx,
        )
    }

    fn request_id(i: usize) -> String {
        format!("00000000-0000-4000-8000-{:012}", i)
    }

    /// 收一条回包并返回 (type, payload, 收到时刻)。
    async fn recv(rx: &mut mpsc::Receiver<String>) -> (String, Value, Instant) {
        let raw = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("timed out waiting for an agent→relay message")
            .expect("control channel closed");
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["session_id"], "sess-test");
        (
            v["type"].as_str().unwrap().to_string(),
            v["payload"].clone(),
            Instant::now(),
        )
    }

    /// 收到给定 request_id 的终态（end 或 error）为止，按序返回中间消息类型。
    async fn drain_until_terminal(
        rx: &mut mpsc::Receiver<String>,
        request_id: &str,
    ) -> (Vec<(String, Value)>, String, Value) {
        let mut acc = Vec::new();
        loop {
            let (ty, payload, _) = recv(rx).await;
            assert_eq!(payload["request_id"], request_id, "unexpected request_id");
            if ty == "web:end" || ty == "web:error" {
                return (acc, ty, payload);
            }
            acc.push((ty, payload));
        }
    }

    /// 把 chunk 还原成原始字节，同时校验 seq 从 0 连续、块大小不超限。
    fn reassemble(chunks: &[(String, Value)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, (ty, payload)) in chunks.iter().enumerate() {
            assert_eq!(ty, "web:chunk");
            assert_eq!(
                payload["seq"].as_u64().unwrap(),
                i as u64,
                "chunk seq must be contiguous from 0"
            );
            let bytes = B64.decode(payload["data"].as_str().unwrap()).unwrap();
            assert!(
                bytes.len() <= MAX_CHUNK_BYTES,
                "chunk of {} bytes exceeds the {} byte limit",
                bytes.len(),
                MAX_CHUNK_BYTES
            );
            out.extend_from_slice(&bytes);
        }
        out
    }

    // ── 纯函数：URL / 方法 / 头 ──────────────────────────────────────────

    #[test]
    fn test_validate_url_accepts_http_and_https() {
        assert!(validate_url("http://127.0.0.1/").is_ok());
        assert!(validate_url("https://example.com/a?b=1").is_ok());
        // 远端设备网络地址是功能目标，不是 SSRF 漏洞：显式允许。
        for url in [
            "http://127.0.0.1:8080/",
            "http://10.0.0.5/",
            "http://172.16.3.4/",
            "http://192.168.1.1/",
            "http://169.254.10.1/",
            "http://[::1]:9000/",
            "http://localhost:3000/login",
        ] {
            assert!(validate_url(url).is_ok(), "{url} 应被允许（设备侧地址）");
        }
    }

    #[test]
    fn test_validate_url_rejects_other_schemes_and_relative() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "javascript:alert(1)",
            "data:text/html,hi",
            "ws://127.0.0.1:1/",
            "/relative/path",
            "example.com",
            "",
        ] {
            assert!(validate_url(url).is_err(), "{url} 必须被拒绝");
        }
    }

    #[test]
    fn test_filter_headers_keeps_only_end_to_end_render_headers() {
        let headers: Vec<(&str, &[u8])> = vec![
            ("Content-Type", b"text/html; charset=utf-8"),
            ("content-length", b"1234"),
            ("Cache-Control", b"no-store"),
            ("ETag", b"\"abc\""),
            ("Last-Modified", b"Wed, 21 Oct 2015 07:28:00 GMT"),
            ("Content-Disposition", b"inline"),
            // 必须剔除：
            ("Set-Cookie", b"sid=secret"),
            ("Access-Control-Allow-Origin", b"*"),
            ("Content-Security-Policy", b"default-src 'none'"),
            ("Transfer-Encoding", b"chunked"),
            ("Connection", b"keep-alive"),
            ("Keep-Alive", b"timeout=5"),
            ("Proxy-Authenticate", b"Basic"),
            ("TE", b"trailers"),
            ("Trailer", b"Expires"),
            ("Upgrade", b"h2c"),
            ("X-Powered-By", b"express"),
        ];
        let kept = filter_headers(headers);
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "content-type",
                "content-length",
                "cache-control",
                "etag",
                "last-modified",
                "content-disposition"
            ]
        );
        assert_eq!(kept[0].1, "text/html; charset=utf-8");
        // 值里的秘密绝不能顺着白名单头泄漏出去。
        assert!(!format!("{kept:?}").contains("sid=secret"));
    }

    #[test]
    fn test_filter_headers_deduplicates_and_survives_bad_utf8() {
        let kept = filter_headers(vec![
            ("content-type", b"text/plain".as_slice()),
            ("Content-Type", b"text/html".as_slice()),
            ("etag", &[0xff, 0xfe]),
        ]);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].1, "text/plain", "同名头只保留第一个");
        assert!(!kept[1].1.is_empty(), "坏 UTF-8 头应有损解码而非丢弃");
    }

    #[test]
    fn test_encode_chunks_splits_at_64k_with_contiguous_seq() {
        let data: Vec<u8> = (0..MAX_CHUNK_BYTES * 2 + 7)
            .map(|i| (i % 251) as u8)
            .collect();
        let chunks = encode_chunks(0, &data);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].0, 0);
        assert_eq!(chunks[1].0, 1);
        assert_eq!(chunks[2].0, 2);
        assert_eq!(B64.decode(&chunks[0].1).unwrap().len(), MAX_CHUNK_BYTES);
        assert_eq!(B64.decode(&chunks[2].1).unwrap().len(), 7);
        // 从任意 seq 续写时序号仍连续。
        let more = encode_chunks(7, &data[..MAX_CHUNK_BYTES + 1]);
        assert_eq!(more.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![7, 8]);
    }

    #[test]
    fn test_readonly_sessions_may_request_pages() {
        // 只读会话必须能开网页（页面 GET 不改远端状态），但写操作仍被拦。
        assert!(readonly_allowed("web:request"));
        assert!(readonly_allowed("web:cancel"));
        assert!(!readonly_allowed("fs:write"));
        assert!(!readonly_allowed("terminal:input"));
        assert!(crate::proto::requires_write("web:request"));
        assert!(crate::proto::requires_write("web:cancel"));
    }

    // ── 抓取全流程（本地 127.0.0.1 临时 HTTP 服务）───────────────────────

    #[tokio::test]
    async fn test_fetch_local_page_returns_head_chunks_end() {
        let server = serve(Router::new().route(
            "/",
            get(|| async {
                (
                    [
                        ("content-type", "text/html; charset=utf-8"),
                        ("set-cookie", "sid=should-not-leak"),
                        ("content-security-policy", "default-src 'none'"),
                        ("cache-control", "no-store"),
                    ],
                    "<html><body>设备本机页面</body></html>",
                )
                    .into_response()
            }),
        ))
        .await;
        let (proxy, mut rx) = proxy(Limits::default());
        let rid = request_id(1);
        proxy
            .handle_request(&json!({
                "request_id": rid,
                "url": format!("{}/", server.base),
                "method": "GET",
            }))
            .await;

        let (chunks, term, payload) = drain_until_terminal(&mut rx, &rid).await;
        assert_eq!(term, "web:end", "正常结束必须是 web:end: {payload}");
        // 首条必须是 head。
        let (head_ty, head) = &chunks[0];
        assert_eq!(head_ty, "web:head");
        assert_eq!(head["status"], 200);
        let headers = head["headers"].as_array().unwrap();
        let names: Vec<&str> = headers
            .iter()
            .map(|h| h.as_array().unwrap()[0].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"content-type"),
            "缺少 content-type: {names:?}"
        );
        assert!(
            names.contains(&"cache-control"),
            "缺少 cache-control: {names:?}"
        );
        assert!(
            names.iter().all(|n| FORWARDED_HEADERS.contains(n)),
            "回包里出现了非白名单头: {names:?}"
        );
        assert!(
            !head.to_string().contains("should-not-leak"),
            "Set-Cookie 绝不能出现在回包: {head}"
        );
        assert_eq!(head["final_url"], format!("{}/", server.base));

        let body = String::from_utf8(reassemble(&chunks[1..])).unwrap();
        assert_eq!(body, "<html><body>设备本机页面</body></html>");
        // 正常结束后不再有第二条终态。
        assert!(rx.try_recv().is_err(), "end 之后不得再发任何消息");
    }

    #[tokio::test]
    async fn test_fetch_large_body_streams_in_contiguous_chunks() {
        // >64KiB 必须拆成多块并可按 seq 顺序完整还原。
        let payload: Vec<u8> = (0..200 * 1024).map(|i| (i % 256) as u8).collect();
        let body = payload.clone();
        let server =
            serve(
                Router::new().route(
                    "/big.bin",
                    get(move || {
                        let body = body.clone();
                        async move {
                            ([("content-type", "application/octet-stream")], body).into_response()
                        }
                    }),
                ),
            )
            .await;
        let (proxy, mut rx) = proxy(Limits::default());
        let rid = request_id(2);
        proxy
            .handle_request(&json!({
                "request_id": rid,
                "url": format!("{}/big.bin", server.base),
                "method": "GET",
            }))
            .await;

        let (chunks, term, _) = drain_until_terminal(&mut rx, &rid).await;
        assert_eq!(term, "web:end");
        assert!(chunks.len() > 2, "200KiB 应产生多块，实际 {}", chunks.len());
        assert_eq!(
            reassemble(&chunks[1..]),
            payload,
            "字节必须逐字节一致且有序"
        );
    }

    #[tokio::test]
    async fn test_invalid_method_and_scheme_emit_single_error() {
        let (proxy, mut rx) = proxy(Limits::default());

        let rid = request_id(3);
        proxy
            .handle_request(&json!({
                "request_id": rid, "url": "http://127.0.0.1:1/", "method": "POST"
            }))
            .await;
        let (ty, payload, _) = recv(&mut rx).await;
        assert_eq!(ty, "web:error");
        assert_eq!(payload["kind"], "unsupported_method");
        assert_eq!(payload["request_id"], rid);

        let rid = request_id(4);
        proxy
            .handle_request(&json!({
                "request_id": rid, "url": "file:///etc/passwd", "method": "GET"
            }))
            .await;
        let (ty, payload, _) = recv(&mut rx).await;
        assert_eq!(ty, "web:error");
        assert_eq!(payload["kind"], "invalid_url");

        // 失败不先发 head，且只发一个终态。
        assert!(rx.try_recv().is_err(), "每次失败只允许一条 web:error");
        assert_eq!(proxy.active_count(), 0);
    }

    #[tokio::test]
    async fn test_missing_request_id_is_dropped_without_reply() {
        let (proxy, mut rx) = proxy(Limits::default());
        proxy
            .handle_request(&json!({"url": "http://127.0.0.1:1/", "method": "GET"}))
            .await;
        assert!(
            rx.try_recv().is_err(),
            "没有 request_id 时无法关联回包，应静默丢弃"
        );
    }

    #[tokio::test]
    async fn test_connect_failure_emits_connect_error() {
        // 端口 1 上没有任何监听者 → 立刻 connection refused。
        let (proxy, mut rx) = proxy(Limits::default());
        let rid = request_id(5);
        proxy
            .handle_request(&json!({
                "request_id": rid, "url": "http://127.0.0.1:1/", "method": "GET"
            }))
            .await;
        let (chunks, term, payload) = drain_until_terminal(&mut rx, &rid).await;
        assert_eq!(term, "web:error");
        assert_eq!(payload["kind"], "connect");
        assert!(chunks.is_empty(), "失败时不得先发 head");
        // 错误正文不得泄漏目标 URL（可能含内网地址/凭据）。
        assert!(!payload["message"].as_str().unwrap().contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn test_http_error_status_is_still_proxied_as_head() {
        // 4xx/5xx 是「页面存在但返回错误状态」，必须原样透传状态码而不是
        // 变成 web:error —— 浏览器要能看到目标站的错误页。
        let server = serve(Router::new().route(
            "/missing",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "nope") }),
        ))
        .await;
        let (proxy, mut rx) = proxy(Limits::default());
        let rid = request_id(6);
        proxy
            .handle_request(&json!({
                "request_id": rid,
                "url": format!("{}/missing", server.base),
                "method": "GET",
            }))
            .await;
        let (chunks, term, _) = drain_until_terminal(&mut rx, &rid).await;
        assert_eq!(term, "web:end");
        assert_eq!(chunks[0].0, "web:head");
        assert_eq!(chunks[0].1["status"], 404);
        assert_eq!(String::from_utf8(reassemble(&chunks[1..])).unwrap(), "nope");
    }

    #[tokio::test]
    async fn test_slow_first_byte_times_out_with_one_error() {
        // idle 上限压缩到 200ms 验证「超时 → 恰好一条 timeout 错误」。
        let server = serve(Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "late"
            }),
        ))
        .await;
        let (proxy, mut rx) = proxy(Limits {
            idle: Duration::from_millis(200),
            total: Duration::from_secs(30),
            max_redirects: 5,
        });
        let rid = request_id(7);
        proxy
            .handle_request(&json!({
                "request_id": rid,
                "url": format!("{}/slow", server.base),
                "method": "GET",
            }))
            .await;
        let (chunks, term, payload) = drain_until_terminal(&mut rx, &rid).await;
        assert_eq!(term, "web:error");
        assert_eq!(payload["kind"], "timeout");
        assert!(chunks.is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_cancel_stops_stream_and_is_idempotent() {
        // 持续慢速输出：取消后必须尽快停下，且只回一条 cancelled。
        let server = serve(Router::new().route(
            "/trickle",
            get(|| async {
                let chunks = futures_util::stream::unfold(0u32, |i| async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Some((Ok::<_, std::io::Error>(vec![b'x'; 1024]), i + 1))
                });
                axum::body::Body::from_stream(chunks).into_response()
            }),
        ))
        .await;
        let (proxy, mut rx) = proxy(Limits::default());
        let rid = request_id(8);
        proxy
            .handle_request(&json!({
                "request_id": rid,
                "url": format!("{}/trickle", server.base),
                "method": "GET",
            }))
            .await;
        // 收到 head + 至少一个块后再取消（确保取消发生在 body 阶段）。
        let (ty, _, _) = recv(&mut rx).await;
        assert_eq!(ty, "web:head");
        recv(&mut rx).await;
        proxy.handle_cancel(&json!({"request_id": rid}));
        // 重复 cancel / 未知 id 都必须安全忽略。
        proxy.handle_cancel(&json!({"request_id": rid}));
        proxy.handle_cancel(&json!({"request_id": "unknown-id"}));
        proxy.handle_cancel(&json!({}));

        let (chunks, term, payload) = drain_until_terminal(&mut rx, &rid).await;
        assert_eq!(term, "web:error");
        assert_eq!(payload["kind"], "cancelled");
        // 取消后不得再有任何消息（end 与 error 不可同时出现）。
        assert!(rx.try_recv().is_err(), "取消后仍收到消息: {chunks:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(proxy.active_count(), 0, "取消后必须清理 map");
    }

    #[tokio::test]
    async fn test_shutdown_cancels_everything_without_extra_messages() {
        let server = serve(Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "late"
            }),
        ))
        .await;
        let (proxy, mut rx) = proxy(Limits::default());
        for i in 0..3 {
            proxy
                .handle_request(&json!({
                    "request_id": request_id(20 + i),
                    "url": format!("{}/slow", server.base),
                    "method": "GET",
                }))
                .await;
        }
        assert_eq!(proxy.active_count(), 3);
        proxy.shutdown();
        assert_eq!(proxy.active_count(), 0, "session 退出必须清空全部任务");
        // session 已结束：任务被 abort，不再向旧上行发终态。
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(rx.try_recv().is_err(), "session 结束后不得再发回包");
    }

    #[tokio::test]
    async fn test_concurrency_limit_returns_busy() {
        let server = serve(Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "late"
            }),
        ))
        .await;
        let (proxy, mut rx) = proxy(Limits::default());
        for i in 0..MAX_CONCURRENT {
            proxy
                .handle_request(&json!({
                    "request_id": request_id(100 + i),
                    "url": format!("{}/slow", server.base),
                    "method": "GET",
                }))
                .await;
        }
        assert_eq!(proxy.active_count(), MAX_CONCURRENT);
        let rid = request_id(999);
        proxy
            .handle_request(&json!({
                "request_id": rid,
                "url": format!("{}/slow", server.base),
                "method": "GET",
            }))
            .await;
        let (ty, payload, _) = recv(&mut rx).await;
        assert_eq!(ty, "web:error");
        assert_eq!(payload["kind"], "busy");
        assert_eq!(payload["request_id"], rid);
        assert_eq!(
            proxy.active_count(),
            MAX_CONCURRENT,
            "被拒的请求不得占用配额"
        );

        // 取消一个 → 配额立即归还（对慢响应返回 503/429 的 relay 依赖它，
        // 否则一次取消就能让整个会话的网页功能永久卡在 busy）。
        proxy.handle_cancel(&json!({"request_id": request_id(100)}));
        assert_eq!(proxy.active_count(), MAX_CONCURRENT - 1);
        let rid = request_id(1000);
        proxy
            .handle_request(&json!({
                "request_id": rid,
                "url": format!("{}/slow", server.base),
                "method": "GET",
            }))
            .await;
        assert_eq!(
            proxy.active_count(),
            MAX_CONCURRENT,
            "取消后腾出的配额必须可被新请求使用"
        );
        proxy.shutdown();
    }

    #[tokio::test]
    async fn test_slow_page_does_not_block_dispatch_of_other_messages() {
        // 慢响应期间：同一句柄上的其它请求（含取消）必须立刻被受理，不被
        // 卡住 —— 主循环只做派发、抓取在独立 task。
        let server = serve(
            Router::new()
                .route(
                    "/slow",
                    get(|| async {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        "late"
                    }),
                )
                .route("/fast", get(|| async { "fast-body" })),
        )
        .await;
        let (proxy, mut rx) = proxy(Limits::default());
        let slow_id = request_id(30);
        let started = Instant::now();
        proxy
            .handle_request(&json!({
                "request_id": slow_id,
                "url": format!("{}/slow", server.base),
                "method": "GET",
            }))
            .await;
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "派发慢请求必须立即返回，实际 {:?}",
            started.elapsed()
        );

        // 慢请求在途时，第二个请求照常完成。
        let fast_id = request_id(31);
        proxy
            .handle_request(&json!({
                "request_id": fast_id,
                "url": format!("{}/fast", server.base),
                "method": "GET",
            }))
            .await;
        let (chunks, term, _) = drain_until_terminal(&mut rx, &fast_id).await;
        assert_eq!(term, "web:end");
        assert_eq!(
            String::from_utf8(reassemble(&chunks[1..])).unwrap(),
            "fast-body"
        );

        // 取消慢请求同样立即生效。
        let started = Instant::now();
        proxy.handle_cancel(&json!({"request_id": slow_id}));
        assert!(started.elapsed() < Duration::from_millis(100));
        let (_, term, payload) = drain_until_terminal(&mut rx, &slow_id).await;
        assert_eq!(term, "web:error");
        assert_eq!(payload["kind"], "cancelled");
    }

    #[tokio::test]
    async fn test_duplicate_request_id_replaces_previous_silently() {
        let server = serve(Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "late"
            }),
        ))
        .await;
        let (proxy, mut rx) = proxy(Limits::default());
        let rid = request_id(40);
        for _ in 0..2 {
            proxy
                .handle_request(&json!({
                    "request_id": rid,
                    "url": format!("{}/slow", server.base),
                    "method": "GET",
                }))
                .await;
        }
        assert_eq!(
            proxy.active_count(),
            1,
            "同一 request_id 只保留一个在途任务"
        );
        // 旧任务被静默取消（不回 cancelled），否则 relay 会把迟到终态当成
        // 新请求的结果。
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(rx.try_recv().is_err());
        proxy.shutdown();
    }
}
