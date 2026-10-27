//! DoH（RFC 8484）解析与企业级降级链路（MYS-2425 阶段2）。
//!
//! 目标网络是「中国大陆 + DNS 被污染」：直连 `github.com` 经常解析到黑洞 IP
//! 或被投毒。升级下载因此不能只依赖系统解析，链路按三级降级：
//!
//! 1. **系统解析**（`getaddrinfo`）：正常网络下唯一一次系统调用，最省事；
//! 2. **预置 DoH 提供商**（RFC 8484 wire format，POST `application/dns-message`）：
//!    国内阿里 DoH + 国外 Cloudflare DoH。预置 IP **只用于连接 DoH 服务自身**，
//!    URL 的 host、TLS SNI 与证书校验仍按 DoH 服务商自己的域名做——不做「IP
//!    直连 + 跳过证书」，那等于把 DoH 变成可被中间人顶替的明文通道；
//! 3. **relay 代解析** `/agent/resolve?host=`：relay 在它自己的网络里解析，回
//!    IP + 过期时间（rw Token 鉴权、CDN 域名白名单）。agent 侧同样只用返回的
//!    IP 建连，URL host / SNI / 证书仍是原 CDN 域名。
//!
//! 解析结果只是「连接候选 IP」：制品身份由 relay 下发的 SHA-256 决定，DNS 被
//! 污染最多让某次连接失败，不可能装错字节。
//!
//! 白名单是硬约束：只解析**本次升级任务里出现的 CDN 主机**，不让 agent 变成
//! 任意 DNS 客户端（否则它就是内网探测/SSRF 的跳板）。

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio_stream::StreamExt;

use crate::agent::download::ProxyConfig;

/// DNS 类型：A。
pub const DNS_TYPE_A: u16 = 1;
/// DNS 类型：AAAA。
pub const DNS_TYPE_AAAA: u16 = 28;
/// DNS CLASS：IN。
pub const DNS_CLASS_IN: u16 = 1;

/// 单次 DoH 响应的字节上限。正常的 A/AAAA 应答是百字节量级，8 KiB 足够并挡住
/// 异常/恶意大响应（DoH 服务商返回 HTML 错误页时也不会被当成解析结果）。
pub const DOH_MAX_BYTES: usize = 8 * 1024;
/// 单次 DoH 请求的总超时。
pub const DOH_TIMEOUT: Duration = Duration::from_secs(5);
/// 连接 DoH 服务商的超时。
pub const DOH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 系统解析的超时（`getaddrinfo` 在污染网络里没有内建超时，会挂很久）。
pub const SYSTEM_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// relay 代解析的请求超时。
pub const RELAY_RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);
/// 缓存 TTL 下限：TTL=0 表示「别缓存」，其余小 TTL 也抬到下限，避免同一个
/// 下载任务里反复解析。
pub const DNS_TTL_MIN_SECS: u32 = 30;
/// 缓存 TTL 上限：与 relay 侧 `RESOLVE_TTL_SECS` 一致，DNS 变更不会被一次
/// 解析永久封死。
pub const DNS_TTL_MAX_SECS: u32 = 300;

/// 一个预置 DoH 提供商。`bootstrap_ips` 只用于连接 DoH 自身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DohProvider {
    /// 日志/进度里显示的名字。
    pub label: String,
    /// DoH 端点完整 URL（host 决定 SNI 与证书校验目标）。
    pub url: String,
    /// 预置 IP（连接该端点用；URL 显式端口优先于这里的 443）。
    pub bootstrap_ips: Vec<IpAddr>,
}

/// 首期预置的两家 DoH：国内（阿里）+ 国外（Cloudflare）。
///
/// 顺序即优先级：国内网络先走阿里（不受跨境干扰），阿里不可用时再试
/// Cloudflare；两家都在公网、证书由公共 CA 签发，因此不需要额外的信任配置。
pub fn default_providers() -> Vec<DohProvider> {
    vec![
        DohProvider {
            label: "aliyun".to_string(),
            url: "https://dns.alidns.com/dns-query".to_string(),
            bootstrap_ips: vec![IpAddr::from([223, 5, 5, 5]), IpAddr::from([223, 6, 6, 6])],
        },
        DohProvider {
            label: "cloudflare".to_string(),
            url: "https://cloudflare-dns.com/dns-query".to_string(),
            bootstrap_ips: vec![IpAddr::from([1, 1, 1, 1]), IpAddr::from([1, 0, 0, 1])],
        },
    ]
}

/// DoH 客户端配置。
#[derive(Debug, Clone)]
pub struct DohConfig {
    pub providers: Vec<DohProvider>,
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_bytes: usize,
    /// 显式代理配置（默认从环境变量读）。
    pub proxy: ProxyConfig,
    /// 额外信任根（PEM）。仅测试注入本地 CA 用，生产恒为空。
    pub extra_roots: Vec<Vec<u8>>,
}

impl Default for DohConfig {
    fn default() -> Self {
        Self {
            providers: default_providers(),
            timeout: DOH_TIMEOUT,
            connect_timeout: DOH_CONNECT_TIMEOUT,
            max_bytes: DOH_MAX_BYTES,
            proxy: ProxyConfig::from_env(),
            extra_roots: Vec::new(),
        }
    }
}

/// 一条解析记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsAnswer {
    pub ip: IpAddr,
    pub ttl: u32,
}

/// 解析结果 + 走了哪一级（只用于日志/进度诊断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIps {
    pub addrs: Vec<IpAddr>,
    /// `system` / `doh:<label>` / `relay`。
    pub method: String,
}

/// 解析器抽象：下载器只依赖它，测试可注入静态/故障实现。
pub trait HostResolver: Send + Sync {
    fn resolve_host<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ResolvedIps, String>> + Send + 'a>>;
}

