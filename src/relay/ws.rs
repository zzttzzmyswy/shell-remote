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

/// First ≤8 bytes of a token for audit logs, cut on a char boundary (fixed
/// `--key` values may be non-ASCII; a byte slice would panic the handler).
fn token_prefix(token: &str) -> &str {
    crate::agent::client::snippet(token, 8)
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

/// 带超时的下载事件投递（5s）。返回 false = 超时或通道关闭，调用方应放弃
/// 该下载并通知 agent 停发。超时而非无限 await 是这里的关键：一个不消费
/// 的浏览器不能把 agent 的整条上行路由拖住。
async fn send_download_event(
    tx: &mpsc::Sender<crate::relay::file_transfer::DownloadEvent>,
    ev: crate::relay::file_transfer::DownloadEvent,
) -> bool {
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), tx.send(ev)).await,
        Ok(Ok(()))
    )
}

/// 通知 agent 停止某个下载（复用 file_transfer 既有的 `fs:read_cancel` 路径，
/// 走 per-session bulk 子通道，不挤占交互消息）。
async fn cancel_download(state: &Arc<SharedState>, session_id: &str, cid: &str) {
    let bulk = state
        .agent_broadcast
        .read()
        .await
        .get(session_id)
        .and_then(|cm| cm.agent_bulk.clone());
    if let Some(bulk) = bulk {
        let proto = json!({
            "type": "fs:read_cancel",
            "session_id": session_id,
            "payload": { "_mcp_request_id": cid }
        });
        crate::relay::file_transfer::deliver_bulk(&bulk, proto.to_string()).await;
    }
}

