use anyhow::{bail, Context};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use crate::proto::Message as ProtoMessage;

enum Transport {
    Ws {
        ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    },
    Http {
        client: reqwest::Client,
        send_url: String,
        events_rx: mpsc::UnboundedReceiver<String>,
        last_event_id: Option<u64>,
        _task: tokio::task::JoinHandle<()>,
    },
}

pub struct RelayClient {
    transport: Transport,
    pub session_id: String,
    pub tokens: Vec<(String, String)>,
}

impl RelayClient {
    pub async fn connect(
        relay_url: &str,
        fixed_key: Option<String>,
        token_type: &str,
    ) -> anyhow::Result<Self> {
        if relay_url.starts_with("ws://") || relay_url.starts_with("wss://") {
            Self::connect_ws(relay_url, fixed_key, token_type).await
        } else {
            Self::connect_http(relay_url, fixed_key, token_type).await
        }
    }

    async fn connect_ws(
        relay_url: &str,
        fixed_key: Option<String>,
        token_type: &str,
    ) -> anyhow::Result<Self> {
        let url = format!("{}/agent", relay_url.trim_end_matches('/'));
        let url = if let Some(key) = &fixed_key {
            format!("{}?key={}&token_type={}", url, key, token_type)
        } else {
            format!("{}?token_type={}", url, token_type)
        };

        let (ws, _) = connect_async(&url)
            .await
            .context("Failed to connect to relay WebSocket")?;

        let mut client = Self {
            transport: Transport::Ws { ws },
            session_id: String::new(),
            tokens: Vec::new(),
        };

        let register_msg = json!({
            "type": "agent:register",
            "key": fixed_key,
            "token_type": token_type
        }).to_string();

        client.send_raw(&register_msg).await?;

        let response_text = client.recv_raw().await
            .context("Failed to receive register response")?;
        let response: serde_json::Value = serde_json::from_str(&response_text)
            .context("Failed to parse register response")?;

        Self::handle_register_response(&mut client, &response)?;
        Ok(client)
    }