/// 系统解析抽象（测试注入用；生产走 `tokio::net::lookup_host`）。
pub trait SystemLookup: Send + Sync {
    fn lookup<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, String>> + Send + 'a>>;
}

/// 真实系统解析：`getaddrinfo` 包一层超时（污染网络里它会挂很久）。
#[derive(Debug, Clone)]
pub struct RealSystemLookup {
    pub timeout: Duration,
}

impl Default for RealSystemLookup {
    fn default() -> Self {
        Self {
            timeout: SYSTEM_LOOKUP_TIMEOUT,
        }
    }
}

impl SystemLookup for RealSystemLookup {
    fn lookup<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, String>> + Send + 'a>> {
        Box::pin(async move {
            let fut = tokio::net::lookup_host((host, 443));
            let resolved = tokio::time::timeout(self.timeout, fut)
                .await
                .map_err(|_| {
                    format!(
                        "system DNS lookup for {host} timed out after {:?}",
                        self.timeout
                    )
                })?
                .map_err(|e| format!("system DNS lookup for {host} failed: {e}"))?;
            let mut ips: Vec<IpAddr> = Vec::new();
            for addr in resolved {
                if !ips.contains(&addr.ip()) {
                    ips.push(addr.ip());
                }
            }
            if ips.is_empty() {
                return Err(format!("system DNS returned no address for {host}"));
            }
            Ok(ips)
        })
    }
}

/// DoH 查询抽象（测试注入用）。
pub trait DohLookup: Send + Sync {
    fn query<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DnsAnswer>, String>> + Send + 'a>>;
}

/// DoH 客户端（RFC 8484 wire format）。
pub struct DohClient {
    cfg: DohConfig,
}

impl DohClient {
    pub fn new(cfg: DohConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &DohConfig {
        &self.cfg
    }

    /// 依次尝试每个提供商，第一个给出有效地址的胜出。
    async fn resolve(&self, host: &str) -> Result<Vec<DnsAnswer>, String> {
        if self.cfg.providers.is_empty() {
            return Err("no DoH provider configured".to_string());
        }
        let mut failures: Vec<String> = Vec::new();
        for p in &self.cfg.providers {
            // A 优先；没有 A 才问 AAAA（纯 IPv6 主机）。两次请求只在必要时发生，
            // 不给污染网络增加无谓流量。
            match self.query_provider(p, host, DNS_TYPE_A).await {
                Ok(a) if !a.is_empty() => return Ok(a),
                Ok(_) => {}
                Err(e) => failures.push(format!("{}: {e}", p.label)),
            }
            match self.query_provider(p, host, DNS_TYPE_AAAA).await {
                Ok(a) if !a.is_empty() => return Ok(a),
                Ok(_) => failures.push(format!("{}: empty AAAA answer", p.label)),
                Err(e) => failures.push(format!("{}: {e}", p.label)),
            }
        }
        Err(format!("all DoH providers failed: {}", failures.join("; ")))
    }

    async fn query_provider(
        &self,
        provider: &DohProvider,
        host: &str,
        qtype: u16,
    ) -> Result<Vec<DnsAnswer>, String> {
        let client = self.build_client(provider)?;
        let id: u16 = rand::random();
        let body = encode_query(id, host, qtype)?;
        let request = client
            .post(&provider.url)
            .header(reqwest::header::CONTENT_TYPE, "application/dns-message")
            .header(reqwest::header::ACCEPT, "application/dns-message")
            .body(body);
        let resp = tokio::time::timeout(self.cfg.timeout, request.send())
            .await
            .map_err(|_| format!("DoH request timed out after {:?}", self.cfg.timeout))?
            .map_err(|e| format!("DoH request failed: {}", error_chain(&e)))?;
        if !resp.status().is_success() {
            return Err(format!("DoH returned HTTP {}", resp.status()));
        }
        if let Some(len) = resp.content_length() {
            if len as usize > self.cfg.max_bytes {
                return Err(format!(
                    "DoH response of {len} bytes exceeds the {} byte limit",
                    self.cfg.max_bytes
                ));
            }
        }
        // 不信任 Content-Length：边读边限长，异常响应不会撑爆内存。
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        loop {
            let next = tokio::time::timeout(self.cfg.timeout, stream.next())
                .await
                .map_err(|_| format!("DoH body stalled for {:?}", self.cfg.timeout))?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|e| format!("DoH body error: {e}"))?;
            if buf.len() + chunk.len() > self.cfg.max_bytes {
                return Err(format!(
                    "DoH response exceeds the {} byte limit",
                    self.cfg.max_bytes
                ));
            }
            buf.extend_from_slice(&chunk);
        }
        parse_response(&buf, id)
    }

    /// 为某个提供商建客户端：预置 IP 只作为**连接目标覆盖**，URL 的 host 不变，
    /// 因此 TLS SNI 与证书校验仍然是 DoH 服务商自己的域名。
    fn build_client(&self, provider: &DohProvider) -> Result<reqwest::Client, String> {
        crate::tlsutil::install_rustls_provider();
        let mut builder = reqwest::Client::builder()
            .connect_timeout(self.cfg.connect_timeout)
            .timeout(self.cfg.timeout);
        if let Some(host) = url_host(&provider.url) {
            if !provider.bootstrap_ips.is_empty() {
                let addrs: Vec<SocketAddr> = provider
                    .bootstrap_ips
                    .iter()
                    .map(|ip| SocketAddr::new(*ip, 443))
                    .collect();
                builder = builder.resolve_to_addrs(&host, &addrs);
            }
        }
        for pem in &self.cfg.extra_roots {
            let cert = reqwest::Certificate::from_pem(pem)
                .map_err(|e| format!("invalid extra root certificate: {e}"))?;
            builder = builder.add_root_certificate(cert);
        }
        builder = self.cfg.proxy.apply(builder);
        builder
            .build()
            .map_err(|e| format!("cannot build DoH HTTP client: {e}"))
    }
}

