use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response, Sse};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use uuid::Uuid;

use axum::http::StatusCode;

use crate::proto::{requires_write, Message as ProtoMessage, Permission, TokenType};

use crate::relay::{ChannelMap, SharedState, MAX_SESSIONS, SSE_CHANNEL_CAPACITY};

/// Message types that are safe to silently drop when a downstream SSE channel
/// is full: they are either high-volume output (terminal frames) or state
/// broadcasts that are superseded by newer ones. Dropping them under a stuck
/// consumer degrades only that consumer; non-lossy types (input, file chunks,
/// results, control) are logged when dropped so a stuck session is visible
/// without stalling other sessions.
fn is_lossy_msg_type(t: &str) -> bool {
    matches!(
        t,
        "terminal:output"
            | "terminal:resize"
            | "session:users"
            | "session:tab_list"
            | "session:tab_switched"
    )
}

/// Non-blocking delivery to a bounded SSE channel. Never awaits, so it is safe
/// to call while holding a read lock. On overflow, lossy types are dropped
/// silently and everything else is dropped with a warning — the alternative
/// (unbounded buffering) would let one stuck session OOM the whole relay and
/// stall every other session.
pub fn deliver(tx: &mpsc::Sender<String>, msg_type: &str, msg: String) {
    match tx.try_send(msg) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            if !is_lossy_msg_type(msg_type) {
                tracing::warn!(
                    "SSE channel full; dropping non-lossy {} message for a stuck/downstream session",
                    msg_type
                );
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

/// 批次5 告警判据：内容活跃（非静止）但编码帧率 <10 —— 动态内容异常降帧
/// （用户铁律：动态画面不降帧，正常动态 fps ≥15）。静止（active=false，
/// fps=1）属正常，不告警。纯函数便于单测。
fn kpi_anomalous(active: bool, fps: u32) -> bool {
    active && fps < 10
}

// ── Shared agent message routing ─────────────────────────────────────

pub async fn route_agent_message(state: &Arc<SharedState>, session_id: &str, text_str: &str) {
    // Agent 心跳（15s）格式特殊：`{"type":"ping","session_id":...,"kpi":{...}}`
    // **没有 payload 字段**，严格的 [`ProtoMessage`] 反序列化会因
    // `missing field 'payload'` 失败（真实心跳现即如此）。因此在严格解析
    // 之前先用宽松 JSON 检查 ping：采样桌面 KPI 进 admin 曲线历史后拦截
    // （ping 是 agent→relay 保活，无需转发浏览器；R5 丙111/140 admin KPI）。
    if let Ok(loose) = serde_json::from_str::<serde_json::Value>(text_str) {
        if loose.get("type").and_then(|v| v.as_str()) == Some("ping") {
            if let Some(k) = loose.get("kpi") {
                let at_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let sample = crate::relay::AgentKpiSample {
                    at_unix_ms,
                    running: k.get("running").and_then(|v| v.as_bool()).unwrap_or(false),
                    codec: k
                        .get("codec")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    fps: k.get("fps").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    quality_permille: k
                        .get("quality_permille")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32,
                    bitrate_kbps: k
                        .get("bitrate_kbps")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32,
                    encode_ms: k.get("encode_ms").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    active: k.get("active").and_then(|v| v.as_bool()).unwrap_or(false),
                    bp_count: k.get("bp_count").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    rss_kb: k.get("rss_kb").and_then(|v| v.as_u64()).unwrap_or(0),
                    cpu_ms: k.get("cpu_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                    cpu_temp: k.get("cpu_temp").and_then(|v| v.as_f64()).unwrap_or(0.0),
                };
                // 批次5 告警雏形：active 且 fps<10（动态内容异常降帧）→ 告警
                // 日志（≥30s/会话限频）。静止（active=false,fps=1）不告警。
                if sample.running && kpi_anomalous(sample.active, sample.fps) {
                    let should_alert = {
                        let guard = state.last_kpi_alert.read().await;
                        guard
                            .get(session_id)
                            .map_or(true, |t| t.elapsed() >= std::time::Duration::from_secs(30))
                    };
                    if should_alert {
                        state
                            .last_kpi_alert
                            .write()
                            .await
                            .insert(session_id.to_string(), std::time::Instant::now());
                        tracing::warn!(
                            session = %session_id,
                            fps = sample.fps,
                            active = sample.active,
                            "KPI anomaly: dynamic content at abnormally low fps (R5 告警)"
                        );
                    }
                }
                let mut hist = state.kpi_history.write().await;
                let dq = hist.entry(session_id.to_string()).or_default();
                if dq.len() >= crate::relay::KPI_HISTORY_CAP {
                    dq.pop_front();
                }
                dq.push_back(sample);
            }
            return;
        }
    }
    if let Ok(proto_msg) = serde_json::from_str::<ProtoMessage>(text_str) {
        // Agent self-upgrade progress: capture it for the admin panel's
        // devices page (polled via /api/overview) and stop here — the frame
        // is relay→admin telemetry, not something browsers or MCP callers see.
        if proto_msg.msg_type == "agent:upgrade_progress" {
            state
                .agent_upgrades
                .write()
                .await
                .insert(session_id.to_string(), proto_msg.payload.clone());
            return;
        }
        // Recording: capture terminal:output for the session's cast file.
        if proto_msg.msg_type == "terminal:output" {
            if let Some(rec) = &state.recorder {
                if let Some(data) = proto_msg.payload.get("data").and_then(|v| v.as_str()) {
                    rec.record(
                        session_id,
                        crate::relay::recorder::RecordEvent::Output(
                            crate::relay::recorder::decode_terminal_data(data),
                        ),
                    );
                }
            }
        }
        // Desktop video: fan out init/fragments to browsers subscribed on
        // /agent/desktop/stream. Own channel — never goes through the normal
        // broadcast list or the replay EventBuffer (fragments are large and
        // of no use to late SSE joiners — the desktop stream replays its own
        // init cache).
        if proto_msg.msg_type == "desktop:video" {
            let kind = proto_msg.payload.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let data_b64 = proto_msg.payload.get("data").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(bytes) = crate::agent::fs::decode_b64(data_b64) {
                let ds = {
                    let mut streams = state.desktop_streams.write().await;
                    streams
                        .entry(session_id.to_string())
                        .or_insert_with(crate::relay::desktop::DesktopStream::new)
                        .clone()
                };
                tracing::debug!("desktop:video kind={kind} bytes={}", bytes.len());
                if kind == "init" {
                    ds.set_init(bytes).await;
                } else {
                    let is_key = proto_msg
                        .payload
                        .get("key")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let congested = ds.push_frag(is_key, bytes).await;
                    // R5#16 回传拥塞信号：fan-out 丢旧保新发生（viewer 消费
                    // 跟不上——弱网/页面休眠/切后台）→ 限频（≥5s）向 agent
                    // 回传 desktop:congested。agent 侧作为"relay→浏览器传输段
                    // 拥塞"证据参与弱网归因（浏览器 qos 上报的 e2e/dq 是
                    // 浏览器段，relay drop 是传输段，两者互补）。仅回 agent，
                    // 不进 broadcast_types，也不入 EventBuffer（现发现报）。
                    if congested > 0 {
                        let should_notify = {
                            let guard = state.last_congest_notify.read().await;
                            guard
                                .get(session_id)
                                .map_or(true, |t| t.elapsed() >= std::time::Duration::from_secs(5))
                        };
                        if should_notify {
                            state
                                .last_congest_notify
                                .write()
                                .await
                                .insert(session_id.to_string(), std::time::Instant::now());
                            if let Some(tx) = state
                                .agent_broadcast
                                .read()
                                .await
                                .get(session_id)
                                .and_then(|cm| cm.agent.clone())
                            {
                                let _ = deliver(
                                    &tx,
                                    "desktop:congested",
                                    serde_json::json!({
                                        "type": "desktop:congested",
                                        "session_id": session_id,
                                        "payload": { "dropped": congested }
                                    })
                                    .to_string(),
                                );
                            }
                        }
                    }
                }
            }
            return;
        }

        // 桌面控制事件必须回到浏览器（web/session.js 监听这三个）。
        // desktop:stopped 同时清理该会话的 fan-out 流，避免 DesktopStream
        // 长期残留在 map 中。
        // desktop:started 要**预建** fan-out 流：浏览器收到 started 后立即
        // GET /agent/desktop/stream，若此时流尚未被首个 desktop:video(init)
        // 懒创建，会立刻 404 → 黑屏（真实用户复现）。预建后 viewer 加入即
        // 挂起等待，init 一到就全量广播。
        if proto_msg.msg_type == "desktop:started" {
            // 桌面流生命周期追踪（R2 丙129 / R5#24）：创建原因入日志，供
            // 复盘"流为何存在/何时创建"，避免长期残留无从查起。
            state
                .desktop_streams
                .write()
                .await
                .entry(session_id.to_string())
                .or_insert_with(crate::relay::desktop::DesktopStream::new);
            // R5#12：记录当前运行状态，供 SSE（重建）握手补发 desktop:state。
            state
                .desktop_states
                .write()
                .await
                .insert(session_id.to_string(), true);
            tracing::info!(
                session = %session_id,
                reason = "desktop:started",
                "desktop stream created/pre-existed"
            );
        }
        if proto_msg.msg_type == "desktop:stopped" {
            // 生命周期追踪：agent 主动停止 → 移除并记原因。
            let removed = state.desktop_streams.write().await.remove(session_id);
            state
                .desktop_states
                .write()
                .await
                .insert(session_id.to_string(), false);
            tracing::info!(
                session = %session_id,
                reason = "desktop:stopped",
                existed = removed.is_some(),
                "desktop stream removed"
            );
        }

        let broadcast_types = [
            "session:users",
            "session:tab_list",
            "session:tab_switched",
            "terminal:output",
            "fs:result",
            "fs:mkdir",
            "mcp:result",
            "desktop:started",
            "desktop:stopped",
            "desktop:error",
            "desktop:capabilities",
            "desktop:uplink",
            "desktop:clipboard",
            "desktop:qos-ack",
            "desktop:cmd-ack",
            "desktop:cursor",
            "test-delay-ack",
        ];
        if broadcast_types.contains(&proto_msg.msg_type.as_str()) {
            // fs:result is browser-facing (file manager reads + downloads).
            // A browser download reuses `_mcp_request_id` as its correlation
            // id, so it must still be broadcast — treating it as an MCP RPC
            // reply would swallow it (the download was never registered in
            // pending_mcp). MCP RPC replies use mcp:result / mcp:exec_result,
            // never fs:result. The fs:result oneshot block below only fires
            // for entries actually in pending_mcp, so it stays harmless.
            let is_mcp_rpc = proto_msg.msg_type != "fs:result"
                && proto_msg.payload.get("_mcp_request_id").is_some();
            if !is_mcp_rpc {
                let sse_sessions = state.sse_sessions.read().await;
                let broadcast = state.agent_broadcast.read().await;
                if let Some(channel_map) = broadcast.get(session_id) {
                    let target_user = proto_msg
                        .payload
                        .get("_target_user_id")
                        .and_then(|v| v.as_str());
                    for (uid, sse_sid) in &channel_map.browser_sessions {
                        if target_user.is_none_or(|t| t == uid.as_str()) {
                            if let Some(tx) = sse_sessions.get(sse_sid) {
                                // R5#29 控制消息优先级（腾位窗口）：lossy 数据
                                // （terminal:output 等）维持 try_send 静默丢；
                                // non-lossy 控制消息在 channel 满时给 100ms
                                // 腾位窗口（浏览器消费端正在排空）——弱网/瞬间
                                // 积压下控制消息不被数据挤掉；仍满才告警丢。
                                if is_lossy_msg_type(&proto_msg.msg_type) {
                                    deliver(tx, &proto_msg.msg_type, text_str.to_string());
                                } else {
                                    let tx_clone = tx.clone();
                                    let msg = text_str.to_string();
                                    if tokio::time::timeout(
                                        std::time::Duration::from_millis(100),
                                        tx_clone.send(msg),
                                    )
                                    .await
                                    .is_err()
                                    {
                                        tracing::warn!(
                                            "SSE control channel still full after 100ms; dropping non-lossy {} for a stuck browser",
                                            proto_msg.msg_type
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // MCP oneshot
        if proto_msg.msg_type == "mcp:result" || proto_msg.msg_type == "mcp:exec_result" {
            if let Some(request_id) = proto_msg
                .payload
                .get("_mcp_request_id")
                .and_then(|v| v.as_str())
            {
                let mut pending = state.pending_mcp.write().await;
                if let Some((_sid, tx)) = pending.remove(request_id) {
                    let result_text = if proto_msg.msg_type == "mcp:exec_result" {
                        serde_json::to_string(&proto_msg.payload).unwrap_or_default()
                    } else {
                        serde_json::to_string(&json!({
                            "stdout": proto_msg.payload.get("stdout").and_then(|v| v.as_str()).unwrap_or(""),
                            "stderr": proto_msg.payload.get("stderr").and_then(|v| v.as_str()).unwrap_or(""),
                            "exit_code": proto_msg.payload.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0)
                        })).unwrap_or_default()
                    };
                    let _ = tx.send(result_text);
                }
            }
        }

        // FS oneshot
        if proto_msg.msg_type == "fs:result" {
            if let Some(request_id) = proto_msg
                .payload
                .get("_mcp_request_id")
                .and_then(|v| v.as_str())
            {
                let mut pending = state.pending_mcp.write().await;
                if let Some((_sid, tx)) = pending.remove(request_id) {
                    let result_text = serde_json::to_string(&json!({
                        "success": proto_msg.payload.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
                        "error": proto_msg.payload.get("error").and_then(|v| v.as_str()).unwrap_or(""),
                        "kind": proto_msg.payload.get("kind").and_then(|v| v.as_str()).unwrap_or("other"),
                        "content": proto_msg.payload.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                        "entries": proto_msg.payload.get("entries"),
                        "path": proto_msg.payload.get("path").and_then(|v| v.as_str()).unwrap_or(""),
                        "new_path": proto_msg.payload.get("new_path").and_then(|v| v.as_str()).unwrap_or("")
                    })).unwrap_or_default();
                    let _ = tx.send(result_text);
                }
            }
        }

        // Download streaming: fs:result carrying a correlation_id registered in
        // download_streams is a file chunk pushed by the agent; decode and forward
        // to the GET response task via the sink. Independent of pending_mcp.
        if proto_msg.msg_type == "fs:result" {
            if let Some(cid) = proto_msg
                .payload
                .get("_mcp_request_id")
                .and_then(|v| v.as_str())
            {
                let sink_opt = state.download_streams.write().await.remove(cid);
                if let Some(mut sink) = sink_opt {
                    let success = proto_msg
                        .payload
                        .get("success")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if !success {
                        let kind = proto_msg
                            .payload
                            .get("kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or("other")
                            .to_string();
                        let err = proto_msg
                            .payload
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("download error")
                            .to_string();
                        let _ = sink
                            .tx
                            .send(crate::relay::file_transfer::DownloadEvent::Error {
                                kind,
                                msg: err,
                            })
                            .await;
                    } else if let Some(content) =
                        proto_msg.payload.get("content").and_then(|v| v.as_str())
                    {
                        if let Some(bytes) = crate::agent::fs::decode_b64(content) {
                            sink.bytes += bytes.len() as u64;
                            let file_size =
                                proto_msg.payload.get("file_size").and_then(|v| v.as_u64());
                            let _ = sink
                                .tx
                                .send(crate::relay::file_transfer::DownloadEvent::Chunk {
                                    data: bytes,
                                    file_size,
                                })
                                .await;
                            // Detect last chunk: is_last field (Task 6) or chunk_index+1 >= total_chunks.
                            let chunk_index = proto_msg
                                .payload
                                .get("chunk_index")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let total_chunks = proto_msg
                                .payload
                                .get("total_chunks")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(1);
                            let is_last = proto_msg
                                .payload
                                .get("is_last")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                                || chunk_index + 1 >= total_chunks;
                            if is_last {
                                let _ = sink
                                    .tx
                                    .send(crate::relay::file_transfer::DownloadEvent::End)
                                    .await;
                                // sink dropped → removed from download_streams
                            } else {
                                sink.last_activity = std::time::Instant::now();
                                state
                                    .download_streams
                                    .write()
                                    .await
                                    .insert(cid.to_string(), sink);
                            }
                        } else {
                            let _ = sink
                                .tx
                                .send(crate::relay::file_transfer::DownloadEvent::Error {
                                    kind: "other".to_string(),
                                    msg: "base64 decode failed".to_string(),
                                })
                                .await;
                            // sink dropped → removed from download_streams (error terminates)
                        }
                    }
                }
            }
        }

        // 未知消息丢弃（R3 丙137 / R5#21）：agent 上行类型不在白名单 →
        // 显式丢弃并记日志（防垃圾/未知控制消息进入广播或 MCP 通道）。
        // 白名单 = 上方已处理的所有类型 + 浏览器控制输入（terminal:input
        // 等经 /agent/send 单独路由，不在此函数）。这里只防"agent 侧未知
        // 类型"静默吞掉造成排障盲区——记一条 info 足够，不误杀新增类型。
        {
            const KNOWN: &[&str] = &[
                "agent:upgrade_progress",
                "terminal:output",
                "desktop:video",
                "desktop:started",
                "desktop:stopped",
                "desktop:error",
                "desktop:capabilities",
                "desktop:uplink",
                "desktop:clipboard",
                "desktop:qos-ack",
                "desktop:cmd-ack",
                "desktop:cursor",
                "test-delay-ack",
                "mcp:result",
                "mcp:exec_result",
                "fs:result",
                "fs:mkdir",
                "session:users",
                "session:tab_list",
                "session:tab_switched",
            ];
            if !KNOWN.contains(&proto_msg.msg_type.as_str()) {
                tracing::info!(
                    session = %session_id,
                    msg_type = %proto_msg.msg_type,
                    "dropping unknown agent message (not in whitelist)"
                );
            }
        }
    }
}

// ── Agent WS uplink (agent → relay, replaces per-batch HTTP POST) ────

/// WebSocket upgrade for the agent's desktop uplink.
///
/// Auth mirrors `/agent/send`: the query string must carry the server
/// password (`?auth=...`) OR the session must already be registered (the
/// agent registers over HTTP before opening this socket). Text frames carry
/// one JSON message each (same shape as a `/agent/send` body element) and are
/// routed through [`route_agent_message`]. Binary frames are rejected.
///
/// Latency win over batched HTTP POST: no per-message TCP+TLS handshake, no
/// 80ms coalescing window on the agent, and TCP congestion control stays warm
/// across frames.
pub async fn agent_ws_send_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    // R5#22 WS/HTTP 限流等价：WS uplink 与 HTTP `/agent/events` 共享 per-IP
    // 连接频率配额（ev: 30/min）——agent 无法切通道绕过限流。长连接建立
    // 时检查一次即可（agent 正常不断连，30/min 裕量充足）。
    if !agent_conn_rate_ok(&state, &headers).await {
        return (StatusCode::TOO_MANY_REQUESTS, "WS uplink rate limited").into_response();
    }
    let session_id = match params.get("session") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return (StatusCode::BAD_REQUEST, "missing ?session=").into_response(),
    };
    // Registered session OR server password. A not-yet-registered session id
    // is rejected so an unauthenticated client cannot inject messages.
    let registered = state.agent_broadcast.read().await.contains_key(&session_id);
    if !registered {
        let auth = params
            .get("auth")
            .map(|s| s.as_str())
            .or_else(|| {
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
            })
            .unwrap_or("");
        let server_auth = state.server_auth.read().await.clone();
        if !server_auth.is_empty()
            && !crate::relay::auth::constant_time_eq(auth, &server_auth)
        {
            return (StatusCode::UNAUTHORIZED, "Invalid server password").into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_agent_ws_uplink(state, session_id, socket))
}

/// One established uplink socket. Forwards every text frame into the relay
/// router; replies to pings so NATs keep the mapping alive.
async fn handle_agent_ws_uplink(
    state: Arc<SharedState>,
    session_id: String,
    mut socket: axum::extract::ws::WebSocket,
) {
    use axum::extract::ws::Message;
    tracing::info!(session = %session_id, "agent WS uplink connected");
    // Periodic server-side ping: keeps the socket alive through idle proxies
    // and detects a dead agent within ~35s.
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(20));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if socket.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        state
                            .last_activity
                            .write()
                            .await
                            .insert(session_id.clone(), Instant::now());
                        // Single object (normal case); tolerate an array for
                        // symmetry with the batch HTTP transport.
                        if text.trim_start().starts_with('[') {
                            if let Ok(items) =
                                serde_json::from_str::<Vec<Value>>(&text)
                            {
                                for m in items {
                                    let sid = m["session_id"].as_str().unwrap_or("").to_string();
                                    let t = serde_json::to_string(&m).unwrap_or_default();
                                    route_agent_message(&state, &sid, &t).await;
                                }
                            }
                        } else {
                            let sid = serde_json::from_str::<Value>(&text)
                                .ok()
                                .and_then(|v| {
                                    v["session_id"].as_str().map(|s| s.to_string())
                                })
                                .unwrap_or_else(|| session_id.clone());
                            route_agent_message(&state, &sid, &text).await;
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if socket.send(Message::Pong(p)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => { /* binary frames are not part of the protocol */ }
                    Some(Err(_)) => break,
                }
            }
        }
    }
    tracing::info!(session = %session_id, "agent WS uplink disconnected");
}

// ── Agent send handler (POST, for HTTP-mode agents) ──────────────────

pub async fn agent_send_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // Batch transport (v0.20.5+): the agent coalesces several messages into a
    // JSON array so the desktop video uplink is no longer rate-limited by one
    // HTTP round-trip per frame. Each element is routed like a single message
    // (it carries its own `session_id`).
    if let Some(messages) = body.as_array() {
        if messages.is_empty() {
            return axum::http::StatusCode::OK.into_response();
        }
        // 认证与单条路径共用：每个 batch 元素都带 session_id，任意一个不
        // 属于已知会话则拒绝整个 batch（防未认证注入）。
        for m in messages {
            let sid = m["session_id"].as_str().unwrap_or("");
            if sid.is_empty() || !state.agent_broadcast.read().await.contains_key(sid) {
                let auth_header = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .unwrap_or("");
                let body_auth = m["auth"].as_str().unwrap_or("");
                let server_auth = state.server_auth.read().await.clone();
                if !server_auth.is_empty()
                    && !crate::relay::auth::constant_time_eq(auth_header, &server_auth)
                    && !crate::relay::auth::constant_time_eq(body_auth, &server_auth)
                {
                    return (
                        axum::http::StatusCode::UNAUTHORIZED,
                        "Invalid server password",
                    )
                        .into_response();
                }
            }
        }
        for m in messages {
            let sid = m["session_id"].as_str().unwrap_or("").to_string();
            let text = serde_json::to_string(m).unwrap_or_default();
            state.last_activity.write().await.insert(sid.clone(), Instant::now());
            route_agent_message(&state, &sid, &text).await;
        }
        return axum::http::StatusCode::OK.into_response();
    }

    let msg_type = body["type"].as_str().unwrap_or("");

    // agent:register is allowed without server auth (agents use keys for identity)
    if msg_type == "agent:register" {
        // Rate limit registrations per client IP to prevent session-flooding DoS.
        let client_ip = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();
        {
            let mut rl = state.rate_limiter.write().await;
            let limit = state
                .registration_rate_limit_per_min
                .load(std::sync::atomic::Ordering::Relaxed);
            // key 带命名空间前缀：register/events/mcp/file_transfer 的限流
            // 各自独立计数（MYS-886 抖动回归根因——旧版共用裸 IP 作 key，
            // register 的高频消耗挤爆 events 的 30/min 配额 → SSE 被 429 →
            // agent 断连雪崩）。
            if !rl.check(&format!("reg:{client_ip}"), limit.max(1), std::time::Duration::from_secs(60)) {
                return (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    "Too many registrations from this address",
                )
                    .into_response();
            }
        }

        // Hard cap on total sessions.
        if state.sessions.count().await >= MAX_SESSIONS {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Session limit reached",
            )
                .into_response();
        }

        // If the agent supplied a cached token set (auto-reconnect), reuse
        // those exact tokens instead of minting fresh random ones.
        let cached_tokens: Option<Vec<(String, Permission)>> = body
            .get("tokens")
            .and_then(|t| t.as_array())
            .filter(|a| !a.is_empty())
            .and_then(|arr| {
                let mut v = Vec::with_capacity(arr.len());
                for t in arr {
                    let tok = t.get("token").and_then(|x| x.as_str())?;
                    let perm = match t.get("permission").and_then(|x| x.as_str())? {
                        "rw" => Permission::ReadWrite,
                        "ro" => Permission::ReadOnly,
                        _ => return None,
                    };
                    v.push((tok.to_string(), perm));
                }
                Some(v)
            });

        // Custom session id (--session-id on the agent). Validated by the
        // registry too; an empty/absent value means "relay picks a random id".
        let desired_session_id = body
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let register_result = if let Some(ct) = cached_tokens {
            state
                .sessions
                .register_existing(ct, desired_session_id)
                .await
        } else {
            let fixed_key = body["key"].as_str().map(|s| s.to_string());
            let token_type_str = body["token_type"].as_str().unwrap_or("rw");
            let token_type = crate::proto::TokenType::from_str_val(token_type_str)
                .unwrap_or(crate::proto::TokenType::Rw);
            state
                .sessions
                .register(fixed_key.clone(), token_type.as_str(), desired_session_id)
                .await
        };
        let (session_id, tokens, evicted) = match register_result {
            Ok(r) => (r.session_id, r.tokens, r.evicted),
            Err(crate::relay::session::RegisterError::InvalidId) => {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    "invalid session_id (5-20 alphanumeric)",
                )
                    .into_response();
            }
        };

        // R5#44 capability 协商最小子集：注册消息可带 capabilities 数组
        // （agent 声明 codec/后端/功能），存入会话供 admin overview 展示与
        // 浏览器协商。老版本 agent 不带 → 保持空。
        if let Some(caps) = body["capabilities"].as_array() {
            let caps: Vec<String> = caps
                .iter()
                .filter_map(|c| c.as_str().map(|s| s.to_string()))
                .collect();
            if !caps.is_empty() {
                state
                    .sessions
                    .update_capabilities(&session_id, caps)
                    .await;
            }
        }

        let tokens_json: Vec<Value> = tokens
            .iter()
            .map(|(token, perm)| {
                let perm_str = match perm {
                    Permission::ReadWrite => "rw",
                    Permission::ReadOnly => "ro",
                };
                json!({"token": token, "permission": perm_str})
            })
            .collect();

        let key_info = if body.get("tokens").map(|v| v.is_array()).unwrap_or(false) {
            "reconnect".to_string()
        } else {
            body["key"]
                .as_str()
                .map(|k| format!("key:{}", k))
                .unwrap_or_else(|| "temp".to_string())
        };
        tracing::info!(session = %session_id, key = %key_info, evicted, "session created (HTTP-mode)");
        if evicted {
            tracing::warn!(
                session = %session_id,
                "session_id reused — a previous session with this id/token was evicted"
            );
        }
        for (token, perm) in &tokens {
            let perm_str = match perm {
                Permission::ReadWrite => "rw",
                Permission::ReadOnly => "ro",
            };
            tracing::info!(session = %session_id, permission = perm_str, "token: {}", token);
        }

        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast
                .entry(session_id.clone())
                .or_insert_with(ChannelMap::new);
        }

        // Store the agent's best-effort host probe (CPU model, arch, OS, …).
        // Older agents omit `device` entirely → stays None; a malformed object
        // is ignored rather than failing the registration.
        let device = body
            .get("device")
            .and_then(|v| serde_json::from_value::<crate::proto::DeviceInfo>(v.clone()).ok())
            .filter(|d| !d.is_empty());
        state.sessions.set_device(&session_id, device).await;

        // Agent binary version (drives the admin upgrade UI). Older agents
        // omit it entirely → None.
        let agent_version = body
            .get("agent_version")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        state
            .sessions
            .set_agent_version(&session_id, agent_version)
            .await;

        return Json(json!({
            "type": "agent:registered",
            "session_id": session_id,
            "evicted": evicted,
            "payload": { "tokens": tokens_json }
        }))
        .into_response();
    }

    // All other message types require server auth, unless they carry a valid session_id
    let server_auth = state.server_auth.read().await.clone();
    if !server_auth.is_empty() {
        let session_for_auth = body["session_id"].as_str().unwrap_or("");
        let has_valid_session = if !session_for_auth.is_empty() {
            let broadcasts = state.agent_broadcast.read().await;
            broadcasts.contains_key(session_for_auth)
        } else {
            false
        };
        if !has_valid_session {
            let auth_header = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or("");
            let body_auth = body["auth"].as_str().unwrap_or("");
            if !crate::relay::auth::constant_time_eq(auth_header, &server_auth)
                && !crate::relay::auth::constant_time_eq(body_auth, &server_auth)
            {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    "Invalid server password",
                )
                    .into_response();
            }
        }
    }

    let session_id = body["session_id"].as_str().unwrap_or("").to_string();
    if session_id.is_empty() {
        return (axum::http::StatusCode::BAD_REQUEST, "Missing session_id").into_response();
    }

    let text_str = serde_json::to_string(&body).unwrap_or_default();

    {
        let mut activity = state.last_activity.write().await;
        activity.insert(session_id.clone(), Instant::now());
    }

    route_agent_message(&state, &session_id, &text_str).await;

    axum::http::StatusCode::OK.into_response()
}

/// Biased merge of the interactive and bulk agent channels into one stream
/// for the agent SSE. Interactive is drained first each cycle, so file chunks
/// (bulk) yield to terminal:input / mcp:exec / control messages. Both
/// receivers are bounded independently; bulk backpressure cannot stall
/// interactive.
fn merge_biased(
    mut interactive: tokio::sync::mpsc::Receiver<String>,
    mut bulk: tokio::sync::mpsc::Receiver<String>,
) -> impl tokio_stream::Stream<Item = String> {
    async_stream::stream! {
        loop {
            tokio::select! {
                biased;
                m = interactive.recv() => match m { Some(s) => yield s, None => {
                    // drain remaining bulk
                    while let Some(s) = bulk.recv().await { yield s; }
                    break;
                }},
                m = bulk.recv() => match m { Some(s) => yield s, None => {
                    // drain remaining interactive
                    while let Some(s) = interactive.recv().await { yield s; }
                    break;
                }},
            }
        }
    }
}

/// Drop guard for the relay→agent SSE stream. When the stream ends (the
/// agent's connection dropped), clears this session's agent channel entries —
/// but only if they still point at THIS connection (`same_channel`), so a
/// newer reconnect's channels are never clobbered by a stale stream's teardown.
/// Without this guard, a dead agent keeps a receive-able-looking `cm.agent`,
/// and MCP `mcp:exec` messages get silently dropped into a closed channel,
/// making every call hang for the full timeout instead of fast-failing with
/// "No agent connected".
struct AgentEventsCleanup {
    tx: mpsc::Sender<String>,
    bulk_tx: mpsc::Sender<String>,
    state: Arc<SharedState>,
    session_id: String,
}

impl Drop for AgentEventsCleanup {
    fn drop(&mut self) {
        let state = self.state.clone();
        let session_id = self.session_id.clone();
        let tx = self.tx.clone();
        let bulk_tx = self.bulk_tx.clone();
        tokio::spawn(async move {
            // Capture the browsers to notify while holding the broadcast lock,
            // then deliver after dropping it (deliver needs the sse_sessions
            // read lock, and nested lock ordering here is awkward).
            let notify_browsers: Vec<String> = {
                let mut broadcast = state.agent_broadcast.write().await;
                if let Some(cm) = broadcast.get_mut(&session_id) {
                    let mut cleared = false;
                    if cm.agent.as_ref().is_some_and(|cur| cur.same_channel(&tx)) {
                        cm.agent = None;
                        cleared = true;
                    }
                    if cm.agent_bulk.as_ref().is_some_and(|cur| cur.same_channel(&bulk_tx)) {
                        cm.agent_bulk = None;
                        cleared = true;
                    }
                    if cleared {
                        tracing::info!(
                            session = %session_id,
                            "agent SSE disconnected — agent channels cleared"
                        );
                        // Agent genuinely gone：清理其桌面流与升级任务，防止
                        // desktop_streams / agent_upgrades 长期残留（审计缺口，
                        // MYS-886）。生命周期追踪（R5#24）：记移除原因。
                        let removed = state.desktop_streams.write().await.remove(&session_id);
                        tracing::info!(
                            session = %session_id,
                            reason = "agent_sse_disconnect",
                            existed = removed.is_some(),
                            "desktop stream removed on agent disconnect"
                        );
                        state.agent_upgrades.write().await.remove(&session_id);
                        // No newer connection replaced this one → the agent is
                        // genuinely gone. Tell every connected browser so it can
                        // show a status and auto-rejoin instead of staring at a
                        // frozen/empty terminal.
                        cm.browser_sessions.values().cloned().collect()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            };
            if !notify_browsers.is_empty() {
                let msg = serde_json::json!({
                    "type": "session:agent_disconnect",
                    "session_id": session_id,
                    "payload": {},
                })
                .to_string();
                let sse_sessions = state.sse_sessions.read().await;
                for sse_sid in notify_browsers {
                    if let Some(tx) = sse_sessions.get(&sse_sid) {
                        deliver(tx, "session:agent_disconnect", msg.clone());
                    }
                }
            }
        });
    }
}

// ── Agent self-upgrade artifact download ─────────────────────────────

/// Serve a staged agent upgrade artifact to the agent performing its atomic
/// self-upgrade. Auth mirrors the file-transfer endpoints: an `X-SR-Token`
/// header carrying a valid read-write token (the agent sends its own).
pub async fn upgrade_blob_handler(
    State(state): State<Arc<SharedState>>,
    axum::extract::Path(filename): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let token = headers
        .get("x-sr-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    match state.sessions.authenticate(token).await {
        Some((_sid, Permission::ReadWrite)) => {}
        Some((_, Permission::ReadOnly)) => {
            return (
                axum::http::StatusCode::FORBIDDEN,
                Json(json!({"error": "read-write token required"})),
            )
                .into_response();
        }
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid token"})),
            )
                .into_response();
        }
    }
    if !crate::relay::admin::valid_artifact_filename(&filename) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid artifact name"})),
        )
            .into_response();
    }
    let dir = match &*state.upgrade_dir.read().await {
        Some(d) => d.clone(),
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "upgrades disabled"})),
            )
                .into_response();
        }
    };
    let path = dir.join(&filename);
    match tokio::fs::File::open(&path).await {
        Ok(file) => {
            let metadata = match file.metadata().await {
                Ok(m) => m,
                Err(_) => {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "cannot stat artifact"})),
                    )
                        .into_response();
                }
            };
            let stream = tokio_util::io::ReaderStream::new(file);
            axum::http::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/octet-stream",
                )
                .header(
                    axum::http::header::CONTENT_LENGTH,
                    metadata.len().to_string(),
                )
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }
        Err(_) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"error": "artifact not found"})),
        )
            .into_response(),
    }
}