    async fn connect_http(
        relay_url: &str,
        fixed_key: Option<String>,
        token_type: &str,
    ) -> anyhow::Result<Self> {
        let base = relay_url.trim_end_matches('/');
        let send_url = format!("{}/agent/send", base);

        let http_client = reqwest::Client::new();

        let register_msg = json!({
            "type": "agent:register",
            "key": fixed_key,
            "token_type": token_type
        });

        let resp = http_client.post(&send_url)
            .json(&register_msg)
            .send()
            .await
            .context("Failed to POST register message")?;

        let status = resp.status();
        let body_text = resp.text().await.context("Failed to read register response")?;

        if !status.is_success() {
            anyhow::bail!("Registration failed (HTTP {}): {}", status, body_text);
        }

        let response: serde_json::Value = serde_json::from_str(&body_text)
            .with_context(|| format!("Failed to parse register response (status {}): {}", status, &body_text[..body_text.len().min(500)]))?;

        let session_id = response["session_id"].as_str()
            .context("Missing session_id in register response")?.to_string();

        let events_url = format!("{}/agent/events?session={}", base, session_id);

        let (tx, rx) = mpsc::unbounded_channel::<String>();

        let sse_client = http_client.clone();
        let sse_task = tokio::spawn(async move {
            let mut stream = match sse_client.get(&events_url).send().await {
                Ok(resp) => resp.bytes_stream(),
                Err(e) => {
                    tracing::error!("SSE connection failed: {}", e);
                    return;
                }
            };

            let mut buf = String::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buf.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(pos) = buf.find("\n\n") {
                            let event_str = buf[..pos].to_string();
                            buf = buf[pos + 2..].to_string();
                            for line in event_str.lines() {
                                if let Some(data) = line.strip_prefix("data:") {
                                    let _ = tx.send(data.trim().to_string());
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut client = Self {
            transport: Transport::Http {
                client: http_client,
                send_url,
                events_rx: rx,
                last_event_id: None,
                _task: sse_task,
            },
            session_id: String::new(),
            tokens: Vec::new(),
        };

        Self::handle_register_response(&mut client, &response)?;
        Ok(client)
    }

    fn handle_register_response(client: &mut Self, response: &serde_json::Value) -> anyhow::Result<()> {
        let msg_type = response["type"].as_str().unwrap_or("");
        if msg_type != "agent:registered" {
            bail!("Unexpected register response type: {}", msg_type);
        }

        client.session_id = response["session_id"].as_str()
            .context("Missing session_id")?.to_string();

        let payload = &response["payload"];
        if let Some(tokens_array) = payload["tokens"].as_array() {
            for t in tokens_array {
                let token = t["token"].as_str()
                    .context("Missing token in tokens array")?.to_string();
                let permission = t["permission"].as_str()
                    .context("Missing permission in tokens array")?.to_string();
                client.tokens.push((token, permission));
            }
        }

        Ok(())
    }

    pub async fn connect_with_retry(
        relay_url: &str,
        fixed_key: Option<String>,
        token_type: &str,
        max_retries: u32,
    ) -> anyhow::Result<Self> {
        let mut delay = tokio::time::Duration::from_secs(1);
        let max_delay = tokio::time::Duration::from_secs(300);

        for attempt in 0..=max_retries {
            match Self::connect(relay_url, fixed_key.clone(), token_type).await {
                Ok(client) => return Ok(client),
                Err(e) => {
                    if attempt == max_retries {
                        return Err(e);
                    }
                    tracing::warn!(
                        "Connection attempt {} failed: {}. Retrying in {:?}...",
                        attempt + 1, e, delay
                    );
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, max_delay);
                }
            }
        }

        anyhow::bail!("Failed to connect after {} retries", max_retries)
    }

    async fn send_raw(&mut self, text: &str) -> anyhow::Result<()> {
        match &mut self.transport {
            Transport::Ws { ws } => {
                ws.send(Message::Text(text.to_string()))
                    .await
                    .context("Failed to send WebSocket message")?;
            }
            Transport::Http { client, send_url, .. } => {
                let body: serde_json::Value = serde_json::from_str(text)
                    .context("Failed to parse outgoing message")?;
                let resp = client.post(send_url.as_str())
                    .json(&body)
                    .send()
                    .await
                    .context("Failed to POST agent message")?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!("Agent POST failed ({}): {}", status, &body[..body.len().min(200)]);
                }
            }
        }
        Ok(())
    }

    pub async fn send(&mut self, msg: &ProtoMessage) -> anyhow::Result<()> {
        let text = serde_json::to_string(msg).context("Failed to serialize message")?;
        self.send_raw(&text).await
    }

    async fn recv_raw(&mut self) -> Option<String> {
        match &mut self.transport {
            Transport::Ws { ws } => {
                loop {
                    match ws.next().await {
                        Some(Ok(Message::Text(text))) => return Some(text.to_string()),
                        Some(Ok(Message::Close(_))) => return None,
                        Some(Ok(Message::Ping(data))) => {
                            if ws.send(Message::Pong(data)).await.is_err() {
                                return None;
                            }
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => {
                            tracing::warn!("WebSocket error: {}", e);
                            return None;
                        }
                        None => return None,
                    }
                }
            }
            Transport::Http { events_rx, .. } => {
                events_rx.recv().await
            }
        }
    }

    pub async fn recv(&mut self) -> Option<ProtoMessage> {
        loop {
            match self.recv_raw().await {
                Some(text) => {
                    match serde_json::from_str::<ProtoMessage>(&text) {
                        Ok(msg) => return Some(msg),
                        Err(e) => {
                            tracing::warn!("Failed to parse relay message: {}", e);
                            continue;
                        }
                    }
                }
                None => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_placeholder() {
        assert!(true);
    }
}