impl DohLookup for DohClient {
    fn query<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DnsAnswer>, String>> + Send + 'a>> {
        Box::pin(self.resolve(host))
    }
}

/// relay 代解析（`GET /agent/resolve?host=`）。
#[derive(Clone)]
pub struct RelayResolver {
    /// 复用 relay 上行客户端：`--relay-insecure` 自签 relay 也能代解析。
    pub http: reqwest::Client,
    /// relay 根地址（`http(s)://host:port`，无尾斜杠）。
    pub base: String,
    /// rw Token：代解析与制品下载同一把钥匙。
    pub token: String,
    pub timeout: Duration,
}

impl std::fmt::Debug for RelayResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Token 永不进日志。
        f.debug_struct("RelayResolver")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl RelayResolver {
    /// 向 relay 要一个主机的解析结果。返回的 `expires_at`（Unix 秒）会被遵守。
    pub async fn resolve(&self, host: &str) -> Result<(Vec<IpAddr>, u64), String> {
        let url = format!("{}/agent/resolve", self.base.trim_end_matches('/'));
        let resp = tokio::time::timeout(
            self.timeout,
            self.http
                .get(&url)
                .query(&[("host", host)])
                .header("X-SR-Token", &self.token)
                .send(),
        )
        .await
        .map_err(|_| format!("relay resolve timed out after {:?}", self.timeout))?
        .map_err(|e| format!("relay resolve request failed: {}", error_chain(&e)))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("relay resolve returned HTTP {status}"));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("relay resolve returned invalid JSON: {e}"))?;
        let mut ips: Vec<IpAddr> = Vec::new();
        for item in body
            .get("addresses")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(s) = item.as_str() {
                if let Ok(ip) = s.parse::<IpAddr>() {
                    if !ips.contains(&ip) {
                        ips.push(ip);
                    }
                }
            }
        }
        if ips.is_empty() {
            return Err(format!(
                "relay resolve returned no usable address for {host}"
            ));
        }
        let expires_at = body
            .get("expires_at")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(now_unix);
        Ok((ips, expires_at))
    }
}

/// 三级解析链路：系统 → DoH → relay，带 TTL 缓存与主机白名单。
pub struct ResolveChain {
    /// 只允许解析的主机（本次升级任务的 CDN 主机，小写）。
    allowed: Vec<String>,
    system: std::sync::Arc<dyn SystemLookup>,
    doh: std::sync::Arc<dyn DohLookup>,
    relay: Option<RelayResolver>,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    addrs: Vec<IpAddr>,
    method: String,
    expires_at: Instant,
}

impl ResolveChain {
    pub fn new(
        allowed: Vec<String>,
        system: std::sync::Arc<dyn SystemLookup>,
        doh: std::sync::Arc<dyn DohLookup>,
        relay: Option<RelayResolver>,
    ) -> Self {
        Self {
            allowed: allowed
                .into_iter()
                .map(|h| h.to_ascii_lowercase())
                .collect(),
            system,
            doh,
            relay,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 生产构造：真实系统解析 + 预置 DoH + 可选 relay 代解析。
    pub fn production(
        allowed: Vec<String>,
        doh_cfg: DohConfig,
        relay: Option<RelayResolver>,
    ) -> Self {
        Self::new(
            allowed,
            std::sync::Arc::new(RealSystemLookup::default()),
            std::sync::Arc::new(DohClient::new(doh_cfg)),
            relay,
        )
    }

    pub async fn resolve(&self, host: &str) -> Result<ResolvedIps, String> {
        let host = host.to_ascii_lowercase();
        if !self.is_allowed(&host) {
            return Err(format!(
                "host {host} is not part of this upgrade task; refusing to resolve it"
            ));
        }
        if let Some(hit) = self.cache_get(&host) {
            return Ok(hit);
        }

        let mut failures: Vec<String> = Vec::new();

        // 1. 系统解析
        match self.system.lookup(&host).await {
            Ok(addrs) if !addrs.is_empty() => {
                // 系统解析没有 TTL 可用，只在本次任务内短期复用。
                let resolved = ResolvedIps {
                    addrs,
                    method: "system".to_string(),
                };
                self.cache_put(
                    &host,
                    &resolved,
                    Duration::from_secs(DNS_TTL_MIN_SECS as u64),
                );
                return Ok(resolved);
            }
            Ok(_) => failures.push("system: no address".to_string()),
            Err(e) => failures.push(format!("system: {e}")),
        }

        // 2. 预置 DoH
        match self.doh.query(&host).await {
            Ok(answers) if !answers.is_empty() => {
                let mut addrs: Vec<IpAddr> = Vec::new();
                let mut ttl = 0u32;
                for a in &answers {
                    if !addrs.contains(&a.ip) {
                        addrs.push(a.ip);
                    }
                    ttl = ttl.max(a.ttl);
                }
                let resolved = ResolvedIps {
                    addrs,
                    method: "doh".to_string(),
                };
                self.cache_put(&host, &resolved, Duration::from_secs(ttl_from_dns(ttl)));
                return Ok(resolved);
            }
            Ok(_) => failures.push("doh: no address".to_string()),
            Err(e) => failures.push(format!("doh: {e}")),
        }

        // 3. relay 代解析
        match &self.relay {
            Some(relay) => match relay.resolve(&host).await {
                Ok((addrs, expires_at)) => {
                    let ttl = expires_at
                        .saturating_sub(now_unix())
                        .clamp(1, DNS_TTL_MAX_SECS as u64);
                    let resolved = ResolvedIps {
                        addrs,
                        method: "relay".to_string(),
                    };
                    self.cache_put(&host, &resolved, Duration::from_secs(ttl));
                    Ok(resolved)
                }
                Err(e) => {
                    failures.push(format!("relay: {e}"));
                    Err(failures.join("; "))
                }
            },
            None => {
                failures.push("relay: no relay fallback available".to_string());
                Err(failures.join("; "))
            }
        }
    }

    fn is_allowed(&self, host: &str) -> bool {
        self.allowed.iter().any(|h| h == host)
    }

    fn cache_get(&self, host: &str) -> Option<ResolvedIps> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        cache.retain(|_, e| e.expires_at > now);
        cache.get(host).map(|e| ResolvedIps {
            addrs: e.addrs.clone(),
            method: format!("{}:cached", e.method),
        })
    }

    fn cache_put(&self, host: &str, resolved: &ResolvedIps, ttl: Duration) {
        // TTL=0（上游明确不缓存）时仍给一个最小窗口，避免同一任务里反复打 DoH。
        let ttl = if ttl.is_zero() {
            Duration::from_secs(DNS_TTL_MIN_SECS as u64)
        } else {
            ttl
        };
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(
            host.to_string(),
            CacheEntry {
                addrs: resolved.addrs.clone(),
                method: resolved.method.clone(),
                expires_at: Instant::now() + ttl,
            },
        );
    }
}

impl HostResolver for ResolveChain {
    fn resolve_host<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ResolvedIps, String>> + Send + 'a>> {
        Box::pin(self.resolve(host))
    }
}