/// 阶段2 LAN 直连：解析注册消息的 `lan_addr` 字段（agent 本地桌面流 HTTP
/// 端点地址 "ip:port"）。缺省/空串/非字符串 → None（老版本 agent 或未开启
/// `--desktop-lan-port`，兼容）。
fn parse_lan_addr(body: &Value) -> Option<String> {
    body.get("lan_addr")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 批次5 告警判据：内容活跃（非静止）但编码帧率 <10 —— 动态内容异常降帧
/// （用户铁律：动态画面不降帧，正常动态 fps ≥15）。静止（active=false，
/// fps=1）属正常，不告警。纯函数便于单测。
fn kpi_anomalous(active: bool, fps: u32) -> bool {
    active && fps < 10
}

/// 向 agent 回传 `desktop:congested`（R5#16），限频 ≥5s/会话。
///
/// agent 侧据此把"relay→浏览器"或"agent→relay"丢帧当作传输段拥塞证据，
/// 触发降码率。带宽限速丢帧（MYS-2287）复用同一个信令，不新增消息类型。
async fn notify_congested(state: &Arc<SharedState>, session_id: &str, dropped: u32) {
    let should_notify = {
        let guard = state.last_congest_notify.read().await;
        guard
            .get(session_id)
            .map_or(true, |t| t.elapsed() >= std::time::Duration::from_secs(5))
    };
    if !should_notify {
        return;
    }
    state
        .last_congest_notify
        .write()
        .await
        .insert(session_id.to_string(), std::time::Instant::now());
    let tx = state
        .agent_broadcast
        .read()
        .await
        .get(session_id)
        .and_then(|cm| cm.agent.clone());
    if let Some(tx) = tx {
        let _ = deliver(
            &tx,
            "desktop:congested",
            serde_json::json!({
                "type": "desktop:congested",
                "session_id": session_id,
                "payload": { "dropped": dropped }
            })
            .to_string(),
        );
    }
}

// ── Shared agent message routing ─────────────────────────────────────

pub async fn route_agent_message(state: &Arc<SharedState>, session_id: &str, text_str: &str) {
    // 未注册的 sid 直接丢弃：下面各分支都会 `entry(sid).or_default()` 写
    // `kpi_history` / `desktop_proto` / `desktop_streams` 等按 sid 索引的表，
    // 让任意 sid 通过就会凭空造出永不清除的条目（内存单调增长）。
    // 空 sid 同样拒绝（旧代码会把它当合法 key 写进表里）。
    if session_id.is_empty()
        || !state
            .agent_broadcast
            .read()
            .await
            .contains_key(session_id)
    {
        tracing::warn!(
            session = %session_id,
            "route_agent_message: unregistered session — dropped"
        );
        return;
    }
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
        // 网页代理回包（MYS-2383 阶段2）：`web:head/chunk/end/error` 按
        // request_id 送进该请求**独立**的 bounded channel，然后直接返回。
        // 必须在广播/MCP/录制分支之前拦截——网页内容既不进 `broadcast_types`
        // （会污染终端 SSE），也不进 terminal 录制和 MCP oneshot。
        if crate::relay::web_proxy::route_agent_message(state, session_id, &proto_msg).await {
            return;
        }
        // 上行带宽记账（MYS-2287）：agent 上行的每条消息都计入上行用量，只记账
        // 不等待（交互/控制消息不能因限速变慢）。桌面视频例外——它在自己的分支
        // 里按解码后的帧字节 `try_acquire`/`consume`，这里再记会重复计数。
        if proto_msg.msg_type != "desktop:video" {
            state.bandwidth.up.consume(text_str.len());
        }
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
                // R5#43 协议开销观测 + R5#41 seq 丢帧检测：累计 base64
                // 编码/解码字节（量化 JSON 膨胀 ≈33%）；消息级 seq 跳变检测
                // agent→relay 段丢帧（浏览器段 moof seqn 互补）。
                let msg_seq = proto_msg
                    .payload
                    .get("seq")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                {
                    let mut proto = state.desktop_proto.write().await;
                    let ent = proto.entry(session_id.to_string()).or_insert((0, 0, 0, 0));
                    ent.0 += data_b64.len() as u64;
                    ent.1 += bytes.len() as u64;
                    if msg_seq > 0 && ent.2 > 0 && msg_seq > ent.2 + 1 {
                        ent.3 += msg_seq - ent.2 - 1;
                    }
                    if msg_seq > 0 {
                        ent.2 = msg_seq;
                    }
                }
                let ds = {
                    let mut streams = state.desktop_streams.write().await;
                    streams
                        .entry(session_id.to_string())
                        .or_insert_with(|| crate::relay::desktop::DesktopStream::with_limiter(Some(state.bandwidth.clone())))
                        .clone()
                };
                tracing::debug!("desktop:video kind={kind} bytes={}", bytes.len());
                if kind == "init" {
                    // init 段是解码前提，丢了下游就再也解不出画面 → 只记账不丢
                    // （与下行 init 重发同一个口径：consume 可透支成负额度）。
                    state.bandwidth.up.consume(bytes.len());
                    ds.set_init(bytes).await;
                } else {
                    // 上行带宽（MYS-2287）：agent→relay 的视频帧走 try_acquire，
                    // 令牌不足直接丢这一帧并让所有 viewer 回到"等关键帧"状态。
                    // **不能在这里 await**——路由循环里 await 会阻塞该会话的
                    // 全部上行（终端输出也会停）。
                    if !state.bandwidth.up.try_acquire(bytes.len()) {
                        let n = ds.invalidate_keys().await;
                        state.bandwidth.count_dropped_frame(1);
                        tracing::debug!(
                            session = %session_id,
                            bytes = bytes.len(),
                            viewers = n,
                            "desktop:video upstream throttled — frame dropped"
                        );
                        notify_congested(state, session_id, 1).await;
                        return;
                    }
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
                        notify_congested(state, session_id, congested).await;
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
                .or_insert_with(|| crate::relay::desktop::DesktopStream::with_limiter(Some(state.bandwidth.clone())));
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
            // Task 2 P2P 信令（agent→浏览器）：answer/candidate/state 属
            // 控制消息（non-lossy），与 desktop:started 同路径广播。offer 是
            // 浏览器→agent 方向，不进这里。约定：p2p-offer/answer/candidate
            // 已在 proto requires_write 的 read_only_types（浏览器只读观看者
            // 也能参与协商）。
            "desktop:p2p-answer",
            "desktop:p2p-candidate",
            "desktop:p2p-state",
        ];
        if broadcast_types.contains(&proto_msg.msg_type.as_str()) {
            // 复盘日志（Task 6 轻量）：P2P 状态切换（connecting/connected/failed）
            // 广播给全体浏览器——浏览器据此知道传输层选中 p2p 还是回退 relay。
            if proto_msg.msg_type == "desktop:p2p-state" {
                tracing::info!(
                    session = %session_id,
                    p2p_state = %proto_msg.payload.get("state").and_then(|v| v.as_str()).unwrap_or("?"),
                    "transport: desktop:p2p-state broadcast"
                );
            }
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
                // 先在锁内收集目标 SSE 发送端快照（`tx.clone()` 是 Arc 级廉价
                // 克隆），释放锁后再发送——**不在持有 sse_sessions/agent_broadcast
                // 读锁期间 await send**。non-lossy 控制消息满时的 100ms 腾位窗口
                // 若在锁内 await，会阻塞整个 agent 路由任务，且 tokio RwLock 写优先
                // → 等写锁的 register/reconnect/reaper 全部排队 → relay 头阻塞
                // （MYS-969 review 修复；此前违反 `deliver` 的"持锁不 await"契约）。
                let targets: Vec<mpsc::Sender<String>> = {
                    let sse_sessions = state.sse_sessions.read().await;
                    let broadcast = state.agent_broadcast.read().await;
                    match broadcast.get(session_id) {
                        Some(channel_map) => {
                            let target_user = proto_msg
                                .payload
                                .get("_target_user_id")
                                .and_then(|v| v.as_str());
                            channel_map
                                .browser_sessions
                                .iter()
                                .filter(|(uid, _)| target_user.is_none_or(|t| t == uid.as_str()))
                                .filter_map(|(_, sse_sid)| sse_sessions.get(sse_sid).cloned())
                                .collect()
                        }
                        None => Vec::new(),
                    }
                }; // sse_sessions / agent_broadcast 读锁在此释放
                let lossy = is_lossy_msg_type(&proto_msg.msg_type);
                for tx in targets {
                    // R5#29 控制消息优先级（腾位窗口）：lossy 数据（terminal:output
                    // 等）维持 try_send 静默丢；non-lossy 控制消息在 channel 满时给
                    // 100ms 腾位窗口（浏览器消费端正在排空），仍满才告警丢。现在在
                    // 锁外 await，单个卡住的浏览器不再阻塞路由与写锁。
                    if lossy {
                        deliver(&tx, &proto_msg.msg_type, text_str.to_string());
                    } else if tokio::time::timeout(
                        std::time::Duration::from_millis(100),
                        tx.send(text_str.to_string()),
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
                // 关键：**不在锁内 await，也不把 sink 移出 map 后再 await**。
                // 旧实现先 `remove` 再 `send().await`，又把 sink 放回去；一旦
                // 浏览器侧不消费（下载卡住），这个 await 会把该 agent 的整条
                // 上行路由一起阻塞（终端输出和其它会话的帧全停）。
                // 现在持锁 clone 出 sender 并就地更新计数/活跃时间，释放锁后
                // 带 5s 超时投递；超时或通道关闭 → 移除该下载并通知 agent 停发。
                let tx = {
                    let mut streams = state.download_streams.write().await;
                    let success = proto_msg
                        .payload
                        .get("success")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    match streams.get_mut(cid) {
                        Some(sink) => {
                            if success {
                                if let Some(n) = proto_msg
                                    .payload
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .and_then(crate::agent::fs::decode_b64)
                                    .map(|b| b.len() as u64)
                                {
                                    sink.bytes += n;
                                }
                            }
                            sink.last_activity = std::time::Instant::now();
                            Some(sink.tx.clone())
                        }
                        None => None,
                    }
                };
                if let Some(tx) = tx {
                    let success = proto_msg
                        .payload
                        .get("success")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    // 组装本次要投递的事件序列（空 = 无需投递）。
                    let mut events: Vec<crate::relay::file_transfer::DownloadEvent> = Vec::new();
                    let mut terminal = false;
                    if !success {
                        events.push(crate::relay::file_transfer::DownloadEvent::Error {
                            kind: proto_msg
                                .payload
                                .get("kind")
                                .and_then(|v| v.as_str())
                                .unwrap_or("other")
                                .to_string(),
                            msg: proto_msg
                                .payload
                                .get("error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("download error")
                                .to_string(),
                        });
                        terminal = true;
                    } else if let Some(bytes) = proto_msg
                        .payload
                        .get("content")
                        .and_then(|v| v.as_str())
                        .and_then(crate::agent::fs::decode_b64)
                    {
                        let file_size = proto_msg
                            .payload
                            .get("file_size")
                            .and_then(|v| v.as_u64());
                        // 末块判定：is_last 字段，或 chunk_index+1 >= total_chunks。
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
                        let last = proto_msg
                            .payload
                            .get("is_last")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                            || chunk_index + 1 >= total_chunks;
                        events.push(crate::relay::file_transfer::DownloadEvent::Chunk {
                            data: bytes,
                            file_size,
                        });
                        if last {
                            events.push(crate::relay::file_transfer::DownloadEvent::End);
                            terminal = true;
                        }
                    } else {
                        events.push(crate::relay::file_transfer::DownloadEvent::Error {
                            kind: "other".to_string(),
                            msg: "base64 decode failed".to_string(),
                        });
                        terminal = true;
                    }

                    let mut stalled = false;
                    for ev in events {
                        if !send_download_event(&tx, ev).await {
                            stalled = true;
                            break;
                        }
                    }
                    // 终态或投递失败 → 从 map 移除；失败时通知 agent 停发，
                    // 否则它会继续把整个文件读完、再逐块超时 5s 地卡下去。
                    if stalled || terminal {
                        state.download_streams.write().await.remove(cid);
                    }
                    if stalled {
                        tracing::warn!(
                            session = %session_id,
                            cid = %cid,
                            "download sink stalled >5s — download aborted and agent asked to stop"
                        );
                        cancel_download(&state, session_id, cid).await;
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
    let session_param = params.get("session").map(String::as_str).unwrap_or("");
    if !agent_conn_rate_ok(&state, &headers, session_param).await {
        return (StatusCode::TOO_MANY_REQUESTS, "WS uplink rate limited").into_response();
    }
    let session_id = match params.get("session") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return (StatusCode::BAD_REQUEST, "missing ?session=").into_response(),
    };
    // agent 身份校验（P0）：与 `/agent/events` 同语义。`Verified` 直接放行
    // （已证明是该会话主人，无需再验服务器口令）；`Denied` 一律 401。
    match crate::relay::auth::agent_auth(&state, &session_id, &headers).await {
        crate::relay::auth::AgentAuth::Verified => {}
        crate::relay::auth::AgentAuth::Denied => {
            tracing::warn!(
                session = %session_id,
                "agent WS uplink denied — missing or mismatched X-SR-Agent-Token"
            );
            return (StatusCode::UNAUTHORIZED, "Invalid agent token").into_response();
        }
        crate::relay::auth::AgentAuth::Legacy => {
            // Registered session OR server password. A not-yet-registered session
            // id is rejected so an unauthenticated client cannot inject messages.
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
                        //
                        // 消息**必须**属于本连接自己的 sid（P1 越权修复）：
                        // 此前按消息体里的 `session_id` 路由，任何已注册的
                        // agent 都能替他会话伪造终端输出/桌面流（并顺带把
                        // 录像文件写到别的目录）。与连接 sid 不符的直接丢弃。
                        if text.trim_start().starts_with('[') {
                            if let Ok(items) =
                                serde_json::from_str::<Vec<Value>>(&text)
                            {
                                for m in items {
                                    let sid = m["session_id"]
                                        .as_str()
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or(&session_id);
                                    if sid != session_id {
                                        tracing::warn!(
                                            session = %session_id,
                                            claimed = %sid,
                                            "agent WS uplink: message session_id mismatch — dropped"
                                        );
                                        continue;
                                    }
                                    let t = serde_json::to_string(&m).unwrap_or_default();
                                    route_agent_message(&state, &session_id, &t).await;
                                }
                            }
                        } else {
                            let sid = serde_json::from_str::<Value>(&text)
                                .ok()
                                .and_then(|v| {
                                    v["session_id"].as_str().map(|s| s.to_string())
                                })
                                .unwrap_or_else(|| session_id.clone());
                            if sid != session_id {
                                tracing::warn!(
                                    session = %session_id,
                                    claimed = %sid,
                                    "agent WS uplink: message session_id mismatch — dropped"
                                );
                                continue;
                            }
                            route_agent_message(&state, &session_id, &text).await;
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if socket.send(Message::Pong(p)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    // R5 #41 binary 传输消费侧就绪：agent 上行 desktop:video
                    // 二进制帧（格式见 proto::encode_bin_frame）→ 字节直转
                    // desktop_streams（无 base64 膨胀）。老 agent 仍走 Text
                    // JSON 路径；本分支为未来 agent 切换的接收端。
                    Some(Ok(Message::Binary(bin))) => {
                        if let Some((flags, seq, payload)) = crate::proto::decode_bin_frame(&bin) {
                            route_bin_desktop_frame(&state, &session_id, flags, seq, payload).await;
                        } else {
                            tracing::warn!("agent binary frame: bad header, dropped");
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
    tracing::info!(session = %session_id, "agent WS uplink disconnected");
}

/// 处理 agent 上行的 desktop:video 二进制帧（R5 #41 binary 传输消费侧）。
/// init 帧 → `set_init`（重放给新加入 viewer）；frag 帧 → `push_frag`（丢旧
/// 保新 + 回传 congested）；seq 丢帧检测 + desktop_proto 开销统计与 JSON
/// text 路径同语义（binary 无 base64，encoded==decoded==payload 长度，即
/// 0% 协议膨胀）。老 agent 不感知：此函数仅由 Binary 分支调用。
async fn route_bin_desktop_frame(
    state: &Arc<SharedState>,
    session_id: &str,
    flags: u8,
    seq: u32,
    payload: &[u8],
) {
    use crate::proto::{BIN_FRAME_FLAG_INIT, BIN_FRAME_FLAG_KEY};
    let seq = seq as u64;
    // 开销观测 + 消息级 seq 丢帧检测（desktop_proto，与 text 路径同构）。
    {
        let mut proto = state.desktop_proto.write().await;
        let ent = proto.entry(session_id.to_string()).or_insert((0, 0, 0, 0));
        ent.0 += payload.len() as u64;
        ent.1 += payload.len() as u64;
        if seq > 0 && ent.2 > 0 && seq > ent.2 + 1 {
            ent.3 += seq - ent.2 - 1;
        }
        if seq > 0 {
            ent.2 = seq;
        }
    }
    let ds = {
        let mut streams = state.desktop_streams.write().await;
        streams
            .entry(session_id.to_string())
            .or_insert_with(|| crate::relay::desktop::DesktopStream::with_limiter(Some(state.bandwidth.clone())))
            .clone()
    };
    if flags & BIN_FRAME_FLAG_INIT != 0 {
        tracing::debug!("desktop:video-bin kind=init bytes={}", payload.len());
        // init 段只记账不丢（解码前提），与 text 路径同一口径。
        state.bandwidth.up.consume(payload.len());
        ds.set_init(payload.to_vec()).await;
        return;
    }
    // 上行带宽（MYS-2287）：令牌不足直接丢这一帧，并让所有 viewer 回到
    // "等关键帧"状态（丢帧打断参考链，继续发 delta 会花屏）。
    if !state.bandwidth.up.try_acquire(payload.len()) {
        let viewers = ds.invalidate_keys().await;
        state.bandwidth.count_dropped_frame(1);
        tracing::debug!(
            session = %session_id,
            seq, bytes = %payload.len(), viewers,
            "desktop:video-bin upstream throttled — frame dropped"
        );
        notify_congested(state, session_id, 1).await;
        return;
    }
    let is_key = flags & BIN_FRAME_FLAG_KEY != 0;
    let congested = ds.push_frag(is_key, payload.to_vec()).await;
    tracing::debug!(
        kind = "frag", seq, bytes = %payload.len(), is_key, congested,
        "desktop:video-bin pushed"
    );
    if congested > 0 {
        // R5#16 回传拥塞信号（限频 ≥5s），语义与 JSON text 路径一致。
        notify_congested(state, session_id, congested).await;
    }
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
        // agent 身份校验（P0）：批量通道与单条通道同语义。
        // 带 token → 必须有效，且每条的 `session_id` 都必须属于该 token。
        // 无 token → 严格模式 401；否则视为旧版 agent，逐条回退到
        // 「已注册会话 / 服务器口令」校验。
        let token_owner = match crate::relay::auth::agent_token(&headers) {
            Some(t) => match state.sessions.authenticate(t).await {
                Some((sid, _)) => Some(sid),
                None => {
                    tracing::warn!("agent /agent/send batch denied — invalid X-SR-Agent-Token");
                    return (axum::http::StatusCode::UNAUTHORIZED, "Invalid agent token")
                        .into_response();
                }
            },
            None => {
                if state
                    .strict_agent_auth
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    return (axum::http::StatusCode::UNAUTHORIZED, "Agent token required")
                        .into_response();
                }
                None
            }
        };
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
            // 令牌主人只能为**自己**的会话发消息。
            if let Some(ref owner) = token_owner {
                if &sid != owner {
                    tracing::warn!(
                        claimed = %sid,
                        owner = %owner,
                        "agent /agent/send batch: session_id does not belong to this token — dropped"
                    );
                    continue;
                }
            }
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
        let client_ip = crate::relay::auth::client_ip(&headers);
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

        // 注册抢占防护（P0）：同 id 的会话**当前有在线 agent** 时，请求必须
        // 证明归属（fixed key 相同，或缓存 token 仍属于该会话），否则返回
        // 409。离线会话仍可直接接管（agent 崩溃后重启的正常路径）。
        // 判据必须用「在线」而非「存在」：会话条目在 agent 断线后仍保留
        // （供浏览器重连），把它当成在线会把正常重启也拒掉。
        if let Some(ref sid) = desired_session_id {
            let online = crate::relay::auth::agent_online(&state, sid).await;
            if online
                && !state
                    .sessions
                    .can_reclaim(
                        sid,
                        body["key"].as_str(),
                        cached_tokens.as_deref(),
                    )
                    .await
            {
                tracing::warn!(
                    session = %sid,
                    "agent:register rejected — session has a live agent and the request proved no ownership"
                );
                return (
                    axum::http::StatusCode::CONFLICT,
                    Json(json!({"error": "SESSION_ID_IN_USE"})),
                )
                    .into_response();
            }
        }

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
        //
        // MYS-2383：**每次注册都覆盖**（含空数组）。同一 sid 的新 incarnation
        // 若不覆盖，旧能力会残留——旧 agent 重连后仍被当成支持 `web:proxy-v1`，
        // UI 会显示一个必然 404 的入口。`body["capabilities"]` 缺失时同样写空，
        // 效果与 `register` 新建的默认值一致。
        {
            let caps: Vec<String> = body["capabilities"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| c.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            state
                .sessions
                .update_capabilities(&session_id, caps)
                .await;
        }

        // 阶段2 LAN 直连：注册消息可带 `lan_addr`（"ip:port"），存入会话表
        // 供 admin 展示；浏览器经 agent 的 desktop:capabilities.lan_addrs 拿到
        // 同网段探测直连地址。老版本 / 未开启 --desktop-lan-port → None。
        state
            .sessions
            .set_lan_addr(&session_id, parse_lan_addr(&body))
            .await;

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

        // 注册即视为活跃：否则会话在收到第一条终端输出前没有 last_activity
        // 条目，空闲回收器永远看不到它（曾经注册后静默的会话永不回收，
        // 可耗尽 MAX_SESSIONS）。
        state
            .last_activity
            .write()
            .await
            .insert(session_id.clone(), Instant::now());

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
            // R5 #41 binary 传输能力告知：新 relay 的 agent WS uplink 支持
            // desktop:video 二进制帧（Message::Binary → route_bin_desktop_frame）。
            // 老 relay 无此字段 → agent 保持 JSON+base64（兼容回退）。
            "desktop_binary": true,
            "payload": { "tokens": tokens_json }
        }))
        .into_response();
    }

    // All other message types require server auth, unless they carry a valid session_id
    let session_id = body["session_id"].as_str().unwrap_or("").to_string();
    if session_id.is_empty() {
        return (axum::http::StatusCode::BAD_REQUEST, "Missing session_id").into_response();
    }

    // agent 身份校验（P0），在服务器口令之前：
    // - 带了 token 且有效但属于**别的会话** → 丢弃并 warn（消息不可信，
    //   但请求本身认证通过，不回 401）；
    // - 带了 token 但无效 → 401；
    // - 无 token 时严格模式 401，否则按旧版 agent 处理（沿用下方「服务器
    //   口令 / 已注册会话」校验）。
    if let Some(tok) = crate::relay::auth::agent_token(&headers) {
        match state.sessions.authenticate(tok).await {
            Some((owner, _)) => {
                if owner != session_id {
                    tracing::warn!(
                        claimed = %session_id,
                        owner = %owner,
                        "agent /agent/send: token belongs to another session — dropped"
                    );
                    return axum::http::StatusCode::OK.into_response();
                }
            }
            None => {
                tracing::warn!(
                    session = %session_id,
                    "agent /agent/send denied — invalid X-SR-Agent-Token"
                );
                return (axum::http::StatusCode::UNAUTHORIZED, "Invalid agent token")
                    .into_response();
            }
        }
    } else if state
        .strict_agent_auth
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        tracing::warn!(
            session = %session_id,
            "agent /agent/send denied — --strict-agent-auth requires X-SR-Agent-Token"
        );
        return (axum::http::StatusCode::UNAUTHORIZED, "Agent token required").into_response();
    }

    let server_auth = state.server_auth.read().await.clone();
    if !server_auth.is_empty() {
        let has_valid_session = state
            .agent_broadcast
            .read()
            .await
            .contains_key(&session_id);
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

    // 消息的 session_id 必须是已注册会话（未知 sid 丢弃：不得为不存在的
    // 会话新建任何 per-session 状态条目）。
    if !state
        .agent_broadcast
        .read()
        .await
        .contains_key(&session_id)
    {
        tracing::warn!(
            session = %session_id,
            "agent /agent/send: unregistered session_id — dropped"
        );
        return axum::http::StatusCode::OK.into_response();
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
///
/// Yields `(is_bulk, message)`: the caller uses the flag to decide how the
/// message is charged against the relay's downlink bandwidth budget (bulk
/// file chunks `acquire` = backpressure; interactive traffic only `consume`
/// so terminal latency is never gated by the limiter — MYS-2287).
fn merge_biased(
    mut interactive: tokio::sync::mpsc::Receiver<String>,
    mut bulk: tokio::sync::mpsc::Receiver<String>,
) -> impl tokio_stream::Stream<Item = (bool, String)> {
    async_stream::stream! {
        loop {
            tokio::select! {
                biased;
                m = interactive.recv() => match m { Some(s) => yield (false, s), None => {
                    // drain remaining bulk
                    while let Some(s) = bulk.recv().await { yield (true, s); }
                    break;
                }},
                m = bulk.recv() => match m { Some(s) => yield (true, s), None => {
                    // drain remaining interactive
                    while let Some(s) = interactive.recv().await { yield (false, s); }
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
                        // agent 已下线：桌面必然不在运行。把状态显式置 false，
                        // 否则浏览器（重新）建立 SSE 时拿到的快照仍是
                        // running=true，会去连一个已被移除的桌面流。
                        state
                            .desktop_states
                            .write()
                            .await
                            .insert(session_id.clone(), false);
                        state.agent_upgrades.write().await.remove(&session_id);
                        // 网页代理（MYS-2383 阶段2）：agent 断线 → 撤销该会话
                        // 的全部 ticket（浏览器不能再拿旧 ticket 取资源）、
                        // 取消在途网页请求（其响应任务因通道关闭而结束）。
                        // 注册能力**保留**不动：重连的 agent 会重新注册覆盖，
                        // 浏览器在此期间也仍能读到能力（配合
                        // `session:agent_disconnect` 自行隐藏入口）。
                        let revoked = state.web_proxy.revoke_session_tickets(&session_id).await;
                        let cancelled = state.web_proxy.cancel_session(&session_id).await;
                        if revoked > 0 || cancelled > 0 {
                            tracing::info!(
                                session = %session_id,
                                revoked,
                                cancelled,
                                "web proxy: session tickets revoked and in-flight requests cancelled on agent disconnect"
                            );
                        }
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
            // 下行带宽（MYS-2287）：升级包是大文件，按 chunk 取令牌。限速时
            // agent 侧下载变慢（自升级本身有超时/重试），不会丢字节。
            let limiter = state.bandwidth.clone();
            let stream = async_stream::stream! {
                let mut inner = tokio_util::io::ReaderStream::new(file);
                while let Some(chunk) = inner.next().await {
                    match chunk {
                        Ok(bytes) => {
                            limiter.down.acquire(bytes.len()).await;
                            yield Ok::<_, std::io::Error>(bytes);
                        }
                        Err(e) => {
                            yield Err(e);
                            break;
                        }
                    }
                }
            };
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
/// 限流（R5#22 WS/HTTP 等价）：per-(IP, session) `ev:` 配额 30/min。返回
/// false = 超限（调用方回 429）。**同一 key 让 WS 与 HTTP 共用配额**——agent
/// 无法通过切换通道绕过连接频率限制（防恶意快速重连耗尽资源）。
///
/// key 带 session：仅按 IP 计数时，同一出口 IP（NAT / 反代未带
/// X-Forwarded-For 时全部落到 "unknown"）后的所有 agent 共享 30/min——relay
/// 重启后 >30 个 agent 同时重连，超出的 SSE 全被 429，会话反复断开（MYS-1766
/// 40 agent 并发冒烟复现）。单个 agent 的重连频率仍受 30/min 约束。
async fn agent_conn_rate_ok(
    state: &Arc<SharedState>,
    headers: &axum::http::HeaderMap,
    session: &str,
) -> bool {
    let client_ip = crate::relay::auth::client_ip(headers);
    let window = std::time::Duration::from_secs(60);
    let mut rl = state.rate_limiter.write().await;
    // `session` is caller-supplied and checked before auth: a coarse per-IP
    // cap stops minting a fresh bucket per made-up session id.
    rl.check(&format!("ev:{client_ip}"), 600, window)
        && rl.check(&format!("ev:{client_ip}:{session}"), 30, window)
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
    let session_param = params.get("session").map(String::as_str).unwrap_or("");
    if !agent_conn_rate_ok(&state, &headers, session_param).await {
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

    // agent 身份校验（P0）。旧版 agent 不带 `X-SR-Agent-Token`：仅在非严格
    // 模式下放行，并且**不得顶替在线 agent**（否则知道 sid 就能劫持浏览器
    // 输入、上传块和 mcp:exec）。带正确 token 视为同一 agent 重连，直接覆盖。
    match crate::relay::auth::agent_auth(&state, &session_id, &headers).await {
        crate::relay::auth::AgentAuth::Verified => {}
        crate::relay::auth::AgentAuth::Denied => {
            tracing::warn!(
                session = %session_id,
                "agent /agent/events denied — missing or mismatched X-SR-Agent-Token"
            );
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "AGENT_AUTH_REQUIRED"})),
            )
                .into_response();
        }
        crate::relay::auth::AgentAuth::Legacy => {
            if crate::relay::auth::agent_online(&state, &session_id).await {
                tracing::warn!(
                    session = %session_id,
                    "legacy agent /agent/events rejected — a live agent already owns this session"
                );
                return (
                    axum::http::StatusCode::CONFLICT,
                    axum::Json(json!({"error": "AGENT_ALREADY_CONNECTED"})),
                )
                    .into_response();
            }
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
        // 注意：`desktop:state` 快照只发给**浏览器**（见 browser_sse_handler）。
        // 这里是 agent 的下行通道，agent 自己就是桌面状态的来源，把它发回给
        // agent 既是无用流量，也会让 agent 把自己发出的状态当成远端指令。
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
        while let Some((is_bulk, msg)) = merged.next().await {
            // 下行带宽（MYS-2287）：给 agent 转发的文件块（bulk 通道）走
            // `acquire` 产生端到端背压——relay 出站受限时，agent 会因 SSE
            // 读得慢而自己慢下来，而不是让 relay 无限缓冲。交互消息只记账
            // （`consume`），绝不等待：终端回显不能被限速拖慢。
            if is_bulk {
                state_clone.bandwidth.down.acquire(msg.len()).await;
            } else {
                state_clone.bandwidth.down.consume(msg.len());
            }
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

/// 凭证变更（吊销 / 重新生成 / 降权）后断开受影响的浏览器事件流（A7）。
///
/// 只按 token 精确匹配：`browser_tokens` 记录了每条浏览器 SSE 建立时用的
/// token，命中即把它的 sender 从 `sse_sessions` 移除——sender 一消失，
/// 该响应的 `ReceiverStream` 立即结束，浏览器侧会按既有逻辑重连（重连时
/// 拿新 token / 新权限）。不命中其它浏览器不受影响。
///
/// `tokens = None` 表示断开该会话的**全部**浏览器流（token 已被整体轮换，
/// 没有哪条流还持有有效凭证）。返回断开的流数量。
pub async fn disconnect_browser_streams(
    state: &Arc<SharedState>,
    sid: &str,
    tokens: Option<&[String]>,
) -> usize {
    let sse_sids: Vec<String> = {
        let mut broadcast = state.agent_broadcast.write().await;
        let Some(cm) = broadcast.get_mut(sid) else {
            return 0;
        };
        let matched: Vec<(String, String)> = cm
            .browser_tokens
            .iter()
            .filter(|(_, (_, tok))| match tokens {
                Some(list) => list.iter().any(|t| t == tok),
                None => true,
            })
            .map(|(uid, (sse_sid, _))| (uid.clone(), sse_sid.clone()))
            .collect();
        for (uid, _) in &matched {
            cm.browser_sessions.remove(uid);
            cm.browser_tokens.remove(uid);
        }
        matched.into_iter().map(|(_, sse_sid)| sse_sid).collect()
    };
    if sse_sids.is_empty() {
        return 0;
    }
    let mut sse = state.sse_sessions.write().await;
    let mut closed = 0;
    for s in &sse_sids {
        if sse.remove(s).is_some() {
            closed += 1;
        }
    }
    if closed > 0 {
        tracing::info!(
            session = %sid,
            closed,
            "browser SSE streams closed after credential change"
        );
    }
    closed
}

/// 凭证变更后结束该会话的桌面下行流（A7 的桌面部分）。
///
/// `DesktopStream` 里没有 token 信息（`desktop.rs` 属并行卡 B，本卡不改），
/// 无法按 token 精确断开某个 viewer，所以这里对整条流下手：
/// 移除 `desktop_streams[sid]` 并把 `desktop_states[sid]` 置 false。
/// 移除后 basestream 的 Arc 不再收到 agent 帧（下一帧会在 map 里新建一条新
/// 流），已有 viewer 的 `rx` 在 `/agent/desktop/stream` 侧 12s 空闲超时后
/// 自然结束、`/agent/desktop/ws` 侧同样因 `rx.recv()` 返回 None 而关闭，
/// 浏览器随既有的重连逻辑用新凭证重新拉流。代价是最长 12s 的收敛延迟。
pub async fn end_desktop_streams(state: &Arc<SharedState>, sid: &str) {
    let removed = state.desktop_streams.write().await.remove(sid);
    state
        .desktop_states
        .write()
        .await
        .insert(sid.to_string(), false);
    if removed.is_some() {
        tracing::info!(
            session = %sid,
            "desktop stream ended after credential change"
        );
    }
}

pub async fn browser_sse_handler(
    State(state): State<Arc<SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if !crate::relay::auth::server_password_ok(&state, &headers, params.get("auth")).await {
        return crate::relay::auth::invalid_password_response();
    }
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
            token_prefix(&token),
            perm_str,
            "connect",
        )
        .await;

    {
        let mut broadcast = state.agent_broadcast.write().await;
        if let Some(cm) = broadcast.get_mut(&session_id) {
            cm.browser_sessions.insert(user_id.clone(), sse_sid.clone());
            cm.browser_tokens
                .insert(user_id.clone(), (sse_sid.clone(), token.clone()));
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
    let token_prefix_clone = token_prefix(&token).to_string();
    let token_clone = token.clone();
    let state_snapshot_clone = state.clone();
    let sid_snapshot = session_id.clone();

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
                        cm.browser_tokens.remove(&uid);
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

        // R5#12：浏览器 SSE 建立/重建后立即补发当前桌面运行状态快照（首连
        // 无 last_event_id、或历史事件已被缓冲淘汰时，仍能恢复视图：
        // running=true → 进入桌面观看；false → 退回终端）。快照不入
        // EventBuffer，每次握手现读现发。**只发给浏览器**——发给 agent
        // 属于错对象（A10）。
        if let Some(snapshot) = desktop_state_snapshot(&state_snapshot_clone, &sid_snapshot).await {
            yield Ok::<_, Infallible>(axum::response::sse::Event::default().data(snapshot));
        }

        // MYS-2383：握手补发 agent 注册的能力声明，供 UI 判断是否显示
        // 「网页」入口（旧 agent 不声明 web:proxy-v1 → 入口完全不出现）。
        // payload 只含 capabilities 数组，UI 无需理解其它字段。
        // **agent 断线时能力仍保留**（重连后由重新注册覆盖），浏览器收到
        // `session:agent_disconnect` 自己隐藏入口。
        let caps = state_snapshot_clone
            .sessions
            .get_capabilities(&sid_snapshot)
            .await;
        yield Ok::<_, Infallible>(
            axum::response::sse::Event::default().data(
                json!({
                    "type": "session:capabilities",
                    "session_id": sid_snapshot,
                    "payload": { "capabilities": caps }
                })
                .to_string(),
            ),
        );

        let mut inner_stream = stream;
        while let Some(msg) = tokio_stream::StreamExt::next(&mut inner_stream).await {
            // 下行带宽（MYS-2287）：浏览器 SSE 承载终端输出等交互数据，
            // 只记账不等待——限速不能变成终端卡顿。大流量（文件下载）走
            // /agent/mcp/get 的 body 流，那里是等待式限速。
            state_snapshot_clone.bandwidth.down.consume(msg.len());
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
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if !crate::relay::auth::server_password_ok(&state, &headers, None).await {
        return crate::relay::auth::invalid_password_response();
    }
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

    // 复盘日志（Task 6 轻量）：浏览器发起的 P2P 信令（offer/candidate）转发
    // 给 agent —— 下行传输层协商路径的可观测点。
    if msg_type.starts_with("desktop:p2p") {
        tracing::info!(
            session = %session_id,
            msg = msg_type,
            "transport: browser p2p signaling -> agent"
        );
    }

    {
        let broadcast = state.agent_broadcast.read().await;
        if let Some(cm) = broadcast.get(&session_id) {
            if let Some(ref agent_tx) = cm.agent {
                // 上行/下行记账拆分（MYS-2287）：浏览器发来的是 relay **入站**
                // 流量，记到 up；转发给 agent 是出站，记到 down。两者都只记账
                // 不等待——这是终端输入路径，绝不能限速拖慢回显。
                state.bandwidth.up.consume(forward_msg.len());
                state.bandwidth.down.consume(forward_msg.len());
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

    /// 从 SSE 响应里读出最多 `max` 条事件（`data:` 行解析为 JSON），用于断言
    /// 握手阶段的初始事件序列。读够或流不再产出即返回（不等待 keep-alive）。
    async fn collect_sse_events(resp: Response, max: usize) -> Vec<serde_json::Value> {
        use http_body_util::BodyExt;
        let mut body = resp.into_body();
        let mut out = Vec::new();
        let mut buf = Vec::new();
        while out.len() < max {
            let next = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame()).await;
            let Ok(Some(Ok(frame))) = next else { break };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            buf.extend_from_slice(&data);
            // SSE 帧以空行分隔；只解析 `data:` 行。
            while let Some(pos) = find_double_newline(&buf) {
                let block: Vec<u8> = buf.drain(..pos + 2).collect();
                let text = String::from_utf8_lossy(&block);
                let payload: String = text
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(|l| l.strip_prefix(' ').unwrap_or(l))
                    .collect::<Vec<_>>()
                    .join("\n");
                if payload.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) {
                    out.push(v);
                    if out.len() >= max {
                        return out;
                    }
                }
            }
        }
        out
    }

    fn find_double_newline(buf: &[u8]) -> Option<usize> {
        buf.windows(2).position(|w| w == b"\n\n")
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
        // route_agent_message 只路由已注册会话（需 agent_broadcast 条目）。
        insert_channel_map(&state, &sid).await;

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
                .or_insert_with(|| crate::relay::desktop::DesktopStream::with_limiter(Some(state.bandwidth.clone())))
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
    async fn test_bin_desktop_frame_init_and_frag() {
        // R5 #41 binary 消费侧：init 帧缓存（新 viewer 拿到），frag 帧进
        // viewer 缓冲。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let ds = {
            let mut streams = state.desktop_streams.write().await;
            streams
                .entry(sid.clone())
                .or_insert_with(|| crate::relay::desktop::DesktopStream::with_limiter(Some(state.bandwidth.clone())))
                .clone()
        };
        // init 帧 → set_init 缓存。
        let ib = crate::proto::encode_bin_frame(crate::proto::BIN_FRAME_FLAG_INIT, 1, b"ftypmoov");
        let (f, s, p) = crate::proto::decode_bin_frame(&ib).unwrap();
        route_bin_desktop_frame(&state, &sid, f, s, p).await;
        let (_vid, mut rx, init) = ds.add_viewer().await;
        assert!(init.is_some(), "init 帧应缓存，新 viewer 加入即拿到");
        // frag 帧 → bytes 推入 viewer 缓冲。
        let fb = crate::proto::encode_bin_frame(crate::proto::BIN_FRAME_FLAG_KEY, 2, b"moo1");
        let (f, s, p) = crate::proto::decode_bin_frame(&fb).unwrap();
        route_bin_desktop_frame(&state, &sid, f, s, p).await;
        let got = rx.try_recv().expect("frag 帧应到达 viewer");
        assert_eq!(got, b"moo1".to_vec());
    }

    #[tokio::test]
    async fn test_bin_desktop_frame_seq_drop_and_congested() {
        // R5 #41 binary 消费侧：seq 跳变丢帧检测 + 缓冲满回传 congested，
        // 开销统计 encoded==decoded（binary 无 base64 膨胀）。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let (_agent_tx, mut agent_rx) = insert_channel_map(&state, &sid).await;
        let ds = {
            let mut streams = state.desktop_streams.write().await;
            streams
                .entry(sid.clone())
                .or_insert_with(|| crate::relay::desktop::DesktopStream::with_limiter(Some(state.bandwidth.clone())))
                .clone()
        };
        let (_vid, _rx, _init) = ds.add_viewer().await;
        // seq 1..3 → 5（跳 4）：agent→relay 段丢帧检测 +1。
        for s in [1u32, 2, 3, 5] {
            let b = crate::proto::encode_bin_frame(0, s, b"x");
            let (f, seq, p) = crate::proto::decode_bin_frame(&b).unwrap();
            route_bin_desktop_frame(&state, &sid, f, seq, p).await;
        }
        {
            let proto = state.desktop_proto.read().await;
            let ent = proto.get(&sid).expect("desktop_proto 应有记录");
            assert_eq!(ent.3, 1, "seq 跳变应检测 1 帧丢失");
            assert_eq!(ent.0, ent.1, "binary 帧 encoded==decoded（无 base64 膨胀）");
        }
        // 继续 key 帧填满 16 缓冲 + 超量 → 丢旧 → 回传 congested。
        for i in 6..40u32 {
            let b = crate::proto::encode_bin_frame(1, i, b"y");
            let (f, seq, p) = crate::proto::decode_bin_frame(&b).unwrap();
            route_bin_desktop_frame(&state, &sid, f, seq, p).await;
        }
        let mut saw_congested = false;
        while let Ok(msg) = agent_rx.try_recv() {
            if msg.contains("desktop:congested") {
                saw_congested = true;
            }
        }
        assert!(saw_congested, "binary 帧触发缓冲拥塞应回传 desktop:congested");
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
    async fn test_web_replies_never_reach_browser_broadcast() {
        // MYS-2383 硬约束：web:head/chunk/end/error 必须走网页专用回包通道，
        // 绝不能进 broadcast_types —— 否则远端页面内容会灌进终端 SSE、
        // 触发录制、甚至被当成 MCP 结果。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        state
            .sessions
            .update_capabilities(&sid, vec![crate::relay::web_proxy::CAPABILITY.to_string()])
            .await;
        // 一条 agent 通道 + 一条「浏览器」SSE 通道。
        let (agent_tx, mut agent_rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        let (bulk_tx, mut bulk_rx) = mpsc::channel::<String>(crate::relay::BULK_CHANNEL_CAPACITY);
        let (browser_tx, mut browser_rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        let sse_sid = "bs_web_isolation".to_string();
        state
            .sse_sessions
            .write()
            .await
            .insert(sse_sid.clone(), browser_tx);
        {
            let mut broadcast = state.agent_broadcast.write().await;
            let mut cm = ChannelMap::new();
            cm.agent = Some(agent_tx.clone());
            cm.agent_bulk = Some(bulk_tx.clone());
            cm.browser_sessions
                .insert("user-1".to_string(), sse_sid.clone());
            broadcast.insert(sid.clone(), cm);
        }
        // 保活：通道不掉线（agent_rx/bulk_rx 只用于维持发送端有效）。
        let _keep = (agent_tx, agent_rx, bulk_tx, bulk_rx);

        // 登记一个在途网页请求，让回包有主（否则按"未知请求"丢弃）。
        let (tx, mut rx) = mpsc::channel::<crate::relay::web_proxy::WebEvent>(8);
        state
            .web_proxy
            .register_stream(&sid, "rid-iso", tx)
            .await
            .unwrap();

        let page_b64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(b"<html>secret</html>")
        };
        for (ty, payload) in [
            (
                "web:head",
                json!({"request_id": "rid-iso", "status": 200,
                                "headers": [["content-type", "text/html"]],
                                "final_url": "http://127.0.0.1/"}),
            ),
            (
                "web:chunk",
                json!({"request_id": "rid-iso", "seq": 0,
                                 "data": page_b64}),
            ),
            ("web:end", json!({"request_id": "rid-iso"})),
        ] {
            route_agent_message(
                &state,
                &sid,
                &json!({"type": ty, "session_id": sid, "payload": payload}).to_string(),
            )
            .await;
        }
        // 回包全部到了网页专用通道。
        let mut got = Vec::new();
        for _ in 0..3 {
            let ev = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
                .await
                .expect("回包必须进网页通道")
                .expect("网页通道未关闭");
            got.push(ev);
        }
        assert!(
            matches!(
                got[0],
                crate::relay::web_proxy::WebEvent::Head { status: 200, .. }
            ),
            "第一条应是 web:head，实收 {:?}",
            got[0]
        );
        assert!(
            matches!(
                got[1],
                crate::relay::web_proxy::WebEvent::Chunk { seq: 0, .. }
            ),
            "第二条应是 web:chunk，实收 {:?}",
            got[1]
        );
        assert!(
            matches!(got[2], crate::relay::web_proxy::WebEvent::End),
            "第三条应是 web:end，实收 {:?}",
            got[2]
        );

        // 浏览器 SSE 一条都没收到（含页面正文）。
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), browser_rx.recv())
                .await
                .is_err(),
            "网页回包绝不允许进入浏览器广播通道"
        );
    }

    #[tokio::test]
    async fn test_register_overwrites_capabilities_including_empty() {
        // MYS-2383：同一 sid 的新 incarnation 必须**覆盖**旧能力声明，含空数组。
        // 不覆盖的话，旧 agent 重连后仍被当成支持 web:proxy-v1 → UI 显示一个
        // 必然 404 的「网页」入口。
        let state = make_state("");
        let reg = |caps: Value| {
            let state = state.clone();
            async move {
                let body = json!({
                    "type": "agent:register",
                    "token_type": "rw",
                    "session_id": "capovr01",
                    "capabilities": caps,
                });
                let resp = agent_send_handler(
                    State(state.clone()),
                    axum::http::HeaderMap::new(),
                    Json(body),
                )
                .await
                .into_response();
                assert_eq!(resp.status(), 200);
                state.sessions.get_capabilities("capovr01").await
            }
        };

        // 第一次注册声明新 agent 的能力。
        let caps = reg(json!(["web:proxy-v1", "codec:av1"])).await;
        assert_eq!(
            caps,
            vec!["web:proxy-v1".to_string(), "codec:av1".to_string()]
        );

        // 旧 agent 用同一 sid 重连（不带 capabilities）：必须清空，不能让能力残留。
        let caps = reg(json!(null)).await;
        assert!(
            caps.is_empty(),
            "不带 capabilities 的重注册必须覆盖为空，实际 {caps:?}"
        );

        // 显式空数组同样覆盖。
        let _ = reg(json!(["web:proxy-v1"])).await;
        let caps = reg(json!([])).await;
        assert!(caps.is_empty(), "空数组也必须覆盖旧值，实际 {caps:?}");
    }

    #[tokio::test]
    async fn test_browser_sse_sends_session_capabilities() {
        // MYS-2383：浏览器 SSE 握手补发 session:capabilities，payload 只含
        // capabilities 数组（UI 据此决定是否显示「网页」入口）。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        state
            .sessions
            .update_capabilities(&sid, vec!["web:proxy-v1".to_string()])
            .await;
        let (_tx, _agent_rx) = insert_channel_map(&state, &sid).await;
        let mut params = HashMap::new();
        params.insert("token".to_string(), r.tokens[0].0.clone());
        let resp = browser_sse_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Query(params),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), 200);
        let collected = collect_sse_events(resp, 3).await;
        let caps_ev = collected
            .iter()
            .find(|v| v["type"] == "session:capabilities")
            .unwrap_or_else(|| panic!("未收到 session:capabilities，实收 {collected:?}"));
        assert_eq!(caps_ev["session_id"], sid);
        assert_eq!(
            caps_ev["payload"]["capabilities"],
            json!(["web:proxy-v1"]),
            "payload 必须只含 capabilities 数组"
        );
        // connected 必须先到（既有握手语义不变）。
        assert_eq!(collected[0]["type"], "browser:connected");
    }

    #[tokio::test]
    async fn test_browser_sse_capabilities_empty_for_old_agent() {
        // 旧 agent 不声明能力 → 发空数组（UI 看到空就不显示入口，无需猜版本）。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        let (_tx, _agent_rx) = insert_channel_map(&state, &sid).await;
        let mut params = HashMap::new();
        params.insert("token".to_string(), r.tokens[0].0.clone());
        let resp = browser_sse_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Query(params),
        )
        .await
        .into_response();
        let collected = collect_sse_events(resp, 3).await;
        let caps_ev = collected
            .iter()
            .find(|v| v["type"] == "session:capabilities")
            .expect("即使为空也必须有该事件");
        assert_eq!(caps_ev["payload"]["capabilities"], json!([]));
    }

    #[tokio::test]
    async fn test_register_lan_addr_roundtrip() {
        // 阶段2 LAN 直连：agent:register 消息带 lan_addr → parse → set_lan_addr
        // 存进 SessionInfo；缺省/空串/非字符串保持 None（老版本兼容）。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        assert!(
            state.sessions.get(&sid).await.unwrap().lan_addr.is_none(),
            "未上报 lan_addr 应为 None"
        );

        let with_addr = serde_json::json!({
            "type": "agent:register",
            "lan_addr": "192.168.1.5:43210",
        });
        state
            .sessions
            .set_lan_addr(&sid, parse_lan_addr(&with_addr))
            .await;
        assert_eq!(
            state.sessions.get(&sid).await.unwrap().lan_addr.as_deref(),
            Some("192.168.1.5:43210")
        );

        // 空串 → None（不存空地址）。
        let empty = serde_json::json!({ "type": "agent:register", "lan_addr": "" });
        assert!(parse_lan_addr(&empty).is_none(), "空串 lan_addr 应解析为 None");
        // 缺省字段 / 非字符串 → None。
        let missing = serde_json::json!({ "type": "agent:register" });
        assert!(parse_lan_addr(&missing).is_none());
        let non_str = serde_json::json!({ "type": "agent:register", "lan_addr": 42 });
        assert!(parse_lan_addr(&non_str).is_none(), "非字符串 lan_addr 应解析为 None");
    }

    #[tokio::test]
    async fn test_desktop_protocol_overhead_stats() {
        // R5#43 二进制直转观察性子集：base64 编码/解码字节累计——量化 JSON
        // 协议膨胀（"AQID"=4 字符编码 3 字节解码），binary 化 ROI 数据。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        // route_agent_message 只路由已注册会话（需 agent_broadcast 条目）。
        insert_channel_map(&state, &sid).await;
        for _ in 0..2 {
            let frag = serde_json::json!({
                "type": "desktop:video",
                "session_id": sid,
                "payload": { "kind": "frag", "data": "AQID", "key": true }
            });
            route_agent_message(&state, &sid, &frag.to_string()).await;
        }
        let proto = state.desktop_proto.read().await;
        let (enc, dec, last_seq, dropped) = proto.get(&sid).expect("desktop:video must be counted");
        assert_eq!(*enc, 8, "AQID=4 字符 × 2");
        assert_eq!(*dec, 6, "3 字节 × 2");
        assert_eq!(*last_seq, 0, "无 seq 字段不推进 last_seq");
        assert_eq!(*dropped, 0, "无 seq 无丢帧检测");
    }

    #[tokio::test]
    async fn test_desktop_seq_gap_detects_dropped_frames() {
        // R5#41 seq 丢帧检测：消息级 seq 1→3 跳变 → 检测到 1 帧丢失
        // （agent→relay 段；浏览器段 moof seqn 互补）。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        // route_agent_message 只路由已注册会话（需 agent_broadcast 条目）。
        insert_channel_map(&state, &sid).await;
        for seq in [1u64, 3u64] {
            let frag = serde_json::json!({
                "type": "desktop:video",
                "session_id": sid,
                "payload": { "kind": "frag", "data": "AQID", "key": true, "seq": seq }
            });
            route_agent_message(&state, &sid, &frag.to_string()).await;
        }
        let proto = state.desktop_proto.read().await;
        let (_, _, last_seq, dropped) = proto.get(&sid).expect("desktop:video must be counted");
        assert_eq!(*last_seq, 3, "last_seq 应推进到 3");
        assert_eq!(*dropped, 1, "seq 1→3 跳变应检测 1 帧丢失");
    }

    #[tokio::test]
    async fn test_desktop_started_precreates_stream() {
        // 竞态回归: 浏览器收到 desktop:started 后立即 GET /agent/desktop/stream。
        // relay 必须在 started 时就预建 fan-out 流, 否则在首个 desktop:video
        // (init) 到达前浏览器会吃到 404 → 黑屏。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        // route_agent_message 只路由已注册会话（需 agent_broadcast 条目）。
        insert_channel_map(&state, &sid).await;
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

    #[tokio::test]
    async fn test_desktop_p2p_signaling_broadcasts_to_browsers() {
        // Task 2 P2P 信令：agent→浏览器 的 desktop:p2p-answer/candidate/state
        // 必须走 broadcast_types 广播（non-lossy 控制消息）；且不触发
        // desktop:video / desktop:started 的特殊路径（不建桌面 fan-out 流）。
        let state = make_state("");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        insert_channel_map(&state, &sid).await;
        let mut sse_rx = add_browser(&state, &sid, "brow1").await;

        for ty in ["desktop:p2p-answer", "desktop:p2p-candidate", "desktop:p2p-state"] {
            let payload = match ty {
                "desktop:p2p-state" => serde_json::json!({ "state": "connected" }),
                "desktop:p2p-candidate" => serde_json::json!({ "candidate": "candidate:1 1 udp" }),
                _ => serde_json::json!({ "sdp": "{}", "candidates": [] }),
            };
            let msg = serde_json::json!({
                "type": ty,
                "session_id": sid,
                "payload": payload
            });
            route_agent_message(&state, &sid, &msg.to_string()).await;
            let got = tokio::time::timeout(std::time::Duration::from_millis(2000), sse_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("browser must receive {ty}"))
                .unwrap();
            assert!(got.contains(ty), "got: {got}");
        }
        assert!(
            state.desktop_streams.read().await.is_empty(),
            "p2p 消息不应建桌面 fan-out 流（与 desktop:video/started 路径隔离）"
        );
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
        insert_channel_map(&state, "sid1").await;
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
        insert_channel_map(&state, "s").await;
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
        insert_channel_map(&state, "sid1").await;
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
        insert_channel_map(&state, "sid1").await;
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
            assert!(agent_conn_rate_ok(&state, &headers, "s1").await, "窗口内 30 次应放行");
        }
        assert!(!agent_conn_rate_ok(&state, &headers, "s1").await, "第 31 次应超限（HTTP/WS 同配额）");
        // 不同 IP 独立配额，不受影响。
        let other = mk("10.0.0.10");
        assert!(agent_conn_rate_ok(&state, &other, "s1").await, "不同 IP 独立配额");
        // 同一出口 IP 后的其它 agent（不同 session）不被挤占。
        assert!(agent_conn_rate_ok(&state, &headers, "s2").await, "同 IP 不同 session 独立配额");
        // 但 session 由调用方提供：同一 IP 伪造大量 session 仍受 per-IP 总配额约束。
        let flood = mk("10.0.0.77");
        for i in 0..600 {
            assert!(agent_conn_rate_ok(&state, &flood, &format!("f{i}")).await);
        }
        assert!(!agent_conn_rate_ok(&state, &flood, "f-new").await, "per-IP 总配额 600/min");
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
        let resp = browser_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
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
        let resp = browser_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
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
        let resp = browser_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 403);
    }

    #[tokio::test]
    async fn test_browser_send_readonly_cannot_mkdir() {
        // A6：`fs:mkdir` 已移出只读白名单——只读 token 不得创建目录。
        let state = make_state("");
        let r = state.sessions.register(None, "ro", Some("rofs01".to_string())).await.unwrap();
        let sid = r.session_id.clone();
        let token = r.tokens[0].0.clone();
        state
            .agent_broadcast
            .write()
            .await
            .insert(sid, ChannelMap::new());
        let body = json!({"token": token, "type": "fs:mkdir", "payload": {"path": "/tmp/x"}});
        let resp = browser_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 403, "read-only token must not create directories");
    }

    #[tokio::test]
    async fn test_browser_send_readonly_cannot_forge_agent_types() {
        // A6：`terminal:output` / `fs:result` / `mcp:result` / `mcp:exec_result`
        // 是 agent→relay 的回包，浏览器从不发送，已移出只读白名单。
        let state = make_state("");
        let r = state.sessions.register(None, "ro", Some("rofg01".to_string())).await.unwrap();
        let sid = r.session_id.clone();
        let token = r.tokens[0].0.clone();
        state
            .agent_broadcast
            .write()
            .await
            .insert(sid, ChannelMap::new());
        for ty in ["terminal:output", "fs:result", "mcp:result", "mcp:exec_result"] {
            let body = json!({"token": token, "type": ty, "payload": {"data": "eA=="}});
            let resp = browser_send_handler(
                State(state.clone()),
                axum::http::HeaderMap::new(),
                Json(body),
            )
            .await
            .into_response();
            assert_eq!(resp.status(), 403, "ro token must not send {ty}");
        }
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
        let resp = browser_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
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
        let i_pos = got.iter().position(|(_, s)| s == "i0").unwrap();
        let first_bulk = got.iter().position(|(_, s)| s.starts_with('b')).unwrap();
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
            first.1, "interactive-ping",
            "interactive must be very first item under heavy bulk flood; got {:?}",
            first
        );
        assert!(!first.0, "the first item must be flagged interactive (is_bulk=false)");
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
            got[0].1, "terminal:input",
            "interactive must be first in merged output; got {:?}",
            got.first()
        );
        assert!(!got[0].0, "is_bulk flag must be false for interactive");
        assert!(got[1].0, "is_bulk flag must be true for bulk file chunks");
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

    // ── agent 身份校验（A2 / A3 / A4 / A8 / A9 / A10）───────────────

    /// 起一个带 token 已注册、且「在线」（有未关闭的 agent 通道）的会话。
    async fn online_session(
        state: &Arc<SharedState>,
        sid: &str,
    ) -> (mpsc::Receiver<String>, String) {
        let r = state
            .sessions
            .register(None, "rw", Some(sid.to_string()))
            .await
            .unwrap();
        let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        let mut cm = ChannelMap::new();
        cm.agent = Some(tx);
        state
            .agent_broadcast
            .write()
            .await
            .insert(sid.to_string(), cm);
        (rx, r.tokens[0].0.clone())
    }

    fn token_header(token: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            crate::relay::auth::AGENT_TOKEN_HEADER,
            token.parse().unwrap(),
        );
        h
    }

    fn events_params(sid: &str) -> Query<HashMap<String, String>> {
        let mut p = HashMap::new();
        p.insert("session".to_string(), sid.to_string());
        Query(p)
    }

    #[tokio::test]
    async fn test_agent_events_legacy_conflicts_with_online_agent() {
        // 不带 token 且该会话已有在线 agent → 409（不能顶替在线 agent）。
        let state = make_state("");
        let (_rx, _token) = online_session(&state, "live01").await;
        let resp = agent_events_handler(
            State(state),
            axum::http::HeaderMap::new(),
            events_params("live01"),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), 409, "legacy must not hijack a live agent");
    }

    #[tokio::test]
    async fn test_agent_events_wrong_token_returns_401() {
        let state = make_state("");
        let (_rx, _token) = online_session(&state, "live02").await;
        let resp = agent_events_handler(
            State(state),
            token_header("not-a-real-token"),
            events_params("live02"),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), 401, "wrong token must be denied, not treated as legacy");
    }

    #[tokio::test]
    async fn test_agent_events_correct_token_returns_200() {
        let state = make_state("");
        let (_rx, token) = online_session(&state, "live03").await;
        let resp = agent_events_handler(
            State(state),
            token_header(&token),
            events_params("live03"),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), 200, "the session's own token may reconnect");
    }

    #[tokio::test]
    async fn test_agent_events_legacy_allowed_when_agent_offline() {
        // 旧版 agent 在会话无在线 agent 时仍可连接（兼容路径）。
        let state = make_state("");
        state
            .agent_broadcast
            .write()
            .await
            .insert("off01".to_string(), ChannelMap::new());
        let resp = agent_events_handler(
            State(state),
            axum::http::HeaderMap::new(),
            events_params("off01"),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn test_strict_agent_auth_rejects_events_and_send() {
        // --strict-agent-auth：不带头的 /agent/events 与 /agent/send 都 401。
        let state = make_state("");
        state.set_strict_agent_auth(true);
        state
            .agent_broadcast
            .write()
            .await
            .insert("strict01".to_string(), ChannelMap::new());

        let resp = agent_events_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            events_params("strict01"),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), 401, "strict mode must reject tokenless /agent/events");

        let body = json!({"type":"terminal:output","session_id":"strict01","payload":{"data":"eA=="}});
        let resp = agent_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 401, "strict mode must reject tokenless /agent/send");
    }

    #[tokio::test]
    async fn test_register_conflicts_when_id_held_by_live_agent() {
        // 同 id + 有在线 agent + 无归属凭据 → 409 SESSION_ID_IN_USE。
        let state = make_state("");
        let (_rx, _token) = online_session(&state, "taken01").await;
        let body = json!({"type":"agent:register","token_type":"rw","session_id":"taken01"});
        let resp = agent_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 409);
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["error"], "SESSION_ID_IN_USE");
    }

    #[tokio::test]
    async fn test_register_same_fixed_key_reclaims_live_session() {
        // fixed key 相同即证明归属 → 允许接管（agent 重启的正常路径）。
        let state = make_state("");
        let r = state
            .sessions
            .register(Some("key-A".to_string()), "rw", Some("keyed01".to_string()))
            .await
            .unwrap();
        let (_tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        let mut cm = ChannelMap::new();
        cm.agent = Some(_tx);
        state
            .agent_broadcast
            .write()
            .await
            .insert("keyed01".to_string(), cm);
        let _ = rx;
        assert!(crate::relay::auth::agent_online(&state, "keyed01").await);

        let body = json!({
            "type":"agent:register","token_type":"rw",
            "key":"key-A","session_id":"keyed01"
        });
        let resp = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200, "matching --key proves ownership");
        let _ = r;
    }

    #[tokio::test]
    async fn test_register_cached_token_reclaims_live_session() {
        // 缓存 token 命中同样证明归属（自动重连），且 evicted=true。
        let state = make_state("");
        let (_rx, token) = online_session(&state, "cached01").await;
        let body = json!({
            "type":"agent:register",
            "session_id":"cached01",
            "tokens":[{"token": token, "permission": "rw"}]
        });
        let resp = agent_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200, "cached token proves ownership");
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["evicted"], true);
    }

    #[tokio::test]
    async fn test_register_offline_session_can_be_taken_over() {
        // agent 离线（无在线通道）时同 id 重注册保持旧行为：直接接管。
        let state = make_state("");
        state.sessions
            .register(None, "rw", Some("idle01".to_string()))
            .await
            .unwrap();
        state
            .agent_broadcast
            .write()
            .await
            .insert("idle01".to_string(), ChannelMap::new());
        let body = json!({"type":"agent:register","token_type":"rw","session_id":"idle01"});
        let resp = agent_send_handler(State(state), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn test_register_writes_last_activity() {
        // 注册即写 last_activity，否则静默会话永不被空闲回收。
        let state = make_state("");
        let body = json!({"type":"agent:register","token_type":"rw","session_id":"actv01"});
        let _ = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert!(
            state.last_activity.read().await.contains_key("actv01"),
            "registration must seed last_activity"
        );
    }

    #[tokio::test]
    async fn test_agent_send_rejects_token_for_other_session() {
        // 令牌主人只能给自己的会话发消息：用 A 的 token 发 session_id=B 的
        // 消息必须被丢弃（不能投递到 B 的浏览器）。
        let state = make_state("");
        let (_rx_a, token_a) = online_session(&state, "sessAA").await;
        let _rx_b = {
            let r = state
                .sessions
                .register(None, "rw", Some("sessBB".to_string()))
                .await
                .unwrap();
            let _ = r;
            let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
            let mut cm = ChannelMap::new();
            cm.agent = Some(tx);
            state
                .agent_broadcast
                .write()
                .await
                .insert("sessBB".to_string(), cm);
            rx
        };
        let mut rx_b = _rx_b;
        let body = json!({
            "type":"terminal:output","session_id":"sessBB",
            "payload":{"data":"Zm9yZ2Vk"}
        });
        let resp = agent_send_handler(State(state.clone()), token_header(&token_a), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200, "request accepted, message dropped");
        assert!(
            rx_b.try_recv().is_err(),
            "A's token must not inject output into B's session"
        );
        let _ = (state, token_a);
    }

    #[tokio::test]
    async fn test_agent_send_rejects_unknown_session_id() {
        // 未注册 sid → 丢弃，且不得为该 sid 建任何 per-session 状态条目。
        let state = make_state("");
        let body = json!({"type":"terminal:output","session_id":"ghost01","payload":{"data":"eA=="}});
        let resp = agent_send_handler(State(state.clone()), axum::http::HeaderMap::new(), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), 200);
        assert!(
            state.kpi_history.read().await.is_empty()
                && state.desktop_proto.read().await.is_empty()
                && state.desktop_streams.read().await.is_empty(),
            "unregistered sid must not create state rows"
        );
    }

    #[tokio::test]
    async fn test_route_agent_message_drops_unregistered_session() {
        // route_agent_message 遇到未注册 sid 直接返回（旧行为会凭空建条目）。
        let state = make_state("");
        let msg = json!({
            "type": "ping", "session_id": "nope01",
            "kpi": {"running": true, "fps": 30}
        })
        .to_string();
        route_agent_message(&state, "nope01", &msg).await;
        assert!(state.kpi_history.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_download_sink_full_does_not_block_agent_uplink() {
        // A8：浏览器不消费下载块时，agent 上行不得被卡住——其它消息仍须
        // 正常路由（旧实现把所有上行堵在这个 await 上）。
        let state = make_state("");
        let r = state
            .sessions
            .register(None, "rw", Some("dlblk1".to_string()))
            .await
            .unwrap();
        let sid = r.session_id;
        // 容量 1 的 sink：先填满，之后每次 send 都会等 5s 超时。
        use crate::relay::file_transfer::DownloadEvent;
        let (sink_tx, _sink_rx) = mpsc::channel::<DownloadEvent>(1);
        state.download_streams.write().await.insert(
            "dl-full".to_string(),
            crate::relay::file_transfer::DownloadSink {
                tx: sink_tx,
                last_activity: Instant::now(),
                bytes: 0,
            },
        );
        // 让 sink 先满
        {
            let mut ds = state.download_streams.write().await;
            let sink = ds.get_mut("dl-full").unwrap();
            let _ = sink.tx.try_send(DownloadEvent::Chunk {
                data: vec![0u8; 8],
                file_size: None,
            });
        }
        let _agent_rx = {
            let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
            state
                .agent_broadcast
                .write()
                .await
                .entry(sid.clone())
                .or_insert_with(ChannelMap::new)
                .agent = Some(tx);
            rx
        };
        let mut browser_rx = add_browser(&state, &sid, "brow-dl").await;

        // 一个会被卡住的下载块
        let chunk = json!({
            "type":"fs:result","session_id":sid,
            "payload":{"success":true,"content":"aGk=","path":"/x",
            "chunk_index":0,"total_chunks":2,"_mcp_request_id":"dl-full"}
        })
        .to_string();
        let st = state.clone();
        let sid_c = sid.clone();
        let blocker = tokio::spawn(async move {
            route_agent_message(&st, &sid_c, &chunk).await;
        });
        // 卡住的这段时间里，终端输出必须仍能送达浏览器。
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let out = json!({"type":"terminal:output","session_id":sid,"payload":{"data":"b2s="}})
            .to_string();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            route_agent_message(&state, &sid, &out),
        )
        .await
        .expect("terminal output must not be blocked by a stuck download sink");
        // fs:result 本身也在广播白名单里，浏览器会先收到它；继续排空直到
        // 拿到 terminal:output。
        let mut got = String::new();
        for _ in 0..4 {
            let m = tokio::time::timeout(std::time::Duration::from_secs(2), browser_rx.recv())
                .await
                .expect("browser must receive terminal output while the sink is stalled")
                .unwrap();
            if m.contains("terminal:output") {
                got = m;
                break;
            }
        }
        assert!(got.contains("terminal:output"), "got: {got}");
        // 5s 超时后卡住的调用也应结束
        tokio::time::timeout(std::time::Duration::from_secs(8), blocker)
            .await
            .expect("stalled sink send must time out rather than block forever")
            .unwrap();
    }

    #[tokio::test]
    async fn test_desktop_state_snapshot_sent_to_browser_not_agent() {
        // A10：desktop:state 快照只发给浏览器；agent 下行不再收到它。
        let state = make_state("");
        let r = state
            .sessions
            .register(None, "rw", Some("snap01".to_string()))
            .await
            .unwrap();
        let sid = r.session_id.clone();
        let token = r.tokens[0].0.clone();
        let (agent_tx, mut agent_rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        {
            let mut cm = ChannelMap::new();
            cm.agent = Some(agent_tx);
            state
                .agent_broadcast
                .write()
                .await
                .insert(sid.clone(), cm);
        }
        // agent 报 desktop:started → desktop_states[sid] = true
        let started = json!({"type":"desktop:started","session_id":sid,"payload":{}}).to_string();
        route_agent_message(&state, &sid, &started).await;
        // 清掉 agent 侧刚才可能收到的广播（started 属 broadcast_types）
        while agent_rx.try_recv().is_ok() {}

        let mut params = HashMap::new();
        params.insert("token".to_string(), token);
        let resp = browser_sse_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Query(params),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), 200);
        // 浏览器流里应能读到 desktop:state
        use tokio_stream::StreamExt as _;
        let mut body = resp.into_body().into_data_stream();
        let mut saw_state = false;
        for _ in 0..10 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), body.next()).await {
                Ok(Some(Ok(chunk))) => {
                    let text = String::from_utf8_lossy(&chunk);
                    if text.contains("desktop:state") {
                        saw_state = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(saw_state, "browser SSE must carry the desktop:state snapshot");
        drop(body);
        // agent 侧不应收到 desktop:state
        while let Ok(m) = agent_rx.try_recv() {
            assert!(
                !m.contains("desktop:state"),
                "desktop:state must not be sent to the agent: {m}"
            );
        }
    }

    #[tokio::test]
    async fn test_revoke_closes_browser_stream_using_that_token() {
        // A7：吊销 token 后，用该 token 建立的浏览器 SSE 必须结束。
        let state = make_state("");
        let r = state
            .sessions
            .register(None, "both", Some("revk01".to_string()))
            .await
            .unwrap();
        let sid = r.session_id.clone();
        let rw = r.tokens[0].0.clone();
        let ro = r.tokens[1].0.clone();
        let (agent_tx, _agent_rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        {
            let mut cm = ChannelMap::new();
            cm.agent = Some(agent_tx);
            state
                .agent_broadcast
                .write()
                .await
                .insert(sid.clone(), cm);
        }
        // 两条浏览器流：rw 与 ro 各一条
        let mut params_rw = HashMap::new();
        params_rw.insert("token".to_string(), rw.clone());
        let resp_rw = browser_sse_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Query(params_rw),
        )
        .await
        .into_response();
        assert_eq!(resp_rw.status(), 200);
        let mut params_ro = HashMap::new();
        params_ro.insert("token".to_string(), ro.clone());
        let resp_ro = browser_sse_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Query(params_ro),
        )
        .await
        .into_response();
        assert_eq!(resp_ro.status(), 200);

        let closed = disconnect_browser_streams(&state, &sid, Some(std::slice::from_ref(&rw))).await;
        assert_eq!(closed, 1, "only the stream that used the revoked token closes");
        let cm = state.agent_broadcast.read().await;
        let cm = cm.get(&sid).unwrap();
        assert_eq!(cm.browser_sessions.len(), 1, "the ro stream must survive");
    }

    #[tokio::test]
    async fn test_regenerate_closes_all_browser_streams() {
        let state = make_state("");
        let r = state
            .sessions
            .register(None, "rw", Some("regen1".to_string()))
            .await
            .unwrap();
        let sid = r.session_id.clone();
        let token = r.tokens[0].0.clone();
        let (agent_tx, _agent_rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        {
            let mut cm = ChannelMap::new();
            cm.agent = Some(agent_tx);
            state
                .agent_broadcast
                .write()
                .await
                .insert(sid.clone(), cm);
        }
        let mut params = HashMap::new();
        params.insert("token".to_string(), token);
        let resp = browser_sse_handler(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Query(params),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), 200);
        // 轮换后旧 token 全失效 → 全部断开
        let closed = disconnect_browser_streams(&state, &sid, None).await;
        assert_eq!(closed, 1);
        assert!(state
            .agent_broadcast
            .read()
            .await
            .get(&sid)
            .unwrap()
            .browser_sessions
            .is_empty());
    }

    #[tokio::test]
    async fn test_remove_session_state_clears_all_maps() {
        // A9：回收/kick 后各 per-session map 中不再有该 sid。
        let state = make_state("");
        let sid = "clean01";
        state
            .agent_broadcast
            .write()
            .await
            .insert(sid.to_string(), ChannelMap::new());
        state.last_activity.write().await.insert(sid.to_string(), Instant::now());
        state.kpi_history.write().await.insert(sid.to_string(), Default::default());
        state.desktop_proto.write().await.insert(sid.to_string(), (1, 2, 3, 4));
        state.desktop_states.write().await.insert(sid.to_string(), true);
        state
            .last_congest_notify
            .write()
            .await
            .insert(sid.to_string(), Instant::now());
        state.last_kpi_alert.write().await.insert(sid.to_string(), Instant::now());
        state
            .agent_upgrades
            .write()
            .await
            .insert(sid.to_string(), json!({"p": 1}));
        state
            .desktop_streams
            .write()
            .await
            .insert(sid.to_string(), crate::relay::desktop::DesktopStream::new());
        let _ = state.buffer_agent_event(sid, "{}").await;

        crate::relay::remove_session_state(&state, sid).await;

        assert!(state.last_activity.read().await.get(sid).is_none());
        assert!(state.kpi_history.read().await.get(sid).is_none());
        assert!(state.desktop_proto.read().await.get(sid).is_none());
        assert!(state.desktop_states.read().await.get(sid).is_none());
        assert!(state.last_congest_notify.read().await.get(sid).is_none());
        assert!(state.last_kpi_alert.read().await.get(sid).is_none());
        assert!(state.agent_upgrades.read().await.get(sid).is_none());
        assert!(state.desktop_streams.read().await.get(sid).is_none());
        assert!(state.agent_event_buffers.read().await.get(sid).is_none());
    }

    #[tokio::test]
    async fn test_end_desktop_streams_clears_state() {
        // A7 桌面部分：凭证变更后桌面下行流必须结束（否则旧流继续推帧）。
        let state = make_state("");
        let sid = "desk01";
        state
            .desktop_streams
            .write()
            .await
            .insert(sid.to_string(), crate::relay::desktop::DesktopStream::new());
        state.desktop_states.write().await.insert(sid.to_string(), true);
        end_desktop_streams(&state, sid).await;
        assert!(state.desktop_streams.read().await.get(sid).is_none());
        assert_eq!(
            state.desktop_states.read().await.get(sid),
            Some(&false),
            "browsers must see desktop as stopped after the stream is torn down"
        );
    }

    #[tokio::test]
    async fn test_agent_disconnect_marks_desktop_stopped() {
        // agent 断线后 desktop_states 应显式置 false（否则浏览器重建 SSE
        // 时拿到的快照仍是 running=true）。
        let state = make_state("");
        let sid = "dcstate1";
        let (tx, _rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        let (bulk_tx, _bulk_rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);
        {
            let mut cm = ChannelMap::new();
            cm.agent = Some(tx.clone());
            cm.agent_bulk = Some(bulk_tx.clone());
            state
                .agent_broadcast
                .write()
                .await
                .insert(sid.to_string(), cm);
        }
        state.desktop_states.write().await.insert(sid.to_string(), true);
        drop(AgentEventsCleanup {
            tx,
            bulk_tx,
            state: state.clone(),
            session_id: sid.to_string(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            state.desktop_states.read().await.get(sid),
            Some(&false),
            "agent disconnect must mark desktop stopped"
        );
    }
}
