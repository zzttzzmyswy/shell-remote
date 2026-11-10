//! Agent 侧 LAN 直连桌面流（阶段2 基础）：本地 HTTP server 暴露与 relay 同构的
//! `GET /agent/desktop/stream` 端点，同局域网浏览器直连 agent 拉桌面流、绕开中转。
//!
//! 复用 relay 的 [`crate::relay::desktop::DesktopStream`] fan-out（纯 tokio 结构：
//! init 缓存 + viewer set，无 relay 特有依赖）；桌面流字节来自 `run_desktop_loop`
//! 的 `post_fn` 镜像（Task 4，与 DataChannel 镜像同源）。编码/relay POST 零改动。
//!
//! 生命周期：`agent::run_session` 仅在 `--desktop-lan-port N`（N != 0）时 spawn 本
//! 服务；会话结束 drop 时中止 server/feed 任务（端口释放，重连可重新 bind）。
//! 默认 0 = 不开任何端口（零对外暴露）。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use std::convert::Infallible;

use crate::relay::desktop::{DesktopStream, ViewerGuard};

/// 当前会话的有效 token 集合（rw / ro 全量）。由 `agent::mod` 在每次注册
/// 成功 / 重连复用 / 刷新后**整体覆盖**写入；LAN 处理器只读取它做鉴权。
///
/// 同步 `RwLock`：处理器在 `await` 之前一次性读完（不跨 await 持锁），
/// 写入方也只是短暂持锁替换，没必要为了两三个字符串引入 tokio 的异步锁。
pub type LanTokens = Arc<RwLock<Vec<String>>>;

/// LAN 流端点的 axum 共享状态：fan-out + 有效 token 集合 + CORS 允许来源。
#[derive(Clone)]
struct LanState {
    stream: DesktopStream,
    /// 有效会话 token 集合（rw + ro）。缺失/为空时**一切请求都拒绝**
    /// （fail-closed：注册成功之前没有任何合法 token）。
    tokens: LanTokens,
    /// LAN 流响应 `Access-Control-Allow-Origin` 值（Task 6 复审收窄：relay
    /// 同源 `scheme://host[:port]`；relay_url 解析失败时拒绝启动，见
    /// [`LanDesktop::spawn`]——fail-closed，不放行 `*`）。
    cors_origin: Arc<String>,
}

/// Agent 侧 LAN 桌面流：本地 HTTP server + [`DesktopStream`] fan-out 投递口。
///
/// - `spawn(port)` 绑定 `0.0.0.0:port`（`port=0` 时 OS 分配随机端口）；
/// - `feed` 把 post_fn 镜像的 fMP4 字节喂进 fan-out（init→`set_init`，
///   frag→`push_frag`，丢旧保新）；
/// - `addr_report()` 给出同局域网浏览器直连地址（Task 5 下发给浏览器）。
pub struct LanDesktop {
    /// 实际绑定端口（`spawn(0)` 时为 OS 分配随机端口）。
    port: u16,
    /// 对外广播的 IP（复用 `p2p::pick_advertised_ip`：UDP connect 出口 IP，
    /// 失败回退 127.0.0.1）。HTTP 监听绑 0.0.0.0，浏览器用 `bind_addr:port` 连接。
    bind_addr: IpAddr,
    /// 投递口（post_fn → feed task）：有界 64 帧，满则丢旧（LAN 丢旧保新；
    /// 与 relay fan-out 的 viewer 缓冲同一语义）。
    feed_tx: tokio::sync::mpsc::Sender<(bool, bool, Vec<u8>)>,
    /// LAN 流响应的 CORS `Access-Control-Allow-Origin` 值（Task 6 收窄；见
    /// [`LanState`]。恒为具体 relay 同源，**不可能**是 `"*"`——origin 解析
    /// 失败时 [`Self::spawn`] 直接拒绝启动，fail-closed）。
    allowed_origin: String,
    /// 直连 fan-out（与 feed task 共享同一 Inner）：用于重建时清空缓存 init。
    stream: DesktopStream,
    /// server 任务句柄：drop 时 abort，释放端口供重连重新 bind。
    _server: tokio::task::JoinHandle<()>,
    /// feed 消费任务句柄（drop 时 abort；正常路径 feed_tx 断开后自然退出）。
    _feed: tokio::task::JoinHandle<()>,
}