/// DNS TTL → 缓存时长：0 视为「不缓存」，其余夹在 [MIN, MAX]。
fn ttl_from_dns(ttl: u32) -> u64 {
    if ttl == 0 {
        return 0;
    }
    ttl.clamp(DNS_TTL_MIN_SECS, DNS_TTL_MAX_SECS) as u64
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 取 URL 的裸主机名（小写）。只用于给 reqwest 做连接地址覆盖。
pub fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// 把 reqwest 错误连同底层原因串起来。
///
/// `reqwest::Error` 的 `Display` 只说「error sending request for url (...)」，
/// 证书校验失败、DNS 失败、连接拒绝都长一样——现场排查必须看 cause 链
/// （例如 `invalid peer certificate: NotValidForName`）。
pub(crate) fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    let mut depth = 0;
    while let Some(s) = src {
        if depth >= 5 {
            break;
        }
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
        depth += 1;
    }
    out
}

// ── DNS wire format（RFC 1035 报文 + RFC 8484 传输）────────────────────

/// 构造一条查询报文：单问题、RD=1、IN。
pub fn encode_query(id: u16, host: &str, qtype: u16) -> Result<Vec<u8>, String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.len() > 253 {
        return Err(format!("invalid DNS name {host:?}"));
    }
    let mut out = Vec::with_capacity(32 + host.len());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&0x0100u16.to_be_bytes()); // QR=0, Opcode=0, RD=1
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("invalid DNS label in {host:?}"));
        }
        // 主机名只允许字母数字/连字符/下划线：非法字符会被当成标签走私。
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(format!("invalid DNS label {label:?} in {host:?}"));
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
    Ok(out)
}