// ── Agent SSE handler (GET, for HTTP-mode agent receive) ─────────────

/// agent 上行通道（HTTP SSE `/agent/events` 与 WS uplink）共享的连接频率
/// 限流（R5#22 WS/HTTP 等价）：per-IP `ev:` 配额 30/min。返回 false = 超限
/// （调用方回 429）。**同一 key 让 WS 与 HTTP 共用配额**——agent 无法通过
/// 切换通道绕过连接频率限制（防恶意快速重连耗尽资源）。
async fn agent_conn_rate_ok(state: &Arc<SharedState>, headers: &axum::http::HeaderMap) -> bool {
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let mut rl = state.rate_limiter.write().await;
    rl.check(&format!("ev:{client_ip}"), 30, std::time::Duration::from_secs(60))
}

/// R5#12：SSE 建立/重建时补发的桌面状态快照事件字符串。从 `desktop_states`
/// 缓存读该会话最近一次 `desktop:started`/`desktop:stopped` 记录；从未出现
/// 过桌面事件（无人点过开始）返回 None，避免多余事件。
async fn desktop_state_snapshot(state: &SharedState, session_id: &str) -> Option<String> {
    let states = state.desktop_states.read().await;
    let running = *states.get(session_id)?;
    Some(
        serde_json::json!({ "type": "desktop:state", "payload": { "running": running } })
            .to_string(),
    )
}

