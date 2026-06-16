use anyhow::{bail, Context};
use tokio_stream::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;

use crate::proto::Message as ProtoMessage;

struct Transport {
    client: reqwest::Client,
    send_url: String,
    events_rx: mpsc::UnboundedReceiver<String>,
    last_event_id: Option<u64>,
    _task: tokio::task::JoinHandle<()>,
}

pub struct RelayClient {
    transport: Transport,
    pub session_id: String,
    pub tokens: Vec<(String, String)>,
}

impl RelayClient {
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
            let mut stream = match sse_client.get(&events_url)
                .header("Accept", "text/event-stream")
                .header("Cache-Control", "no-cache")
                .send().await
            {
                Ok(resp) => {
                    let status = resp.status();
                    let ct = resp.headers().get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    eprintln!("[agent] SSE connected: status={}, content-type={}", status, ct);
                    resp.bytes_stream()
                }
                Err(e) => {
                    eprintln!("[agent] SSE connection failed: {}", e);
                    return;
                }
            };

            let mut buf = String::new();
            let mut event_count: u64 = 0;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk_bytes) => {
                        let text = String::from_utf8_lossy(&chunk_bytes);
                        buf.push_str(&text);
                        while let Some(pos) = buf.find("\n\n") {
                            let event_str = buf[..pos].to_string();
                            buf = buf[pos + 2..].to_string();
                            for line in event_str.lines() {
                                if let Some(data) = line.strip_prefix("data:") {
                                    event_count += 1;
                                    if event_count <= 3 {
                                        eprintln!("[agent] SSE event #{}: {}", event_count, data.trim());
                                    }
                                    let _ = tx.send(data.trim().to_string());
                                }
                            }
                        }
                    }
                    Err(_) => {
                        eprintln!("[agent] SSE stream error after {} events", event_count);
                        break;
                    }
                }
            }
            eprintln!("[agent] SSE stream ended, {} events received", event_count);
        });

        let mut client = Self {
            transport: Transport {
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
        let relay_url = relay_url.trim_end_matches('/');
        let mut delay = tokio::time::Duration::from_secs(1);
        let max_delay = tokio::time::Duration::from_secs(300);

        for attempt in 0..=max_retries {
            match Self::connect_http(relay_url, fixed_key.clone(), token_type).await {
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
        let body: serde_json::Value = serde_json::from_str(text)
            .context("Failed to parse outgoing message")?;
        let resp = self.transport.client
            .post(&self.transport.send_url)
            .json(&body)
            .send()
            .await
            .context("Failed to POST agent message")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!("Agent POST failed ({}): {}", status, &body[..body.len().min(200)]);
        }
        Ok(())
    }

    pub async fn send(&mut self, msg: &ProtoMessage) -> anyhow::Result<()> {
        let text = serde_json::to_string(msg).context("Failed to serialize message")?;
        self.send_raw(&text).await
    }

    async fn recv_raw(&mut self) -> Option<String> {
        self.transport.events_rx.recv().await
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
