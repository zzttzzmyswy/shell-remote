#![allow(dead_code)]
use tokio::sync::mpsc;

/// Capacity for the per-session relay→agent bulk (file-transfer) sub-channel.
/// Deliberately smaller than [`SSE_CHANNEL_CAPACITY`] so file chunks yield to
/// interactive traffic under the agent's biased select.
pub const BULK_CHANNEL_CAPACITY: usize = 16;

/// Backpressure delivery to the bulk sub-channel. File chunks MUST NOT be
/// dropped (a dropped chunk corrupts the file), so this awaits on a full
/// channel instead of try_send. The interactive channel stays independent,
/// so this backpressure cannot stall interactive traffic. `msg` is a full
/// serialized `Message` JSON string — sent unchanged (no re-framing).
pub async fn deliver_bulk(tx: &mpsc::Sender<String>, msg: String) {
    let _ = tx.send(msg).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_deliver_bulk_never_drops() {
        // Capacity 2; send 4 full messages. deliver_bulk must await (not drop),
        // so all 4 arrive byte-identical.
        let (tx, mut rx) = mpsc::channel::<String>(2);
        let msgs: Vec<String> = (0..4u32)
            .map(|i| serde_json::json!({"type":"fs:upload","session_id":"s","payload":{"i":i}}).to_string())
            .collect();
        let msgs_clone = msgs.clone();
        let h = tokio::spawn(async move {
            for m in msgs_clone {
                deliver_bulk(&tx, m).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut got = Vec::new();
        while let Some(s) = rx.recv().await {
            got.push(s);
            if got.len() == 4 { break; }
        }
        h.await.unwrap();
        assert_eq!(got, msgs, "deliver_bulk must not drop or alter file chunks");
    }
}