/// 解析应答报文：校验 id / QR / TC / RCODE，抽 A 与 AAAA 记录（带 TTL）。
///
/// 不做 CNAME 链校验：应答已绑定到本次查询的 id 与问题，且最终身份由制品
/// SHA-256 决定，CNAME 只影响我们连到哪个 IP。
pub fn parse_response(buf: &[u8], expect_id: u16) -> Result<Vec<DnsAnswer>, String> {
    if buf.len() < 12 {
        return Err(format!(
            "DNS response is {} bytes, shorter than a header",
            buf.len()
        ));
    }
    let id = u16::from_be_bytes([buf[0], buf[1]]);
    if id != expect_id {
        return Err(format!(
            "DNS response id mismatch (got {id}, expected {expect_id})"
        ));
    }
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    if flags & 0x8000 == 0 {
        return Err("DNS response is not a reply (QR=0)".to_string());
    }
    if flags & 0x0200 != 0 {
        return Err("DNS response is truncated (TC=1)".to_string());
    }
    let rcode = flags & 0x000f;
    if rcode != 0 {
        let reason = match rcode {
            1 => "FORMERR",
            2 => "SERVFAIL",
            3 => "NXDOMAIN",
            4 => "NOTIMP",
            5 => "REFUSED",
            _ => "unknown rcode",
        };
        return Err(format!("DNS response carries {reason} (rcode={rcode})"));
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    let mut off = 12usize;
    for _ in 0..qdcount {
        skip_name(buf, &mut off)?;
        if off + 4 > buf.len() {
            return Err("DNS response question section is truncated".to_string());
        }
        off += 4;
    }
    let mut out: Vec<DnsAnswer> = Vec::new();
    for _ in 0..ancount {
        skip_name(buf, &mut off)?;
        if off + 10 > buf.len() {
            return Err("DNS response answer header is truncated".to_string());
        }
        let rtype = u16::from_be_bytes([buf[off], buf[off + 1]]);
        let rclass = u16::from_be_bytes([buf[off + 2], buf[off + 3]]);
        let ttl = u32::from_be_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
        let rdlen = u16::from_be_bytes([buf[off + 8], buf[off + 9]]) as usize;
        off += 10;
        if off + rdlen > buf.len() {
            return Err("DNS response rdata is truncated".to_string());
        }
        let rdata = &buf[off..off + rdlen];
        if rclass == DNS_CLASS_IN {
            match (rtype, rdlen) {
                (DNS_TYPE_A, 4) => out.push(DnsAnswer {
                    ip: IpAddr::from([rdata[0], rdata[1], rdata[2], rdata[3]]),
                    ttl,
                }),
                (DNS_TYPE_AAAA, 16) => {
                    let mut octets = [0u8; 16];
                    octets.copy_from_slice(rdata);
                    out.push(DnsAnswer {
                        ip: IpAddr::from(octets),
                        ttl,
                    });
                }
                _ => {}
            }
        }
        off += rdlen;
    }
    if out.is_empty() {
        return Err("DNS response carries no A/AAAA record".to_string());
    }
    Ok(out)
}

/// 跳过（不解释）一个域名：支持压缩指针，拒绝越界与压缩环。
fn skip_name(buf: &[u8], off: &mut usize) -> Result<(), String> {
    let mut guard = 0usize;
    loop {
        let b = *buf
            .get(*off)
            .ok_or_else(|| "DNS name runs past the end of the message".to_string())?;
        match b & 0xc0 {
            0xc0 => {
                // 压缩指针：2 字节，域名到此结束。指针目标只做边界校验。
                if *off + 1 >= buf.len() {
                    return Err("DNS compression pointer is truncated".to_string());
                }
                let ptr = (u16::from_be_bytes([buf[*off], buf[*off + 1]]) & 0x3fff) as usize;
                if ptr >= buf.len() {
                    return Err("DNS compression pointer points outside the message".to_string());
                }
                *off += 2;
                return Ok(());
            }
            0x00 if b == 0 => {
                *off += 1;
                return Ok(());
            }
            0x00 => {
                *off += 1 + b as usize;
                if *off > buf.len() {
                    return Err("DNS label runs past the end of the message".to_string());
                }
            }
            _ => return Err("DNS name uses a reserved label type".to_string()),
        }
        guard += 1;
        if guard > 128 {
            return Err("DNS name has too many labels".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── wire format ──────────────────────────────────────────────

    #[test]
    fn test_encode_query_wire_format() {
        let q = encode_query(0x1234, "Example.COM", DNS_TYPE_A).unwrap();
        assert_eq!(&q[0..2], &[0x12, 0x34]);
        assert_eq!(&q[2..4], &[0x01, 0x00], "RD=1");
        assert_eq!(&q[4..6], &[0x00, 0x01], "QDCOUNT=1");
        assert_eq!(&q[6..12], &[0, 0, 0, 0, 0, 0]);
        // QNAME：7"example"3"com"0
        assert_eq!(&q[12..13], &[7]);
        assert_eq!(&q[13..20], b"example");
        assert_eq!(&q[20..21], &[3]);
        assert_eq!(&q[21..24], b"com");
        assert_eq!(&q[24..25], &[0]);
        assert_eq!(&q[25..27], &DNS_TYPE_A.to_be_bytes());
        assert_eq!(&q[27..29], &DNS_CLASS_IN.to_be_bytes());
        assert_eq!(q.len(), 29);
    }

    #[test]
    fn test_encode_query_rejects_invalid_names() {
        assert!(encode_query(1, "", DNS_TYPE_A).is_err());
        assert!(encode_query(1, "a..b", DNS_TYPE_A).is_err());
        assert!(encode_query(1, "bad name", DNS_TYPE_A).is_err());
        assert!(encode_query(1, "evil/../host", DNS_TYPE_A).is_err());
        assert!(encode_query(1, &format!("{}.com", "a".repeat(64)), DNS_TYPE_A).is_err());
        assert!(encode_query(1, &"a".repeat(254), DNS_TYPE_A).is_err());
    }

    /// 构造一条应答：固定 id、NoError、1 问题 + 若干答案（无压缩）。
    fn build_response(id: u16, host: &str, answers: &[(u16, u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&0x8180u16.to_be_bytes()); // QR=1, RD=1, RA=1, RCODE=0
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&(answers.len() as u16).to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        for label in host.split('.') {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
        out.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
        for (rtype, ttl, rdata) in answers {
            // 用压缩指针指向问题里的域名（offset 12）。
            out.extend_from_slice(&[0xc0, 0x0c]);
            out.extend_from_slice(&rtype.to_be_bytes());
            out.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
            out.extend_from_slice(&ttl.to_be_bytes());
            out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            out.extend_from_slice(rdata);
        }
        out
    }

    #[test]
    fn test_parse_response_a_and_aaaa() {
        let v6: Vec<u8> = (0..16).map(|i| i as u8).collect();
        let buf = build_response(
            7,
            "cdn.example",
            &[
                (DNS_TYPE_A, 120, vec![93, 184, 216, 34]),
                (DNS_TYPE_AAAA, 60, v6.clone()),
            ],
        );
        let got = parse_response(&buf, 7).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ip, IpAddr::from([93, 184, 216, 34]));
        assert_eq!(got[0].ttl, 120);
        assert_eq!(
            got[1].ip,
            IpAddr::from(<[u8; 16]>::try_from(v6.as_slice()).unwrap())
        );
        assert_eq!(got[1].ttl, 60);
    }

    #[test]
    fn test_parse_response_skips_cname_then_a() {
        // CNAME 在前，A 在后：两条答案都要被正确跳过/读取。
        let cname_rdata = {
            let mut v = vec![3];
            v.extend_from_slice(b"cdn");
            v.push(7);
            v.extend_from_slice(b"example");
            v.push(0);
            v
        };
        let buf = build_response(
            9,
            "cdn.example",
            &[
                (5, 300, cname_rdata),
                (DNS_TYPE_A, 30, vec![10, 0, 0, 1]),
                (16, 300, vec![0xde, 0xad]), // TXT：忽略
            ],
        );
        let got = parse_response(&buf, 9).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].ip, IpAddr::from([10, 0, 0, 1]));
    }

    #[test]
    fn test_parse_response_rejects_bad_ids_and_rcodes() {
        let good = build_response(5, "cdn.example", &[(DNS_TYPE_A, 60, vec![1, 2, 3, 4])]);
        assert!(parse_response(&good, 5).is_ok());
        let err = parse_response(&good, 6).unwrap_err();
        assert!(err.contains("id mismatch"), "{err}");

        // NXDOMAIN
        let mut nx = good.clone();
        nx[3] = 0x83; // RCODE=3
        let err = parse_response(&nx, 5).unwrap_err();
        assert!(err.contains("NXDOMAIN"), "{err}");

        // SERVFAIL
        let mut sf = good.clone();
        sf[3] = 0x82;
        let err = parse_response(&sf, 5).unwrap_err();
        assert!(err.contains("SERVFAIL"), "{err}");
    }

    #[test]
    fn test_parse_response_rejects_truncated_and_garbage() {
        // 截断（TC=1）
        let mut tc = build_response(1, "a.example", &[(DNS_TYPE_A, 60, vec![1, 1, 1, 1])]);
        tc[2] |= 0x02;
        assert!(parse_response(&tc, 1).unwrap_err().contains("truncated"));

        // 短于头部
        assert!(parse_response(&[0u8; 5], 1).is_err());
        // 空输入
        assert!(parse_response(&[], 1).is_err());

        // QR=0（是个查询，不是应答）
        let mut q = build_response(1, "a.example", &[(DNS_TYPE_A, 60, vec![1, 1, 1, 1])]);
        q[2] &= 0x7f;
        assert!(parse_response(&q, 1).unwrap_err().contains("QR=0"));

        // 应答被截断在 rdata 中间
        let mut cut = build_response(1, "a.example", &[(DNS_TYPE_A, 60, vec![1, 1, 1, 1])]);
        cut.truncate(cut.len() - 2);
        assert!(parse_response(&cut, 1).is_err());

        // 没有 A/AAAA 记录
        let empty = build_response(1, "a.example", &[(16, 60, vec![1, 2])]);
        assert!(parse_response(&empty, 1).unwrap_err().contains("no A/AAAA"));

        // 保留标签类型（0x40）
        let mut reserved = build_response(1, "a.example", &[(DNS_TYPE_A, 60, vec![1, 1, 1, 1])]);
        reserved[12] = 0x40;
        assert!(parse_response(&reserved, 1).is_err());
    }

    #[test]
    fn test_ttl_policy() {
        assert_eq!(ttl_from_dns(0), 0, "TTL=0 不缓存");
        assert_eq!(ttl_from_dns(5), DNS_TTL_MIN_SECS as u64);
        assert_eq!(ttl_from_dns(120), 120);
        assert_eq!(ttl_from_dns(99999), DNS_TTL_MAX_SECS as u64);
    }

    #[test]
    fn test_url_host() {
        assert_eq!(
            url_host("https://dns.alidns.com/dns-query").unwrap(),
            "dns.alidns.com"
        );
        assert_eq!(url_host("https://1.1.1.1:8443/x?y=1").unwrap(), "1.1.1.1");
        assert_eq!(url_host("http://cdn.test:9/path").unwrap(), "cdn.test");
        assert_eq!(url_host("ftp://cdn.test/x"), None);
        assert_eq!(url_host("https://"), None);
    }

    // ── 三级链路顺序 ──────────────────────────────────────────

    struct FakeSystem(Result<Vec<IpAddr>, String>);
    impl SystemLookup for FakeSystem {
        fn lookup<'a>(
            &'a self,
            _host: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, String>> + Send + 'a>> {
            let r = self.0.clone();
            Box::pin(async move { r })
        }
    }

    struct FakeDoh(
        Result<Vec<DnsAnswer>, String>,
        std::sync::atomic::AtomicUsize,
    );
    impl FakeDoh {
        fn new(r: Result<Vec<DnsAnswer>, String>) -> Self {
            Self(r, std::sync::atomic::AtomicUsize::new(0))
        }
        fn calls(&self) -> usize {
            self.1.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl DohLookup for FakeDoh {
        fn query<'a>(
            &'a self,
            _host: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<DnsAnswer>, String>> + Send + 'a>> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let r = self.0.clone();
            Box::pin(async move { r })
        }
    }

    fn answer(ip: [u8; 4], ttl: u32) -> Vec<DnsAnswer> {
        vec![DnsAnswer {
            ip: IpAddr::from(ip),
            ttl,
        }]
    }

    #[tokio::test]
    async fn test_chain_prefers_system_dns() {
        // 系统解析成功 → 不再打扰 DoH / relay。
        let doh = std::sync::Arc::new(FakeDoh::new(Ok(answer([9, 9, 9, 9], 60))));
        let chain = ResolveChain::new(
            vec!["cdn.example".into()],
            std::sync::Arc::new(FakeSystem(Ok(vec![IpAddr::from([1, 2, 3, 4])]))),
            doh.clone(),
            None,
        );
        let got = chain.resolve("cdn.example").await.unwrap();
        assert_eq!(got.method, "system");
        assert_eq!(got.addrs, vec![IpAddr::from([1, 2, 3, 4])]);
        assert_eq!(doh.calls(), 0, "系统解析成功时不应调用 DoH");
    }

    #[tokio::test]
    async fn test_chain_falls_back_to_doh_when_system_fails() {
        let doh = std::sync::Arc::new(FakeDoh::new(Ok(answer([9, 9, 9, 9], 60))));
        let chain = ResolveChain::new(
            vec!["cdn.example".into()],
            std::sync::Arc::new(FakeSystem(Err("SERVFAIL from polluted resolver".into()))),
            doh,
            None,
        );
        let got = chain.resolve("cdn.example").await.unwrap();
        assert_eq!(got.method, "doh");
        assert_eq!(got.addrs, vec![IpAddr::from([9, 9, 9, 9])]);
    }

    #[tokio::test]
    async fn test_chain_falls_back_to_relay_when_doh_invalid() {
        // DoH 回了一个无效应答（解析失败）→ 用 relay 代解析。
        let (base, hits) = spawn_resolve_mock(&["10.1.2.3"], 300).await;
        let relay = RelayResolver {
            http: reqwest::Client::new(),
            base,
            token: "rw-token".to_string(),
            timeout: Duration::from_secs(5),
        };
        let chain = ResolveChain::new(
            vec!["cdn.example".into()],
            std::sync::Arc::new(FakeSystem(Err("lookup timed out".into()))),
            std::sync::Arc::new(FakeDoh::new(Err("invalid DNS response".into()))),
            Some(relay),
        );
        let got = chain.resolve("cdn.example").await.unwrap();
        assert_eq!(got.method, "relay");
        assert_eq!(got.addrs, vec![IpAddr::from([10, 1, 2, 3])]);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_chain_reports_all_three_failures() {
        let chain = ResolveChain::new(
            vec!["cdn.example".into()],
            std::sync::Arc::new(FakeSystem(Err("no route".into()))),
            std::sync::Arc::new(FakeDoh::new(Err("DoH returned HTTP 500".into()))),
            None,
        );
        let err = chain.resolve("cdn.example").await.unwrap_err();
        assert!(err.contains("system: no route"), "{err}");
        assert!(err.contains("doh: DoH returned HTTP 500"), "{err}");
        assert!(err.contains("relay: no relay fallback"), "{err}");
    }

    #[tokio::test]
    async fn test_chain_refuses_host_outside_allowlist() {
        let chain = ResolveChain::new(
            vec!["cdn.example".into()],
            std::sync::Arc::new(FakeSystem(Ok(vec![IpAddr::from([1, 1, 1, 1])]))),
            std::sync::Arc::new(FakeDoh::new(Err("must not be called".into()))),
            None,
        );
        // 内网/任意主机都不允许，避免 agent 变成 DNS 跳板。
        for host in ["internal.corp", "127.0.0.1", "localhost", "evil.example"] {
            let err = chain.resolve(host).await.unwrap_err();
            assert!(
                err.contains("not part of this upgrade task"),
                "{host}: {err}"
            );
        }
        // 大小写不敏感
        assert_eq!(chain.resolve("CDN.EXAMPLE").await.unwrap().method, "system");
    }

    #[tokio::test]
    async fn test_chain_caches_within_ttl() {
        let doh = std::sync::Arc::new(FakeDoh::new(Ok(answer([9, 9, 9, 9], 120))));
        let chain = ResolveChain::new(
            vec!["cdn.example".into()],
            std::sync::Arc::new(FakeSystem(Err("down".into()))),
            doh.clone(),
            None,
        );
        assert_eq!(chain.resolve("cdn.example").await.unwrap().method, "doh");
        let second = chain.resolve("cdn.example").await.unwrap();
        assert_eq!(second.method, "doh:cached");
        assert_eq!(doh.calls(), 1, "TTL 内不应重复查询 DoH");
    }

    /// 起一个真实 relay 代解析 mock：校验 `X-SR-Token`、按 host 返回地址。
    async fn spawn_resolve_mock(
        addresses: &'static [&'static str],
        ttl: u64,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::routing::get;
        use axum::Router;
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_inner = hits.clone();
        let app = Router::new().route(
            "/agent/resolve",
            get(
                move |headers: axum::http::HeaderMap,
                      axum::extract::Query(q): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| {
                    let hits = hits_inner.clone();
                    async move {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let token = headers
                            .get("x-sr-token")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        if token != "rw-token" {
                            return (
                                axum::http::StatusCode::UNAUTHORIZED,
                                axum::Json(serde_json::json!({"error": "invalid token"})),
                            );
                        }
                        let host = q.get("host").cloned().unwrap_or_default();
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "host": host,
                                "addresses": addresses,
                                "expires_at": now_unix() + ttl,
                                "cached": false,
                            })),
                        )
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), hits)
    }

    // ── 真实 TLS mock：DoH over HTTPS ─────────────────────────

    mod tls_mock {
        use super::*;
        use std::sync::Arc;

        pub struct TestPki {
            pub ca_pem: Vec<u8>,
            pub leaf_pem: Vec<u8>,
            pub key_pem: Vec<u8>,
        }

        /// 生成「测试 CA + 指定 SAN 的叶子证书」。CA 只信任在测试客户端的
        /// `extra_roots` 里，不污染进程级信任库。
        pub fn make_pki(san: &str) -> TestPki {
            use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

            let ca_key = KeyPair::generate().unwrap();
            let mut ca_params = CertificateParams::new(vec!["test-ca".to_string()]).unwrap();
            ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            let ca_cert = ca_params.self_signed(&ca_key).unwrap();

            let leaf_key = KeyPair::generate().unwrap();
            let leaf_params = CertificateParams::new(vec![san.to_string()]).unwrap();
            let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();
            TestPki {
                ca_pem: ca_cert.pem().into_bytes(),
                leaf_pem: leaf_cert.pem().into_bytes(),
                key_pem: leaf_key.serialize_pem().into_bytes(),
            }
        }

        /// 起一个 HTTPS mock：每个请求交给 `handler` 处理。
        pub async fn spawn_tls_server<F, Fut>(
            pki: &TestPki,
            handler: F,
        ) -> (u16, tokio::task::JoinHandle<()>)
        where
            F: Fn(axum::extract::Request) -> Fut + Clone + Send + Sync + 'static,
            Fut: std::future::Future<Output = axum::response::Response> + Send + 'static,
        {
            use axum::Router;
            crate::tlsutil::install_rustls_provider();
            let app = Router::new().fallback(move |req: axum::extract::Request| {
                let handler = handler.clone();
                async move { handler(req).await }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let config = axum_server::tls_rustls::RustlsConfig::from_pem(
                pki.leaf_pem.clone(),
                pki.key_pem.clone(),
            )
            .await
            .unwrap();
            let listener = listener.into_std().unwrap();
            let handle = tokio::spawn(async move {
                let _ = axum_server::from_tcp_rustls(listener, config)
                    .serve(app.into_make_service())
                    .await;
            });
            (port, handle)
        }
    }

    /// DoH 全链路：真实 TLS（证书只对 `doh.test` 有效），预置 IP 指向 127.0.0.1。
    /// 能成功说明我们**没有**把 TLS 校验目标换成 IP。
    #[tokio::test]
    async fn test_doh_over_tls_uses_provider_hostname_not_bootstrap_ip() {
        use axum::response::IntoResponse;

        let pki = tls_mock::make_pki("doh.test");
        let (port, _srv) =
            tls_mock::spawn_tls_server(&pki, |req: axum::extract::Request| async move {
                // 校验请求确实是 RFC 8484 wire format，并按查询回一条 A 记录。
                let body = axum::body::to_bytes(req.into_body(), 4096).await.unwrap();
                let id = u16::from_be_bytes([body[0], body[1]]);
                let resp =
                    build_response(id, "cdn.example", &[(DNS_TYPE_A, 60, vec![203, 0, 113, 7])]);
                (
                    [(axum::http::header::CONTENT_TYPE, "application/dns-message")],
                    resp,
                )
                    .into_response()
            })
            .await;

        let doh = DohClient::new(DohConfig {
            providers: vec![DohProvider {
                label: "test".into(),
                url: format!("https://doh.test:{port}/dns-query"),
                bootstrap_ips: vec![IpAddr::from([127, 0, 0, 1])],
            }],
            timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            max_bytes: DOH_MAX_BYTES,
            proxy: ProxyConfig::empty(),
            extra_roots: vec![pki.ca_pem.clone()],
        });
        // 系统解析一定失败（doh.test 不存在），DoH 是唯一出路。
        let chain = ResolveChain::new(
            vec!["cdn.example".into()],
            std::sync::Arc::new(FakeSystem(Err("no such host".into()))),
            std::sync::Arc::new(doh),
            None,
        );
        let got = chain.resolve("cdn.example").await.expect("DoH over TLS");
        assert_eq!(got.addrs, vec![IpAddr::from([203, 0, 113, 7])]);
        assert_eq!(got.method, "doh");
    }

    /// 同一 mock 但证书只对别的域名有效：必须因证书校验失败而拒绝——这证明
    /// 我们不是在「连上就行」地跳过校验。
    #[tokio::test]
    async fn test_doh_rejects_certificate_for_other_hostname() {
        let pki = tls_mock::make_pki("other.test");
        let (port, _srv) =
            tls_mock::spawn_tls_server(&pki, |_req: axum::extract::Request| async move {
                axum::response::Response::new(axum::body::Body::empty())
            })
            .await;
        let doh = DohClient::new(DohConfig {
            providers: vec![DohProvider {
                label: "test".into(),
                url: format!("https://doh.test:{port}/dns-query"),
                bootstrap_ips: vec![IpAddr::from([127, 0, 0, 1])],
            }],
            timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            max_bytes: DOH_MAX_BYTES,
            proxy: ProxyConfig::empty(),
            extra_roots: vec![pki.ca_pem.clone()],
        });
        let err = doh.resolve("cdn.example").await.unwrap_err();
        assert!(
            err.to_lowercase().contains("certificate")
                || err.to_lowercase().contains("tls")
                || err.to_lowercase().contains("invalid peer"),
            "证书域名不匹配必须握手失败: {err}"
        );
    }

    /// **真实网络**冒烟（默认 ignored）：预置两家 DoH 各自能否解析出地址，以及
    /// 系统解析失败时链路真的降级到 DoH。手工跑：
    /// `cargo test --lib --no-default-features real_doh_smoke -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "需要真实外网；手工跑 --ignored"]
    async fn real_doh_smoke() {
        let doh = DohClient::new(DohConfig::default());
        // 逐家直查：不能只靠链路（系统解析成功时链路根本不会碰 DoH）。
        for p in default_providers() {
            for host in ["github.com", "gh-proxy.com"] {
                let got = doh.query_provider(&p, host, DNS_TYPE_A).await;
                println!("[{}] {host} A => {got:?}", p.label);
                let answers = got.unwrap_or_else(|e| panic!("{} {host}: {e}", p.label));
                assert!(!answers.is_empty(), "{} {host} 无地址", p.label);
            }
        }
        // 系统解析强制失败 → 污染网络下的真实路径：必须由 DoH 兜住。
        let chain = ResolveChain::new(
            vec!["github.com".into()],
            std::sync::Arc::new(FakeSystem(Err("forced failure".into()))),
            std::sync::Arc::new(doh),
            None,
        );
        let got = chain.resolve("github.com").await.expect("DoH fallback");
        println!("chain => {got:?}");
        assert_eq!(got.method, "doh");
        assert!(!got.addrs.is_empty());
    }

    /// 响应体超限必须被拒绝（不能把大响应读进内存）。
    #[tokio::test]
    async fn test_doh_rejects_oversized_response() {
        let pki = tls_mock::make_pki("doh.test");
        let (port, _srv) =
            tls_mock::spawn_tls_server(&pki, |_req: axum::extract::Request| async move {
                let big = vec![0u8; DOH_MAX_BYTES + 1];
                axum::response::Response::new(axum::body::Body::from(big))
            })
            .await;
        let doh = DohClient::new(DohConfig {
            providers: vec![DohProvider {
                label: "test".into(),
                url: format!("https://doh.test:{port}/dns-query"),
                bootstrap_ips: vec![IpAddr::from([127, 0, 0, 1])],
            }],
            timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            max_bytes: DOH_MAX_BYTES,
            proxy: ProxyConfig::empty(),
            extra_roots: vec![pki.ca_pem.clone()],
        });
        let err = doh.resolve("cdn.example").await.unwrap_err();
        assert!(err.contains("byte limit"), "{err}");
    }
}