impl LanDesktop {
    /// 起本地 HTTP server（绑 `0.0.0.0:port`；`port=0` 随机端口），返回
    /// 已就绪的 [`LanDesktop`]（含实际端口/广播地址）。
    ///
    /// `relay_origin` 是 relay 的 `scheme://host[:port]`（LAN 页面从该源加载，
    /// 浏览器到 agent LAN 端点是跨源 fetch，CORS 须放行此源才能读流）。为
    /// `None` 时（relay_url 解析失败）**拒绝启动**（fail-closed，final-review
    /// #4）：LAN server 属显式 opt-in，relay_url 解析失败时 serve `*` 是把
    /// 无认证的桌面流端点开放给任意网页的唯一泄漏面，宁可不启。生产 relay_url
    /// 恒为 `http(s)://host[:port]`，本路径常温冷。
    ///
    /// `tokens` 是**共享的**有效会话 token 集合（rw + ro）：调用方在每次注册
    /// 成功 / 重连复用缓存 / 刷新 token 后整体覆盖写入，刷新后旧 token 立即
    /// 失效（这是"吊销即时生效"的唯一保证点）。集合为空时一切请求 401。
    pub async fn spawn(
        port: u16,
        relay_origin: Option<String>,
        tokens: LanTokens,
    ) -> std::io::Result<Self> {
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
        let local_port = listener.local_addr()?.port();

        let Some(cors_origin) = relay_origin else {
            drop(listener);
            tracing::error!(
                "LAN CORS fail-closed: relay_url 无法解析出可靠 origin，拒绝启动 LAN 桌面流服务（不放行 Access-Control-Allow-Origin: *）"
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "lan desktop server refused: no relay origin (fail-closed)",
            ));
        };
        let stream = DesktopStream::new();
        let state = LanState {
            stream: stream.clone(),
            tokens: tokens.clone(),
            cors_origin: Arc::new(cors_origin.clone()),
        };

        // feed 投递口：post_fn（同步闭包）只 try_send，后台 task 串行执行
        // set_init / push_frag（它们都是 async）。有界 64 帧防无限积压，
        // 满了丢最旧（LAN viewer 由浏览器 reqkey/重连兜底）。
        let (feed_tx, feed_rx) = tokio::sync::mpsc::channel::<(bool, bool, Vec<u8>)>(64);
        let consumer = stream.clone();
        let _feed = tokio::spawn(async move {
            let mut rx = feed_rx;
            while let Some((is_init, is_key, bytes)) = rx.recv().await {
                if is_init {
                    consumer.set_init(bytes).await;
                } else {
                    consumer.push_frag(is_key, bytes).await;
                }
            }
        });

        // LAN 直连是"本机网段直连"：端点绑 0.0.0.0，同网段任何主机都能访问，
        // 因此必须校验会话 token（见 `lan_stream_handler` 的 401 分支）。token
        // 集合由 `agent::mod` 在注册/刷新时写入；为空时一律拒绝。服务本身由
        // 显式 --desktop-lan-port N 开启才有；默认 0 不 spawn（零暴露）。
        let router = axum::Router::new()
            .route(
                "/agent/desktop/stream",
                axum::routing::get(lan_stream_handler).options(cors_preflight),
            )
            .with_state(state);
        let _server = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::warn!("LAN desktop stream server error: {e}");
            }
        });

        let bind_addr = IpAddr::V4(crate::agent::p2p::pick_advertised_ip());
        Ok(Self {
            port: local_port,
            bind_addr,
            feed_tx,
            allowed_origin: cors_origin,
            stream,
            _server,
            _feed,
        })
    }

    /// 清空缓存的 init（MYS-954 重建竞态修复）：桌面流重建（切 codec / 切
    /// 灰度 / 选屏）后调用，让新 viewer 的 `add_viewer` 不再拿到旧 codec 的
    /// 缓存 init，改走 `wait_first_init` 等新 init —— 否则旧 init + 新格式帧
    /// 不匹配，浏览器 AV1 解码偶发异常（切灰度/切编码可复现）。
    pub async fn clear_init(&self) {
        self.stream.clear_init().await;
    }

    /// 当前 LAN 流 CORS `Access-Control-Allow-Origin` 值（日志/复审观测用；
    /// 恒为具体 relay 同源，绝不可能是 `"*"`——fail-closed）。
    pub(crate) fn cors_origin(&self) -> &str {
        &self.allowed_origin
    }

    /// 测试用：实际绑定端口（浏览器/测试统一走 `127.0.0.1:<port>` 或
    /// `bind_addr:<port>` 连接）。
    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 上报字符串（`bind_addr:port`，Task 5 下发给浏览器同网段探测用）。
    pub fn addr_report(&self) -> String {
        format!("{}:{}", self.bind_addr, self.port)
    }

    /// 投递一帧 fMP4 字节（post_fn 镜像，同步口）：
    /// - `is_init` → `set_init`（缓存 + 广播给现有 viewer；新 viewer 回放）；
    /// - 否则 → `push_frag(is_key, ...)`（非关键帧等首个关键帧后放行）。
    pub fn feed(&self, is_init: bool, is_key: bool, bytes: Vec<u8>) {
        let _ = self.feed_tx.try_send((is_init, is_key, bytes));
    }
}