pub async fn agent_events_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if !agent_conn_rate_ok(&state, &headers).await {
        return axum::http::StatusCode::TOO_MANY_REQUESTS.into_response();
    }

    let session_id = match params.get("session") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return axum::http::StatusCode::BAD_REQUEST.into_response(),
    };

    {
        let broadcast = state.agent_broadcast.read().await;
        if !broadcast.contains_key(&session_id) {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
    }

    let last_event_id: Option<u64> = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY); // interactive
    let (bulk_tx, bulk_rx) = mpsc::channel::<String>(crate::relay::BULK_CHANNEL_CAPACITY);

    {
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(cm) = broadcast.get_mut(&session_id) {
            cm.agent = Some(tx.clone());
            cm.agent_bulk = Some(bulk_tx.clone());
        }
    }

    let state_clone = state.clone();
    let sid_clone = session_id.clone();

    let stream = async_stream::stream! {
        // R5#12：SSE 首次连接/断线重建时，立即补发当前桌面运行状态快照。
        // 首连无 last_event_id、或事件缓冲已过期时，浏览器仍能据此恢复视图
        // （running=true → 进入桌面观看；false → 退回终端），不再依赖历史
        // 事件恰好还在缓冲里。快照未入 EventBuffer，每次握手现读现发。
        if let Some(snapshot) = desktop_state_snapshot(&state_clone, &sid_clone).await {
            yield Ok::<_, Infallible>(
                axum::response::sse::Event::default().data(snapshot)
            );
        }
        if let Some(last_id) = last_event_id {
            let buffers = state_clone.agent_event_buffers.read().await;
            if let Some(buf) = buffers.get(&sid_clone) {
                for (id, msg) in buf.replay_from(last_id) {
                    yield Ok::<_, Infallible>(
                        axum::response::sse::Event::default()
                            .id(id.to_string())
                            .data(msg)
                    );
                }
            }
        }

        // When this stream ends (agent disconnected), clear the channel-map
        // entries for this connection so MCP calls fast-fail with "No agent
        // connected" instead of being dropped into a dead channel and hanging
        // until the 30s timeout.
        let _cleanup = AgentEventsCleanup {
            tx,
            bulk_tx,
            state: state_clone.clone(),
            session_id: sid_clone.clone(),
        };

        let mut merged = Box::pin(merge_biased(rx, bulk_rx));
        while let Some(msg) = merged.next().await {
            let id = state_clone.buffer_agent_event(&sid_clone, &msg).await;
            yield Ok::<_, Infallible>(
                axum::response::sse::Event::default()
                    .id(id.to_string())
                    .data(msg)
            );
        }
    };

    let mut response = axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(5))
            .text("k"))
        .into_response();
    response.headers_mut().insert(
        axum::http::header::HeaderName::from_static("x-accel-buffering"),
        axum::http::header::HeaderValue::from_static("no"),
    );
    response
}

