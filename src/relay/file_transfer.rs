#![allow(dead_code)]
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};

/// One event on a download's relay-internal sink. The relay response task
/// reads these to drive the HTTP response body; Chunk 0 decides 200 vs 500.
#[derive(Debug)]
pub enum DownloadEvent {
    Chunk(Vec<u8>),
    Error(String),
    End,
}

/// The relay-side handle for one in-flight download. `route_agent_message`
/// pushes decoded file bytes (or errors) into `tx`; the `get_handler` task
/// drains `tx` into the HTTP response body.
pub struct DownloadSink {
    pub tx: mpsc::Sender<DownloadEvent>,
    pub created_at: Instant,
    pub bytes: u64,
}

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

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use uuid::Uuid;

const CHUNK_SIZE: usize = 256 * 1024;

pub async fn get_handler(
    State(state): State<Arc<crate::relay::SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let client_ip = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).unwrap_or("unknown").to_string();
    {
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&client_ip, 60, std::time::Duration::from_secs(60)) {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }
    let token = headers.get("x-sr-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let (session_id, permission) = match state.sessions.authenticate(token).await {
        Some(r) => r,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let path = match params.get("path") {
        Some(p) => p.clone(),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    let bulk_tx = {
        let broadcast = state.agent_broadcast.read().await;
        match broadcast.get(&session_id).and_then(|cm| cm.agent_bulk.clone()) {
            Some(tx) => tx,
            None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    };
    let correlation_id = Uuid::new_v4().to_string();
    let (sink_tx, mut sink_rx) = mpsc::channel::<DownloadEvent>(16);
    {
        let mut ds = state.download_streams.write().await;
        ds.insert(correlation_id.clone(), DownloadSink { tx: sink_tx, created_at: std::time::Instant::now(), bytes: 0 });
    }
    // Send fs:read to agent on bulk channel.
    let proto = json!({"type":"fs:read","session_id":&session_id,
        "payload":{"path":&path,"_mcp_request_id":&correlation_id}});
    let _ = deliver_bulk(&bulk_tx, proto.to_string()).await;

    // Wait for chunk 0 to decide 200 vs 500.
    let first = sink_rx.recv().await;
    match first {
        Some(DownloadEvent::Error(msg)) => {
            state.download_streams.write().await.remove(&correlation_id);
            audit_ft(&state, &session_id, token, &permission, &path, 0, "downfile_failed", &msg).await;
            return (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error":msg}))).into_response();
        }
        Some(DownloadEvent::Chunk(first_bytes)) => {
            // Build streaming body: first_bytes + rest from sink_rx until End.
            // Use a spawned task + ReceiverStream so cleanup runs even on
            // client disconnect (task continues past the dropped body_rx).
            let first_len = first_bytes.len() as u64;
            let state_c = state.clone();
            let sid_c = session_id.clone();
            let token_c = token.to_string();
            let perm_c = permission.clone();
            let path_c = path.clone();
            let cid_c = correlation_id.clone();
            let (body_tx, body_rx) = mpsc::channel::<std::result::Result<Vec<u8>, std::convert::Infallible>>(16);
            tokio::spawn(async move {
                // Send first_bytes.
                if body_tx.send(Ok(first_bytes)).await.is_err() {
                    let _ = state_c.download_streams.write().await.remove(&cid_c);
                    audit_ft(&state_c, &sid_c, &token_c, &perm_c, &path_c, 0, "downfile_failed", "client disconnected").await;
                    return;
                }
                let mut total = first_len;
                while let Some(ev) = sink_rx.recv().await {
                    match ev {
                        DownloadEvent::Chunk(b) => {
                            let chunk_len = b.len() as u64;
                            if body_tx.send(Ok(b)).await.is_err() {
                                let _ = state_c.download_streams.write().await.remove(&cid_c);
                                audit_ft(&state_c, &sid_c, &token_c, &perm_c, &path_c, total, "downfile_failed", "client disconnected").await;
                                return;
                            }
                            total += chunk_len;
                        }
                        DownloadEvent::Error(e) => {
                            let _ = state_c.download_streams.write().await.remove(&cid_c);
                            audit_ft(&state_c, &sid_c, &token_c, &perm_c, &path_c, total, "downfile_failed", &e).await;
                            return;
                        }
                        DownloadEvent::End => break,
                    }
                }
                let _ = state_c.download_streams.write().await.remove(&cid_c);
                audit_ft(&state_c, &sid_c, &token_c, &perm_c, &path_c, total, "downfile", "").await;
            });
            let body = axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx));
            let mut resp = body.into_response();
            resp.headers_mut().insert("content-type", "application/octet-stream".parse().unwrap());
            resp
        }
        _ => {
            state.download_streams.write().await.remove(&correlation_id);
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}

pub async fn put_handler(
    State(state): State<Arc<crate::relay::SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: axum::body::Body,
) -> axum::response::Response {
    // Rate limit (mirrors upload_handler).
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    {
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&client_ip, 20, std::time::Duration::from_secs(60)) {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }
    let token = headers
        .get("x-sr-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let (session_id, permission) = match state.sessions.authenticate(token).await {
        Some(r) => r,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    use crate::proto::Permission;
    if permission == Permission::ReadOnly {
        return StatusCode::FORBIDDEN.into_response();
    }
    let path = match params.get("path") {
        Some(p) => p.clone(),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    let content_len = match headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(n) => n,
        None => return StatusCode::LENGTH_REQUIRED.into_response(),
    };
    let total_chunks = ((content_len as usize + CHUNK_SIZE - 1) / CHUNK_SIZE).max(1) as u32;
    let upload_id = Uuid::new_v4().to_string();

    let bulk_tx = {
        let broadcast = state.agent_broadcast.read().await;
        match broadcast
            .get(&session_id)
            .and_then(|cm| cm.agent_bulk.clone())
        {
            Some(tx) => tx,
            None => return StatusCode::SERVICE_UNAVAILABLE.into_response(), // no agent
        }
    };

    // Stream the body into CHUNK_SIZE chunks → fs:upload on bulk channel.
    use tokio_stream::StreamExt;
    let mut reader = body.into_data_stream();
    let mut chunk_index: u32 = 0;
    let mut bytes_sent: u64 = 0;
    let mut carry: Vec<u8> = Vec::new();
    loop {
        // Collect enough data for one chunk (or until EOF).
        while carry.len() < CHUNK_SIZE {
            match reader.next().await {
                Some(Ok(b)) => carry.extend_from_slice(&b),
                Some(Err(_)) => break,
                None => break,
            }
        }
        // Take at most CHUNK_SIZE bytes for this chunk.
        let send_len = carry.len().min(CHUNK_SIZE);
        let chunk_data: Vec<u8> = carry.drain(..send_len).collect();
        // Now `carry` holds any leftover bytes beyond CHUNK_SIZE.

        if chunk_data.is_empty() && chunk_index >= total_chunks {
            break;
        }
        if chunk_data.is_empty() && bytes_sent < content_len {
            // client closed prematurely (received fewer bytes than promised)
            // send abort to agent
            let abort = json!({"type":"fs:upload","session_id":&session_id,
                "payload":{"upload_id":&upload_id,"final_path":&path,"aborted":true,
                "_mcp_request_id":Uuid::new_v4().to_string()}});
            let _ = bulk_tx.send(abort.to_string()).await;
            audit_ft(
                &state,
                &session_id,
                token,
                &permission,
                &path,
                bytes_sent,
                "upfile_failed",
                "client closed",
            )
            .await;
            return StatusCode::BAD_REQUEST.into_response();
        }
        bytes_sent += chunk_data.len() as u64;
        let is_last = chunk_index + 1 >= total_chunks;
        let mcp_req_id = Uuid::new_v4().to_string();
        let payload = json!({
            "final_path": &path, "content": BASE64.encode(&chunk_data),
            "upload_id": &upload_id, "chunk_index": chunk_index, "total_chunks": total_chunks,
            "_mcp_request_id": &mcp_req_id
        });
        if is_last {
            // register oneshot, await agent fs:result
            let (tx, rx) = tokio::sync::oneshot::channel();
            state
                .pending_mcp
                .write()
                .await
                .insert(mcp_req_id.clone(), (session_id.clone(), tx));
            let proto = json!({"type":"fs:upload","session_id":&session_id,"payload":payload});
            let _ = deliver_bulk(&bulk_tx, proto.to_string()).await;
            match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
                Ok(Ok(result)) => {
                    let v: serde_json::Value =
                        serde_json::from_str(&result).unwrap_or_default();
                    if v.get("success").and_then(|b| b.as_bool()).unwrap_or(false) {
                        audit_ft(
                            &state,
                            &session_id,
                            token,
                            &permission,
                            &path,
                            bytes_sent,
                            "upfile",
                            "",
                        )
                        .await;
                        return axum::Json(json!({"ok":true,"bytes":bytes_sent})).into_response();
                    } else {
                        let err = v
                            .get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("agent write failed");
                        audit_ft(
                            &state,
                            &session_id,
                            token,
                            &permission,
                            &path,
                            bytes_sent,
                            "upfile_failed",
                            err,
                        )
                        .await;
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(json!({"error": err})),
                        )
                            .into_response();
                    }
                }
                _ => {
                    state.pending_mcp.write().await.remove(&mcp_req_id);
                    audit_ft(
                        &state,
                        &session_id,
                        token,
                        &permission,
                        &path,
                        bytes_sent,
                        "upfile_failed",
                        "timeout",
                    )
                    .await;
                    return StatusCode::GATEWAY_TIMEOUT.into_response();
                }
            }
        } else {
            let proto = json!({"type":"fs:upload","session_id":&session_id,"payload":payload});
            let _ = deliver_bulk(&bulk_tx, proto.to_string()).await;
            chunk_index += 1;
        }
    }
    // empty file (content_len == 0, total_chunks==1, loop sends chunk 0 as last)
    axum::Json(json!({"ok":true,"bytes":0u64})).into_response()
}

/// Audit helper for file-transfer endpoints. status ∈ {upfile, downfile, *_failed}.
async fn audit_ft(
    state: &crate::relay::SharedState,
    sid: &str,
    token: &str,
    perm: &crate::proto::Permission,
    path: &str,
    bytes: u64,
    status: &str,
    err: &str,
) {
    use crate::relay::recorder::{token_prefix, unix_ms, unix_ms_to_iso, AuditLine};
    let Some(recorder) = &state.recorder else {
        return;
    };
    let ms = unix_ms();
    let perm_str = match perm {
        crate::proto::Permission::ReadWrite => "rw",
        crate::proto::Permission::ReadOnly => "ro",
    };
    recorder.audit_mcp(
        sid,
        AuditLine {
            ts: unix_ms_to_iso(ms),
            unix_ms: ms,
            session_id: sid.to_string(),
            token_prefix: token_prefix(token),
            permission: perm_str.to_string(),
            cmd: path.to_string(), // path stored in cmd field
            timeout_ms: 0,
            duration_ms: 0,
            status: status.to_string(),
            exit_code: None,
            stdout_len: bytes as usize, // bytes stored in stdout_len
            stderr_len: if err.is_empty() { 0 } else { err.len() },
            stdout: String::new(), // never log content
            stderr: err.to_string(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::{Query, State};
    use axum::http::HeaderMap;
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

    fn make_state() -> Arc<crate::relay::SharedState> {
        Arc::new(crate::relay::SharedState::new(
            String::new(),
            100 * 1024 * 1024,
            None,
            String::new(),
            String::new(),
            None,
        ))
    }

    #[tokio::test]
    async fn test_put_unauthorized_no_token() {
        let state = make_state();
        let mut params = HashMap::new();
        params.insert("path".into(), "/x".into());
        let resp =
            put_handler(State(state), HeaderMap::new(), Query(params), Body::empty()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_put_readonly_forbidden() {
        let state = make_state();
        let (_sid, tokens) = state
            .sessions
            .register(None, "ro", None)
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        let mut params = HashMap::new();
        params.insert("path".into(), "/x".into());
        let resp =
            put_handler(State(state), headers, Query(params), Body::empty()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_put_missing_content_length_411() {
        let state = make_state();
        let (_sid, tokens) = state
            .sessions
            .register(None, "rw", None)
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        // Body without Content-Length → 411
        let mut params = HashMap::new();
        params.insert("path".into(), "/x".into());
        let resp =
            put_handler(State(state), headers, Query(params), Body::from("partial")).await;
        assert_eq!(resp.status(), axum::http::StatusCode::LENGTH_REQUIRED);
    }

    #[tokio::test]
    async fn test_put_streams_chunks_and_awaits_last_result() {
        let state = make_state();
        let (_sid, tokens) = state
            .sessions
            .register(None, "rw", None)
            .await
            .unwrap();
        let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            let cm = crate::relay::ChannelMap {
                agent: None,
                agent_bulk: Some(bulk_tx),
                browser_sessions: HashMap::new(),
            };
            broadcast.insert(_sid.clone(), cm);
        }
        // Exactly 3 chunks at CHUNK_SIZE each
        let data = vec![0xABu8; CHUNK_SIZE * 3];
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        headers.insert(
            "content-length",
            (data.len()).to_string().parse().unwrap(),
        );
        let mut params = HashMap::new();
        params.insert("path".into(), "/remote/x".into());

        // Drain bulk channel: respond to the last chunk's fs:result via pending_mcp.
        let state_c = state.clone();
        let h = tokio::spawn(async move {
            let mut last_req_id: Option<String> = None;
            let mut count = 0u32;
            while let Some(m) = bulk_rx.recv().await {
                let v: serde_json::Value = serde_json::from_str(&m).unwrap();
                if v["type"] == "fs:upload" {
                    count += 1;
                    let ci = v["payload"]["chunk_index"].as_u64().unwrap() as u32;
                    let tc = v["payload"]["total_chunks"].as_u64().unwrap() as u32;
                    if ci + 1 >= tc {
                        last_req_id = Some(
                            v["payload"]["_mcp_request_id"]
                                .as_str()
                                .unwrap()
                                .to_string(),
                        );
                        // fulfill oneshot
                        let mut pending = state_c.pending_mcp.write().await;
                        if let Some((_sid, tx)) = pending.remove(last_req_id.as_ref().unwrap()) {
                            let _ = tx
                                .send(serde_json::json!({"success":true}).to_string());
                        }
                        break;
                    }
                }
            }
            assert_eq!(count, 3, "exactly 3 chunks for CHUNK_SIZE*3 bytes");
        });

        let resp = put_handler(
            State(state),
            headers,
            Query(params),
            Body::from(data),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn test_get_streams_file_bytes_to_response() {
        let state = make_state();
        let (_sid, tokens) = state.sessions.register(None, "ro", None).await.unwrap(); // ro allowed for download
        let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast.insert(_sid.clone(), crate::relay::ChannelMap {
                agent: None, agent_bulk: Some(bulk_tx), browser_sessions: HashMap::new(),
            });
        }
        // Mock agent: when relay sends fs:read via bulk, push 2 fs:result chunks back.
        let state_c = state.clone();
        tokio::spawn(async move {
            let m = bulk_rx.recv().await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&m).unwrap();
            assert_eq!(v["type"], "fs:read");
            let cid = v["payload"]["_mcp_request_id"].as_str().unwrap().to_string();
            // push chunks via route_agent_message
            for (i, (bytes, is_last)) in [(b"a".to_vec(), false), (b"b".to_vec(), true)].iter().enumerate() {
                let chunk = serde_json::json!({
                    "type":"fs:result","session_id":&_sid,
                    "payload":{"success":true,"content":BASE64.encode(bytes),
                    "chunk_index":i,"total_chunks":2,"is_last":*is_last,
                    "_mcp_request_id":&cid}
                }).to_string();
                crate::relay::ws::route_agent_message(&state_c, &_sid, &chunk).await;
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        let mut params = HashMap::new(); params.insert("path".into(), "/remote/x".into());
        let resp = get_handler(State(state), headers, Query(params)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        use http_body_util::BodyExt;
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"ab");
    }

    #[tokio::test]
    async fn test_put_zero_byte_file_succeeds() {
        let state = make_state();
        let (_sid, tokens) = state
            .sessions
            .register(None, "rw", None)
            .await
            .unwrap();
        let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            let cm = crate::relay::ChannelMap {
                agent: None,
                agent_bulk: Some(bulk_tx),
                browser_sessions: HashMap::new(),
            };
            broadcast.insert(_sid.clone(), cm);
        }
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        headers.insert("content-length", "0".parse().unwrap());
        let mut params = HashMap::new();
        params.insert("path".into(), "/remote/empty".into());

        // Drain bulk channel: fulfill the lone chunk's fs:result via pending_mcp.
        let state_c = state.clone();
        let h = tokio::spawn(async move {
            while let Some(m) = bulk_rx.recv().await {
                let v: serde_json::Value = serde_json::from_str(&m).unwrap();
                if v["type"] == "fs:upload" {
                    let ci = v["payload"]["chunk_index"].as_u64().unwrap() as u32;
                    let tc = v["payload"]["total_chunks"].as_u64().unwrap() as u32;
                    assert_eq!(ci, 0);
                    assert_eq!(tc, 1);
                    assert_eq!(v["payload"]["content"].as_str().unwrap(), "");
                    let req_id = v["payload"]["_mcp_request_id"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    let mut pending = state_c.pending_mcp.write().await;
                    if let Some((_sid, tx)) = pending.remove(&req_id) {
                        let _ = tx.send(serde_json::json!({"success":true}).to_string());
                    }
                    break;
                }
            }
        });

        let resp = put_handler(
            State(state),
            headers,
            Query(params),
            Body::empty(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["bytes"], 0);
        h.await.unwrap();
    }
}
