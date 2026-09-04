use anyhow::{bail, Context};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::proto::Message as ProtoMessage;

/// How long the relay→agent SSE may be silent before the agent treats the
/// connection as dead and reconnects. The relay sends an SSE keep-alive
/// comment every ~15s, so this is a comfortable multiple of that.
const AGENT_SSE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Read SSE chunks from the relay, forward each `data:` payload to `tx`, and
/// return when the stream ends, errors, or no bytes arrive within
/// `idle_timeout`. The idle timeout turns a silently killed (half-open)
/// connection into a reconnect: without it the agent would block on `recv()`
/// forever even though the relay→agent path is dead.
pub(crate) async fn pump_sse_events<S, B, E>(
    mut stream: S,
    tx: mpsc::UnboundedSender<String>,
    idle_timeout: std::time::Duration,
) where
    S: tokio_stream::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
{
    let mut buf = String::new();
    let mut event_count: u64 = 0;
    loop {
        let next_chunk = tokio::time::timeout(idle_timeout, stream.next());
        let chunk = match next_chunk.await {
            Ok(Some(Ok(bytes))) => bytes,
            Ok(Some(Err(_))) => {
                tracing::warn!("agent SSE stream error after {} events", event_count);
                break;
            }
            Ok(None) => break,
            Err(_) => {
                tracing::warn!(
                    "agent SSE idle for {:.1}s ({} events) — treating relay connection as dead",
                    idle_timeout.as_secs_f64(),
                    event_count
                );
                break;
            }
        };
        let text = String::from_utf8_lossy(chunk.as_ref());
        buf.push_str(&text);
        while let Some(pos) = buf.find("\n\n") {
            let event_str = buf[..pos].to_string();
            buf = buf[pos + 2..].to_string();
            for line in event_str.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    event_count += 1;
                    if event_count <= 3 {
                        tracing::debug!(
                            event = event_count,
                            "agent SSE event: {}",
                            data.trim()
                        );
                    }
                    let _ = tx.send(data.trim().to_string());
                }
            }
        }
    }
    tracing::debug!("agent SSE stream ended, {} events received", event_count);
}

struct Transport {
    client: reqwest::Client,
    send_url: String,
    /// Server password captured at registration ("" when key-mode), used to
    /// authenticate the desktop WS uplink socket.
    server_auth: String,
    events_rx: mpsc::UnboundedReceiver<String>,
    #[allow(dead_code)]
    last_event_id: Option<u64>,
    _task: tokio::task::JoinHandle<()>,
}

pub struct RelayClient {
    transport: Transport,
    pub session_id: String,
    pub tokens: Vec<(String, String)>,
    /// 自签 https relay 模式：上行(register/SSE/桌面WS)跳过证书校验。
    pub insecure_tls: bool,
}