// ── Browser SSE handler ────────────────────────────────────────────

pub async fn browser_sse_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    // Prefer the Authorization header so tokens don't land in access logs via
    // the query string; fall back to ?token= for backward compatibility.
    let token = match crate::relay::auth::extract_token_from_headers_or_query(
        &headers,
        params.get("token"),
    ) {
        Some(t) => t,
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "Missing token"})),
            )
                .into_response()
        }
    };

    let (session_id, permission) = match state.sessions.authenticate(&token).await {
        Some(r) => r,
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "Invalid token"})),
            )
                .into_response()
        }
    };

    // Fast-fail with a clear error instead of connecting into a session whose
    // agent link is absent or not yet established — otherwise the browser sits
    // on a permanently empty terminal (the classic "registered + token issued,
    // but agent SSE never/again connected" case). The browser client shows the
    // message and auto-retries until the agent comes back.
    let agent_alive = {
        let broadcast = state.agent_broadcast.read().await;
        broadcast
            .get(&session_id)
            .and_then(|cm| cm.agent.as_ref())
            .is_some()
    };
    if !agent_alive {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({
                "error": "AGENT_NOT_CONNECTED",
                "message": "agent is not connected"
            })),
        )
            .into_response();
    }

    let user_id = Uuid::new_v4().to_string();
    let sse_sid = format!("bs_{}", Uuid::new_v4());
    let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);

    {
        state.sse_sessions.write().await.insert(sse_sid.clone(), tx);
    }

    let perm_str = match permission {
        Permission::ReadWrite => "rw",
        Permission::ReadOnly => "ro",
    };

    // Bastion-style audit: record this browser access (token prefix only).
    state
        .log_conn(
            &session_id,
            &token[..token.len().min(8)],
            perm_str,
            "connect",
        )
        .await;

    {
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(cm) = broadcast.get_mut(&session_id) {
            cm.browser_sessions.insert(user_id.clone(), sse_sid.clone());
        }
    }

    // Send session:join to the agent and VERIFY it was actually accepted. A
    // stale sender (the agent's SSE incarnation died / was replaced, or the
    // reconnect window before the new SSE lands) makes try_send return Closed;
    // a never-draining queue makes it return Full. Both are unrecoverable for
    // THIS join — silently dropping it is exactly the "device registered +
    // token ok, but the browser terminal stays empty with zero logs on the
    // agent" failure. Instead, tell this browser (via its own healthy SSE) to
    // re-join so it keeps retrying until the agent link is real.
    let join_msg = json!({
        "type": "session:join",
        "session_id": session_id,
        "payload": { "user_id": user_id, "permission": perm_str }
    })
    .to_string();
    let mut join_delivered = false;
    {
        let broadcast = state.agent_broadcast.read().await;
        if let Some(cm) = broadcast.get(&session_id) {
            if let Some(ref agent_tx) = cm.agent {
                match agent_tx.try_send(join_msg) {
                    Ok(()) => join_delivered = true,
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        tracing::warn!(
                            session = %session_id,
                            "session:join dropped — agent channel closed (stale SSE); browser will retry"
                        );
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!(
                            session = %session_id,
                            "session:join dropped — agent channel full; browser will retry"
                        );
                    }
                }
            }
        }
    }
    if !join_delivered {
        let err_msg = serde_json::json!({
            "type": "session:error",
            "session_id": session_id,
            "payload": { "code": "AGENT_NOT_CONNECTED" }
        })
        .to_string();
        let sse_sessions = state.sse_sessions.read().await;
        if let Some(tx) = sse_sessions.get(&sse_sid) {
            deliver(tx, "session:error", err_msg);
        }
    }

    // Broadcast updated user count to all browsers
    {
        let sse_sessions = state.sse_sessions.read().await;
        let broadcast = state.agent_broadcast.read().await;
        if let Some(cm) = broadcast.get(&session_id) {
            let count = cm.browser_sessions.len();
            let users_msg = json!({
                "type": "session:users",
                "session_id": session_id,
                "payload": { "count": count }
            })
            .to_string();
            for sse_sid_val in cm.browser_sessions.values() {
                if let Some(stx) = sse_sessions.get(sse_sid_val) {
                    deliver(stx, "session:users", users_msg.clone());
                }
            }
        }
    }

    let state_clone = state.clone();
    let sid_clone = session_id.clone();
    let uid_clone = user_id.clone();
    let _sse_sid_clone = sse_sid.clone();
    let perm_clone = perm_str.to_string();
    let token_prefix_clone = token[..token.len().min(8)].to_string();

    // connected event data
    let connected_data = json!({
        "type": "browser:connected",
        "session_id": session_id,
        "payload": { "user_id": user_id, "permission": perm_str }
    });

    let stream = crate::relay::mcp::SseCleanup {
        inner: ReceiverStream::new(rx),
        state: state.clone(),
        sid: sse_sid.clone(),
        on_drop: Some(Box::new(move || {
            let s = state_clone.clone();
            let sid = sid_clone.clone();
            let uid = uid_clone.clone();
            let perm = perm_clone.clone();
            let tprefix = token_prefix_clone.clone();
            tokio::spawn(async move {
                // Bastion-style audit: record disconnect (token prefix only).
                s.log_conn(&sid, &tprefix, &perm, "disconnect").await;

                let count = {
                    let mut broadcast = s.agent_broadcast.write().await;
                    if let Some(cm) = broadcast.get_mut(&sid) {
                        cm.browser_sessions.remove(&uid);
                        cm.browser_sessions.len()
                    } else {
                        0
                    }
                };

                // Broadcast updated count to remaining browsers
                let users_msg = json!({
                    "type": "session:users",
                    "session_id": sid,
                    "payload": { "count": count }
                })
                .to_string();
                {
                    let sse_sessions = s.sse_sessions.read().await;
                    let broadcast = s.agent_broadcast.read().await;
                    if let Some(cm) = broadcast.get(&sid) {
                        for sse_sid_val in cm.browser_sessions.values() {
                            if let Some(stx) = sse_sessions.get(sse_sid_val) {
                                deliver(stx, "session:users", users_msg.clone());
                            }
                        }
                    }
                }

                // Send session:leave to agent
                let leave_msg = json!({
                    "type": "session:leave",
                    "session_id": sid,
                    "payload": { "user_id": uid, "permission": perm }
                })
                .to_string();
                let broadcast = s.agent_broadcast.read().await;
                if let Some(cm) = broadcast.get(&sid) {
                    if let Some(ref agent_tx) = cm.agent {
                        deliver(agent_tx, "session:leave", leave_msg);
                    }
                }
            });
        })),
    };

    let sse_stream = async_stream::stream! {
        yield Ok::<_, Infallible>(axum::response::sse::Event::default()
            .event("connected")
            .data(serde_json::to_string(&connected_data).unwrap_or_default()));

        let mut inner_stream = stream;
        while let Some(msg) = tokio_stream::StreamExt::next(&mut inner_stream).await {
            yield Ok::<_, Infallible>(axum::response::sse::Event::default()
                .data(msg));
        }
    };

    use axum::response::sse::KeepAlive;
    let mut response = Sse::new(sse_stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(5)))
        .into_response();
    response.headers_mut().insert(
        axum::http::header::HeaderName::from_static("x-accel-buffering"),
        axum::http::header::HeaderValue::from_static("no"),
    );
    response
}

