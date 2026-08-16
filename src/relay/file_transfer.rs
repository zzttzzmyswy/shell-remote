#![allow(dead_code)]
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};

/// One event on a download's relay-internal sink. The relay response task
/// reads these to drive the HTTP response body; Chunk 0 decides 200 vs 206.
#[derive(Debug)]
pub enum DownloadEvent {
    /// `file_size` is the full file length (agent reports it on every chunk;
    /// `None` from an old agent that predates the field).
    Chunk {
        data: Vec<u8>,
        file_size: Option<u64>,
    },
    /// `kind` categorizes the failure so the relay can map it to an HTTP
    /// status: not_found | is_directory | invalid_path | permission_denied | other.
    Error {
        kind: String,
        msg: String,
    },
    End,
}

/// The relay-side handle for one in-flight download. `route_agent_message`
/// pushes decoded file bytes (or errors) into `tx`; the `get_handler` task
/// drains `tx` into the HTTP response body.
#[derive(Debug, Clone)]
pub struct DownloadSink {
    pub tx: mpsc::Sender<DownloadEvent>,
    pub last_activity: Instant,
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

const CHUNK_SIZE: usize = 1024 * 1024;

/// Parse an HTTP Range header (`bytes=a-b` / `bytes=a-`). Returns `None` when
/// the header is absent, the unit is not bytes, or the range is a suffix
/// (`bytes=-n`, which needs the total length before the request is sent).
/// Single-range only, per the file-transfer design.
fn parse_range_header(h: Option<&str>) -> Option<(u64, Option<u64>)> {
    let h = h?.strip_prefix("bytes=")?;
    if h.is_empty() || h.starts_with('-') {
        return None;
    }
    let (start_s, end_s) = match h.split_once('-') {
        Some(p) => p,
        None => return None,
    };
    if start_s.is_empty() {
        return None;
    }
    let start: u64 = start_s.parse().ok()?;
    let end = if end_s.is_empty() {
        None
    } else {
        Some(end_s.parse::<u64>().ok()?)
    };
    Some((start, end))
}

/// Map an agent-reported error kind to an HTTP status code.
fn kind_to_status(kind: &str) -> StatusCode {
    match kind {
        "not_found" => StatusCode::NOT_FOUND,
        "is_directory" | "invalid_path" => StatusCode::BAD_REQUEST,
        "permission_denied" => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Extract a single `path` query value, rejecting duplicated keys.
/// Returns `Err(status)` for both missing and duplicated `path`.
fn extract_unique_path(params: Vec<(String, String)>) -> Result<String, StatusCode> {
    let mut path: Option<String> = None;
    for (k, v) in params {
        if k == "path" {
            if path.is_some() {
                return Err(StatusCode::BAD_REQUEST);
            }
            path = Some(v);
        }
    }
    path.ok_or(StatusCode::BAD_REQUEST)
}

/// Basename of a (URL-decoded) path for the Content-Disposition header.
fn download_filename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download")
        .to_string()
}

/// RFC 3986 percent-encode (reused by the MCP upload tool as well).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Content-Disposition value: ASCII names inline; non-ASCII via RFC 5987
/// `filename*` so Chinese filenames survive as UTF-8.
fn content_disposition(path: &str) -> String {
    let name = download_filename(path);
    let quoted: String = name
        .chars()
        .map(|c| match c {
            '"' | '\\' => format!("\\{}", c),
            c if c.is_ascii() => c.to_string(),
            _ => '?'.to_string(),
        })
        .collect();
    if name.is_ascii() {
        format!("attachment; filename=\"{}\"", quoted)
    } else {
        format!(
            "attachment; filename=\"download\"; filename*=UTF-8''{}",
            percent_encode(&name)
        )
    }
}

pub async fn get_handler(
    State(state): State<Arc<crate::relay::SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<Vec<(String, String)>>,
) -> axum::response::Response {
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    {
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&client_ip, 60, std::time::Duration::from_secs(60)) {
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
    let path = match extract_unique_path(params) {
        Ok(p) => p,
        Err(s) => return s.into_response(),
    };
    // Range [start, end] — end is exclusive-cap semantics (inclusive in HTTP);
    // a suffix range (`bytes=-n`) is ignored (None) and the full file is served.
    let range = parse_range_header(headers.get("range").and_then(|v| v.to_str().ok()));
    if let Some((s, Some(e))) = range {
        if e < s {
            return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        }
    }
    let bulk_tx = {
        let broadcast = state.agent_broadcast.read().await;
        match broadcast
            .get(&session_id)
            .and_then(|cm| cm.agent_bulk.clone())
        {
            Some(tx) => tx,
            None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    };
    let correlation_id = Uuid::new_v4().to_string();
    let (sink_tx, mut sink_rx) = mpsc::channel::<DownloadEvent>(16);
    {
        let mut ds = state.download_streams.write().await;
        ds.insert(
            correlation_id.clone(),
            DownloadSink {
                tx: sink_tx,
                last_activity: std::time::Instant::now(),
                bytes: 0,
            },
        );
    }
    // Send fs:read to agent on bulk channel. For a range request the offset and
    // limit ride optional payload fields; an old agent ignores them → full file.
    let mut read_payload = json!({
        "path": &path, "_mcp_request_id": &correlation_id
    });
    if let Some((start, end)) = range {
        read_payload["offset"] = json!(start);
        if let Some(e) = end {
            read_payload["limit"] = json!(e - start + 1);
        }
    }
    let proto = json!({"type":"fs:read","session_id":&session_id, "payload": read_payload});
    let _ = deliver_bulk(&bulk_tx, proto.to_string()).await;

    // Wait for chunk 0 to decide 200 vs 206 vs error status.
    let first = sink_rx.recv().await;
    match first {
        Some(DownloadEvent::Error { kind, msg }) => {
            state.download_streams.write().await.remove(&correlation_id);
            // Agent reported an error on chunk 0 — it has already stopped, but
            // send fs:read_cancel defensively (no-op if no cancel token).
            let proto = json!({"type":"fs:read_cancel","session_id":&session_id,
                "payload":{"_mcp_request_id":&correlation_id}});
            let _ = deliver_bulk(&bulk_tx, proto.to_string()).await;
            audit_ft(
                &state,
                &session_id,
                token,
                &permission,
                &path,
                0,
                "downfile_failed",
                &msg,
            )
            .await;
            let status = kind_to_status(&kind);
            (status, axum::Json(json!({"error":msg}))).into_response()
        }
        Some(DownloadEvent::Chunk {
            data: first_bytes,
            file_size,
        }) => {
            // Byte length of the requested range: from an explicit end, or from
            // the agent-reported file size for an open-ended (`bytes=a-`) range.
            let range_len: Option<u64> = if let Some((s, e)) = range {
                Some(if let Some(en) = e {
                    en - s + 1
                } else {
                    file_size.map_or(0, |t| t.saturating_sub(s))
                })
            } else {
                None
            };
            // 416 check: range start at/beyond EOF is unsatisfiable (only when
            // the agent reports the full size; includes empty files).
            if let (Some((start, _)), Some(total)) = (range, file_size) {
                if start >= total {
                    state.download_streams.write().await.remove(&correlation_id);
                    let proto = json!({"type":"fs:read_cancel","session_id":&session_id,
                        "payload":{"_mcp_request_id":&correlation_id}});
                    let _ = deliver_bulk(&bulk_tx, proto.to_string()).await;
                    return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                }
            }
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
            let bulk_c = bulk_tx.clone();
            let (body_tx, body_rx) =
                mpsc::channel::<std::result::Result<Vec<u8>, std::convert::Infallible>>(16);
            tokio::spawn(async move {
                // On any disconnect/abandon path, remove the sink from
                // `download_streams` AND send `fs:read_cancel` to the agent so it
                // stops reading/posting the whole file into a relay that drops
                // every chunk. Order: remove sink, THEN send cancel.
                let send_cancel = || {
                    let cid = cid_c.clone();
                    let sid = sid_c.clone();
                    let bulk = bulk_c.clone();
                    async move {
                        let proto = json!({"type":"fs:read_cancel","session_id":&sid,
                            "payload":{"_mcp_request_id":&cid}});
                        let _ = deliver_bulk(&bulk, proto.to_string()).await;
                    }
                };
                // Send first_bytes.
                if body_tx.send(Ok(first_bytes)).await.is_err() {
                    let _ = state_c.download_streams.write().await.remove(&cid_c);
                    send_cancel().await;
                    audit_ft(
                        &state_c,
                        &sid_c,
                        &token_c,
                        &perm_c,
                        &path_c,
                        0,
                        "downfile_failed",
                        "client disconnected",
                    )
                    .await;
                    return;
                }
                let mut total = first_len;
                let mut saw_end = false;
                while let Some(ev) = sink_rx.recv().await {
                    match ev {
                        DownloadEvent::Chunk { data: b, .. } => {
                            let chunk_len = b.len() as u64;
                            if body_tx.send(Ok(b)).await.is_err() {
                                let _ = state_c.download_streams.write().await.remove(&cid_c);
                                send_cancel().await;
                                audit_ft(
                                    &state_c,
                                    &sid_c,
                                    &token_c,
                                    &perm_c,
                                    &path_c,
                                    total,
                                    "downfile_failed",
                                    "client disconnected",
                                )
                                .await;
                                return;
                            }
                            total += chunk_len;
                        }
                        DownloadEvent::Error { msg, .. } => {
                            let _ = state_c.download_streams.write().await.remove(&cid_c);
                            send_cancel().await;
                            audit_ft(
                                &state_c,
                                &sid_c,
                                &token_c,
                                &perm_c,
                                &path_c,
                                total,
                                "downfile_failed",
                                &msg,
                            )
                            .await;
                            return;
                        }
                        DownloadEvent::End => {
                            saw_end = true;
                            break;
                        }
                    }
                }
                let _ = state_c.download_streams.write().await.remove(&cid_c);
                if saw_end {
                    audit_ft(
                        &state_c, &sid_c, &token_c, &perm_c, &path_c, total, "downfile", "",
                    )
                    .await;
                } else {
                    // sink_rx returned None without End — the sink was removed
                    // out from under the download (reaped by the inactivity
                    // reaper, or otherwise dropped). Honest status: failed.
                    send_cancel().await;
                    audit_ft(
                        &state_c,
                        &sid_c,
                        &token_c,
                        &perm_c,
                        &path_c,
                        total,
                        "downfile_failed",
                        "stream interrupted",
                    )
                    .await;
                }
            });
            let body =
                axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx));
            let mut resp = body.into_response();
            resp.headers_mut()
                .insert("content-type", "application/octet-stream".parse().unwrap());
            // Content-Disposition with the remote basename (RFC 5987 for non-ASCII).
            resp.headers_mut().insert(
                "content-disposition",
                content_disposition(&path).parse().unwrap(),
            );
            if range.is_some() {
                // HTTP status: partial content. Content-Range needs both a
                // known end and the full size (from a current agent).
                if let (Some((start, _)), Some(total), Some(len)) = (range, file_size, range_len) {
                    let end = start + len - 1;
                    resp.headers_mut().insert(
                        "content-range",
                        format!("bytes {}-{}/{}", start, end, total)
                            .parse()
                            .unwrap(),
                    );
                    resp.headers_mut()
                        .insert("content-length", len.to_string().parse().unwrap());
                }
                *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            } else if let Some(total) = file_size {
                // Non-range: send the known full length so clients see progress.
                resp.headers_mut()
                    .insert("content-length", total.to_string().parse().unwrap());
            }
            resp
        }
        _ => {
            state.download_streams.write().await.remove(&correlation_id);
            // Sink was reaped before chunk 0 arrived — tell the agent to stop.
            let proto = json!({"type":"fs:read_cancel","session_id":&session_id,
                "payload":{"_mcp_request_id":&correlation_id}});
            let _ = deliver_bulk(&bulk_tx, proto.to_string()).await;
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}

pub async fn put_handler(
    State(state): State<Arc<crate::relay::SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<Vec<(String, String)>>,
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
    let path = match extract_unique_path(params) {
        Ok(p) => p,
        Err(s) => return s.into_response(),
    };
    let content_len = match headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(n) => n,
        None => return StatusCode::LENGTH_REQUIRED.into_response(),
    };
    let total_chunks = (content_len as usize).div_ceil(CHUNK_SIZE).max(1) as u32;
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
        let mut stream_done = false;
        while carry.len() < CHUNK_SIZE {
            match reader.next().await {
                Some(Ok(b)) => carry.extend_from_slice(&b),
                Some(Err(_)) | None => {
                    stream_done = true;
                    break;
                }
            }
        }
        // Take at most CHUNK_SIZE bytes for this chunk.
        let send_len = carry.len().min(CHUNK_SIZE);
        let chunk_data: Vec<u8> = carry.drain(..send_len).collect();
        // Now `carry` holds any leftover bytes beyond CHUNK_SIZE.

        if chunk_data.is_empty() && chunk_index >= total_chunks {
            break;
        }
        // Premature close: the body stream ended (EOF or error) before
        // `content_len` bytes arrived. This covers BOTH the zero-leftover
        // case (chunk_data empty) and the short-final-chunk case (partial
        // chunk_data). Either way the remote file would be truncated vs the
        // declared Content-Length, so we must NOT report success — send the
        // abort message to the agent and return 400. (The 0-byte case is
        // excluded: content_len==0 → `0 < 0` is false.)
        if stream_done && bytes_sent + (chunk_data.len() as u64) < content_len {
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
                    let v: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();
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
                        let kind = v.get("kind").and_then(|e| e.as_str()).unwrap_or("other");
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
                        return (kind_to_status(kind), axum::Json(json!({"error": err})))
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
#[allow(clippy::too_many_arguments)]
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
            .map(|i| {
                serde_json::json!({"type":"fs:upload","session_id":"s","payload":{"i":i}})
                    .to_string()
            })
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
            if got.len() == 4 {
                break;
            }
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
        let params: Vec<(String, String)> = vec![("path".into(), "/x".into())];
        let resp = put_handler(State(state), HeaderMap::new(), Query(params), Body::empty()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_put_readonly_forbidden() {
        let state = make_state();
        let r = state.sessions.register(None, "ro", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        let params: Vec<(String, String)> = vec![("path".into(), "/x".into())];
        let resp = put_handler(State(state), headers, Query(params), Body::empty()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_put_missing_content_length_411() {
        let state = make_state();
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        // Body without Content-Length → 411
        let params: Vec<(String, String)> = vec![("path".into(), "/x".into())];
        let resp = put_handler(State(state), headers, Query(params), Body::from("partial")).await;
        assert_eq!(resp.status(), axum::http::StatusCode::LENGTH_REQUIRED);
    }

    #[tokio::test]
    async fn test_put_streams_chunks_and_awaits_last_result() {
        let state = make_state();
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
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
        headers.insert("content-length", (data.len()).to_string().parse().unwrap());
        let params: Vec<(String, String)> = vec![("path".into(), "/remote/x".into())];

        // Drain bulk channel: respond to the last chunk's fs:result via pending_mcp.
        let state_c = state.clone();
        let h = tokio::spawn(async move {
            let mut count = 0u32;
            while let Some(m) = bulk_rx.recv().await {
                let v: serde_json::Value = serde_json::from_str(&m).unwrap();
                if v["type"] == "fs:upload" {
                    count += 1;
                    let ci = v["payload"]["chunk_index"].as_u64().unwrap() as u32;
                    let tc = v["payload"]["total_chunks"].as_u64().unwrap() as u32;
                    if ci + 1 >= tc {
                        let req_id = v["payload"]["_mcp_request_id"]
                            .as_str()
                            .unwrap()
                            .to_string();
                        // fulfill oneshot
                        let mut pending = state_c.pending_mcp.write().await;
                        if let Some((_sid, tx)) = pending.remove(&req_id) {
                            let _ = tx.send(serde_json::json!({"success":true}).to_string());
                        }
                        break;
                    }
                }
            }
            assert_eq!(count, 3, "exactly 3 chunks for CHUNK_SIZE*3 bytes");
        });

        let resp = put_handler(State(state), headers, Query(params), Body::from(data)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn test_get_streams_file_bytes_to_response() {
        let state = make_state();
        let r = state.sessions.register(None, "ro", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
        let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast.insert(
                _sid.clone(),
                crate::relay::ChannelMap {
                    agent: None,
                    agent_bulk: Some(bulk_tx),
                    browser_sessions: HashMap::new(),
                },
            );
        }
        // Mock agent: when relay sends fs:read via bulk, push 2 fs:result chunks back.
        let state_c = state.clone();
        tokio::spawn(async move {
            let m = bulk_rx.recv().await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&m).unwrap();
            assert_eq!(v["type"], "fs:read");
            let cid = v["payload"]["_mcp_request_id"]
                .as_str()
                .unwrap()
                .to_string();
            // push chunks via route_agent_message
            for (i, (bytes, is_last)) in [(b"a".to_vec(), false), (b"b".to_vec(), true)]
                .iter()
                .enumerate()
            {
                let chunk = serde_json::json!({
                    "type":"fs:result","session_id":&_sid,
                    "payload":{"success":true,"content":BASE64.encode(bytes),
                    "chunk_index":i,"total_chunks":2,"is_last":*is_last,
                    "_mcp_request_id":&cid}
                })
                .to_string();
                crate::relay::ws::route_agent_message(&state_c, &_sid, &chunk).await;
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        let params: Vec<(String, String)> = vec![("path".into(), "/remote/x".into())];
        let resp = get_handler(State(state), headers, Query(params)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        use http_body_util::BodyExt;
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"ab");
    }

    #[tokio::test]
    async fn test_put_zero_byte_file_succeeds() {
        let state = make_state();
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
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
        let params: Vec<(String, String)> = vec![("path".into(), "/remote/empty".into())];

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

        let resp = put_handler(State(state), headers, Query(params), Body::empty()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["bytes"], 0);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn test_get_download_disconnect_sends_fs_read_cancel() {
        // Finding 1: when the HTTP body client disconnects mid-stream, the
        // get_handler body task must send fs:read_cancel to the agent bulk
        // channel so the agent stops streaming the rest of the file.
        let state = make_state();
        let r = state.sessions.register(None, "ro", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
        let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast.insert(
                _sid.clone(),
                crate::relay::ChannelMap {
                    agent: None,
                    agent_bulk: Some(bulk_tx),
                    browser_sessions: HashMap::new(),
                },
            );
        }
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let state_c = state.clone();
        let sid_c = _sid.clone();
        tokio::spawn(async move {
            // Read fs:read, extract correlation_id, push chunk 0 (non-final).
            let m = bulk_rx.recv().await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&m).unwrap();
            assert_eq!(v["type"], "fs:read");
            let cid = v["payload"]["_mcp_request_id"]
                .as_str()
                .unwrap()
                .to_string();
            let chunk0 = serde_json::json!({
                "type":"fs:result","session_id":&sid_c,
                "payload":{"success":true,"content":BASE64.encode(b"a"),
                "chunk_index":0,"total_chunks":2,"is_last":false,"_mcp_request_id":&cid}
            })
            .to_string();
            crate::relay::ws::route_agent_message(&state_c, &sid_c, &chunk0).await;
            // Wait for the test to drop the response body.
            go_rx.await.unwrap();
            // Push chunk 1 (final) — the body task's body_tx.send will fail
            // (client gone), triggering the fs:read_cancel path.
            let chunk1 = serde_json::json!({
                "type":"fs:result","session_id":&sid_c,
                "payload":{"success":true,"content":BASE64.encode(b"b"),
                "chunk_index":1,"total_chunks":2,"is_last":true,"_mcp_request_id":&cid}
            })
            .to_string();
            crate::relay::ws::route_agent_message(&state_c, &sid_c, &chunk1).await;
            // Drain bulk_rx until fs:read_cancel arrives.
            while let Some(m) = bulk_rx.recv().await {
                let v: serde_json::Value = serde_json::from_str(&m).unwrap();
                if v["type"] == "fs:read_cancel" {
                    assert_eq!(v["payload"]["_mcp_request_id"], cid);
                    let _ = done_tx.send(());
                    return;
                }
            }
            panic!("fs:read_cancel never arrived on bulk channel");
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        let params: Vec<(String, String)> = vec![("path".into(), "/remote/x".into())];
        let resp = get_handler(State(state), headers, Query(params)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        // Simulate client disconnect by dropping the response body.
        drop(resp);
        let _ = go_tx.send(());
        // fs:read_cancel must arrive within 5s.
        match tokio::time::timeout(std::time::Duration::from_secs(5), done_rx).await {
            Ok(Ok(())) => {}
            _ => panic!("fs:read_cancel not received within 5s"),
        }
    }

    #[tokio::test]
    async fn test_put_short_final_chunk_returns_400() {
        // Finding 2: a single-chunk upload where Content-Length exceeds the
        // actual body bytes (premature close) must return 400, not 200 — the
        // relay must not report a truncated upload as success.
        let state = make_state();
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
        let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast.insert(
                _sid.clone(),
                crate::relay::ChannelMap {
                    agent: None,
                    agent_bulk: Some(bulk_tx),
                    browser_sessions: HashMap::new(),
                },
            );
        }
        // Drain bulk: assert the abort message is sent.
        let h = tokio::spawn(async move {
            while let Some(m) = bulk_rx.recv().await {
                let v: serde_json::Value = serde_json::from_str(&m).unwrap();
                if v["type"] == "fs:upload" {
                    assert_eq!(v["payload"]["aborted"], true);
                    return;
                }
            }
            panic!("no abort message received");
        });

        // Declare 100 bytes but send only 3, then close the stream.
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        headers.insert("content-length", "100".parse().unwrap());
        let params: Vec<(String, String)> = vec![("path".into(), "/remote/x".into())];
        let resp = put_handler(State(state), headers, Query(params), Body::from("abc")).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn test_download_sink_last_activity_refreshed_on_chunk() {
        // Finding 4 (relay): route_agent_message must refresh
        // DownloadSink.last_activity on a non-final chunk so a slow-but-progressing
        // download is not reaped mid-stream.
        use crate::relay::file_transfer::{DownloadEvent, DownloadSink};
        use std::time::{Duration, Instant};
        let state = make_state();
        let (tx, mut rx) = mpsc::channel(16);
        let old = Instant::now() - Duration::from_secs(600);
        state.download_streams.write().await.insert(
            "dl-act".to_string(),
            DownloadSink {
                tx,
                last_activity: old,
                bytes: 0,
            },
        );
        let msg = serde_json::json!({
            "type":"fs:result","session_id":"sid1",
            "payload":{"success":true,"content":"aGk=","path":"/x",
            "chunk_index":0,"total_chunks":2,"is_last":false,"_mcp_request_id":"dl-act"}
        })
        .to_string();
        crate::relay::ws::route_agent_message(&state, "sid1", &msg).await;
        // Drain the chunk so the sender isn't back-pressured.
        let _ = rx.recv().await;
        let now = Instant::now();
        let sink = state.download_streams.read().await.get("dl-act").cloned();
        assert!(sink.is_some(), "non-final chunk must re-insert the sink");
        let la = sink.unwrap().last_activity;
        assert!(
            now.duration_since(la) < Duration::from_secs(1),
            "last_activity must be refreshed on chunk push"
        );
    }

    #[test]
    fn test_parse_range_header_variants() {
        assert_eq!(parse_range_header(None), None);
        assert_eq!(parse_range_header(Some("bytes=0-99")), Some((0, Some(99))));
        assert_eq!(parse_range_header(Some("bytes=100-")), Some((100, None)));
        // suffix ranges are ignored (full file served instead)
        assert_eq!(parse_range_header(Some("bytes=-500")), None);
        // non-bytes unit or garbage
        assert_eq!(parse_range_header(Some("items=0-9")), None);
        assert_eq!(parse_range_header(Some("bytes=abc")), None);
        assert_eq!(parse_range_header(Some("")), None);
        // multi-range is not supported → None
        assert_eq!(parse_range_header(Some("bytes=0-1,3-4")), None);
    }

    #[test]
    fn test_kind_to_status_mapping() {
        assert_eq!(kind_to_status("not_found"), StatusCode::NOT_FOUND);
        assert_eq!(kind_to_status("is_directory"), StatusCode::BAD_REQUEST);
        assert_eq!(kind_to_status("invalid_path"), StatusCode::BAD_REQUEST);
        assert_eq!(kind_to_status("permission_denied"), StatusCode::FORBIDDEN);
        assert_eq!(kind_to_status("other"), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(kind_to_status(""), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_extract_unique_path_variants() {
        let ok = vec![("path".into(), "/a".into())];
        assert_eq!(extract_unique_path(ok).unwrap(), "/a");
        // missing → Err(400)
        assert_eq!(extract_unique_path(vec![]), Err(StatusCode::BAD_REQUEST));
        // duplicate → Err(400)
        let dup = vec![("path".into(), "/a".into()), ("path".into(), "/b".into())];
        assert_eq!(extract_unique_path(dup), Err(StatusCode::BAD_REQUEST));
        // other keys are ignored
        let extra = vec![("foo".into(), "1".into()), ("path".into(), "/x".into())];
        assert_eq!(extract_unique_path(extra).unwrap(), "/x");
    }

    #[test]
    fn test_content_disposition_header() {
        assert_eq!(
            content_disposition("/tmp/report.pdf"),
            "attachment; filename=\"report.pdf\""
        );
        assert_eq!(
            content_disposition("C:\\dir\\x.bin"),
            "attachment; filename=\"x.bin\""
        );
        let cn = content_disposition("/tmp/中文文件.txt");
        assert!(
            cn.starts_with("attachment; filename=\"download\"; filename*=UTF-8''"),
            "got: {cn}"
        );
        assert!(
            cn.contains("%E4%B8%AD"),
            "chinese must be percent-encoded, got: {cn}"
        );
        assert_eq!(download_filename("/tmp/中文文件.txt"), "中文文件.txt");
    }

    #[tokio::test]
    async fn test_get_duplicate_path_params_rejected() {
        let state = make_state();
        let r = state.sessions.register(None, "ro", None).await.unwrap();
        let tokens = r.tokens;
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        let params: Vec<(String, String)> =
            vec![("path".into(), "/a".into()), ("path".into(), "/b".into())];
        let resp = get_handler(State(state), headers, Query(params)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_get_error_kind_maps_to_http_status() {
        // not_found → 404 (old behavior was always 500)
        let state = make_state();
        let r = state.sessions.register(None, "ro", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
        let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast.insert(
                _sid.clone(),
                crate::relay::ChannelMap {
                    agent: None,
                    agent_bulk: Some(bulk_tx),
                    browser_sessions: HashMap::new(),
                },
            );
        }
        let state_c = state.clone();
        let sid_c = _sid.clone();
        tokio::spawn(async move {
            let m = bulk_rx.recv().await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&m).unwrap();
            assert_eq!(v["type"], "fs:read");
            let cid = v["payload"]["_mcp_request_id"]
                .as_str()
                .unwrap()
                .to_string();
            let err = serde_json::json!({
                "type":"fs:result","session_id":&sid_c,
                "payload":{"success":false,"error":"no such file","kind":"not_found",
                "_mcp_request_id":&cid}
            })
            .to_string();
            crate::relay::ws::route_agent_message(&state_c, &sid_c, &err).await;
        });
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        let params: Vec<(String, String)> = vec![("path".into(), "/nonexistent".into())];
        let resp = get_handler(State(state), headers, Query(params)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_range_forwards_offset_limit_returns_206() {
        let state = make_state();
        let r = state.sessions.register(None, "ro", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
        let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast.insert(
                _sid.clone(),
                crate::relay::ChannelMap {
                    agent: None,
                    agent_bulk: Some(bulk_tx),
                    browser_sessions: HashMap::new(),
                },
            );
        }
        // Read the fs:read payload, assert offset/limit ride along, then push
        // a single final chunk matching the requested range.
        let state_c = state.clone();
        let sid_c = _sid.clone();
        tokio::spawn(async move {
            let m = bulk_rx.recv().await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&m).unwrap();
            assert_eq!(v["type"], "fs:read");
            assert_eq!(v["payload"]["offset"], serde_json::json!(10));
            assert_eq!(v["payload"]["limit"], serde_json::json!(5));
            let cid = v["payload"]["_mcp_request_id"]
                .as_str()
                .unwrap()
                .to_string();
            let chunk = serde_json::json!({
                "type":"fs:result","session_id":&sid_c,
                "payload":{"success":true,"content":BASE64.encode(b"hello"),
                "chunk_index":0,"total_chunks":1,"is_last":true,
                "file_size":30,"_mcp_request_id":&cid}
            })
            .to_string();
            crate::relay::ws::route_agent_message(&state_c, &sid_c, &chunk).await;
        });
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        headers.insert("range", "bytes=10-14".parse().unwrap());
        let params: Vec<(String, String)> = vec![("path".into(), "/remote/x".into())];
        let resp = get_handler(State(state), headers, Query(params)).await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get("content-range").unwrap(),
            "bytes 10-14/30"
        );
        assert_eq!(resp.headers().get("content-length").unwrap(), "5");
        assert_eq!(
            resp.headers().get("content-disposition").unwrap(),
            "attachment; filename=\"x\""
        );
        use http_body_util::BodyExt;
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"hello");
    }

    #[tokio::test]
    async fn test_get_range_unsatisfiable_416() {
        let state = make_state();
        let r = state.sessions.register(None, "ro", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
        let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast.insert(
                _sid.clone(),
                crate::relay::ChannelMap {
                    agent: None,
                    agent_bulk: Some(bulk_tx),
                    browser_sessions: HashMap::new(),
                },
            );
        }
        // Agent replies with an empty chunk carrying file_size=10 (EOF reached).
        let state_c = state.clone();
        let sid_c = _sid.clone();
        tokio::spawn(async move {
            let m = bulk_rx.recv().await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&m).unwrap();
            let cid = v["payload"]["_mcp_request_id"]
                .as_str()
                .unwrap()
                .to_string();
            let chunk = serde_json::json!({
                "type":"fs:result","session_id":&sid_c,
                "payload":{"success":true,"content":"","chunk_index":0,"total_chunks":1,
                "is_last":true,"file_size":10,"_mcp_request_id":&cid}
            })
            .to_string();
            crate::relay::ws::route_agent_message(&state_c, &sid_c, &chunk).await;
        });
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        headers.insert("range", "bytes=20-".parse().unwrap());
        let params: Vec<(String, String)> = vec![("path".into(), "/remote/x".into())];
        let resp = get_handler(State(state), headers, Query(params)).await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[tokio::test]
    async fn test_get_full_file_has_content_disposition_and_length() {
        let state = make_state();
        let r = state.sessions.register(None, "ro", None).await.unwrap();
        let _sid = r.session_id;
        let tokens = r.tokens;
        let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast.insert(
                _sid.clone(),
                crate::relay::ChannelMap {
                    agent: None,
                    agent_bulk: Some(bulk_tx),
                    browser_sessions: HashMap::new(),
                },
            );
        }
        let state_c = state.clone();
        let sid_c = _sid.clone();
        tokio::spawn(async move {
            let m = bulk_rx.recv().await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&m).unwrap();
            let cid = v["payload"]["_mcp_request_id"]
                .as_str()
                .unwrap()
                .to_string();
            let chunk = serde_json::json!({
                "type":"fs:result","session_id":&sid_c,
                "payload":{"success":true,"content":BASE64.encode(b"hi"),
                "chunk_index":0,"total_chunks":1,"is_last":true,
                "file_size":2,"_mcp_request_id":&cid}
            })
            .to_string();
            crate::relay::ws::route_agent_message(&state_c, &sid_c, &chunk).await;
        });
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        let params: Vec<(String, String)> = vec![("path".into(), "/remote/report.txt".into())];
        let resp = get_handler(State(state), headers, Query(params)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-disposition").unwrap(),
            "attachment; filename=\"report.txt\""
        );
        assert_eq!(resp.headers().get("content-length").unwrap(), "2");
    }
}