impl RelayClient {
    async fn connect_http(
        relay_url: &str,
        fixed_key: Option<String>,
        token_type: &str,
        desired_session_id: Option<&str>,
        cached_tokens: Option<&[(String, String)]>,
        insecure_tls: bool,
    ) -> anyhow::Result<Self> {
        let base = relay_url.trim_end_matches('/');
        // reqwest 的 register/SSE 是常规 HTTP 通道（wss/ws 仅供桌面 WS 上行
        // 使用）；把 ws://world 映射到 http://、wss:// 映射到 https://。
        let http_base = base
            .replace("ws://", "http://")
            .replace("wss://", "https://");
        let send_url = format!("{}/agent/send", http_base);

        let http_client = if insecure_tls {
            crate::tlsutil::install_rustls_provider();
            reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .context("Failed to build HTTP client (insecure TLS)")?
        } else {
            reqwest::Client::new()
        };

        // Best-effort host probe (CPU model, arch, OS, …) reported with every
        // registration so the admin device panel reflects the current host.
        let device = crate::agent::device::probe().await;

        // On reconnect, replay the cached tokens so the relay reuses them
        // instead of minting new random ones (keeps shared tokens stable).
        let mut register_msg = if let Some(ct) = cached_tokens {
            let arr: Vec<serde_json::Value> = ct
                .iter()
                .map(|(tok, perm)| json!({"token": tok, "permission": perm}))
                .collect();
            json!({
                "type": "agent:register",
                "tokens": arr,
                "device": device,
            })
        } else {
            json!({
                "type": "agent:register",
                "key": fixed_key,
                "token_type": token_type,
                "device": device,
            })
        };
        if let Some(sid) = desired_session_id {
            register_msg["session_id"] = json!(sid);
        }
        register_msg["agent_version"] = json!(env!("CARGO_PKG_VERSION"));

        let resp = http_client
            .post(&send_url)
            .json(&register_msg)
            .send()
            .await
            .context("Failed to POST register message")?;

        let status = resp.status();
        let body_text = resp
            .text()
            .await
            .context("Failed to read register response")?;

        if !status.is_success() {
            anyhow::bail!("Registration failed (HTTP {}): {}", status, body_text);
        }

        let response: serde_json::Value = serde_json::from_str(&body_text).with_context(|| {
            format!(
                "Failed to parse register response (status {}): {}",
                status,
                &body_text[..body_text.len().min(500)]
            )
        })?;

        let session_id = response["session_id"]
            .as_str()
            .context("Missing session_id in register response")?
            .to_string();

        let events_url = format!("{}/agent/events?session={}", http_base, session_id);

        let (tx, rx) = mpsc::unbounded_channel::<String>();

        let sse_client = http_client.clone();
        let sse_task = tokio::spawn(async move {
            let stream = match sse_client
                .get(&events_url)
                .header("Accept", "text/event-stream")
                .header("Cache-Control", "no-cache")
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    let ct = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    tracing::debug!(
                        status = %status,
                        content_type = %ct,
                        "agent SSE connected"
                    );
                    resp.bytes_stream()
                }
                Err(e) => {
                    tracing::warn!("agent SSE connection failed: {}", e);
                    return;
                }
            };

            pump_sse_events(stream, tx, AGENT_SSE_IDLE_TIMEOUT).await;
        });

        let mut client = Self {
            transport: Transport {
                client: http_client,
                send_url,
                server_auth: fixed_key.unwrap_or_default(),
                events_rx: rx,
                last_event_id: None,
                _task: sse_task,
            },
            session_id: String::new(),
            tokens: Vec::new(),
            insecure_tls,
        };

        Self::handle_register_response(&mut client, &response)?;
        Ok(client)
    }

    fn handle_register_response(
        client: &mut Self,
        response: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let msg_type = response["type"].as_str().unwrap_or("");
        if msg_type != "agent:registered" {
            bail!("Unexpected register response type: {}", msg_type);
        }

        client.session_id = response["session_id"]
            .as_str()
            .context("Missing session_id")?
            .to_string();

        // The relay marks the registration with `evicted: true` when this
        // agent took over a session_id / token that a previous incarnation
        // still held — the old session was displaced. Surface it prominently
        // so operators notice a duplicate/restart immediately.
        if response.get("evicted").and_then(|v| v.as_bool()).unwrap_or(false) {
            tracing::warn!(
                session = %client.session_id,
                "registration evicted a previous session with the same session_id/token — duplicate agent detected"
            );
        }

        let payload = &response["payload"];
        if let Some(tokens_array) = payload["tokens"].as_array() {
            for t in tokens_array {
                let token = t["token"]
                    .as_str()
                    .context("Missing token in tokens array")?
                    .to_string();
                let permission = t["permission"]
                    .as_str()
                    .context("Missing permission in tokens array")?
                    .to_string();
                client.tokens.push((token, permission));
            }
        }

        Ok(())
    }

    pub async fn connect_with_retry(
        relay_url: &str,
        fixed_key: Option<String>,
        token_type: &str,
        desired_session_id: Option<&str>,
        cached_tokens: Option<&[(String, String)]>,
        max_retries: u32,
        insecure_tls: bool,
    ) -> anyhow::Result<Self> {
        let relay_url = relay_url.trim_end_matches('/');
        let mut delay = tokio::time::Duration::from_secs(1);
        let max_delay = tokio::time::Duration::from_secs(300);

        for attempt in 0..=max_retries {
            match Self::connect_http(
                relay_url,
                fixed_key.clone(),
                token_type,
                desired_session_id,
                cached_tokens,
                insecure_tls,
            )
            .await
            {
                Ok(client) => return Ok(client),
                Err(e) => {
                    if attempt == max_retries {
                        return Err(e);
                    }
                    tracing::warn!(
                        "Connection attempt {} failed: {}. Retrying in {:?}...",
                        attempt + 1,
                        format!("{e:#}"),
                        delay
                    );
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, max_delay);
                }
            }
        }

        anyhow::bail!("Failed to connect after {} retries", max_retries)
    }

    pub(crate) fn http_client(&self) -> &reqwest::Client {
        &self.transport.client
    }

    pub(crate) fn send_url(&self) -> &str {
        &self.transport.send_url
    }

    /// The `--key` value used at registration ("" when unset). The desktop
    /// WS uplink reuses it for socket authentication.
    pub(crate) fn insecure_tls(&self) -> bool {
        self.insecure_tls
    }

    pub(crate) fn server_auth(&self) -> &str {
        &self.transport.server_auth
    }

    async fn recv_raw(&mut self) -> Option<String> {
        self.transport.events_rx.recv().await
    }

    pub async fn recv(&mut self) -> Option<ProtoMessage> {
        loop {
            match self.recv_raw().await {
                Some(text) => match serde_json::from_str::<ProtoMessage>(&text) {
                    Ok(msg) => return Some(msg),
                    Err(e) => {
                        tracing::warn!("Failed to parse relay message: {}", e);
                        continue;
                    }
                },
                None => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pump_sse_events_forwards_data_payloads() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let stream = tokio_stream::iter(vec![
            Ok::<_, std::io::Error>(b"data: hello\n\ndata: world\n\n".as_slice()),
        ]);
        pump_sse_events(stream, tx, std::time::Duration::from_secs(60)).await;
        assert_eq!(rx.try_recv().unwrap(), "hello");
        assert_eq!(rx.try_recv().unwrap(), "world");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_pump_sse_events_ignores_event_metadata_lines() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let stream = tokio_stream::iter(vec![
            Ok::<_, std::io::Error>(b"event: message\ndata: payload\n\n".as_slice()),
        ]);
        pump_sse_events(stream, tx, std::time::Duration::from_secs(60)).await;
        assert_eq!(rx.try_recv().unwrap(), "payload");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_pump_sse_events_returns_on_idle_timeout() {
        // A stream that never yields (simulating a silently killed / half-open
        // relay→agent SSE) must cause the pump to give up after the idle
        // timeout, so the agent can reconnect instead of blocking forever.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let stream = tokio_stream::pending::<Result<Vec<u8>, std::io::Error>>();
        let start = std::time::Instant::now();
        pump_sse_events(stream, tx, std::time::Duration::from_millis(120)).await;
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(100),
            "must wait for the idle timeout before giving up"
        );
        assert!(rx.try_recv().is_err(), "no events on a silent stream");
    }

    #[tokio::test]
    async fn test_pump_sse_events_returns_on_stream_end() {
        // A clean EOF (relay closed the stream) must end immediately, not wait
        // out the idle timeout.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let stream = tokio_stream::empty::<Result<Vec<u8>, std::io::Error>>();
        let start = std::time::Instant::now();
        pump_sse_events(stream, tx, std::time::Duration::from_secs(60)).await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "empty stream must end immediately, not wait for the timeout"
        );
        assert!(rx.try_recv().is_err());
    }

    /// --relay-url 允许 ws/wss 也用于 register/SSE：reqwest 是常规 HTTP
    /// 通道，ws://→http://、wss://→https:// 映射后请求才能发出。
    #[test]
    fn test_relay_scheme_normalization() {
        let base = "wss://127.0.0.1:3903".trim_end_matches('/');
        let http_base = base
            .replace("ws://", "http://")
            .replace("wss://", "https://");
        assert_eq!(http_base, "https://127.0.0.1:3903");
        let send_url = format!("{}/agent/send", http_base);
        assert_eq!(send_url, "https://127.0.0.1:3903/agent/send");

        let base = "http://127.0.0.1:3902".trim_end_matches('/');
        let http_base = base
            .replace("ws://", "http://")
            .replace("wss://", "https://");
        assert_eq!(http_base, "http://127.0.0.1:3902");
    }

    /// no-verify 连接器：以自签证书为服务端的 wss 握手必须放下可。
    #[tokio::test]
    async fn test_no_verify_connector_builds() {
        use tokio_tungstenite::Connector;
        let cfg = crate::tlsutil::no_verify_client_config();
        match Connector::Rustls(cfg) {
            Connector::Rustls(_) => {}
            _ => panic!("expected rustls connector"),
        }
        // 纯构建测试：真正握手已由人工端到端验证（agent --relay-insecure
        // wss:// 注册+上行+https 拉流），此处不依赖网络。
    }
}