/// 从 relay_url（如 `http://127.0.0.1:3130`、`https://sshx.zztweb.top`）提取
/// CORS 允许来源 `scheme://host[:port]`（浏览器 Origin 的形态）。无 `://` 或
/// host 为空 → `None`（调用方 [`LanDesktop::spawn`] 拒绝启动——fail-closed）。
pub(crate) fn relay_origin(relay_url: &str) -> Option<String> {
    let trimmed = relay_url.trim();
    let (scheme, rest) = trimmed.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        return None;
    }
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    ))
}

impl Drop for LanDesktop {
    fn drop(&mut self) {
        self._server.abort();
        self._feed.abort();
    }
}

/// LAN 直连流端点：**必须**带本会话的有效 token（`?token=`，或
/// `Authorization: Bearer`），否则 401。其余与 relay 的 `stream_handler` 同构
/// ——首块 init（无则等至多 10s），随后 frag，12s 无字节主动收尾，viewer 断开
/// 经 [`ViewerGuard`] 清理。
///
/// 鉴权放在 `add_viewer` **之前**：未授权请求不得占用 viewer 槽位（否则同网段
/// 任何人都能靠刷请求把 fan-out 打满 / 挤掉合法 viewer）。比较走
/// [`crate::relay::auth::constant_time_eq`]（先各自 SHA-256 再逐字节异或累积，
/// 长度与内容都不产生可观测时序差）。
async fn lan_stream_handler(
    State(st): State<LanState>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !lan_token_ok(&st, &headers, params.get("token")) {
        // 401 也带 CORS 头：浏览器要能读到状态码才好在 LAN → P2P → relay 之间
        // 回退（缺 ACAO 时 fetch 直接变成网络错误，状态码不可见）。
        // **不打印 token**（连前缀都不打）：日志会进 systemd journal / 日志文件。
        tracing::debug!("LAN desktop stream rejected: missing or invalid token");
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-store")
            .header("Access-Control-Allow-Origin", st.cors_origin.as_str())
            .header("Vary", "Origin")
            .body(Body::from("Missing or invalid token"))
            .unwrap();
    }
    let ds = st.stream;
    let (vid, mut rx, init) = ds.add_viewer().await;

    let stream = async_stream::stream! {
        let guard = ViewerGuard {
            stream: ds.clone(),
            id: vid.clone(),
        };
        let init = match init {
            Some(i) => Some(i),
            None => ds.wait_first_init(Duration::from_secs(10)).await,
        };
        let Some(init) = init else {
            drop(guard);
            return;
        };
        yield Ok::<_, Infallible>(Bytes::from(init));
        // 空闲超时：健康流每秒都有帧（静止桌面最差 4.5s 一个 IDR），12s 无
        // 字节说明 agent 侧已停/崩溃，主动收尾让浏览器 fetch 结束、viewer 被
        // 清理（对齐 relay stream_handler 语义，MYS-886）。
        while let Ok(Some(chunk)) = tokio::time::timeout(Duration::from_secs(12), rx.recv()).await {
            yield Ok::<_, Infallible>(Bytes::from(chunk));
        }
        drop(guard);
    };

    let body = Body::from_stream(stream);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        // CORS（Task 6 复审收窄）：浏览器页面由 relay 站点加载（如
        // http://127.0.0.1:3120），LAN 直连指向 agent 本地 http://<lan-ip>:port
        // 是跨源 fetch——必须放行 relay 同源才能读到流 body。**只放行 relay 同源**
        // （origin 解析失败时服务根本不启动，fail-closed），陌生来源页面被浏览器
        // CORS 拦截（ACAO 不匹配），降低"任意网页嗅探内网桌面流"的风险面。
        // `Vary: Origin`（final-review MINOR）：ACAO 随请求 Origin 变（缓存是按
        // Origin 分桶的），显式声明避免中间缓存把某来源的响应复用得给别家。
        .header("Access-Control-Allow-Origin", st.cors_origin.as_str())
        .header("Vary", "Origin")
        .body(body)
        .unwrap()
}