// ── Browser send handler (POST) ─────────────────────────────────────

pub async fn browser_send_handler(
    State(state): State<Arc<SharedState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // 防毒包（MYS-886 对齐项 R3丙94）：单条控制消息上限 8MB，超过直接拒绝，
    // 防止异常/恶意客户端把整个内存拖进 JSON 路由。
    let raw_len = serde_json::to_string(&body).map(|s| s.len()).unwrap_or(0);
    if raw_len > 8 * 1024 * 1024 {
        return (
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            axum::Json(json!({"error": "Message too large"})),
        )
            .into_response();
    }
    let token = match body["token"].as_str() {
        Some(t) => t,
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "Missing token"})),
            )
                .into_response()
        }
    };

    let (session_id, permission) = match state.sessions.authenticate(token).await {
        Some(r) => r,
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "Invalid token"})),
            )
                .into_response()
        }
    };

    let msg_type = body["type"].as_str().unwrap_or("");
    if msg_type.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "Missing message type"})),
        )
            .into_response();
    }

    if requires_write(msg_type) && permission == Permission::ReadOnly {
        return (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(json!({"error": "Read-only users cannot send write-type messages"})),
        )
            .into_response();
    }

    // Recording: capture terminal:input for the session's cast file.
    if msg_type == "terminal:input" {
        if let Some(rec) = &state.recorder {
            if let Some(data) = body["payload"]["data"].as_str() {
                rec.record(
                    &session_id,
                    crate::relay::recorder::RecordEvent::Input(
                        crate::relay::recorder::decode_terminal_data(data),
                    ),
                );
            }
        }
    }

    {
        let mut activity = state.last_activity.write().await;
        activity.insert(session_id.clone(), Instant::now());
    }

    let forward_msg = json!({
        "type": msg_type,
        "session_id": session_id,
        "payload": body["payload"]
    })
    .to_string();

    {
        let broadcast = state.agent_broadcast.read().await;
        if let Some(cm) = broadcast.get(&session_id) {
            if let Some(ref agent_tx) = cm.agent {
                deliver(agent_tx, msg_type, forward_msg);
            }
        }
    }

    axum::http::StatusCode::ACCEPTED.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::session::SessionRegistry;
    use crate::relay::RateLimiter;
    use tokio::sync::{oneshot, RwLock};

    fn make_state(server_auth: &str) -> Arc<SharedState> {
        Arc::new(SharedState::new(
            server_auth.to_string(),
            100 * 1024 * 1024,
            None,
            String::new(),
            String::new(),
            None,
        ))
    }

    async fn insert_channel_map(
        state: &Arc<SharedState>,
        session_id: &str,
    ) -> (mpsc::Sender<String>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        let mut cm = ChannelMap::new();
        cm.agent = Some(tx.clone());
        state
            .agent_broadcast
            .write()
            .await
            .insert(session_id.to_string(), cm);
        (tx, rx)
    }

    #[tokio::test]
    async fn test_desktop_state_snapshot_reflects_latest() {
        // R5#12：SSE 握手补发 desktop:state —— started 后 running=true、
        // stopped 后 false、从未出现桌面事件则不发（不打扰未开桌面的会话）。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();

        assert!(
            desktop_state_snapshot(&state, &sid).await.is_none(),
            "no desktop:state until a desktop event was seen"
        );

        let started = serde_json::json!({
            "type": "desktop:started",
            "session_id": sid,
            "payload": {}
        });
        route_agent_message(&state, &sid, &started.to_string()).await;
        let snap = desktop_state_snapshot(&state, &sid).await.expect("snapshot after started");
        let v: serde_json::Value = serde_json::from_str(&snap).unwrap();
        assert_eq!(v["type"], "desktop:state");
        assert_eq!(v["payload"]["running"], true);

        let stopped = serde_json::json!({
            "type": "desktop:stopped",
            "session_id": sid,
            "payload": {}
        });
        route_agent_message(&state, &sid, &stopped.to_string()).await;
        let snap = desktop_state_snapshot(&state, &sid).await.expect("snapshot after stopped");
        let v: serde_json::Value = serde_json::from_str(&snap).unwrap();
        assert_eq!(v["payload"]["running"], false);
    }

    #[tokio::test]
    async fn test_desktop_congested_backpressure_to_agent() {
        // R5#16：relay fan-out 丢旧保新（viewer 消费慢，缓冲满）→ 限频向
        // agent 回传 desktop:congested——传输段拥塞证据（与浏览器段互补）。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let (_agent_tx, mut agent_rx) = insert_channel_map(&state, &sid).await;
        // 预建流 + 一个不消费的 viewer（16 帧缓冲填满后开始丢旧）
        let ds = {
            let mut streams = state.desktop_streams.write().await;
            streams
                .entry(sid.clone())
                .or_insert_with(crate::relay::desktop::DesktopStream::new)
                .clone()
        };
        let (_vid, _rx, _init) = ds.add_viewer().await;
        // 20 个 key 帧：前 16 填缓冲，后 4 个触发丢旧 → 应回传 congested
        for _ in 0..20 {
            let frag = serde_json::json!({
                "type": "desktop:video",
                "session_id": sid,
                "payload": { "kind": "frag", "data": "AQID", "key": true }
            });
            route_agent_message(&state, &sid, &frag.to_string()).await;
        }
        // agent 通道应收到 desktop:congested（限频 5s 内≥1 次）
        let mut saw_congested = false;
        while let Ok(msg) = agent_rx.try_recv() {
            if msg.contains("desktop:congested") {
                saw_congested = true;
            }
        }
        assert!(saw_congested, "viewer 拥塞应回传 desktop:congested 给 agent");
    }

    #[tokio::test]
    async fn test_control_message_gets_drain_window_when_full() {
        // R5#29 控制消息优先级：channel 满时 non-lossy 控制消息给 100ms 腾位
        // 窗口（等待浏览器消费端排空）而 lossy 数据立即 try_send 静默丢。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let bsid = "bs_prio_test".to_string();
        let (tx, _rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        state.sse_sessions.write().await.insert(bsid.clone(), tx);
        let mut cm = ChannelMap::new();
        cm.browser_sessions.insert("uid1".to_string(), bsid.clone());
        state.agent_broadcast.write().await.insert(sid.clone(), cm);

        // lossy 数据填满 channel（terminal:output 在 broadcast_types 中）：
        // 第 1..=CAP 条进队列，之后 try_send 满 → 静默丢，调用立即返回。
        let t0 = std::time::Instant::now();
        for _ in 0..(SSE_CHANNEL_CAPACITY + 8) {
            let out = serde_json::json!({
                "type": "terminal:output", "session_id": sid, "payload": { "text": "x" }
            });
            route_agent_message(&state, &sid, &out.to_string()).await;
        }
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(80),
            "lossy 满时不应阻塞（立即 try_send 丢旧）"
        );

        // 控制消息（desktop:started，non-lossy）：channel 满 → 阻塞 ~100ms
        // 等腾位（浏览器未消费则超时丢，但语义上获得优先递送窗口）。
        let started = serde_json::json!({
            "type": "desktop:started", "session_id": sid, "payload": {}
        });
        let t1 = std::time::Instant::now();
        route_agent_message(&state, &sid, &started.to_string()).await;
        assert!(
            t1.elapsed() >= std::time::Duration::from_millis(80),
            "控制消息满时应等待腾位窗口（≥80ms），实际 {:?}",
            t1.elapsed()
        );
    }

    #[tokio::test]
    async fn test_register_capabilities_roundtrip() {
        // R5#44 capability 协商：注册后 update/get capabilities 往返一致，
        // 老版本（不带 capabilities）保持空。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        assert!(
            state.sessions.get_capabilities(&sid).await.is_empty(),
            "未声明能力应为空"
        );
        let caps = vec![
            "codec:av1".to_string(),
            "backend:x11".to_string(),
            "desktop:gray".to_string(),
        ];
        state.sessions.update_capabilities(&sid, caps.clone()).await;
        assert_eq!(state.sessions.get_capabilities(&sid).await, caps);
    }

    #[tokio::test]
    async fn test_desktop_started_precreates_stream() {
        // 竞态回归: 浏览器收到 desktop:started 后立即 GET /agent/desktop/stream。
        // relay 必须在 started 时就预建 fan-out 流, 否则在首个 desktop:video
        // (init) 到达前浏览器会吃到 404 → 黑屏。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let started = serde_json::json!({
            "type": "desktop:started",
            "session_id": sid,
            "payload": { "codec": "h264", "width": 1920, "height": 1080 }
        });
        route_agent_message(&state, &sid, &started.to_string()).await;
        let streams = state.desktop_streams.read().await;
        assert!(
            streams.contains_key(&sid),
            "desktop:started must pre-create the fan-out stream"
        );
        drop(streams);
        // 预建的流应能立刻接纳 viewer（即使 init 尚未到达）
        let ds = state.desktop_streams.read().await.get(&sid).cloned().unwrap();
        let (_vid, mut rx, init) = ds.add_viewer().await;
        assert!(init.is_none(), "no init yet");
        ds.set_init(vec![0x1, 0x2, 0x3]).await;
        let got = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("init must be replayed to pre-registered viewer")
            .unwrap();
        assert_eq!(got, vec![0x1, 0x2, 0x3]);
    }

    #[tokio::test]
    async fn test_desktop_stopped_clears_stream_and_broadcasts() {
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        // 预先放入一个 stream，desktop:stopped 到达时应被移除
        state
            .desktop_streams
            .write()
            .await
            .insert(sid.clone(), crate::relay::desktop::DesktopStream::new());
        // 浏览器订阅者能收到 desktop:started/stopped 等控制事件
        let (_agent_tx, _agent_rx) = insert_channel_map(&state, &sid).await;
        let mut sse_rx = add_browser(&state, &sid, "brow1").await;

        let stopped = serde_json::json!({
            "type": "desktop:stopped",
            "session_id": sid,
            "payload": {}
        });
        route_agent_message(&state, &sid, &stopped.to_string()).await;

        assert!(
            state.desktop_streams.read().await.is_empty(),
            "desktop:stopped must remove the fan-out stream"
        );
        let got = tokio::time::timeout(std::time::Duration::from_millis(2000), sse_rx.recv())
            .await
            .expect("browser must receive desktop:stopped")
            .unwrap();
        assert!(got.contains("desktop:stopped"), "got: {got}");
    }

    #[tokio::test]
    async fn test_desktop_started_broadcasts_to_browsers() {
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        insert_channel_map(&state, &sid).await;
        let mut sse_rx = add_browser(&state, &sid, "brow1").await;

        let started = serde_json::json!({
            "type": "desktop:started",
            "session_id": sid,
            "payload": { "codec": "h264", "width": 1280, "height": 720 }
        });
        route_agent_message(&state, &sid, &started.to_string()).await;
        let got = tokio::time::timeout(std::time::Duration::from_millis(2000), sse_rx.recv())
            .await
            .expect("browser must receive desktop:started")
            .unwrap();
        assert!(got.contains("desktop:started") && got.contains("h264"), "got: {got}");
    }

    async fn add_browser(
        state: &Arc<SharedState>,
        session_id: &str,
        user_id: &str,
    ) -> mpsc::Receiver<String> {
        let sse_sid = format!("bs_test_{}", Uuid::new_v4());
        let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        state.sse_sessions.write().await.insert(sse_sid.clone(), tx);
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(cm) = broadcast.get_mut(session_id) {
            cm.browser_sessions.insert(user_id.to_string(), sse_sid);
        }
        rx
    }

    // ── route_agent_message tests ────────────────────────────────────

    #[tokio::test]
    async fn test_deliver_drops_terminal_output_when_full() {
        // A bounded channel that's full must drop terminal:output silently
        // (lossy) so a stuck consumer can't stall the producer; non-lossy
        // messages are also dropped (can't block) but that's the degradation
        // bound that keeps other sessions alive.
        let (tx, mut rx) = mpsc::channel::<String>(2);
        deliver(&tx, "terminal:output", "a".into());
        deliver(&tx, "terminal:output", "b".into());
        // Channel now full (cap 2). Third deliver must drop, not block.
        deliver(&tx, "terminal:output", "c".into());
        assert_eq!(rx.recv().await.unwrap(), "a");
        assert_eq!(rx.recv().await.unwrap(), "b");
        // "c" was dropped; channel is empty now.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_route_agent_message_broadcasts_to_all_browsers() {
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;
        let mut rx1 = add_browser(&state, "sid1", "user1").await;
        let mut rx2 = add_browser(&state, "sid1", "user2").await;

        let msg = json!({"type":"terminal:output","session_id":"sid1","payload":{"data":"hello"}})
            .to_string();
        route_agent_message(&state, "sid1", &msg).await;

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }

    /// R5 丙111/140：agent 心跳 ping 携带 KPI → relay 采样进 kpi_history
    /// （admin KPI 曲线数据源）；无 kpi 字段的 ping 不产生样本。
    #[tokio::test]
    async fn test_route_agent_message_samples_ping_kpi() {
        let state = make_state("");
        let msg = json!({
            "type": "ping",
            "session_id": "sid1",
            "kpi": { "running": true, "codec": "av1", "fps": 30,
                    "quality_permille": 1000, "bitrate_kbps": 1388, "encode_ms": 12,
                    "active": true, "bp_count": 3, "rss_kb": 65536, "cpu_ms": 1234, "cpu_temp": 61.5 }
        })
        .to_string();
        route_agent_message(&state, "sid1", &msg).await;
        route_agent_message(&state, "sid1", &msg).await;

        let hist = state.kpi_history.read().await;
        let dq = hist.get("sid1").expect("ping KPI sample captured");
        assert_eq!(dq.len(), 2);
        let s = dq.back().unwrap();
        assert!(s.running && s.codec == "av1" && s.fps == 30);
        assert_eq!(s.quality_permille, 1000);
        assert_eq!(s.bitrate_kbps, 1388);
        assert_eq!(s.encode_ms, 12);
        assert!(s.active, "active must be sampled from agent heartbeat (R5#25)");
        assert_eq!(s.bp_count, 3, "bp_count must be sampled from agent heartbeat (R5#16)");
        assert_eq!(s.rss_kb, 65536, "rss_kb must be sampled (内存画像)");
        assert_eq!(s.cpu_ms, 1234, "cpu_ms must be sampled (功耗画像)");
        assert!((s.cpu_temp - 61.5).abs() < 0.001, "cpu_temp must be sampled, got {}", s.cpu_temp);
        assert!(s.at_unix_ms > 0);

        // 无 kpi 字段的 ping：不产生样本（保持 2）。
        let bare = json!({"type":"ping","session_id":"sid1"}).to_string();
        route_agent_message(&state, "sid1", &bare).await;
        let hist2 = state.kpi_history.read().await;
        assert_eq!(hist2.get("sid1").map(|d| d.len()).unwrap_or(0), 2);
    }

    #[test]
    fn test_kpi_anomalous_threshold() {
        // 批次5 告警判据：active（内容在动）但 fps<10 → 异常（动态不降帧
        // 铁律下正常动态 fps≥15）；静止（active=false, fps=1）不告警。
        assert!(kpi_anomalous(true, 5), "动态但 fps=5 应告警");
        assert!(kpi_anomalous(true, 9));
        assert!(!kpi_anomalous(true, 10), "fps=10 达阈值不告警");
        assert!(!kpi_anomalous(true, 30), "动态满帧不告警");
        assert!(!kpi_anomalous(false, 1), "静止 1fps 正常不告警");
        assert!(!kpi_anomalous(false, 0));
    }

    /// KPI 历史容量到顶丢弃最旧（FIFO，30min 窗口）。
    #[tokio::test]
    async fn test_kpi_history_caps_and_drops_oldest() {
        let state = make_state("");
        let cap = crate::relay::KPI_HISTORY_CAP;
        for _ in 0..(cap + 5) {
            let msg =
                json!({"type":"ping","session_id":"s","kpi":{"fps":30,"bitrate_kbps":1000}})
                    .to_string();
            route_agent_message(&state, "s", &msg).await;
        }
        let hist = state.kpi_history.read().await;
        let dq = hist.get("s").unwrap();
        assert_eq!(dq.len(), cap, "窗口到顶不再增长");
        assert!(dq.front().unwrap().at_unix_ms <= dq.back().unwrap().at_unix_ms);
    }

    #[tokio::test]
    async fn test_route_agent_message_target_user_only() {
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;
        let mut rx1 = add_browser(&state, "sid1", "user1").await;
        let mut rx2 = add_browser(&state, "sid1", "user2").await;

        let msg = json!({"type":"terminal:output","session_id":"sid1","payload":{"data":"hello","_target_user_id":"user1"}}).to_string();
        route_agent_message(&state, "sid1", &msg).await;

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_route_agent_message_missing_session_no_panic() {
        let state = make_state("");
        let msg =
            json!({"type":"terminal:output","session_id":"nonexistent","payload":{}}).to_string();
        route_agent_message(&state, "nonexistent", &msg).await;
    }

    #[tokio::test]
    async fn test_route_agent_message_mcp_result_oneshot() {
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;

        let (tx, mut rx) = oneshot::channel::<String>();
        state
            .pending_mcp
            .write()
            .await
            .insert("req1".to_string(), ("sid1".to_string(), tx));

        let msg = json!({"type":"mcp:result","session_id":"sid1","payload":{"stdout":"hello","stderr":"","exit_code":0,"_mcp_request_id":"req1"}}).to_string();
        route_agent_message(&state, "sid1", &msg).await;

        let result = rx.try_recv().unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["stdout"].as_str().unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_route_agent_message_fs_result_oneshot() {
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;

        let (tx, mut rx) = oneshot::channel::<String>();
        state
            .pending_mcp
            .write()
            .await
            .insert("fs1".to_string(), ("sid1".to_string(), tx));

        let msg = json!({"type":"fs:result","session_id":"sid1","payload":{"success":true,"error":"","content":"ok","path":"/tmp/x","_mcp_request_id":"fs1"}}).to_string();
        route_agent_message(&state, "sid1", &msg).await;

        let result = rx.try_recv().unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["success"].as_bool().unwrap(), true);
    }

    #[tokio::test]
    async fn test_route_agent_message_fs_result_download_broadcasts_to_browser() {
        // A browser download reuses `_mcp_request_id` as its correlation id.
        // The fs:result reply must still be broadcast to the browser (it was
        // never registered in pending_mcp), otherwise the download never
        // arrives. This is the download fix.
        let state = make_state("");
        insert_channel_map(&state, "sid1").await;
        let mut rx = add_browser(&state, "sid1", "user1").await;

        let msg = json!({"type":"fs:result","session_id":"sid1","payload":{"success":true,"content":"aGk=","path":"/tmp/x","_mcp_request_id":"dl-1"}}).to_string();
        route_agent_message(&state, "sid1", &msg).await;

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["type"], "fs:result");
        assert_eq!(v["payload"]["_mcp_request_id"], "dl-1");
        assert_eq!(v["payload"]["content"], "aGk=");
    }

    #[tokio::test]
    async fn test_route_agent_message_invalid_json_no_panic() {
        let state = make_state("");
        route_agent_message(&state, "sid1", "not valid json {{{").await;
    }

    #[tokio::test]
    async fn test_route_agent_message_download_chunk_pushes_to_sink() {
        use crate::relay::file_transfer::{DownloadEvent, DownloadSink};
        let state = make_state("");
        let (tx, mut rx) = mpsc::channel(16);
        state.download_streams.write().await.insert(
            "dl-1".to_string(),
            DownloadSink {
                tx,
                last_activity: Instant::now(),
                bytes: 0,
            },
        );
        let msg = json!({
            "type": "fs:result", "session_id": "sid1",
            "payload": {"success": true, "content": "aGk=", "path": "/x",
                        "chunk_index": 0, "total_chunks": 1,
                        "_mcp_request_id": "dl-1"}
        })
        .to_string();
        route_agent_message(&state, "sid1", &msg).await;
        match rx.recv().await.unwrap() {
            DownloadEvent::Chunk { data, .. } => assert_eq!(data, b"hi"),
            _ => panic!("expected Chunk"),
        }
        // Only one chunk with total_chunks=1 → should receive End next
        match rx.recv().await {
            Some(DownloadEvent::End) => {} // expected
            other => panic!("expected End, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_route_agent_message_download_bad_b64_sends_error() {
        use crate::relay::file_transfer::{DownloadEvent, DownloadSink};
        let state = make_state("");
        let (tx, mut rx) = mpsc::channel(16);
        state.download_streams.write().await.insert(
            "dl-bad".to_string(),
            DownloadSink {
                tx,
                last_activity: Instant::now(),
                bytes: 0,
            },
        );
        // "!!!" is not valid base64
        let msg = json!({
            "type": "fs:result", "session_id": "sid1",
            "payload": {"success": true, "content": "!!!", "path": "/x",
                        "chunk_index": 0, "total_chunks": 1,
                        "_mcp_request_id": "dl-bad"}
        })
        .to_string();
        route_agent_message(&state, "sid1", &msg).await;
        match rx.recv().await.unwrap() {
            DownloadEvent::Error { msg, .. } => assert!(msg.contains("base64 decode failed")),
            other => panic!("expected Error, got {:?}", other),
        }
    }

    // ── agent_send_handler tests ─────────────────────────────────────

    #[tokio::test]
    async fn test_agent_send_register_creates_session() {
        let state = make_state("");
        let body = json!({"type":"agent:register","token_type":"rw"});
        let headers = axum::http::HeaderMap::new();
        let resp = agent_send_handler(State(state), headers, Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn test_agent_send_register_rate_limited_per_ip() {
        // 每 IP 注册限流（默认 120/min；测试设成 2 验证 429 路径）。
        let state = make_state("");
        state.set_registration_rate_limit(2);
        let body = json!({"type":"agent:register","token_type":"rw"});
        let headers = axum::http::HeaderMap::new();
        for _ in 0..2 {
            let resp = agent_send_handler(State(state.clone()), headers.clone(), Json(body.clone()))
                .await
                .into_response();
            assert_eq!(resp.status(), 200, "窗口内前 2 次应放行");
        }
        let resp = agent_send_handler(State(state.clone()), headers, Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 429, "窗口内超额注册应被限流");
    }

    /// R5#22 WS/HTTP 限流等价：`agent_conn_rate_ok` 用同一 `ev:` key——WS
    /// uplink 与 HTTP `/agent/events` 共享 per-IP 30/min 配额，切通道无法
    /// 绕过连接频率限制。
    #[tokio::test]
    async fn test_agent_conn_rate_shared_ws_http() {
        let state = make_state("");
        let mk = |ip: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert(
                axum::http::header::HeaderName::from_static("x-forwarded-for"),
                ip.parse().unwrap(),
            );
            h
        };
        let headers = mk("10.0.0.9");
        for _ in 0..30 {
            assert!(agent_conn_rate_ok(&state, &headers).await, "窗口内 30 次应放行");
        }
        assert!(!agent_conn_rate_ok(&state, &headers).await, "第 31 次应超限（HTTP/WS 同配额）");
        // 不同 IP 独立配额，不受影响。
        let other = mk("10.0.0.10");
        assert!(agent_conn_rate_ok(&state, &other).await, "不同 IP 独立配额");
    }

    #[tokio::test]
    async fn test_agent_send_register_stores_device() {
        let state = make_state("");
        let body = json!({
            "type":"agent:register",
            "token_type":"rw",
            "session_id":"devA01",
            "device": {
                "hostname": "box-a",
                "platform": "linux",
                "arch": "x86_64",
                "os": "Linux",
                "kernel": "6.1.0",
                "cpu_model": "Intel(R) Xeon(R)"
            }
        });
        let resp = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
        let sessions = state.sessions.list_sessions().await;
        let (sid, info) = &sessions[0];
        assert_eq!(sid, "devA01");
        let d = info.device.as_ref().expect("device must be stored");
        assert_eq!(d.hostname.as_deref(), Some("box-a"));
        assert_eq!(d.arch.as_deref(), Some("x86_64"));
        assert_eq!(d.cpu_model.as_deref(), Some("Intel(R) Xeon(R)"));
    }

    #[tokio::test]
    async fn test_agent_send_register_ignores_malformed_device() {
        let state = make_state("");
        let body = json!({
            "type":"agent:register",
            "token_type":"rw",
            "device": "not-an-object"
        });
        let resp = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200, "malformed device must not fail registration");
        let sessions = state.sessions.list_sessions().await;
        assert!(sessions[0].1.device.is_none());
    }

    #[tokio::test]
    async fn test_agent_send_register_reuses_cached_tokens() {
        let state = make_state("");
        let body = json!({
            "type": "agent:register",
            "tokens": [
                {"token": "reused-rw", "permission": "rw"},
                {"token": "reused-ro", "permission": "ro"}
            ]
        });
        let headers = axum::http::HeaderMap::new();
        let resp = agent_send_handler(State(state.clone()), headers, Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        // Returned tokens are exactly the ones supplied
        let tokens = v["payload"]["tokens"].as_array().unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0]["token"], "reused-rw");
        assert_eq!(tokens[1]["token"], "reused-ro");
        // They now authenticate against the new session
        let sid = v["session_id"].as_str().unwrap();
        let (auth_sid, _) = state.sessions.authenticate("reused-rw").await.unwrap();
        assert_eq!(auth_sid, sid);
    }

    #[tokio::test]
    async fn test_agent_send_no_session_id_returns_400() {
        let state = make_state("");
        let body = json!({"type":"terminal:output","payload":{"data":"x"}});
        let headers = axum::http::HeaderMap::new();
        let resp = agent_send_handler(State(state), headers, Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn test_agent_send_batch_routes_all_messages() {
        // v0.20.5 批量上行: 一个 POST 携带多条消息(含 desktop:video 帧),
        // 不再受"每帧一个 HTTP 往返"限制。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let (_agent_tx, mut agent_rx) = insert_channel_map(&state, &sid).await;

        let batch = json!([
            {
                "type": "desktop:started",
                "session_id": sid,
                "payload": { "codec": "h264" }
            },
            {
                "type": "desktop:video",
                "session_id": sid,
                "payload": { "kind": "init", "data": "AQID" }
            },
            {
                "type": "desktop:video",
                "session_id": sid,
                "payload": { "kind": "frag", "key": true, "data": "AgME" }
            }
        ]);
        let resp = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(batch))
            .await
            .into_response();
        assert_eq!(resp.status(), 200, "batch must return 200");

        // started 预建扇出流, init 写入缓存; 数组消息逐条路由生效。
        let ds = state.desktop_streams.read().await.get(&sid).cloned().expect("pre-created stream");
        let (_vid, mut rx, init) = ds.add_viewer().await;
        assert_eq!(init, Some(vec![0x01, 0x02, 0x03]), "init must be cached");
        // 晚加入的 viewer 不接收 batch 处理期之前的普通帧(仅缓存 init),
        // 流本身可用: 新到达的关键帧正常送达。
        ds.push_frag(true, vec![0x02, 0x03, 0x04]).await;
        let frag = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("key frag must be pushed")
            .unwrap();
        assert_eq!(frag, vec![0x02, 0x03, 0x04]);
        let _ = &mut agent_rx;
    }

    // ── agent_events_handler tests ───────────────────────────────────

    #[tokio::test]
    async fn test_agent_events_missing_session_returns_400() {
        let state = make_state("");
        let params = HashMap::new();
        let headers = axum::http::HeaderMap::new();
        let resp = agent_events_handler(State(state), headers, Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn test_agent_events_nonexistent_session_returns_404() {
        let state = make_state("");
        let mut params = HashMap::new();
        params.insert("session".to_string(), "nonexistent".to_string());
        let headers = axum::http::HeaderMap::new();
        let resp = agent_events_handler(State(state), headers, Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn test_agent_events_valid_session_returns_200() {
        let state = make_state("");
        state
            .agent_broadcast
            .write()
            .await
            .insert("sid1".to_string(), ChannelMap::new());
        let mut params = HashMap::new();
        params.insert("session".to_string(), "sid1".to_string());
        let headers = axum::http::HeaderMap::new();
        let resp = agent_events_handler(State(state), headers, Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
    }

    // ── browser_send_handler tests ────────────────────────────────────

    #[tokio::test]
    async fn test_browser_sse_rejects_when_agent_absent() {
        // Registered session (valid token) but no agent channel → the browser
        // must be rejected with a clear error instead of connecting into an
        // empty terminal.
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let mut params = HashMap::new();
        params.insert("token".to_string(), r.tokens[0].0.clone());
        let resp = browser_sse_handler(State(state), axum::http::HeaderMap::new(), Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 503);
    }

    #[tokio::test]
    async fn test_browser_sse_rejects_when_agent_channel_gone() {
        // Channel map exists but its agent link was cleared (e.g. stale SSE
        // died) — same rejection, no empty terminal.
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        {
            let mut broadcast = state.agent_broadcast.write().await;
            broadcast.insert(sid, ChannelMap::new());
        }
        let mut params = HashMap::new();
        params.insert("token".to_string(), r.tokens[0].0.clone());
        let resp = browser_sse_handler(State(state), axum::http::HeaderMap::new(), Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 503);
    }

    #[tokio::test]
    async fn test_browser_sse_allows_when_agent_connected() {
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        insert_channel_map(&state, &r.session_id).await;
        let mut params = HashMap::new();
        params.insert("token".to_string(), r.tokens[0].0.clone());
        let resp = browser_sse_handler(State(state), axum::http::HeaderMap::new(), Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn test_browser_sse_join_delivered_to_agent() {
        // With a live agent channel, session:join must actually reach the
        // agent receiver — the browser must not be left on a silent terminal.
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let (tx, mut agent_rx) = insert_channel_map(&state, &r.session_id).await;
        let _keep_tx = tx;
        let mut params = HashMap::new();
        params.insert("token".to_string(), r.tokens[0].0.clone());
        let resp = browser_sse_handler(State(state), axum::http::HeaderMap::new(), Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), agent_rx.recv())
            .await
            .expect("join must be delivered to the agent")
            .unwrap();
        let v: Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["type"], "session:join");
        assert_eq!(v["payload"]["user_id"].as_str().is_some(), true);
    }

    #[tokio::test]
    async fn test_browser_sse_join_dropped_on_stale_channel_notifies_browser() {
        // cm.agent still points at a CLOSED sender (stale SSE incarnation from
        // a reconnect/evict window). The accept check must notice and push a
        // session:error to this browser so it auto-rejoins instead of staring
        // at an empty terminal with zero agent-side logs.
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let (closed_tx, closed_rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        drop(closed_rx); // close the receiving end → sender reports Closed
        {
            let mut broadcast = state.agent_broadcast.write().await;
            let mut cm = ChannelMap::new();
            cm.agent = Some(closed_tx);
            broadcast.insert(sid, cm);
        }
        let mut params = HashMap::new();
        params.insert("token".to_string(), r.tokens[0].0.clone());
        let resp = browser_sse_handler(State(state), axum::http::HeaderMap::new(), Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), 200, "agent sender exists, so not a hard 503");
        // The join cannot be delivered → the browser SSE must carry a
        // session:error ... read the SSE response stream until we see it.
        let mut data_stream = resp.into_body().into_data_stream();
        let mut saw_error = false;
        use tokio_stream::StreamExt as _;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), data_stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    let text = String::from_utf8_lossy(&chunk).to_string();
                    for line in text.lines() {
                        if let Some(json_str) = line.strip_prefix("data: ") {
                            if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                                if v["type"] == "session:error"
                                    && v["payload"]["code"] == "AGENT_NOT_CONNECTED"
                                {
                                    saw_error = true;
                                }
                            }
                        }
                    }
                    if saw_error {
                        break;
                    }
                }
                _ => break,
            }
        }
        drop(data_stream);
        assert!(saw_error, "browser SSE must carry session:error for a stale agent channel");
    }

    #[tokio::test]
    async fn test_browser_send_missing_token() {
        let state = make_state("");
        let body = json!({"type": "terminal:input", "payload": {}});
        let resp = browser_send_handler(State(state), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn test_browser_send_oversized_rejected() {
        // MYS-886 对齐项（R3丙94）：单条控制消息 >8MB 直接拒掉（防毒包）。
        let state = make_state("");
        let big = "x".repeat(9 * 1024 * 1024);
        let body = json!({"type": "terminal:input", "payload": {"data": big}});
        let resp = browser_send_handler(State(state), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 413);
    }

    #[tokio::test]
    async fn test_browser_send_readonly_write_forbidden() {
        let state = make_state("");
        let r = state.sessions.register(None, "ro", None).await.unwrap();
        let sid = r.session_id;
        let tokens = r.tokens;
        let token = &tokens[0].0;
        state
            .agent_broadcast
            .write()
            .await
            .insert(sid.clone(), ChannelMap::new());
        let body = json!({"token": token, "type": "terminal:input", "payload": {}});
        let resp = browser_send_handler(State(state), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 403);
    }

    #[tokio::test]
    async fn test_agent_send_register_with_custom_id() {
        let state = make_state("");
        let body = json!({"type":"agent:register","token_type":"rw","session_id":"mydev01"});
        let resp = agent_send_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Json(body),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), 200);
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["session_id"], "mydev01");
    }

    #[tokio::test]
    async fn test_agent_send_register_custom_id_reusable() {
        // Session ids are reusable: a second agent registering under the same
        // id succeeds and evicts the first (no 409).
        let state = make_state("");
        let b1 = json!({"type":"agent:register","token_type":"rw","session_id":"mydev02"});
        let r1 = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(b1))
            .await
            .into_response();
        assert_eq!(r1.status(), 200);
        let b2 = json!({"type":"agent:register","token_type":"rw","session_id":"mydev02"});
        let r2 = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(b2))
            .await
            .into_response();
        assert_eq!(
            r2.status(),
            200,
            "reusing a session id must succeed, not 409"
        );
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r2.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            v["evicted"], true,
            "re-registering an in-use id evicts the old session"
        );
        assert_eq!(v["session_id"], "mydev02");
    }

    #[tokio::test]
    async fn test_agent_send_register_invalid_custom_id() {
        let state = make_state("");
        let body = json!({"type":"agent:register","token_type":"rw","session_id":"ab!"});
        let resp = agent_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 400);
    }

    fn make_state_with_recorder() -> (
        Arc<SharedState>,
        std::sync::Arc<crate::relay::recorder::Recorder>,
    ) {
        let dir = std::env::temp_dir().join(format!(
            "sr-rec-ws-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let rec = std::sync::Arc::new(crate::relay::recorder::Recorder::new(dir));
        let state = Arc::new(SharedState::new(
            "".to_string(),
            100 * 1024 * 1024,
            None,
            String::new(),
            String::new(),
            Some(rec.clone()),
        ));
        (state, rec)
    }

    #[tokio::test]
    async fn test_route_agent_message_records_output() {
        let (state, recorder) = make_state_with_recorder();
        insert_channel_map(&state, "sid1").await;
        let mut rx = add_browser(&state, "sid1", "u1").await;
        let msg = json!({"type":"terminal:output","session_id":"sid1","payload":{"data":"hi"}})
            .to_string();
        route_agent_message(&state, "sid1", &msg).await;
        // browser still received it
        assert!(rx.try_recv().is_ok());
        // recorder has an open writer
        assert!(recorder.is_recording("sid1"));
        recorder.close("sid1");
    }

    #[tokio::test]
    async fn test_browser_send_records_input() {
        let (state, recorder) = make_state_with_recorder();
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id;
        let tokens = r.tokens;
        state
            .agent_broadcast
            .write()
            .await
            .insert(sid.clone(), ChannelMap::new());
        let body =
            json!({"token": tokens[0].0, "type": "terminal:input", "payload": {"data": "ls"}});
        let resp = browser_send_handler(State(state), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 202);
        assert!(recorder.is_recording(&sid));
        recorder.close(&sid);
    }

    // ── merge_biased tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_merge_biased_drains_interactive_first() {
        use tokio::sync::mpsc;
        let (itx, irx) = mpsc::channel::<String>(8);
        let (btx, brx) = mpsc::channel::<String>(8);
        // pre-fill: 3 bulk, then 1 interactive, then 1 bulk
        for i in 0..3 {
            btx.send(format!("b{}", i)).await.unwrap();
        }
        itx.send("i0".to_string()).await.unwrap();
        btx.send("b3".to_string()).await.unwrap();
        drop(itx);
        drop(btx); // close so merge terminates
        let mut out = Box::pin(merge_biased(irx, brx));
        let mut got = Vec::new();
        use tokio_stream::StreamExt as _;
        while let Some(s) = out.next().await {
            got.push(s);
        }
        // interactive i0 must come before all bulk (biased: interactive drained first each cycle)
        let i_pos = got.iter().position(|s| s == "i0").unwrap();
        let first_bulk = got.iter().position(|s| s.starts_with('b')).unwrap();
        assert!(
            i_pos < first_bulk,
            "interactive must precede bulk under biased merge; got {:?}",
            got
        );
    }

    /// Stronger biased-merge test: pre-fill bulk with 100 chunks to simulate a
    /// bandwidth-heavy download flooding the bulk channel.  An interactive
    /// message injected last must still emerge from the merged stream before
    /// any queued bulk message — regardless of channel depth.
    #[tokio::test]
    async fn test_merge_biased_interactive_beats_flooded_bulk() {
        use tokio::sync::mpsc;
        let (itx, irx) = mpsc::channel::<String>(128);
        let (btx, brx) = mpsc::channel::<String>(128);
        // Pre-fill bulk with 100 messages, then one interactive.
        for i in 0..100 {
            btx.send(format!("bulk-chunk-{:04}", i)).await.unwrap();
        }
        itx.send("interactive-ping".to_string()).await.unwrap();
        // Close both senders so merge terminates.
        drop(itx);
        drop(btx);
        let mut out = Box::pin(merge_biased(irx, brx));
        let mut got = Vec::new();
        use tokio_stream::StreamExt as _;
        while let Some(s) = out.next().await {
            got.push(s);
        }
        // The interactive message must be the *first* item in the merged
        // output — biased draining means interactive is polled before bulk
        // on every cycle.
        assert_eq!(got.len(), 101, "should receive 101 messages total");
        let first = got.first().expect("non-empty");
        assert_eq!(
            first, "interactive-ping",
            "interactive must be very first item under heavy bulk flood; got {:?}",
            first
        );
    }

    /// Relay-level integration test for biased merge.  Channels are wired into
    /// the shared state's `agent_broadcast` (as `agent_events_handler` does),
    /// then pre-filled with bulk chunks and one interactive message.  The merged
    /// stream drains interactive first thanks to biased select.
    ///
    /// A true end-to-end test with a live agent pumping chunks over HTTP while a
    /// browser concurrently POSTs terminal:input is deferred — it requires a
    /// process-spanning harness.  This relay-level test validates the ordering
    /// property at the boundary where the merged SSE stream is assembled.
    #[tokio::test]
    async fn test_relay_biased_merge_interactive_beats_bulk_in_stream() {
        // Create fresh channels and wire them into agent_broadcast — exactly
        // what agent_events_handler does.
        let (itx, irx) = mpsc::channel::<String>(16);
        let (btx, brx) = mpsc::channel::<String>(128);

        // Simulate bulk channel being flooded with download chunks.
        for i in 0..5 {
            let _ = btx.send(format!("bulk-download-chunk-{}", i)).await;
        }
        // Send the interactive terminal:input after bulk is queued.
        let _ = itx.send("terminal:input".to_string()).await;

        drop(itx);
        drop(btx);

        let mut merged = Box::pin(merge_biased(irx, brx));
        use tokio_stream::StreamExt as _;
        let mut got = Vec::new();
        while let Some(s) = merged.next().await {
            got.push(s);
        }

        assert_eq!(got.len(), 6, "one interactive + five bulk");
        assert_eq!(
            got[0],
            "terminal:input",
            "interactive must be first in merged output; got {:?}",
            got.first()
        );
    }

    // ── Access-audit (conn_log) tests ────────────────────────────────

    #[tokio::test]
    async fn test_log_conn_records_entry() {
        let state = make_state("");
        state.log_conn("sess1", "abc12345", "rw", "connect").await;
        let q = state.conn_log.read().await;
        assert_eq!(q.len(), 1);
        let e = &q[0];
        assert_eq!(e.session, "sess1");
        assert_eq!(e.prefix, "abc12345");
        assert_eq!(e.permission, "rw");
        assert_eq!(e.kind, "connect");
        assert!(e.at > 0);
    }

    #[tokio::test]
    async fn test_conn_log_bounded() {
        let state = make_state("");
        for i in 0..600 {
            state
                .log_conn(&format!("s{}", i), "tok12345", "ro", "disconnect")
                .await;
        }
        let q = state.conn_log.read().await;
        assert!(q.len() <= 500, "conn log must be bounded");
        // Oldest entries evicted.
        assert_eq!(q.front().unwrap().session, "s100");
        assert_eq!(q.back().unwrap().session, "s599");
    }

    // ── Agent SSE disconnect cleanup tests ───────────────────────────

    #[tokio::test]
    async fn test_agent_events_cleanup_clears_stale_channels() {
        // When the relay→agent SSE stream ends (agent disconnected), the
        // agent channel entries must be cleared so a later MCP call fast-fails
        // with "No agent connected" instead of hanging until timeout.
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id;
        let (tx, _rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        let (bulk_tx, _bulk_rx) = mpsc::channel::<String>(crate::relay::BULK_CHANNEL_CAPACITY);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            let mut cm = ChannelMap::new();
            cm.agent = Some(tx.clone());
            cm.agent_bulk = Some(bulk_tx.clone());
            broadcast.insert(sid.clone(), cm);
        }
        // Dropping the guard simulates the SSE stream ending.
        drop(AgentEventsCleanup {
            tx,
            bulk_tx,
            state: state.clone(),
            session_id: sid.clone(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let broadcast = state.agent_broadcast.read().await;
        let cm = broadcast.get(&sid).unwrap();
        assert!(cm.agent.is_none(), "stale agent channel must be cleared");
        assert!(
            cm.agent_bulk.is_none(),
            "stale bulk channel must be cleared"
        );
    }

    #[tokio::test]
    async fn test_agent_events_cleanup_notifies_browsers() {
        // When the agent SSE stream ends (agent gone) and no newer connection
        // replaced it, every connected browser of that session must receive a
        // `session:agent_disconnect` event so it can auto-rejoin instead of
        // staring at a frozen/empty terminal.
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id;
        let (tx, _rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        let (bulk_tx, _bulk_rx) = mpsc::channel::<String>(crate::relay::BULK_CHANNEL_CAPACITY);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            let mut cm = ChannelMap::new();
            cm.agent = Some(tx.clone());
            cm.agent_bulk = Some(bulk_tx.clone());
            broadcast.insert(sid.clone(), cm);
        }
        let mut browser_rx = add_browser(&state, &sid, "user1").await;
        drop(AgentEventsCleanup {
            tx,
            bulk_tx,
            state: state.clone(),
            session_id: sid.clone(),
        });
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), browser_rx.recv())
            .await
            .expect("browser must be notified")
            .unwrap();
        let v: Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["type"], "session:agent_disconnect");
        assert_eq!(v["session_id"], sid);
    }

    #[tokio::test]
    async fn test_agent_events_cleanup_preserves_newer_connection() {
        // A stale stream's cleanup must NOT clear a channel that a newer
        // (reconnecting) SSE connection has already replaced.
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id;
        let (old_tx, _old_rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        let (old_bulk, _old_bulk_rx) =
            mpsc::channel::<String>(crate::relay::BULK_CHANNEL_CAPACITY);
        let (new_tx, _new_rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        let (new_bulk, _new_bulk_rx) =
            mpsc::channel::<String>(crate::relay::BULK_CHANNEL_CAPACITY);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            let mut cm = ChannelMap::new();
            cm.agent = Some(new_tx.clone());
            cm.agent_bulk = Some(new_bulk.clone());
            broadcast.insert(sid.clone(), cm);
        }
        // Old stream ends AFTER the newer connection took over the channels.
        drop(AgentEventsCleanup {
            tx: old_tx,
            bulk_tx: old_bulk,
            state: state.clone(),
            session_id: sid.clone(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let broadcast = state.agent_broadcast.read().await;
        let cm = broadcast.get(&sid).unwrap();
        assert!(
            cm.agent.is_some(),
            "cleanup from a stale stream must not clear the newer channel"
        );
        assert!(cm.agent_bulk.is_some());
    }
}
