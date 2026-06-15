use anyhow::{bail, Context};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use crate::proto::Message as ProtoMessage;

pub struct RelayClient {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    pub session_id: String,
    pub tokens: Vec<(String, String)>,
}

impl RelayClient {
    pub async fn connect(
        relay_url: &str,
        fixed_key: Option<String>,
        token_type: &str,
    ) -> anyhow::Result<Self> {
        let url = if let Some(key) = &fixed_key {
            format!("{}?key={}&token_type={}", relay_url, key, token_type)
        } else {
            format!("{}?token_type={}", relay_url, token_type)
        };

        let (ws, _) = connect_async(&url)
            .await
            .context("Failed to connect to relay WebSocket")?;

        let mut client = Self {
            ws,
            session_id: String::new(),
            tokens: Vec::new(),
        };

        let register_msg = json!({
            "type": "agent:register",
            "key": fixed_key,
            "token_type": token_type
        })
        .to_string();

        client
            .ws
            .send(Message::Text(register_msg))
            .await
            .context("Failed to send register message")?;

        match client.ws.next().await {
            Some(Ok(Message::Text(text))) => {
                let response: serde_json::Value = serde_json::from_str(&text)
                    .context("Failed to parse register response")?;

                let msg_type = response["type"].as_str().unwrap_or("");
                if msg_type != "agent:registered" {
                    bail!("Unexpected register response type: {}", msg_type);
                }

                client.session_id = response["session_id"]
                    .as_str()
                    .context("Missing session_id")?
                    .to_string();

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
            }
            Some(Ok(_)) => bail!("Unexpected message type on register"),
            Some(Err(e)) => bail!("WebSocket error during register: {}", e),
            None => bail!("WebSocket closed during register"),
        }

        Ok(client)
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
                        attempt + 1,
                        e,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, max_delay);
                }
            }
        }

        anyhow::bail!("Failed to connect after {} retries", max_retries)
    }

    pub async fn send(&mut self, msg: &ProtoMessage) -> anyhow::Result<()> {
        let text = serde_json::to_string(msg).context("Failed to serialize message")?;
        self.ws
            .send(Message::Text(text))
            .await
            .context("Failed to send WebSocket message")?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Option<ProtoMessage> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ProtoMessage>(&text) {
                        Ok(msg) => return Some(msg),
                        Err(e) => {
                            tracing::warn!("Failed to parse relay message: {}", e);
                            continue;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) => return None,
                Some(Ok(Message::Ping(data))) => {
                    if self.ws.send(Message::Pong(data)).await.is_err() {
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
}