/// 校验 LAN 请求携带的 token 是否是本会话当前有效的 token（rw 或 ro）。
///
/// - `?token=` 优先（浏览器走这条，避免自定义头的 CORS 预检），其次
///   `Authorization: Bearer <t>`（curl / 脚本友好）；
/// - 两端都**不**打印 token：日志会落 journal / 文件；
/// - 集合为空 → 恒 false（注册成功前 fail-closed）；
/// - 比较用 [`crate::relay::auth::constant_time_eq`]：它先对两侧做 SHA-256
///   再逐字节异或累积，长度差异不产生可观测时序差（列表长度本身会泄漏
///   "有几个 token"，那是固定 1~2 且与会话权限相关，非秘密）。
fn lan_token_ok(
    st: &LanState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&String>,
) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
        })
        .filter(|t| !t.is_empty());
    let supplied = query_token
        .map(String::as_str)
        .filter(|t| !t.is_empty())
        .or(bearer);
    let Some(supplied) = supplied else {
        return false;
    };
    // 锁只在这一小段持有：不跨 await，也不在比较期间阻塞写入方太久。
    let Ok(valid) = st.tokens.read() else {
        // 锁中毒（写入方 panic）→ 保守拒绝。
        return false;
    };
    valid
        .iter()
        .any(|t| crate::relay::auth::constant_time_eq(t, supplied))
}

/// CORS 预检：GET+Accept 属 simple request 通常不触发预检；兜底处理 OPTIONS，
/// 回同样的 ACAO（与 GET 一致，收窄为 relay 同源），避免探测请求加自定义头
/// 时被预检挡住。**预检不做鉴权**（不带凭据是预检的本性，且它不产生任何字节
/// ——viewer 槽位由 GET 的鉴权守住）。
async fn cors_preflight(State(st): State<LanState>) -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("Access-Control-Allow-Origin", st.cors_origin.as_str())
        .header("Vary", "Origin")
        .header("Access-Control-Allow-Methods", "GET, OPTIONS")
        .header("Access-Control-Allow-Headers", "*")
        .header("Access-Control-Max-Age", "86400")
        .body(Body::empty())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// 测试用固定 token 集合（rw + ro）：LAN 端点现在要求鉴权，所有既有
    /// 用例都要带上其中之一。
    const TEST_RW: &str = "tok-rw-0123456789";
    const TEST_RO: &str = "tok-ro-0123456789";

    fn test_tokens() -> LanTokens {
        Arc::new(RwLock::new(vec![TEST_RW.to_string(), TEST_RO.to_string()]))
    }

    /// 带 token 的 spawn（等价于生产路径：注册完成后集合已就绪）。
    async fn spawn_with_tokens(origin: &str) -> LanDesktop {
        LanDesktop::spawn(0, Some(origin.to_string()), test_tokens())
            .await
            .unwrap()
    }

    /// relay_url → CORS origin 提取：常规 http/https、大写 scheme、带路径、
    /// 解析失败（None）。
    #[test]
    fn test_relay_origin_extraction() {
        assert_eq!(relay_origin("http://127.0.0.1:3130"), Some("http://127.0.0.1:3130".into()));
        assert_eq!(
            relay_origin("https://sshx.zztweb.top"),
            Some("https://sshx.zztweb.top".into())
        );
        // scheme/host 大小写归一（浏览器 Origin 恒小写）。
        assert_eq!(
            relay_origin("HTTP://Relay.Example.COM:8080/path?x=1"),
            Some("http://relay.example.com:8080".into())
        );
        assert_eq!(relay_origin("localhost:3130"), None); // 缺 scheme
        assert_eq!(relay_origin(""), None);
    }

    /// CORS 收窄：spawn 传入 relay 同源 → GET 响应与 OPTIONS 预检的
    /// `Access-Control-Allow-Origin` 都等于该源（不再是 `*`），且带
    /// `Vary: Origin`。
    #[tokio::test]
    async fn lan_cors_narrowed_to_relay_origin() {
        let lan = spawn_with_tokens("http://127.0.0.1:3130").await;
        lan.feed(true, true, b"ftyp-cors-ok".to_vec());
        tokio::time::sleep(Duration::from_millis(120)).await;

        let body = http_get_until(&lan, &rw_query(), b"ftyp-cors-ok").await;
        let head = String::from_utf8_lossy(&body);
        assert!(head.starts_with("HTTP/1.1 200"), "got {head}");
        assert!(
            head.contains("access-control-allow-origin: http://127.0.0.1:3130"),
            "ACAO must be narrowed to relay origin, got: {head}"
        );
        assert!(
            !head.contains("*"),
            "wildcard ACAO must not appear, got: {head}"
        );
        assert!(
            head.to_ascii_lowercase().contains("vary: origin"),
            "Vary: Origin must be present when ACAO is a fixed origin, got: {head}"
        );
    }

    /// CORS **失败关闭**（final-review #4）：relay origin 解析失败 → spawn 拒绝
    /// 启动（返回 Err），绝不 serve `Access-Control-Allow-Origin: *`。
    #[tokio::test]
    async fn lan_cors_fails_closed_when_no_origin() {
        let err = match LanDesktop::spawn(0, None, test_tokens()).await {
            Ok(_) => panic!(
                "spawn(relay_origin=None) must refuse to start the LAN server (fail-closed)"
            ),
            Err(e) => e,
        };
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "refusal must be a clean InvalidInput error, got {err:?}"
        );
    }

    /// OPTIONS 预检回同样的收窄 ACAO + `Vary: Origin`。
    #[tokio::test]
    async fn lan_cors_preflight_echoes_relay_origin() {
        let lan = spawn_with_tokens("https://relay.example.com").await;
        use tokio::io::AsyncWriteExt;
        let mut sock = tokio::net::TcpStream::connect(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            lan.port(),
        ))
        .await
        .unwrap();
        sock.write_all(
            b"OPTIONS /agent/desktop/stream HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        let resp = http_read_until(&mut sock, b"86400").await;
        let head = String::from_utf8_lossy(&resp);
        assert!(
            head.contains("access-control-allow-origin: https://relay.example.com"),
            "OPTIONS must echo the narrowed origin, got: {head}"
        );
        assert!(
            head.to_ascii_lowercase().contains("vary: origin"),
            "OPTIONS must also carry Vary: Origin, got: {head}"
        );
    }

    /// in-process fan-out：spawn(0) → feed init（缓存）→ HTTP viewer 加入
    /// 首块 = init → 再 feed key frag + delta frag → 同一连接响应体按顺序
    /// 收到 key、delta（fMP4 先 init 后 frag；frag 只投给**已在线** viewer，
    /// 迟到 viewer 只回放 init——与 relay fan-out 语义一致）。
    #[tokio::test]
    async fn lan_fanout_init_then_frags_to_viewer() {
        let lan = spawn_with_tokens("http://127.0.0.1:3130").await;
        lan.feed(true, true, b"ftyp-moov-init-seg".to_vec());
        tokio::time::sleep(Duration::from_millis(120)).await;

        // viewer 在线后首块必须是缓存 init。
        let mut sock = http_connect(&lan, &rw_query()).await;
        let first = http_read_until(&mut sock, b"ftyp-moov-init-seg").await;
        assert!(
            first.starts_with(b"HTTP/1.1 200"),
            "expected 200, got: {}",
            String::from_utf8_lossy(&first)
        );

        // 再推 frags：实时放行，key 先于 delta。
        lan.feed(false, true, b"frag-key-1".to_vec());
        lan.feed(false, false, b"frag-delta-2".to_vec());
        let rest = http_read_until(&mut sock, b"frag-delta-2").await;
        let key: &[u8] = b"frag-key-1";
        let delta: &[u8] = b"frag-delta-2";
        let pos_key = rest
            .windows(key.len())
            .position(|w| w == key)
            .expect("key frag in body");
        let pos_delta = rest
            .windows(delta.len())
            .position(|w| w == delta)
            .expect("delta frag in body");
        assert!(
            pos_key < pos_delta,
            "frag order must be key(|{pos_key}) < delta(|{pos_delta})"
        );
    }

    /// 无 viewer 时 feed（init/key/delta）不得 panic，且不破坏 fan-out：
    /// 之后加入的 viewer 仍拿到缓存 init。
    #[tokio::test]
    async fn lan_feed_with_no_viewer_does_not_panic() {
        let lan = spawn_with_tokens("http://127.0.0.1:3130").await;
        lan.feed(true, true, b"init-no-viewer".to_vec());
        lan.feed(false, true, b"key".to_vec());
        lan.feed(false, false, b"delta".to_vec());
        // 让 feed task 全部消费完（若 set_init/push_frag 对空 viewer 集
        // panic，此处 sleep 后测试即失败）。
        tokio::time::sleep(Duration::from_millis(150)).await;

        let body = http_get_until(&lan, &rw_query(), b"init-no-viewer").await;
        assert!(
            body.windows(14).any(|w| w == b"init-no-viewer"),
            "cached init must still be replayed to a later viewer"
        );
    }

    /// HTTP 冒烟（curl 等价，不引入 http_client 依赖）：裸 TcpStream 发
    /// 最小 HTTP GET，响应必含缓存的 init 字节（fMP4 首段）。
    #[tokio::test]
    async fn http_get_stream_returns_init_bytes() {
        let lan = spawn_with_tokens("http://127.0.0.1:3130").await;
        lan.feed(true, true, b"ftypmdat-lan-init".to_vec());
        tokio::time::sleep(Duration::from_millis(120)).await;

        let body = http_get_until(&lan, &rw_query(), b"ftypmdat-lan-init").await;
        let head = String::from_utf8_lossy(&body);
        assert!(head.starts_with("HTTP/1.1 200"), "expected 200, got: {head}");
        assert!(
            head.contains("ftypmdat-lan-init"),
            "response body must carry the cached init segment"
        );
    }

    /// 带 token 的 GET 查询串（浏览器路径：token 走查询参数，无 CORS 预检）。
    fn rw_query() -> String {
        format!("?token={}", TEST_RW)
    }

    /// 建立到 LAN stream 端的裸 HTTP GET 连接，`query` 形如 `"?token=..."`
    /// 或空串（不带 token → 预期 401）。
    async fn http_connect(lan: &LanDesktop, query: &str) -> tokio::net::TcpStream {
        http_connect_with(lan, query, &[]).await
    }

    /// 同上，但可附加额外请求头（`("authorization", "Bearer ...")`）。
    async fn http_connect_with(
        lan: &LanDesktop,
        query: &str,
        extra_headers: &[(&str, &str)],
    ) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt;
        let mut sock = tokio::net::TcpStream::connect(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            lan.port(),
        ))
        .await
        .expect("connect to LAN stream server");
        let mut req = format!(
            "GET /agent/desktop/stream{query} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n"
        );
        for (k, v) in extra_headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        sock.write_all(req.as_bytes()).await.unwrap();
        sock
    }

    /// 从已连接的 socket 读响应字节，直到出现 `needle`（或读超时/EOF）。
    async fn http_read_until(sock: &mut tokio::net::TcpStream, needle: &[u8]) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if buf.windows(needle.len()).any(|w| w == needle) {
                break;
            }
            let n = tokio::time::timeout(Duration::from_secs(3), sock.read(&mut chunk))
                .await
                .expect("read response timed out")
                .expect("read error");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        buf
    }

    /// 最小 HTTP GET helper：连接 → 读到 `needle` 出现。
    async fn http_get_until(lan: &LanDesktop, query: &str, needle: &[u8]) -> Vec<u8> {
        let mut sock = http_connect(lan, query).await;
        http_read_until(&mut sock, needle).await
    }

    /// 读到 EOF 的全部响应字节（401 之类无响应体的短响应：`Connection: close`
    /// 后 server 主动关连接，读到 0 字节即结束）。
    async fn http_read_all(sock: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(3), sock.read(&mut chunk))
                .await
                .expect("read response timed out")
                .expect("read error");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        buf
    }

    /// GET 一次并读完整响应（401 用例用：无 body，连接会被 server 关闭）。
    async fn http_get_all(lan: &LanDesktop, query: &str, extra: &[(&str, &str)]) -> String {
        let mut sock = http_connect_with(lan, query, extra).await;
        String::from_utf8_lossy(&http_read_all(&mut sock).await).into_owned()
    }

    /// 未授权请求后 viewer 数必须不变（鉴权在 `add_viewer` 之前）。
    async fn viewer_count(lan: &LanDesktop) -> usize {
        lan.stream.stats().await.viewer_count
    }

    // ── LAN token 鉴权（MYS-2765 #5）────────────────────────────

    /// 无 token → 401，且带 CORS 头（浏览器要能读到状态码才好在
    /// LAN → P2P → relay 之间回退）。
    #[tokio::test]
    async fn lan_stream_without_token_is_401() {
        let lan = spawn_with_tokens("http://127.0.0.1:3130").await;
        lan.feed(true, true, b"ftyp-init".to_vec());
        tokio::time::sleep(Duration::from_millis(120)).await;

        let resp = http_get_all(&lan, "", &[]).await;
        assert!(
            resp.starts_with("HTTP/1.1 401"),
            "missing token must be 401, got: {resp}"
        );
        assert!(
            !resp.contains("ftyp-init"),
            "401 must not leak any stream bytes, got: {resp}"
        );
        assert!(
            resp.contains("access-control-allow-origin: http://127.0.0.1:3130"),
            "401 must still carry ACAO so the browser can read the status, got: {resp}"
        );
        assert_eq!(
            viewer_count(&lan).await,
            0,
            "an unauthenticated request must not occupy a viewer slot"
        );
    }

    /// 错误 token → 401（且不占 viewer 槽位）。
    #[tokio::test]
    async fn lan_stream_with_wrong_token_is_401() {
        let lan = spawn_with_tokens("http://127.0.0.1:3130").await;
        lan.feed(true, true, b"ftyp-init".to_vec());
        tokio::time::sleep(Duration::from_millis(120)).await;

        let resp = http_get_all(&lan, "?token=not-a-real-token", &[]).await;
        assert!(
            resp.starts_with("HTTP/1.1 401"),
            "wrong token must be 401, got: {resp}"
        );

        // 空 token 参数等价于"没带"。
        let resp = http_get_all(&lan, "?token=", &[]).await;
        assert!(
            resp.starts_with("HTTP/1.1 401"),
            "empty token must be 401, got: {resp}"
        );

        // Authorization: Bearer 带错误 token 同样拒绝。
        let resp = http_get_all(&lan, "", &[("authorization", "Bearer nope")]).await;
        assert!(
            resp.starts_with("HTTP/1.1 401"),
            "wrong bearer token must be 401, got: {resp}"
        );
        assert_eq!(viewer_count(&lan).await, 0);
    }

    /// 正确 rw 或 ro token → 200 并收到 init（两种权限都能看桌面流，
    /// 与 relay 侧一致：ro 只读仍可观看）。
    #[tokio::test]
    async fn lan_stream_accepts_rw_and_ro_token() {
        let lan = spawn_with_tokens("http://127.0.0.1:3130").await;
        lan.feed(true, true, b"ftyp-rw-ro-init".to_vec());
        tokio::time::sleep(Duration::from_millis(120)).await;

        for token in [TEST_RW, TEST_RO] {
            let query = format!("?token={token}");
            let body = http_get_until(&lan, &query, b"ftyp-rw-ro-init").await;
            let head = String::from_utf8_lossy(&body);
            assert!(
                head.starts_with("HTTP/1.1 200"),
                "token {token:?} must be accepted, got: {head}"
            );
            assert!(
                head.contains("ftyp-rw-ro-init"),
                "accepted viewer must receive the cached init for token {token:?}"
            );
        }
    }

    /// `Authorization: Bearer` 也可用（curl / 脚本友好；浏览器仍走查询参数）。
    #[tokio::test]
    async fn lan_stream_accepts_bearer_header() {
        let lan = spawn_with_tokens("http://127.0.0.1:3130").await;
        lan.feed(true, true, b"ftyp-bearer-init".to_vec());
        tokio::time::sleep(Duration::from_millis(120)).await;

        let mut sock =
            http_connect_with(&lan, "", &[("authorization", "Bearer tok-ro-0123456789")]).await;
        let body = http_read_until(&mut sock, b"ftyp-bearer-init").await;
        let head = String::from_utf8_lossy(&body);
        assert!(head.starts_with("HTTP/1.1 200"), "got: {head}");
    }

    /// 刷新 token：集合被整体覆盖后，旧 token **立即** 401，新 token 200。
    /// 这是"吊销即时生效"的核心断言（浏览器 LAN 拉流不能靠旧 token 续命）。
    #[tokio::test]
    async fn lan_stream_old_token_rejected_after_refresh() {
        let tokens = test_tokens();
        let lan = LanDesktop::spawn(0, Some("http://127.0.0.1:3130".into()), tokens.clone())
            .await
            .unwrap();
        lan.feed(true, true, b"ftyp-refresh-init".to_vec());
        tokio::time::sleep(Duration::from_millis(120)).await;

        // 刷新前：旧 token 可用。
        let body = http_get_until(&lan, &rw_query(), b"ftyp-refresh-init").await;
        assert!(body.starts_with(b"HTTP/1.1 200"));

        // agent 侧刷新：整体覆盖集合（旧 token 从集合里消失）。
        *tokens.write().unwrap() = vec!["tok-rw-after-refresh".to_string()];

        let resp = http_get_all(&lan, &rw_query(), &[]).await;
        assert!(
            resp.starts_with("HTTP/1.1 401"),
            "the old token must be rejected immediately after a refresh, got: {resp}"
        );
        let body = http_get_until(&lan, "?token=tok-rw-after-refresh", b"ftyp-refresh-init").await;
        assert!(
            body.starts_with(b"HTTP/1.1 200"),
            "the new token must be accepted after a refresh"
        );
    }

    /// token 集合为空（注册成功前）→ 一切请求 401（fail-closed），
    /// 包含带 Authorization 头的请求。
    #[tokio::test]
    async fn lan_stream_empty_token_set_fails_closed() {
        let empty: LanTokens = Arc::new(RwLock::new(Vec::new()));
        let lan = LanDesktop::spawn(0, Some("http://127.0.0.1:3130".into()), empty)
            .await
            .unwrap();
        lan.feed(true, true, b"ftyp-init".to_vec());
        tokio::time::sleep(Duration::from_millis(120)).await;

        for (query, extra) in [
            ("?token=anything", &[][..]),
            ("", &[("authorization", "Bearer anything")][..]),
        ] {
            let resp = http_get_all(&lan, query, extra).await;
            assert!(
                resp.starts_with("HTTP/1.1 401"),
                "an empty token set must reject everything, got: {resp}"
            );
        }
        assert_eq!(viewer_count(&lan).await, 0);
    }

    /// 未授权请求不得占用 viewer 槽位：连打若干次 401 之后，带正确 token 的
    /// viewer 仍是第一个（viewer 计数从 0 → 1）。
    #[tokio::test]
    async fn lan_unauthorized_requests_do_not_take_viewer_slots() {
        let lan = spawn_with_tokens("http://127.0.0.1:3130").await;
        lan.feed(true, true, b"ftyp-slot-init".to_vec());
        tokio::time::sleep(Duration::from_millis(120)).await;

        for _ in 0..5 {
            let resp = http_get_all(&lan, "", &[]).await;
            assert!(resp.starts_with("HTTP/1.1 401"), "got: {resp}");
        }
        assert_eq!(
            viewer_count(&lan).await,
            0,
            "rejected requests must never reach add_viewer"
        );

        let mut sock = http_connect(&lan, &rw_query()).await;
        let body = http_read_until(&mut sock, b"ftyp-slot-init").await;
        assert!(body.starts_with(b"HTTP/1.1 200"));
        // 400ms 内 viewer 应稳定为 1（没有 5 个僵尸槽位残留）。
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            viewer_count(&lan).await,
            1,
            "only the authenticated viewer may hold a slot"
        );
        drop(sock);
    }
}
