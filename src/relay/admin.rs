//! Web admin panel: hidden sub-path + login, then session/token/runtime
//! management. Routes are registered dynamically in `relay::start` under the
//! configured `--admin-path`; when that flag is unset, no admin route exists
//! and the panel is completely inaccessible.
#![allow(clippy::too_many_arguments)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::proto::Permission;
use crate::relay::artifact_store::{ArtifactRef, Variant};
use crate::relay::auth::constant_time_eq;
use crate::relay::SharedState;

/// Admin login session lifetime.
const ADMIN_SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

fn perm_str(p: &Permission) -> &'static str {
    match p {
        Permission::ReadWrite => "rw",
        Permission::ReadOnly => "ro",
    }
}

fn parse_perm(s: &str) -> Permission {
    if s.eq_ignore_ascii_case("ro") {
        Permission::ReadOnly
    } else {
        Permission::ReadWrite
    }
}

fn generate_admin_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::thread_rng().gen();
    hex::encode(bytes)
}

/// Extract the `sr_admin` cookie value from the Cookie header.
fn admin_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    for part in cookie.split(';') {
        let p = part.trim();
        if let Some(v) = p.strip_prefix("sr_admin=") {
            return Some(v.to_string());
        }
    }
    None
}

/// Validate the admin cookie. Cleans up expired sessions opportunistically.
async fn check_admin(state: &SharedState, headers: &HeaderMap) -> bool {
    let Some(token) = admin_cookie(headers) else {
        return false;
    };
    let now = Instant::now();
    let mut sessions = state.admin_sessions.write().await;
    sessions.retain(|_, exp| *exp > now);
    matches!(sessions.get(&token), Some(exp) if *exp > now)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "unauthorized"})),
    )
        .into_response()
}

/// Serve the admin single-page app. The HTML lives next to this file (not in
/// `web/`) so the public `static_handler` cannot serve it — the page is only
/// reachable at the configured secret path.
pub async fn admin_page_handler(State(_state): State<Arc<SharedState>>) -> Response {
    let html = include_str!("admin_page.html");
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Body::from(html))
        .unwrap()
}

pub async fn login_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // 登录限流（A11）：5 次/分钟/IP，超出直接 429，且不在限流窗口内校验
    // 口令（避免暴力破解把 CPU 花在 constant_time_eq 上）。key 用
    // `client_ip`（XFF 取最后一跳），伪造 XFF 无法无限换桶。
    {
        let ip = crate::relay::auth::client_ip(&headers);
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&format!("admin:{ip}"), 5, std::time::Duration::from_secs(60)) {
            tracing::warn!(ip = %ip, "admin login rate limited");
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"ok": false, "error": "too many login attempts"})),
            )
                .into_response();
        }
    }
    let user = body["user"].as_str().unwrap_or("");
    let pass = body["pass"].as_str().unwrap_or("");
    if constant_time_eq(user, &state.admin_user) && constant_time_eq(pass, &state.admin_pass) {
        let token = generate_admin_token();
        state
            .admin_sessions
            .write()
            .await
            .insert(token.clone(), Instant::now() + ADMIN_SESSION_TTL);
        let cookie = format!(
            "sr_admin={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
            token,
            ADMIN_SESSION_TTL.as_secs()
        );
        return (
            StatusCode::OK,
            [("set-cookie", cookie.as_str())],
            Json(json!({"ok": true})),
        )
            .into_response();
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"ok": false, "error": "invalid credentials"})),
    )
        .into_response()
}

pub async fn logout_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = admin_cookie(&headers) {
        state.admin_sessions.write().await.remove(&token);
    }
    (
        StatusCode::OK,
        [("set-cookie", "sr_admin=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")],
        Json(json!({"ok": true})),
    )
        .into_response()
}

pub async fn overview_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let sessions = state.sessions.list_sessions().await;
    let broadcasts = state.agent_broadcast.read().await;
    let activity = state.last_activity.read().await;
    let upgrades = state.agent_upgrades.read().await;
    let now = Instant::now();
    let mut sess_json: Vec<Value> = Vec::with_capacity(sessions.len());
    let mut agent_online = 0usize;
    let mut browser_total = 0usize;
    for (sid, info) in &sessions {
        let cm = broadcasts.get(sid);
        let online = cm.map(|c| c.agent.is_some()).unwrap_or(false);
        let browser_count = cm.map(|c| c.browser_sessions.len()).unwrap_or(0);
        if online {
            agent_online += 1;
        }
        browser_total += browser_count;
        let last_active_seconds = activity
            .get(sid)
            .map(|last| now.saturating_duration_since(*last).as_secs());
        let tokens: Vec<Value> = info
            .tokens
            .iter()
            .map(|(t, p)| json!({"token": t, "permission": perm_str(p)}))
            .collect();
        sess_json.push(json!({
            "session_id": sid,
            "online": online,
            "is_temporary": info.is_temporary,
            "fixed_key": info.fixed_key,
            "device": info.device,
            "agent_version": info.agent_version,
            "capabilities": info.capabilities,
            "lan_addr": info.lan_addr,
            "desktop_proto": state
                .desktop_proto
                .read()
                .await
                .get(sid)
                .map(|(enc, dec, _, dropped)| json!({
                    "encoded_bytes": enc,
                    "decoded_bytes": dec,
                    "dropped_frames": dropped,
                })),
            "upgrade": upgrades.get(sid),
            "browser_count": browser_count,
            "last_active_seconds": last_active_seconds,
            "tokens": tokens,
            "recording": state.recorder.as_ref().is_some_and(|r| r.is_recording(sid)),
            "mcp_audit": state.recorder.as_ref().is_some_and(|r| r.is_auditing(sid)),
        }));
    }
    drop(activity);
    drop(broadcasts);
    drop(upgrades);

    // Access-audit trail (browser/MCP connections), newest first.
    let conn_log: Vec<Value> = state
        .conn_log
        .read()
        .await
        .iter()
        .rev()
        .map(|e| {
            json!({
                "session": e.session,
                "prefix": e.prefix,
                "permission": e.permission,
                "at": e.at,
                "kind": e.kind,
            })
        })
        .collect();

    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "agent_count": sessions.len(),
        "agent_online": agent_online,
        "browser_count": browser_total,
        "sessions": sess_json,
        "recording_enabled": state.recorder.is_some(),
        "mcp_audit_enabled": state.recorder.is_some(),
        "conn_log": conn_log,
        // 概览页的带宽卡片（MYS-2287）：与 GET /api/bandwidth 的 `stats` 同构。
        "bandwidth": state.bandwidth.stats().to_json(),
    }))
    .into_response()
}

/// Admin KPI 曲线数据（R5 丙111/140）：返回指定 session 的 agent 心跳 KPI
/// 时间序列（fps/bitrate/quality/codec，15s 粒度、30min 窗口）。前端画
/// canvas 折线用；session 无历史时返回空数组。
pub async fn session_kpi_handler(
    State(state): State<Arc<SharedState>>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let hist = state.kpi_history.read().await;
    let samples = hist.get(&sid).cloned().unwrap_or_default();
    Json(json!({
        "session_id": sid,
        "samples": samples,
    }))
    .into_response()
}

pub async fn kick_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let sid = body["session_id"].as_str().unwrap_or("").to_string();
    if sid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing session_id"})),
        )
            .into_response();
    }
    // Collect browser ids before dropping the channel map.
    let browser_ids: Vec<String> = {
        let bc = state.agent_broadcast.read().await;
        bc.get(&sid)
            .map(|c| c.browser_sessions.keys().cloned().collect())
            .unwrap_or_default()
    };
    // Drop the agent channel (agent disconnects on next downstream op).
    state.agent_broadcast.write().await.remove(&sid);
    // Drop browser SSE senders (browsers disconnect).
    {
        let mut sse = state.sse_sessions.write().await;
        for bid in browser_ids {
            sse.remove(&bid);
        }
    }
    // Invalidate all session tokens.
    state.sessions.remove(&sid).await;
    // 统一清理所有按 sid 索引的状态表（KPI 历史、桌面状态/流、升级状态、
    // 空闲时间、事件缓冲）。此前只清了 2 张，其余随 kick 次数单调增长。
    crate::relay::remove_session_state(&state, &sid).await;
    if let Some(rec) = &state.recorder {
        rec.close(&sid);
    }
    Json(json!({"ok": true})).into_response()
}

pub async fn revoke_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let token = body["token"].as_str().unwrap_or("").to_string();
    if token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing token"})),
        )
            .into_response();
    }
    let sid_before = state.sessions.session_of_token(&token).await;
    let ok = state.sessions.revoke_token(&token).await;
    // A7：用该 token 建立的浏览器流必须立刻断开——否则吊销后它继续收
    // 终端输出/桌面帧（token 已失效但流还活着）。
    if ok {
        if let Some(sid) = sid_before {
            crate::relay::ws::disconnect_browser_streams(
                &state,
                &sid,
                Some(std::slice::from_ref(&token)),
            )
            .await;
            crate::relay::ws::end_desktop_streams(&state, &sid).await;
        }
    }
    Json(json!({"ok": ok})).into_response()
}

pub async fn regenerate_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let sid = body["session_id"].as_str().unwrap_or("").to_string();
    if sid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing session_id"})),
        )
            .into_response();
    }
    match state.sessions.regenerate_session(&sid).await {
        Some(tokens) => {
            // A7：旧 token 全部作废 → 该会话所有浏览器流断开，让它们用新
            // token 重连（旧流不可能自动换 token）。
            crate::relay::ws::disconnect_browser_streams(&state, &sid, None).await;
            crate::relay::ws::end_desktop_streams(&state, &sid).await;
            let t: Vec<Value> = tokens
                .iter()
                .map(|(tok, p)| json!({"token": tok, "permission": perm_str(p)}))
                .collect();
            Json(json!({"ok": true, "tokens": t})).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "session not found"})),
        )
            .into_response(),
    }
}

pub async fn permission_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let token = body["token"].as_str().unwrap_or("").to_string();
    let perm = body["permission"].as_str().unwrap_or("rw");
    if token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing token"})),
        )
            .into_response();
    }
    let sid_before = state.sessions.session_of_token(&token).await;
    let ok = state
        .sessions
        .set_token_permission(&token, parse_perm(perm))
        .await;
    // A7：降权（rw→ro）后，已建立的浏览器流仍按旧权限在用（它能继续发
    // 写类消息）。断开让浏览器拿新权限重连；升权也一并断开，语义一致
    // 且更简单（两者都是「这条流的权限已不再准确」）。
    if ok {
        if let Some(sid) = sid_before {
            crate::relay::ws::disconnect_browser_streams(
                &state,
                &sid,
                Some(std::slice::from_ref(&token)),
            )
            .await;
            crate::relay::ws::end_desktop_streams(&state, &sid).await;
        }
    }
    Json(json!({"ok": ok})).into_response()
}

pub async fn get_server_auth_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let cur = state.server_auth.read().await.clone();
    Json(json!({"server_auth": cur})).into_response()
}

pub async fn set_server_auth_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let new = body["password"].as_str().unwrap_or("");
    if new.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing password"})),
        )
            .into_response();
    }
    *state.server_auth.write().await = new.to_string();
    Json(json!({"ok": true})).into_response()
}

/// A parsed numeric query param. `axum::extract::Query<Value>` deserializes
/// query strings through `serde_urlencoded`, which has no type info, so every
/// value arrives as a JSON string; accept both a JSON number and a parseable
/// string.
fn q_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

// ── 带宽限制（MYS-2287）─────────────────────────────────────────────

/// `GET {ap}/api/bandwidth`：当前生效速率 + 来源 + 实时统计。
pub async fn get_bandwidth_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    Json(state.bandwidth.to_json().await).into_response()
}

/// 校验一个 admin 提交的 Mbps 值：必须是有限数且落在 `0..=MAX_MBPS`。
/// `null`/缺失按"不改这一项"处理（返回 `Ok(None)`）。
fn parse_mbps(v: Option<&Value>) -> Result<Option<f64>, &'static str> {
    let Some(v) = v else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let Some(n) = v.as_f64() else {
        return Err("must be a number");
    };
    if !n.is_finite() || n.is_nan() {
        return Err("must be a finite number");
    }
    if !(0.0..=crate::relay::bandwidth::MAX_MBPS).contains(&n) {
        return Err("out of range");
    }
    Ok(Some(n))
}

/// `POST {ap}/api/bandwidth`，body `{"down_mbps": f64, "up_mbps": f64}`。
///
/// 校验通过后**立即**改内存里的令牌桶（即时生效），再持久化到
/// `--settings-file`。持久化失败仍回 500，但内存值保留，并在响应体里带上
/// `warning` 让前端提示"已生效但重启会丢"。
pub async fn set_bandwidth_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let down = match parse_mbps(body.get("down_mbps")) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": format!("down_mbps {e}")})),
            )
                .into_response()
        }
    };
    let up = match parse_mbps(body.get("up_mbps")) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": format!("up_mbps {e}")})),
            )
                .into_response()
        }
    };
    if down.is_none() && up.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing down_mbps/up_mbps"})),
        )
            .into_response();
    }
    // 未提供的方向保持当前值。
    let new_down = down.unwrap_or_else(|| state.bandwidth.down_mbps());
    let new_up = up.unwrap_or_else(|| state.bandwidth.up_mbps());
    state.bandwidth.set_mbps(new_down, new_up);
    // 后台改过的值就是"当前生效值"，来源不再代表它（仍记录为 admin 时刻的
    // 来源，便于排查"这个数从哪来"）。这里把来源标成 file —— 改动会被写成
    // settings 文件，重启后即以文件为准，与实际情况一致。
    let mut warning: Option<String> = None;
    match state.bandwidth.persist().await {
        Ok(()) => {
            state
                .bandwidth
                .set_sources(
                    crate::relay::bandwidth::RateSource::File,
                    crate::relay::bandwidth::RateSource::File,
                )
                .await;
        }
        Err(e) => {
            warning = Some(format!("已生效，但写入设置文件失败：{e}"));
            tracing::warn!(error = %e, "bandwidth settings persist failed (in-memory value kept)");
        }
    }
    let mut resp = state.bandwidth.to_json().await;
    if let Some(w) = warning {
        resp["ok"] = json!(true);
        resp["warning"] = json!(w);
    } else {
        resp["ok"] = json!(true);
    }
    Json(resp).into_response()
}

/// List recorded files (terminal `.cast` sessions + MCP `.audit.jsonl`
/// commands), newest first, paginated. Requires `--record-dir`; an empty list
/// when recording is disabled. Accepts `page` (1-based, default 1) and
/// `page_size` (default 20, clamped to 1..=100) query params; returns the
/// current page plus total/page/page_size/pages so the admin panel can render
/// a pager.
pub async fn recordings_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let Some(rec) = &state.recorder else {
        return Json(json!({"recordings": [], "enabled": false})).into_response();
    };
    // Non-numeric / out-of-range params fall back to defaults rather than
    // erroring; page 0 or negative is treated as page 1.
    let page = q_u64(&params["page"]).unwrap_or(1).max(1) as usize;
    let page_size = q_u64(&params["page_size"]).unwrap_or(20).clamp(1, 100) as usize;
    let views: Vec<crate::relay::recorder::RecordingView> = rec
        .list_recordings()
        .iter()
        .map(crate::relay::recorder::RecordingView::from_file)
        .collect();
    let total = views.len();
    let pages = total.div_ceil(page_size);
    let start = (page - 1) * page_size;
    let page_views = if start >= total {
        Vec::new()
    } else {
        views[start..(start + page_size).min(total)].to_vec()
    };
    Json(json!({
        "recordings": page_views,
        "total": total,
        "page": page,
        "page_size": page_size,
        "pages": pages,
        "enabled": true,
    }))
    .into_response()
}

/// Serve a recording's raw file content (`?file=<name>`). Cast files are
/// returned as-is so the admin player can parse the asciinema v2 lines; audit
/// files are served as JSONL text. The file name is validated against the
/// record dir to block path traversal.
pub async fn recording_content_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let name = params["file"].as_str().unwrap_or("");
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing file"})),
        )
            .into_response();
    }
    let Some(rec) = &state.recorder else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "recording disabled"}))).into_response();
    };
    let Some(path) = rec.recording_path(name) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid file"}))).into_response();
    };
    // 录像内容下载同样走整体下行限速（MYS-2287）。录像文件通常不大，但
    // 一次拖多个历史录像同样是可观的出站带宽。
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => {
            state.bandwidth.down.acquire(text.len()).await;
            (
                StatusCode::OK,
                [("content-type", "text/plain; charset=utf-8")],
                text,
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("not found: {}", e)})),
        )
            .into_response(),
    }
}

/// Delete a recorded file (`{file: <name>}`). Only files inside the record
/// dir that the recorder owns (`.cast` / `.audit.jsonl`) can be removed.
pub async fn recording_delete_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let name = body["file"].as_str().unwrap_or("");
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing file"})),
        )
            .into_response();
    }
    let Some(rec) = &state.recorder else {
        return (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": "recording disabled"}))).into_response();
    };
    let Some(path) = rec.recording_path(name) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": "invalid file"}))).into_response();
    };
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": "file not found"}))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": format!("{}", e)})),
        )
            .into_response(),
    }
}

// ── Agent self-upgrade (device panel) ────────────────────────────────

/// Canonical artifact arch key for a device-reported arch. Release binaries
/// are named `shell-remote-{key}` with key x86_64 / aarch64 / armv7 — the
/// `uname -m` variants (armv7l, amd64, arm64, …) map onto them.
///
/// 实现在 [`artifact_store::normalize_arch`]（统一制品库的产物命名规则是唯一
/// 真相源），这里保留函数名以不动既有调用点与测试。
pub fn artifact_arch_key(arch: &str) -> String {
    crate::relay::artifact_store::normalize_arch(arch)
}

/// Parse a staged artifact filename into (arch key, windows).
///
/// **兼容语义**：历史版本这里只认 CLI 名（`shell-remote-<arch>[.exe]`）。
/// MYS-2425 起 UI 制品（`shell-remote-ui-<arch>`）也是合法升级制品，因此本
/// 函数对两者都返回 `true`，`arch` 为归一化后的架构 key。需要区分形态的调用方
/// 请用 [`crate::relay::artifact_store::parse_artifact_ref`]。
pub fn parse_artifact_name(name: &str) -> Option<(String, bool)> {
    let r = crate::relay::artifact_store::parse_artifact_ref(name)?;
    Some((r.arch, r.windows))
}

/// True when `name` is a valid staged artifact filename. The blob download
/// endpoint uses this as its only path guard — it must reject every traversal
/// or separator trick.
pub fn valid_artifact_filename(name: &str) -> bool {
    crate::relay::artifact_store::parse_artifact_ref(name).is_some()
}

/// 版本标签：优先用制品库里的 `<filename 去掉 .exe>.version` 伴随文件，其次用
/// 官方清单里的版本，最后退回 `v?`。
fn artifact_version(store: &crate::relay::artifact_store::ArtifactStore, artifact: &ArtifactRef) -> String {
    store
        .declared_version(artifact)
        .unwrap_or_else(|| "v?".to_string())
}

/// 设备的制品形态判定结果。
///
/// **未知形态必须拒绝，不能猜。** 老 agent 不上报 `variant:*`，此时无论默认
/// 成哪个都可能是错的：默认 CLI 会把现有 UI 设备换成 lean CLI（静默丢掉桌面
/// 能力），默认 UI 会给 CLI 设备换上 30MB 桌面包。两者都不是"安全的保守值"，
/// 因为升级是**原地替换二进制**，猜错就是能力丢失。因此第三档改为
/// [`VariantResolution::Unknown`]，由调用方回 409 并要求管理员显式配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariantResolution {
    /// 已确定形态，附带判定依据（`capability` / `override`）。
    Known {
        variant: Variant,
        source: &'static str,
    },
    /// agent 未声明 `variant:*`，且没有 `--upgrade-variant` 兜底。
    Unknown,
}

impl VariantResolution {
    pub fn variant(self) -> Option<Variant> {
        match self {
            Self::Known { variant, .. } => Some(variant),
            Self::Unknown => None,
        }
    }

    pub fn source(self) -> &'static str {
        match self {
            Self::Known { source, .. } => source,
            Self::Unknown => "unknown",
        }
    }
}

/// 决定设备该拿哪个制品形态（见 [`VariantResolution`]）。
///
/// **不能用 `desktop:*` capability 判断**：`build_capabilities` 把
/// `desktop:gray` / `desktop:quality` 等无条件写入，lean CLI 二进制（名义上
/// "仅终端转发"）也会上报这些串——实测 `shell-remote` CLI 注册后的
/// capabilities 里同样有 `desktop:gray`。以它判断会把 CLI 误判成 UI。
///
/// 判定顺序：
///
/// 1. agent 显式声明的 `variant:cli` / `variant:ui`——最可靠；
/// 2. 否则运维显式指定的 `--upgrade-variant`；
/// 3. 否则 [`VariantResolution::Unknown`]——**不猜**。
///
/// 阶段 2 会给 CLI/UI 注册补上 `variant:cli|ui`，届时新 agent 恒走第 1 步。
pub fn variant_for_device(
    caps: &[String],
    override_variant: Option<Variant>,
) -> VariantResolution {
    // 显式能力声明优先：`variant:ui` / `variant:cli`
    for c in caps {
        if let Some(v) = c.strip_prefix("variant:") {
            if let Some(parsed) = Variant::parse(v) {
                return VariantResolution::Known {
                    variant: parsed,
                    source: "capability",
                };
            }
        }
    }
    if let Some(variant) = override_variant {
        return VariantResolution::Known {
            variant,
            source: "override",
        };
    }
    VariantResolution::Unknown
}

/// agent 是否声明了 `upgrade:v2`——即能理解 CDN 优先的升级任务协议。
///
/// 存量 agent（≤ v0.59.2）的 `UpgradeRequest::from_payload` 硬性要求 `url`，
/// 只认"从 relay 下载"这一条路：给它一个 CDN-only（无 `relay_url`）的任务，
/// 它会解析失败、只回一个错误帧，旧进程根本不升级。所以 CDN-only 任务必须
/// 先确认对端是新协议 agent，否则明确拒绝而不是白发送。
pub fn supports_upgrade_v2(caps: &[String]) -> bool {
    caps.iter().any(|c| c == "upgrade:v2")
}

/// List the staged agent upgrade artifacts. Purely file-metadata based (no
/// hashing) so it is cheap enough for the admin page's periodic refresh.
pub async fn upgrade_artifacts_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let Some(store) = state.artifact_store.clone() else {
        return Json(json!({"enabled": false, "artifacts": []})).into_response();
    };
    let artifacts: Vec<Value> = store
        .list()
        .into_iter()
        .map(|local| {
            let version = artifact_version(&store, &local.artifact);
            json!({
                "arch": local.artifact.arch,
                "variant": local.artifact.variant.as_str(),
                "filename": local.artifact.filename(),
                "size": local.size,
                "windows": local.artifact.windows,
                "version": version,
            })
        })
        .collect();
    Json(json!({
        "enabled": true,
        "dir": store.root().display().to_string(),
        "relay_version": env!("CARGO_PKG_VERSION"),
        "allow_custom_upgrade": state.allow_custom_upgrade,
        // `--upgrade-variant` 的值（未配置为 null）：UI 用它判断"形态未知"的
        // 设备能否升级，与后端 variant_for_device 保持一致。
        "upgrade_variant": state.upgrade_variant_override.map(|v| v.as_str()),
        "cdn_base_urls": state.upgrade_cdn_base_urls,
        "artifacts": artifacts,
    }))
    .into_response()
}

/// 取某个版本的官方清单：优先缓存，否则按配置抓取；
/// 抓取失败时回落缓存（网络波动不应让已验证的任务失效）。
///
/// 同版本 digest 与缓存不一致 → 标记发布异常并拒绝使用新清单（不静默换 hash）。
/// 结果同时写进 `manifest_status`，供 admin 概览解释原因。
pub async fn load_manifest(
    state: &Arc<SharedState>,
    store: &crate::relay::artifact_store::ArtifactStore,
    version: &str,
) -> Option<crate::relay::artifact_store::OfficialManifest> {
    use crate::relay::artifact_store as a;

    let cached = a::read_cached_manifest(store, version);
    let fetched = a::fetch_official_manifest(&state.manifest_fetch, version).await;

    let (manifest, detail, is_cached) = match (cached, fetched) {
        (Some(c), Ok(incoming)) => {
            if let Some(conflict) = a::manifest_conflict(&c, &incoming) {
                tracing::error!(
                    version = %version,
                    conflict = %conflict,
                    "official manifest conflicts with cache — refusing this version"
                );
                state.manifest_status.write().await.insert(
                    version.to_string(),
                    json!({"ok": false, "cached": true, "detail": conflict}),
                );
                return None;
            }
            (c, "cache hit".to_string(), true)
        }
        (None, Ok(f)) => {
            if let Err(e) = a::write_manifest_atomic(store, &f) {
                tracing::warn!(version = %version, error = %e, "cannot cache official manifest");
            }
            (f, "fetched from GitHub".to_string(), false)
        }
        (Some(c), Err(e)) => {
            tracing::warn!(
                version = %version,
                error = %e,
                "official manifest unreachable — using cached copy"
            );
            (c, format!("cache hit (fetch failed: {e})"), true)
        }
        (None, Err(e)) => {
            state.manifest_status.write().await.insert(
                version.to_string(),
                json!({"ok": false, "cached": false, "detail": e}),
            );
            return None;
        }
    };
    state.manifest_status.write().await.insert(
        version.to_string(),
        json!({"ok": true, "cached": is_cached, "detail": detail}),
    );
    Some(manifest)
}

/// 触发一台设备的自升级（MYS-2425：版本决策 + 可信任务）。
///
/// 流程：
/// 1. 版本决策：`semver(relay) > semver(agent)` 才继续（相等/更低/未知都不自动
///    升级；`force` 只对"管理员明确要求重装同版本"生效，仍不允许降级以外的
///    猜测）。
/// 2. 取官方清单（缓存优先，GitHub 不可达时回落缓存）。
/// 3. 按设备上报的 capabilities 选 CLI/UI 制品，本地镜像现场算 hash 并交叉校验。
/// 4. 生成不可变任务（version + filename + size + sha256 + official_path +
///    cdn_base_urls + relay_url + source）下发 `agent:upgrade`。
pub async fn agent_upgrade_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let sid = body["session_id"].as_str().unwrap_or("").to_string();
    if sid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing session_id"})),
        )
            .into_response();
    }
    let force = body["force"].as_bool().unwrap_or(false);
    match trigger_upgrade(&state, &sid, force).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (*e).into_response(),
    }
}

/// 单台设备的升级决策与下发（HTTP 无关，单台入口与全量入口共用）。
///
/// 失败时返回**完整的 HTTP 响应**（状态码 + JSON），因为单台入口要把每个失败
/// 原因的不同状态码原样透给调用方（400 参数/未配置、404 无制品、409 版本或
/// 能力不满足）。全量入口只取状态码与 JSON 体，用于汇总。
///
/// `Err` 装箱：`Response` 本身就上百字节，直接塞进 `Result` 会让每个调用点的
/// 栈帧都背上它（clippy `result_large_err`）。失败路径是冷路径，一次分配不心疼。
async fn trigger_upgrade(
    state: &Arc<SharedState>,
    sid: &str,
    force: bool,
) -> Result<Value, Box<Response>> {
    use crate::relay::artifact_store as a;

    // 变量名沿用单台入口的写法（`state` 已是引用，这里只是别名）。
    let state = state.clone();
    let sid = sid.to_string();
    let Some(info) = state.sessions.get(&sid).await else {
        return Err(Box::new((
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "session not found"})),
        )
            .into_response()));
    };
    let (arch, platform) = match info.device.as_ref() {
        Some(d) => (d.arch.clone().unwrap_or_default(), d.platform.clone()),
        None => (String::new(), None),
    };
    if arch.is_empty() {
        return Err(Box::new((
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "device arch unknown — cannot pick an artifact"})),
        )
            .into_response()));
    }
    // 先确认 agent 在线：离线时没有必要做后续任何判断（也不该去抓官方清单），
    // 并保持"agent offline → 409"的既有语义。
    {
        let broadcast = state.agent_broadcast.read().await;
        if broadcast.get(&sid).and_then(|cm| cm.agent.as_ref()).is_none() {
            return Err(Box::new((
                StatusCode::CONFLICT,
                Json(json!({"ok": false, "error": "agent offline"})),
            )
                .into_response()));
        }
    }

    // ── 版本决策 ────────────────────────────────────────────────────
    let relay_version = env!("CARGO_PKG_VERSION");
    let decision = a::version_decision(relay_version, info.agent_version.as_deref());
    // `force` 允许重装同一版本，但绝不降级、也不在版本未知时瞎猜。
    let allowed = decision.needs_upgrade()
        || (force && matches!(decision, a::VersionDecision::UpToDate));
    if !allowed {
        return Err(Box::new((
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": format!("no upgrade needed: {}", decision.reason()),
                "relay_version": relay_version,
                "agent_version": info.agent_version,
                "decision": format!("{decision:?}"),
            })),
        )
            .into_response()));
    }

    let Some(store) = state.artifact_store.clone() else {
        return Err(Box::new((
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "agent upgrades disabled (no artifact dir configured)"})),
        )
            .into_response()));
    };

    // 制品形态由 agent 自报能力决定：不要把桌面 agent 换成 CLI，也不猜。
    let variant_res = variant_for_device(&info.capabilities, state.upgrade_variant_override);
    let Some(variant) = variant_res.variant() else {
        // 未知形态 → 拒绝。原地替换猜错就是能力丢失（UI→CLI），不能"按保守值"
        // 处理。管理员显式配 --upgrade-variant 或等 agent 上报 variant:* 后再试。
        return Err(Box::new((
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": "device variant unknown: agent did not report `variant:cli`/`variant:ui` \
                          and --upgrade-variant is unset; refusing to guess (a wrong guess would \
                          swap a UI agent for lean CLI). Set --upgrade-variant, or upgrade this \
                          agent manually first.",
                "code": "variant_unknown",
                "relay_version": relay_version,
                "agent_capabilities": info.capabilities,
            })),
        )
            .into_response()));
    };
    let variant_source = variant_res.source();
    let windows = platform.as_deref() == Some("windows");
    let Some(artifact) = ArtifactRef::new(variant, &arch, windows) else {
        return Err(Box::new((
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": format!("unusable device arch {arch:?}")})),
        )
            .into_response()));
    };

    // ── 取清单 + 本地镜像 hash ──────────────────────────────────────
    let manifest = load_manifest(&state, &store, relay_version).await;
    let local = store
        .locate(relay_version, &artifact.filename())
        .map(|path| {
            a::LocalArtifact {
                artifact: artifact.clone(),
                path,
                size: 0,
                sha256: None,
            }
            .with_hash()
        })
        .transpose();
    let local = match local {
        Ok(l) => l,
        Err(e) => {
            return Err(Box::new((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": format!("cannot hash local artifact: {e}")})),
            )
                .into_response()));
        }
    };

    let upgrade_id = uuid::Uuid::new_v4().to_string();
    let task = match a::build_task(
        upgrade_id.clone(),
        relay_version,
        &artifact,
        manifest.as_ref(),
        local.as_ref(),
        &state.upgrade_cdn_base_urls,
        state.allow_custom_upgrade,
    ) {
        Ok(t) => t,
        Err(refusal) => {
            // 明确拒绝的原因，而不是给一个"猜出来的" hash。
            return Err(Box::new((
                StatusCode::NOT_FOUND,
                Json(json!({
                    "ok": false,
                    "error": refusal.message(),
                    "refusal": format!("{refusal:?}"),
                    "manifest": state.manifest_status.read().await.get(relay_version),
                })),
            )
                .into_response()));
        }
    };

    // ── 对端能力校验（必须在 deliver 之前）──────────────────────────
    // 存量 agent 的 UpgradeRequest::from_payload 硬性要求 `url`：CDN-only 任务
    // 发过去它只会解析失败、回一个错误帧，旧进程不会升级——看起来"已触发"，
    // 实际什么都没发生。所以没有 relay 副本时必须确认对端是新协议 agent，
    // 否则明确 409 并告诉管理员去镜像制品。
    if !task.legacy_compatible() && !supports_upgrade_v2(&info.capabilities) {
        return Err(Box::new((
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": format!(
                    "agent cannot consume a CDN-only upgrade task: it does not advertise \
                     `upgrade:v2` and there is no relay mirror for {}. Stage {} into the relay \
                     artifact dir (--download-dir) so the task carries a relay_url, then retry.",
                    task.filename, task.filename
                ),
                "code": "legacy_agent_needs_relay_mirror",
                "filename": task.filename,
                "official_path": task.official_path,
                "agent_capabilities": info.capabilities,
            })),
        )
            .into_response()));
    }

    let text = serde_json::to_string(&json!({
        "type": "agent:upgrade",
        "session_id": sid,
        "payload": task.wire_payload(),
    }))
    .unwrap_or_default();
    {
        let broadcast = state.agent_broadcast.read().await;
        match broadcast.get(&sid).and_then(|cm| cm.agent.as_ref()) {
            Some(tx) => crate::relay::ws::deliver(tx, "agent:upgrade", text),
            None => {
                return Err(Box::new((
                    StatusCode::CONFLICT,
                    Json(json!({"ok": false, "error": "agent offline"})),
                )
                    .into_response()));
            }
        }
    }
    // Reflect the trigger immediately in the overview until the agent's first
    // progress frame replaces this entry.
    state.agent_upgrades.write().await.insert(
        sid.clone(),
        json!({
            "stage": "started",
            "version": task.target_version,
            "upgrade_id": task.upgrade_id,
            "source": task.source.as_str(),
            "detail": "upgrade triggered",
        }),
    );
    Ok(json!({
        "ok": true,
        "upgrade_id": task.upgrade_id,
        "version": task.target_version,
        "filename": task.filename,
        "variant": task.variant.as_str(),
        "variant_source": variant_source,
        "arch": task.arch,
        "source": task.source.as_str(),
        "relay_fallback": task.relay_fallback,
        "cdn_count": task.cdn_base_urls.len(),
        // 老 agent 只会从 relay 下载；没有 relay 副本时它无法完成升级。
        "legacy_compatible": task.legacy_compatible(),
    }))
}

/// `POST <admin>/api/agent/upgrade/fleet`：**命令全部（或指定）在线设备升级到
/// relay 的版本**。
///
/// 单台入口 `POST /api/agent/upgrade` 每次只能点一台，几十台设备时运维只能
/// 逐台点、逐台看结果，容易漏。本入口把同一套决策规则（版本 → 形态 → 制品 →
/// 能力校验）复用到整个会话集合上，并为每台设备返回独立结果：
///
/// - 每台设备**独立决策**：已达目标版本的跳过（`up_to_date`），形态/制品/
///   能力不满足的各自失败，一台失败不影响其他设备。
/// - 默认**只升级确实需要升级的**（`semver(relay) > semver(agent)`），与单台
///   入口一致；`force: true` 才对同版本设备重装。
/// - 幂等：同一 `session_id` 已有在途升级（`agent_upgrades` 处于非终态）时跳过，
///   避免一次误触把整片设备重复重启。
/// - `session_ids` 可选：给了就只处理这些会话，便于灰度；不给就是全部在线设备。
///
/// 返回按设备分组的汇总，调用方据此判断哪些成功、哪些为什么没升。
pub async fn agent_upgrade_fleet_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let force = body["force"].as_bool().unwrap_or(false);
    // 可选的显式名单（灰度用）。空/缺省 = 全部在线设备。
    let only: Option<Vec<String>> = body["session_ids"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    });

    // 枚举候选设备：注册表里的全部会话，按 session_id 排序保证结果稳定可对账。
    let mut candidates: Vec<String> = state
        .sessions
        .list_sessions()
        .await
        .into_iter()
        .map(|(sid, _)| sid)
        .filter(|sid| only.as_ref().is_none_or(|list| list.contains(sid)))
        .collect();
    candidates.sort();

    let mut triggered: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();

    for sid in candidates {
        // 幂等闸门必须先于触发：`trigger_upgrade` 里没有"已在升级中"的判断，
        // 连续两次点击会把设备重启两次。终态（done/error/rolled_back）放行。
        if let Some(cur) = state.agent_upgrades.read().await.get(&sid) {
            let stage = cur["stage"].as_str().unwrap_or("");
            if !matches!(stage, "" | "done" | "error" | "failed" | "rolled_back") {
                skipped.push(json!({
                    "session_id": sid,
                    "reason": "upgrade_in_progress",
                    "stage": stage,
                }));
                continue;
            }
        }
        match trigger_upgrade(&state, &sid, force).await {
            Ok(v) => triggered.push(json!({
                "session_id": sid,
                "upgrade_id": v["upgrade_id"],
                "version": v["version"],
                "source": v["source"],
            })),
            Err(resp) => {
                // 单台入口的状态码在这里只用于归类：409 = 不需要/不满足前置条件
                // （属于正常跳过），其余才是真正的失败。
                let status = resp.status();
                let code = status.as_u16();
                let body = response_json_body(*resp).await;
                let error = body["error"].as_str().unwrap_or("unknown error").to_string();
                let detail = body.get("code").and_then(|c| c.as_str()).map(str::to_string);
                if code == StatusCode::CONFLICT.as_u16() {
                    skipped.push(json!({
                        "session_id": sid,
                        "reason": "conflict",
                        "code": detail,
                        "error": error,
                    }));
                } else {
                    failed.push(json!({
                        "session_id": sid,
                        "status": code,
                        "code": detail,
                        "error": error,
                    }));
                }
            }
        }
    }

    Json(json!({
        "ok": true,
        "relay_version": env!("CARGO_PKG_VERSION"),
        "force": force,
        "counts": {
            "triggered": triggered.len(),
            "skipped": skipped.len(),
            "failed": failed.len(),
        },
        "triggered": triggered,
        "skipped": skipped,
        "failed": failed,
    }))
    .into_response()
}

/// 把 `Response` 的 JSON body 读回来（全量入口要拿单台的错误原因做汇总）。
async fn response_json_body(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
        .await
        .unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// `GET <admin>/api/agent/upgrade/plan?session_id=…`：只做版本决策与制品定位，
/// **不**下发指令、不退出任何进程。设备列表用它显示"是否需要升级 / 能否升级"。
pub async fn agent_upgrade_plan_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<Vec<(String, String)>>,
) -> Response {
    use crate::relay::artifact_store as a;

    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let sid = params
        .iter()
        .find(|(k, _)| k == "session_id")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let Some(info) = state.sessions.get(&sid).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "session not found"})),
        )
            .into_response();
    };
    let relay_version = env!("CARGO_PKG_VERSION");
    let decision = a::version_decision(relay_version, info.agent_version.as_deref());
    let (arch, platform) = match info.device.as_ref() {
        Some(d) => (d.arch.clone().unwrap_or_default(), d.platform.clone()),
        None => (String::new(), None),
    };
    let variant_res = variant_for_device(&info.capabilities, state.upgrade_variant_override);
    let variant = variant_res.variant();
    let target = variant.and_then(|v| {
        ArtifactRef::new(v, &arch, platform.as_deref() == Some("windows"))
    });
    let local_present = target
        .as_ref()
        .and_then(|t| state.artifact_store.as_ref()?.locate(relay_version, &t.filename()))
        .is_some();
    // 没有本地镜像 → 任务会是 CDN-only；旧 agent 消费不了（它的
    // `from_payload` 需要 `url`）。这里提前把结论给 UI，别让管理员点了才 409。
    let upgrade_v2 = supports_upgrade_v2(&info.capabilities);
    let blocked_reason = if variant.is_none() {
        Some("variant_unknown")
    } else if !local_present && !upgrade_v2 {
        Some("legacy_agent_needs_relay_mirror")
    } else {
        None
    };

    Json(json!({
        "ok": true,
        "session_id": sid,
        "relay_version": relay_version,
        "agent_version": info.agent_version,
        "decision": format!("{decision:?}"),
        "needs_upgrade": decision.needs_upgrade(),
        "reason": decision.reason(),
        "variant": variant.map(|v| v.as_str()),
        "variant_source": variant_res.source(),
        "variant_unknown": variant.is_none(),
        "supports_upgrade_v2": upgrade_v2,
        // `None` = 可下发；否则是 admin 会返回的错误码，UI 据此禁用并解释。
        "blocked_reason": blocked_reason,
        "arch": arch,
        "target_filename": target.as_ref().map(|t| t.filename()),
        "local_artifact_present": local_present,
        "allow_custom_upgrade": state.allow_custom_upgrade,
        "cdn_base_urls": state.upgrade_cdn_base_urls,
        "manifest": state.manifest_status.read().await.get(relay_version),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::SharedState;

    fn state_with_admin(user: &str, pass: &str) -> Arc<SharedState> {
        Arc::new(SharedState::new(
            "relay-pw".to_string(),
            100 * 1024 * 1024,
            Some("/admin-test".to_string()),
            user.to_string(),
            pass.to_string(),
            None,
        ))
    }

    fn cookie_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("cookie", format!("sr_admin={}", token).parse().unwrap());
        h
    }

    #[tokio::test]
    async fn test_login_success_sets_cookie() {
        let state = state_with_admin("admin", "s3cret");
        let body = Json(json!({"user": "admin", "pass": "s3cret"}));
        let resp = login_handler(State(state.clone()), HeaderMap::new(), body).await;
        assert_eq!(resp.status(), 200);
        let set_cookie = resp
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(set_cookie.contains("sr_admin="));
        assert!(set_cookie.contains("HttpOnly"));
    }

    #[tokio::test]
    async fn test_login_wrong_password() {
        let state = state_with_admin("admin", "s3cret");
        let body = Json(json!({"user": "admin", "pass": "wrong"}));
        let resp = login_handler(State(state), HeaderMap::new(), body).await;
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn test_login_rate_limited_after_five_per_minute() {
        // A11：同一 IP 6 次/分钟 → 第 6 次 429（与是否口令正确无关）。
        let state = state_with_admin("admin", "s3cret");
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "7.7.7.7".parse().unwrap());
        let body = Json(json!({"user": "admin", "pass": "wrong"}));
        for i in 1..=5 {
            let resp = login_handler(State(state.clone()), h.clone(), body.clone()).await;
            assert_eq!(resp.status(), 401, "attempt {i} should be a normal auth failure");
        }
        let resp = login_handler(State(state.clone()), h.clone(), body.clone()).await;
        assert_eq!(resp.status(), 429, "6th attempt in the window must be rate limited");

        // 正确口令 + 另一个 IP 不受影响
        let mut h2 = HeaderMap::new();
        h2.insert("x-forwarded-for", "8.8.8.8".parse().unwrap());
        let ok = Json(json!({"user": "admin", "pass": "s3cret"}));
        let resp = login_handler(State(state), h2, ok).await;
        assert_eq!(resp.status(), 200, "other clients keep working");
    }

    #[tokio::test]
    async fn test_check_admin_rejects_without_cookie() {
        let state = state_with_admin("admin", "s3cret");
        let headers = HeaderMap::new();
        assert!(!check_admin(&state, &headers).await);
    }

    #[tokio::test]
    async fn test_check_admin_accepts_valid_session() {
        let state = state_with_admin("admin", "s3cret");
        state
            .admin_sessions
            .write()
            .await
            .insert("tok123".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        assert!(check_admin(&state, &cookie_headers("tok123")).await);
    }

    #[tokio::test]
    async fn test_check_admin_rejects_expired() {
        let state = state_with_admin("admin", "s3cret");
        state
            .admin_sessions
            .write()
            .await
            .insert(
                "tok123".to_string(),
                Instant::now() - Duration::from_secs(1),
            );
        assert!(!check_admin(&state, &cookie_headers("tok123")).await);
    }

    #[tokio::test]
    async fn test_overview_requires_auth() {
        let state = state_with_admin("admin", "s3cret");
        let resp = overview_handler(State(state), HeaderMap::new()).await;
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn test_overview_returns_sessions() {
        let state = state_with_admin("admin", "s3cret");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id;
        let _t = r.tokens;
        state.admin_sessions.write().await.insert(
            "tok".to_string(),
            Instant::now() + ADMIN_SESSION_TTL,
        );
        let resp = overview_handler(State(state), cookie_headers("tok")).await;
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["agent_count"], 1);
        assert!(v["sessions"].is_array());
        assert_eq!(v["sessions"][0]["session_id"], sid);
        assert_eq!(v["sessions"][0]["tokens"][0]["permission"], "rw");
    }

    #[tokio::test]
    async fn test_overview_includes_device_info() {
        let state = state_with_admin("admin", "s3cret");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id;
        state
            .sessions
            .set_device(
                &sid,
                Some(crate::proto::DeviceInfo {
                    hostname: Some("probe-box".to_string()),
                    platform: Some("linux".to_string()),
                    arch: Some("aarch64".to_string()),
                    os: Some("Linux".to_string()),
                    kernel: Some("6.1.0".to_string()),
                    cpu_model: Some("ARMv8".to_string()),
                }),
            )
            .await;
        state.admin_sessions.write().await.insert(
            "tok".to_string(),
            Instant::now() + ADMIN_SESSION_TTL,
        );
        let resp = overview_handler(State(state), cookie_headers("tok")).await;
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["sessions"][0]["device"]["hostname"], "probe-box");
        assert_eq!(v["sessions"][0]["device"]["arch"], "aarch64");
    }

    #[tokio::test]
    async fn test_kick_removes_session() {
        let state = state_with_admin("admin", "s3cret");
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id;
        let tokens = r.tokens;
        state.admin_sessions.write().await.insert(
            "tok".to_string(),
            Instant::now() + ADMIN_SESSION_TTL,
        );
        let body = Json(json!({"session_id": sid}));
        let resp = kick_handler(State(state.clone()), cookie_headers("tok"), body).await;
        assert_eq!(resp.status(), 200);
        // session + token gone
        assert!(state.sessions.authenticate(&tokens[0].0).await.is_none());
    }

    #[tokio::test]
    async fn test_revoke_and_regenerate_and_perm() {
        let state = state_with_admin("admin", "s3cret");
        let r = state.sessions.register(None, "both", None).await.unwrap();
        let sid = r.session_id;
        let tokens = r.tokens;
        state.admin_sessions.write().await.insert(
            "tok".to_string(),
            Instant::now() + ADMIN_SESSION_TTL,
        );
        let h = cookie_headers("tok");

        // revoke first token
        let r = revoke_handler(State(state.clone()), h.clone(), Json(json!({"token": tokens[0].0})))
            .await;
        assert_eq!(r.status(), 200);
        assert!(state.sessions.authenticate(&tokens[0].0).await.is_none());

        // regenerate
        let r = regenerate_handler(
            State(state.clone()),
            h.clone(),
            Json(json!({"session_id": sid})),
        )
        .await;
        assert_eq!(r.status(), 200);
        let body = axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        let new_tok = v["tokens"][0]["token"].as_str().unwrap().to_string();
        assert!(state.sessions.authenticate(&new_tok).await.is_some());

        // set permission to ro
        let r = permission_handler(
            State(state.clone()),
            h.clone(),
            Json(json!({"token": new_tok, "permission": "ro"})),
        )
        .await;
        assert_eq!(r.status(), 200);
        let (_, perm) = state.sessions.authenticate(&new_tok).await.unwrap();
        assert_eq!(perm, Permission::ReadOnly);
    }

    #[tokio::test]
    async fn test_server_auth_get_and_set() {
        let state = state_with_admin("admin", "s3cret");
        state.admin_sessions.write().await.insert(
            "tok".to_string(),
            Instant::now() + ADMIN_SESSION_TTL,
        );
        let h = cookie_headers("tok");

        // get
        let r = get_server_auth_handler(State(state.clone()), h.clone()).await;
        assert_eq!(r.status(), 200);
        let body = axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["server_auth"], "relay-pw");

        // set
        let r =
            set_server_auth_handler(State(state.clone()), h, Json(json!({"password": "new-pw"})))
                .await;
        assert_eq!(r.status(), 200);
        assert_eq!(&*state.server_auth.read().await, "new-pw");
    }

    #[tokio::test]
    async fn test_overview_reports_recording_flags() {
        let dir = std::env::temp_dir().join(format!(
            "sr-rec-ov-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let recorder = std::sync::Arc::new(crate::relay::recorder::Recorder::new(dir));
        let state = Arc::new(crate::relay::SharedState::new(
            "relay-pw".to_string(),
            100 * 1024 * 1024,
            Some("/admin-test".to_string()),
            "admin".to_string(),
            "s3cret".to_string(),
            Some(recorder.clone()),
        ));
        let r = state.sessions.register(None, "rw", None).await.unwrap();
        let sid = r.session_id;
        let _t = r.tokens;
        state
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        // No event yet → not recording
        let r = overview_handler(State(state.clone()), cookie_headers("tok")).await;
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["recording_enabled"], true);
        assert_eq!(v["sessions"][0]["recording"], false);
        // Fire an event
        recorder.record(&sid, crate::relay::recorder::RecordEvent::Output("x".into()));
        let r = overview_handler(State(state.clone()), cookie_headers("tok")).await;
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["sessions"][0]["recording"], true);
        recorder.close(&sid);
    }

    fn cookie_headers_owned(token: &str) -> HeaderMap {
        cookie_headers(token)
    }

    async fn body_json(r: Response) -> Value {
        serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap())
            .unwrap()
    }

    #[tokio::test]
    async fn test_recordings_list_content_delete() {
        let dir = std::env::temp_dir().join(format!(
            "sr-rec-admin-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let recorder = std::sync::Arc::new(crate::relay::recorder::Recorder::new(dir.clone()));
        let state = Arc::new(crate::relay::SharedState::new(
            "relay-pw".to_string(),
            100 * 1024 * 1024,
            Some("/admin-test".to_string()),
            "admin".to_string(),
            "s3cret".to_string(),
            Some(recorder.clone()),
        ));
        state
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        let h = cookie_headers_owned("tok");

        // Produce one cast recording + one audit file.
        recorder.record("ag-s1", crate::relay::recorder::RecordEvent::Output("hello\n".into()));
        recorder.close("ag-s1");
        recorder.audit_mcp(
            "ag-s2",
            crate::relay::recorder::AuditLine {
                ts: "2026-08-14T00:00:00Z".to_string(),
                unix_ms: 1_784_000_000_000,
                session_id: "ag-s2".to_string(),
                token_prefix: "abcd1234".to_string(),
                permission: "rw".to_string(),
                cmd: "ls".to_string(),
                timeout_ms: 30_000,
                duration_ms: 5,
                status: "ok".to_string(),
                exit_code: Some(0),
                stdout_len: 2,
                stderr_len: 0,
                stdout: "hi".to_string(),
                stderr: "".to_string(),
            },
        );
        recorder.close("ag-s2");
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

        // List (default pagination: page 1, page_size 20)
        let r = recordings_handler(
            State(state.clone()),
            h.clone(),
            axum::extract::Query(serde_json::json!({})),
        )
        .await;
        let v = body_json(r).await;
        assert_eq!(v["enabled"], true);
        assert_eq!(v["total"], 2);
        assert_eq!(v["page"], 1);
        assert_eq!(v["pages"], 1);
        assert_eq!(v["recordings"].as_array().unwrap().len(), 2);
        let cast = v["recordings"].as_array().unwrap().iter().find(|x| x["kind"] == "cast").unwrap().clone();
        assert_eq!(cast["session_id"], "ag-s1");
        let cast_name = cast["name"].as_str().unwrap().to_string();

        // Content (auth required)
        let r = recordings_handler(
            State(state.clone()),
            HeaderMap::new(),
            axum::extract::Query(serde_json::json!({})),
        )
        .await;
        assert_eq!(r.status(), 401);

        // Content fetch
        let q = axum::extract::Query(serde_json::json!({"file": cast_name}));
        let r = recording_content_handler(State(state.clone()), h.clone(), q).await;
        let body = axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("\"version\":2"), "header present: {}", text);
        assert!(text.contains("hello"));

        // Content — traversal rejected
        let q = axum::extract::Query(serde_json::json!({"file": "../../etc/passwd.cast"}));
        let r = recording_content_handler(State(state.clone()), h.clone(), q).await;
        assert_eq!(r.status(), 400);

        // Delete
        let r = recording_delete_handler(
            State(state.clone()),
            h.clone(),
            Json(serde_json::json!({"file": cast_name})),
        )
        .await;
        let v = body_json(r).await;
        assert_eq!(v["ok"], true);
        assert!(!dir.join(&cast_name).exists());

        // Delete again → 404
        let r = recording_delete_handler(
            State(state.clone()),
            h.clone(),
            Json(serde_json::json!({"file": cast_name})),
        )
        .await;
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn test_recordings_disabled_when_no_recorder() {
        let state = Arc::new(crate::relay::SharedState::new(
            "relay-pw".to_string(),
            100 * 1024 * 1024,
            Some("/admin-test".to_string()),
            "admin".to_string(),
            "s3cret".to_string(),
            None,
        ));
        state
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        let h = cookie_headers_owned("tok");
        let r = recordings_handler(
            State(state.clone()),
            h.clone(),
            axum::extract::Query(serde_json::json!({})),
        )
        .await;
        let v = body_json(r).await;
        assert_eq!(v["enabled"], false);
        assert_eq!(v["recordings"].as_array().unwrap().len(), 0);
        // Delete without recorder → 404
        let r = recording_delete_handler(
            State(state.clone()),
            h.clone(),
            Json(serde_json::json!({"file": "x_1.cast"})),
        )
        .await;
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn test_recordings_pagination() {
        let dir = std::env::temp_dir().join(format!(
            "sr-rec-page-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let recorder = std::sync::Arc::new(crate::relay::recorder::Recorder::new(dir.clone()));
        let state = Arc::new(crate::relay::SharedState::new(
            "relay-pw".to_string(),
            100 * 1024 * 1024,
            Some("/admin-test".to_string()),
            "admin".to_string(),
            "s3cret".to_string(),
            Some(recorder.clone()),
        ));
        state
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        let h = cookie_headers_owned("tok");

        // 45 `.cast` files; larger ts suffix sorts first (newest first).
        for i in 0..45u64 {
            std::fs::write(dir.join(format!("sess-{i}_{}.cast", 1000 + i)), b"").unwrap();
        }
        // One audit file, ignored by the (kind-agnostic) page slice counting
        // below but confirms kind filtering still works across pages.
        std::fs::write(dir.join("sess-audit_900.audit.jsonl"), b"{}\n").unwrap();

        // Queries use string values to mirror real HTTP parsing via serde_urlencoded.
        // Default: page 1, page_size 20.
        let r = recordings_handler(
            State(state.clone()),
            h.clone(),
            axum::extract::Query(serde_json::json!({})),
        )
        .await;
        let v = body_json(r).await;
        assert_eq!(v["enabled"], true);
        assert_eq!(v["total"], 46);
        assert_eq!(v["pages"], 3);
        assert_eq!(v["page"], 1);
        assert_eq!(v["page_size"], 20);
        let page1 = v["recordings"].as_array().unwrap();
        assert_eq!(page1.len(), 20);
        assert_eq!(page1[0]["session_id"], "sess-44");
        assert_eq!(page1[19]["session_id"], "sess-25");

        // Explicit middle page.
        let r = recordings_handler(
            State(state.clone()),
            h.clone(),
            axum::extract::Query(serde_json::json!({"page": "2"})),
        )
        .await;
        let v = body_json(r).await;
        let page2 = v["recordings"].as_array().unwrap();
        assert_eq!(page2.len(), 20);
        assert_eq!(page2[0]["session_id"], "sess-24");
        assert_eq!(page2[19]["session_id"], "sess-5");

        // Numeric JSON values (e.g. direct handler calls) also work.
        let r = recordings_handler(
            State(state.clone()),
            h.clone(),
            axum::extract::Query(serde_json::json!({"page": 2})),
        )
        .await;
        let v = body_json(r).await;
        assert_eq!(v["page"], 2);
        assert_eq!(v["page_size"], 20);

        // Last page holds the remainder.
        let r = recordings_handler(
            State(state.clone()),
            h.clone(),
            axum::extract::Query(serde_json::json!({"page": "3"})),
        )
        .await;
        let v = body_json(r).await;
        let page3 = v["recordings"].as_array().unwrap();
        assert_eq!(page3.len(), 6);
        assert_eq!(page3[0]["session_id"], "sess-4");

        // Out-of-range page: empty page but real totals.
        let r = recordings_handler(
            State(state.clone()),
            h.clone(),
            axum::extract::Query(serde_json::json!({"page": "99"})),
        )
        .await;
        let v = body_json(r).await;
        assert_eq!(v["total"], 46);
        assert_eq!(v["pages"], 3);
        assert_eq!(v["recordings"].as_array().unwrap().len(), 0);

        // Smaller page_size.
        let r = recordings_handler(
            State(state.clone()),
            h.clone(),
            axum::extract::Query(serde_json::json!({"page": "1", "page_size": "10"})),
        )
        .await;
        let v = body_json(r).await;
        assert_eq!(v["page_size"], 10);
        assert_eq!(v["pages"], 5);
        assert_eq!(v["recordings"].as_array().unwrap().len(), 10);

        // page_size clamped to the upper bound.
        let r = recordings_handler(
            State(state.clone()),
            h.clone(),
            axum::extract::Query(serde_json::json!({"page_size": "999"})),
        )
        .await;
        let v = body_json(r).await;
        assert_eq!(v["page_size"], 100);

        // Invalid inputs fall back to defaults, never error.
        let r = recordings_handler(
            State(state.clone()),
            h.clone(),
            axum::extract::Query(serde_json::json!({"page": "0", "page_size": "abc"})),
        )
        .await;
        assert_eq!(r.status(), 200);
        let v = body_json(r).await;
        assert_eq!(v["page"], 1);
        assert_eq!(v["page_size"], 20);
        assert_eq!(v["recordings"].as_array().unwrap().len(), 20);
    }

    #[test]
    fn test_artifact_arch_key_normalization() {
        assert_eq!(artifact_arch_key("x86_64"), "x86_64");
        assert_eq!(artifact_arch_key("amd64"), "x86_64");
        assert_eq!(artifact_arch_key("aarch64"), "aarch64");
        assert_eq!(artifact_arch_key("arm64"), "aarch64");
        assert_eq!(artifact_arch_key("armv7l"), "armv7");
        assert_eq!(artifact_arch_key("riscv64"), "riscv64");
        assert_eq!(artifact_arch_key("ARM64"), "aarch64"); // case-insensitive
    }

    #[test]
    fn test_parse_artifact_name() {
        let name = parse_artifact_name("shell-remote-x86_64").unwrap();
        assert_eq!(name, ("x86_64".to_string(), false));
        let name = parse_artifact_name("shell-remote-aarch64.exe").unwrap();
        assert_eq!(name, ("aarch64".to_string(), true));
        assert!(parse_artifact_name("other.file").is_none());
        assert!(parse_artifact_name("shell-remote-").is_none());
        assert!(parse_artifact_name("../../etc/passwd").is_none());
        assert!(parse_artifact_name("shell-remote-x86_64.extra").is_none()); // dot in arch
        assert!(!valid_artifact_filename("shell-remote-../x"));
        assert!(valid_artifact_filename("shell-remote-armv7"));
        assert!(valid_artifact_filename("shell-remote-x86_64.exe"));
    }

    /// 配好统一制品库的 admin state。`--download-dir` 与 `--agent-upgrade-dir`
    /// 合并后，`upgrade_dir` 只是兼容别名，`artifact_store` 才是权威来源。
    fn state_with_admin_artifacts(dir: &std::path::Path) -> Arc<SharedState> {
        state_with_admin_artifacts_tuned(dir, |_| {})
    }

    fn upgrade_dir_with_artifacts() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sr-upg-admin-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shell-remote-x86_64"), b"new-binary-x86").unwrap();
        std::fs::write(dir.join("shell-remote-aarch64.exe"), b"new-exe-aarch64").unwrap();
        std::fs::write(dir.join("shell-remote-armv7"), b"new-armv7").unwrap();
        std::fs::write(dir.join("shell-remote-x86_64.version"), "0.20.0").unwrap();
        std::fs::write(dir.join("README.txt"), b"ignored noise").unwrap();
        dir
    }

    /// 阶段 2 之后新 agent 会声明的能力集（含形态与新协议标记）。
    /// 升级相关用例默认用它，代表"能正常参与升级的新 agent"。
    const NEW_AGENT_CAPS: &[&str] = &[
        "codec:h264",
        "backend:x11",
        "variant:cli",
        "upgrade:v2",
    ];

    /// 阶段 1 时的存量 agent：不上报 `variant:*`，也不认 CDN 优先协议。
    const LEGACY_AGENT_CAPS: &[&str] = &[
        "web:proxy-v1",
        "net:tcp-v1",
        "codec:h264",
        "backend:x11",
        "desktop:gray",
        "desktop:quality",
        "desktop:clipboard",
        "desktop:cursor",
        "desktop:test-delay",
    ];

    /// 把 state 的 admin session 置为已登录。
    async fn login(state: &Arc<SharedState>) -> HeaderMap {
        state
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        cookie_headers("tok")
    }

    /// 注册一台设备并写入 device + capabilities + agent_version。
    async fn register_device(
        state: &Arc<SharedState>,
        name: &str,
        arch: &str,
        platform: &str,
        capabilities: &[&str],
        agent_version: Option<&str>,
    ) -> String {
        let reg = state
            .sessions
            .register(None, "rw", Some(name.to_string()))
            .await
            .unwrap();
        state
            .sessions
            .set_device(
                &reg.session_id,
                Some(crate::proto::DeviceInfo {
                    hostname: None,
                    platform: Some(platform.to_string()),
                    arch: Some(arch.to_string()),
                    os: None,
                    kernel: None,
                    cpu_model: None,
                }),
            )
            .await;
        state
            .sessions
            .set_agent_version(&reg.session_id, agent_version.map(|v| v.to_string()))
            .await;
        if !capabilities.is_empty() {
            state
                .sessions
                .update_capabilities(
                    &reg.session_id,
                    capabilities.iter().map(|s| s.to_string()).collect(),
                )
                .await;
        }
        reg.session_id
    }

    /// 给会话挂一个在线的 agent 通道，返回接收端。
    async fn attach_agent_channel(
        state: &Arc<SharedState>,
        sid: &str,
    ) -> tokio::sync::mpsc::Receiver<String> {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(8);
        state.agent_broadcast.write().await.insert(
            sid.to_string(),
            crate::relay::ChannelMap {
                agent: Some(tx),
                agent_bulk: None,
                browser_sessions: std::collections::HashMap::new(),
                browser_tokens: std::collections::HashMap::new(),
            },
        );
        rx
    }

    #[tokio::test]
    async fn test_upgrade_artifacts_handler_lists_staged_binaries() {
        let dir = upgrade_dir_with_artifacts();
        let state = state_with_admin_artifacts(&dir);
        let h = login(&state).await;

        let r = upgrade_artifacts_handler(State(state.clone()), h.clone()).await;
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["relay_version"], env!("CARGO_PKG_VERSION"));
        assert!(
            v["cdn_base_urls"].as_array().unwrap().len() >= 2,
            "首期至少两个独立 CDN 入口"
        );
        let arts = v["artifacts"].as_array().unwrap();
        assert_eq!(arts.len(), 3, "noise files must be excluded");
        let x86 = arts
            .iter()
            .find(|a| a["arch"] == "x86_64")
            .expect("x86_64 artifact listed");
        assert_eq!(x86["filename"], "shell-remote-x86_64");
        assert_eq!(x86["variant"], "cli");
        assert_eq!(x86["version"], "0.20.0", "companion .version file is used");
        assert_eq!(x86["windows"], false);
        let win = arts
            .iter()
            .find(|a| a["windows"] == true)
            .expect("windows artifact listed");
        assert_eq!(win["arch"], "aarch64");

        // Auth required
        let r = upgrade_artifacts_handler(State(state.clone()), HeaderMap::new()).await;
        assert_eq!(r.status(), 401);

        // Disabled relay (no upgrade dir) → empty list, not an error.
        let state2 = state_with_admin("admin", "s3cret");
        state2
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        let r = upgrade_artifacts_handler(State(state2), cookie_headers("tok")).await;
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["enabled"], false);
        assert_eq!(v["artifacts"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_agent_upgrade_handler_requires_admin() {
        let state = state_with_admin("admin", "s3cret");
        let r = agent_upgrade_handler(
            State(state),
            HeaderMap::new(),
            Json(json!({"session_id": "x"})),
        )
        .await;
        assert_eq!(r.status(), 401);
    }

    /// 制品库齐全但官方清单不可达的 state（模拟 GitHub 被墙）。
    fn state_without_manifest(dir: &std::path::Path) -> Arc<SharedState> {
        state_with_admin_artifacts_tuned(dir, |st| {
            st.manifest_fetch.api_base = "http://127.0.0.1:1".to_string();
            st.manifest_fetch.timeout = std::time::Duration::from_millis(400);
            st.manifest_fetch.connect_timeout = std::time::Duration::from_millis(200);
        })
    }

    #[tokio::test]
    async fn test_agent_upgrade_handler_offline_agent_is_conflict() {
        let dir = upgrade_dir_with_artifacts();
        let state = state_without_manifest(&dir);
        let h = login(&state).await;
        let sid = register_device(
            &state,
            "devbox9",
            "x86_64",
            "linux",
            NEW_AGENT_CAPS,
            Some("0.59.1"),
        )
        .await;

        // No agent channel → offline → 409.
        let r = agent_upgrade_handler(State(state.clone()), h.clone(), Json(json!({"session_id": sid})))
            .await;
        assert_eq!(r.status(), 409);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"], "agent offline");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 起一个本地 release-API mock，只服务 `v<version>`，清单里列出的
    /// `shell-remote-x86_64` 资产 digest/size 与给定字节精确匹配。
    async fn spawn_manifest_mock(version: &'static str, bytes: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        use axum::routing::get;
        use axum::Router;

        let (size, digest) = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(bytes);
            (bytes.len() as u64, hex::encode(h.finalize()))
        };
        let body = json!({
            "tag_name": format!("v{version}"),
            "assets": [{
                "name": "shell-remote-x86_64",
                "size": size,
                "digest": format!("sha256:{digest}"),
                "browser_download_url": format!(
                    "https://github.com/zzttzzmyswy/shell-remote/releases/download/v{version}/shell-remote-x86_64"
                ),
            }],
        });
        let app = Router::new().route(
            "/repos/:owner/:repo/releases/tags/:tag",
            get(move |axum::extract::Path((_o, _r, tag)): axum::extract::Path<(String, String, String)>| {
                let body = body.clone();
                let want = format!("v{version}");
                async move {
                    if tag == want {
                        (axum::http::StatusCode::OK, axum::Json(body))
                    } else {
                        (
                            axum::http::StatusCode::NOT_FOUND,
                            axum::Json(json!({"message": "Not Found"})),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    /// 带本地清单 mock 的 admin state。
    async fn state_with_manifest_mock(
        dir: &std::path::Path,
        bytes: &'static [u8],
    ) -> Arc<SharedState> {
        let (api_base, mock) = spawn_manifest_mock(env!("CARGO_PKG_VERSION"), bytes).await;
        // mock 任务必须在整个测试期间存活；测试进程结束时随之退出。
        std::mem::forget(mock);
        state_with_admin_artifacts_tuned(dir, |st| {
            st.manifest_fetch.api_base = api_base;
        })
    }

    /// 构造统一制品库 state，允许在包成 `Arc` 之前调整启动配置。
    fn state_with_admin_artifacts_tuned(
        dir: &std::path::Path,
        tune: impl FnOnce(&mut SharedState),
    ) -> Arc<SharedState> {
        let mut st = SharedState::new(
            "relay-pw".to_string(),
            100 * 1024 * 1024,
            Some("/admin-test".to_string()),
            "admin".to_string(),
            "s3cret".to_string(),
            None,
        );
        st.download_dir = Some(dir.to_path_buf());
        st.artifact_store = Some(crate::relay::artifact_store::ArtifactStore::new(
            dir.to_path_buf(),
        ));
        tune(&mut st);
        Arc::new(st)
    }

    #[tokio::test]
    async fn test_agent_upgrade_handler_delivers_full_task_with_official_digest() {
        // 官方清单由本地 HTTP mock 提供（不依赖外网），digest 与本地镜像一致
        // → 任务应为 `source: local`，同时带 CDN 与 relay 兜底。
        let dir = upgrade_dir_with_artifacts();
        let local_bytes = b"new-binary-x86";
        let state = state_with_manifest_mock(&dir, local_bytes).await;
        let h = login(&state).await;
        // agent 版本更低 → 需要升级
        let sid = register_device(
            &state,
            "devbox9",
            "x86_64",
            "linux",
            NEW_AGENT_CAPS,
            Some("0.59.1"),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;

        let r = agent_upgrade_handler(
            State(state.clone()),
            h.clone(),
            Json(json!({"session_id": sid})),
        )
        .await;
        assert_eq!(r.status(), 200);
        let got = rx.try_recv().expect("agent channel must receive the upgrade msg");
        let v: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["type"], "agent:upgrade");
        assert_eq!(v["session_id"], sid);

        let p = &v["payload"];
        // 任务字段齐全且与官方 digest 一致
        assert_eq!(p["target_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(p["filename"], "shell-remote-x86_64");
        assert_eq!(p["size"], local_bytes.len() as u64);
        assert_eq!(
            p["sha256"],
            {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(local_bytes);
                hex::encode(hasher.finalize())
            }
        );
        assert_eq!(
            p["official_path"],
            format!(
                "/zzttzzmyswy/shell-remote/releases/download/v{}/shell-remote-x86_64",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(p["source"], "local");
        assert_eq!(p["official_release"], true);
        assert_eq!(p["variant"], "cli");
        assert_eq!(p["arch"], "x86_64");
        assert_eq!(p["windows"], false);
        assert_eq!(p["relay_url"], "/agent/upgrade/blob/shell-remote-x86_64");
        assert_eq!(p["relay_fallback"], true);
        assert!(
            p["cdn_base_urls"].as_array().unwrap().len() >= 2,
            "至少两个独立 CDN 入口"
        );
        assert!(p["upgrade_id"].as_str().unwrap().len() >= 32);

        // Trigger is reflected in the overview immediately.
        let resp = overview_handler(State(state.clone()), h).await;
        let ov: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        let sess = ov["sessions"]
            .as_array()
            .unwrap()
            .into_iter()
            .find(|s| s["session_id"] == sid)
            .unwrap();
        assert_eq!(sess["upgrade"]["stage"], "started");
        assert_eq!(sess["upgrade"]["source"], "local");
        assert_eq!(sess["upgrade"]["version"], env!("CARGO_PKG_VERSION"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_agent_upgrade_handler_refuses_when_agent_version_not_lower() {
        // 版本相等 → 不自动升级（409），这是"只升级更高 semver"的硬约束。
        let dir = upgrade_dir_with_artifacts();
        let state = state_with_manifest_mock(&dir, b"new-binary-x86").await;
        let h = login(&state).await;
        let sid = register_device(
            &state,
            "devbox9",
            "x86_64",
            "linux",
            NEW_AGENT_CAPS,
            Some(env!("CARGO_PKG_VERSION")),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;

        let r = agent_upgrade_handler(
            State(state.clone()),
            h.clone(),
            Json(json!({"session_id": sid})),
        )
        .await;
        assert_eq!(r.status(), 409);
        assert!(rx.try_recv().is_err(), "同版本不得下发任何指令");

        // 显式 force 允许重装同版本
        let r = agent_upgrade_handler(
            State(state.clone()),
            h,
            Json(json!({"session_id": sid, "force": true})),
        )
        .await;
        assert_eq!(r.status(), 200);
        assert!(rx.try_recv().is_ok(), "force 应当下发任务");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_upgrade_fleet_commands_every_eligible_device() {
        // 全量入口的核心契约：一次调用命令**所有**落后设备升级，每台各自决策，
        // 一台失败不影响其他设备。
        let dir = upgrade_dir_with_artifacts();
        let state = state_with_manifest_mock(&dir, b"new-binary-x86").await;
        let h = login(&state).await;

        // 3 台落后（应下发）、1 台已最新（应跳过）。
        let mut outdated = Vec::new();
        let mut rxes = Vec::new();
        for name in ["fleeta", "fleetb", "fleetc"] {
            let sid = register_device(
                &state,
                name,
                "x86_64",
                "linux",
                NEW_AGENT_CAPS,
                Some("0.59.1"),
            )
            .await;
            rxes.push(attach_agent_channel(&state, &sid).await);
            outdated.push(sid);
        }
        let up_to_date = register_device(
            &state,
            "fleetcurrent",
            "x86_64",
            "linux",
            NEW_AGENT_CAPS,
            Some(env!("CARGO_PKG_VERSION")),
        )
        .await;
        let mut rx_current = attach_agent_channel(&state, &up_to_date).await;

        let r = agent_upgrade_fleet_handler(State(state.clone()), h, Json(json!({}))).await;
        assert_eq!(r.status(), 200);
        let body = body_json(r).await;
        assert_eq!(body["counts"]["triggered"], 3, "{body}");
        assert_eq!(body["counts"]["skipped"], 1, "{body}");
        assert_eq!(body["counts"]["failed"], 0, "{body}");

        // 每台落后设备都必须真的收到 agent:upgrade（不是只报了数）。
        for (i, rx) in rxes.iter_mut().enumerate() {
            let got = rx
                .try_recv()
                .unwrap_or_else(|_| panic!("第 {i} 台落后设备没有收到升级指令"));
            let v: Value = serde_json::from_str(&got).unwrap();
            assert_eq!(v["type"], "agent:upgrade");
            assert_eq!(v["session_id"], outdated[i]);
            assert_eq!(v["payload"]["target_version"], env!("CARGO_PKG_VERSION"));
        }
        // 已最新的设备一条指令都不该收到。
        assert!(
            rx_current.try_recv().is_err(),
            "已是最新版本的设备不得收到升级指令"
        );
        // 跳过原因必须可解释，而不是静默少一台。
        let skipped = body["skipped"].as_array().unwrap();
        assert_eq!(skipped[0]["session_id"], up_to_date);
        assert_eq!(skipped[0]["reason"], "conflict");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_upgrade_fleet_skips_device_already_upgrading() {
        // 幂等：同一设备已有在途升级时必须跳过，否则一次误触会把它重复重启。
        let dir = upgrade_dir_with_artifacts();
        let state = state_with_manifest_mock(&dir, b"new-binary-x86").await;
        let h = login(&state).await;

        let sid = register_device(
            &state,
            "busydev",
            "x86_64",
            "linux",
            NEW_AGENT_CAPS,
            Some("0.59.1"),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;
        state
            .agent_upgrades
            .write()
            .await
            .insert(sid.clone(), json!({"stage": "downloading"}));

        let r = agent_upgrade_fleet_handler(State(state.clone()), h.clone(), Json(json!({}))).await;
        let body = body_json(r).await;
        assert_eq!(body["counts"]["triggered"], 0, "{body}");
        assert_eq!(body["counts"]["skipped"], 1, "{body}");
        assert_eq!(body["skipped"][0]["reason"], "upgrade_in_progress");
        assert_eq!(body["skipped"][0]["stage"], "downloading");
        assert!(rx.try_recv().is_err(), "在途设备不得被再次触发");

        // 终态（done）必须放行，否则设备升级完就再也升不了了。
        state
            .agent_upgrades
            .write()
            .await
            .insert(sid.clone(), json!({"stage": "done"}));
        let r = agent_upgrade_fleet_handler(State(state.clone()), h, Json(json!({}))).await;
        let body = body_json(r).await;
        assert_eq!(body["counts"]["triggered"], 1, "终态设备必须可以再次升级: {body}");
        assert!(rx.try_recv().is_ok(), "终态设备应收到新指令");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_upgrade_fleet_honours_explicit_session_list() {
        // 灰度：给了 session_ids 就只碰这些设备，其余一台都不许动。
        let dir = upgrade_dir_with_artifacts();
        let state = state_with_manifest_mock(&dir, b"new-binary-x86").await;
        let h = login(&state).await;

        let a = register_device(&state, "graya", "x86_64", "linux", NEW_AGENT_CAPS, Some("0.59.1")).await;
        let b = register_device(&state, "grayb", "x86_64", "linux", NEW_AGENT_CAPS, Some("0.59.1")).await;
        let mut rx_a = attach_agent_channel(&state, &a).await;
        let mut rx_b = attach_agent_channel(&state, &b).await;

        let r = agent_upgrade_fleet_handler(
            State(state.clone()),
            h,
            Json(json!({"session_ids": [a.clone()]})),
        )
        .await;
        let body = body_json(r).await;
        assert_eq!(body["counts"]["triggered"], 1, "{body}");
        assert!(rx_a.try_recv().is_ok(), "名单内的设备必须被触发");
        assert!(rx_b.try_recv().is_err(), "名单外的设备一台都不许动");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_upgrade_fleet_requires_admin() {
        // 全量入口能一次重启整个设备群，未登录绝不能碰。
        let dir = upgrade_dir_with_artifacts();
        let state = state_with_manifest_mock(&dir, b"new-binary-x86").await;
        register_device(&state, "secdev", "x86_64", "linux", NEW_AGENT_CAPS, Some("0.59.1")).await;
        let r = agent_upgrade_fleet_handler(State(state), HeaderMap::new(), Json(json!({}))).await;
        assert_eq!(r.status(), 401);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_variant_mapping_ignores_desktop_capabilities() {
        // lean CLI 二进制也会上报 desktop:* （build_capabilities 无条件写入），
        // 因此它**不能**作为 UI 的判据——否则 CLI 设备会被换成 30MB 桌面包。
        let lean_cli_caps: Vec<String> = [
            "web:proxy-v1",
            "net:tcp-v1",
            "codec:h264",
            "backend:x11",
            "desktop:gray",
            "desktop:quality",
            "desktop:clipboard",
            "desktop:cursor",
            "desktop:test-delay",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            variant_for_device(&lean_cli_caps, None),
            VariantResolution::Unknown,
            "desktop:* 不构成形态证据；无 variant 声明时必须拒绝猜"
        );
        assert_eq!(variant_for_device(&[], None), VariantResolution::Unknown);

        // 新 agent 显式声明 variant:* 时以它为准
        assert_eq!(
            variant_for_device(&["variant:ui".into(), "desktop:gray".into()], None),
            VariantResolution::Known {
                variant: Variant::Ui,
                source: "capability"
            }
        );
        assert_eq!(
            variant_for_device(&["variant:cli".into()], Some(Variant::Ui)),
            VariantResolution::Known {
                variant: Variant::Cli,
                source: "capability"
            },
            "显式声明优先于运维 override"
        );
        // 无声明时用 override
        assert_eq!(
            variant_for_device(&lean_cli_caps, Some(Variant::Ui)),
            VariantResolution::Known {
                variant: Variant::Ui,
                source: "override"
            }
        );
        // 非法 variant 串被忽略 → 仍然未知（不能因此回落成 CLI）
        assert_eq!(
            variant_for_device(&["variant:desktop".into()], None),
            VariantResolution::Unknown
        );
        assert!(variant_for_device(&[], None).variant().is_none());
        assert_eq!(variant_for_device(&[], None).source(), "unknown");
    }

    #[test]
    fn test_supports_upgrade_v2() {
        assert!(!supports_upgrade_v2(&[]), "老 agent 没有该能力");
        assert!(!supports_upgrade_v2(&[
            "codec:h264".to_string(),
            "desktop:gray".to_string()
        ]));
        assert!(supports_upgrade_v2(&["upgrade:v2".to_string()]));
        assert!(supports_upgrade_v2(&[
            "codec:h264".to_string(),
            "upgrade:v2".to_string()
        ]));
    }

    #[tokio::test]
    async fn test_agent_upgrade_handler_serves_ui_artifact_when_variant_declared() {
        // 显式声明 variant:ui → 选 UI 制品；清单不可达 + 已批准 custom →
        // 任务 source=custom，仅走 relay 镜像。
        let dir = upgrade_dir_with_artifacts();
        std::fs::write(dir.join("shell-remote-ui-x86_64"), b"ui-bytes").unwrap();
        let state = state_with_admin_artifacts_tuned(&dir, |st| {
            st.manifest_fetch.api_base = "http://127.0.0.1:1".to_string();
            st.manifest_fetch.timeout = std::time::Duration::from_millis(400);
            st.manifest_fetch.connect_timeout = std::time::Duration::from_millis(200);
            st.allow_custom_upgrade = true;
        });
        let h = login(&state).await;
        let sid = register_device(
            &state,
            "deskbox",
            "x86_64",
            "linux",
            &["codec:h264", "desktop:gray", "variant:ui"],
            Some("0.59.1"),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;

        let r = agent_upgrade_handler(State(state.clone()), h, Json(json!({"session_id": sid})))
            .await;
        assert_eq!(r.status(), 200);
        let got = rx.try_recv().expect("upgrade msg");
        let v: Value = serde_json::from_str(&got).unwrap();
        let p = &v["payload"];
        assert_eq!(
            p["filename"], "shell-remote-ui-x86_64",
            "声明 variant:ui 的 agent 必须拿 UI 制品"
        );
        assert_eq!(p["variant"], "ui");
        assert_eq!(p["source"], "custom");
        assert_eq!(p["official_release"], false);
        assert!(
            p["cdn_base_urls"].as_array().unwrap().is_empty(),
            "custom 不走外部 CDN"
        );
        assert!(p.get("official_path").is_none());
        // custom 的 hash 来自本地实测，必须等于文件真实 digest。
        assert_eq!(
            p["sha256"],
            {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(b"ui-bytes");
                hex::encode(hasher.finalize())
            }
        );
        assert_eq!(p["relay_url"], "/agent/upgrade/blob/shell-remote-ui-x86_64");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_agent_upgrade_handler_refuses_unapproved_custom_build() {
        // 同样的场景但不批准 custom → 明确拒绝，且不下发任何 hash。
        let dir = upgrade_dir_with_artifacts();
        std::fs::write(dir.join("shell-remote-ui-x86_64"), b"ui-bytes").unwrap();
        let state = state_with_admin_artifacts_tuned(&dir, |st| {
            st.manifest_fetch.api_base = "http://127.0.0.1:1".to_string();
            st.manifest_fetch.timeout = std::time::Duration::from_millis(400);
            st.manifest_fetch.connect_timeout = std::time::Duration::from_millis(200);
        });
        let h = login(&state).await;
        let sid = register_device(
            &state,
            "deskbox",
            "x86_64",
            "linux",
            &["variant:cli", "desktop:gray"],
            Some("0.59.1"),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;

        let r = agent_upgrade_handler(State(state.clone()), h, Json(json!({"session_id": sid})))
            .await;
        assert_eq!(r.status(), 404);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("--allow-custom-upgrade"),
            "错误信息必须告诉管理员怎么批准: {body}"
        );
        assert!(rx.try_recv().is_err(), "拒绝时不得下发指令");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_agent_upgrade_handler_rejects_local_hash_mismatch() {
        // 本地镜像与官方 digest 不一致 → 发布异常，拒绝下发（不静默换 hash）。
        let dir = upgrade_dir_with_artifacts();
        // mock 清单声明的是 "official-bytes"，本地文件是 "new-binary-x86"
        let state = state_with_manifest_mock(&dir, b"official-bytes").await;
        let h = login(&state).await;
        let sid = register_device(
            &state,
            "devbox9",
            "x86_64",
            "linux",
            NEW_AGENT_CAPS,
            Some("0.59.1"),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;

        let r = agent_upgrade_handler(State(state.clone()), h, Json(json!({"session_id": sid})))
            .await;
        assert_eq!(r.status(), 404);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert!(
            body["error"].as_str().unwrap().contains("digest"),
            "必须报 hash 冲突: {body}"
        );
        assert!(rx.try_recv().is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 验收回归（MYS-2425 阶段1 review）────────────────────────────

    /// 回归 1 的前置条件：旧 agent 且未配 override 时，先被 variant_unknown 拦下
    /// ——两条拒绝路径都在 deliver 之前，都不会白发送。
    #[tokio::test]
    async fn test_legacy_agent_without_variant_is_refused_before_delivery() {
        let dir = std::env::temp_dir().join(format!(
            "sr-upg-cdnonly-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state = state_with_manifest_mock(&dir, b"irrelevant").await;
        let h = login(&state).await;
        // 旧 agent：有 desktop:*，但没有 variant:* 也没有 upgrade:v2
        let sid = register_device(
            &state,
            "oldbox",
            "x86_64",
            "linux",
            LEGACY_AGENT_CAPS,
            Some("0.59.1"),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;

        let r = agent_upgrade_handler(State(state.clone()), h, Json(json!({"session_id": sid})))
            .await;
        assert_eq!(r.status(), 409);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], "variant_unknown");
        assert!(rx.try_recv().is_err(), "不得下发任何指令");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归 1 的核心路径：形态已知（override）+ 旧 agent + CDN-only → 409 且
    /// 错误信息指向"去 relay 镜像制品"。
    #[tokio::test]
    async fn test_cdn_only_task_refused_when_legacy_agent_has_no_relay_mirror() {
        let dir = std::env::temp_dir().join(format!(
            "sr-upg-cdnonly2-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 官方清单由本地 mock 提供；制品库里没有 x86_64 制品 → 任务必然 CDN-only。
        let (api_base, mock) = spawn_manifest_mock(env!("CARGO_PKG_VERSION"), b"official").await;
        std::mem::forget(mock);
        let state = state_with_admin_artifacts_tuned(&dir, |st| {
            // 形态由 --upgrade-variant 兜底，绕开 variant_unknown，
            // 从而单独验证"CDN-only + 旧 agent"这条拒绝路径。
            st.upgrade_variant_override = Some(Variant::Cli);
            st.manifest_fetch.api_base = api_base;
        });
        let h = login(&state).await;
        // 旧 agent：无 upgrade:v2，也无 variant（形态靠 override）
        let sid = register_device(
            &state,
            "oldbox2",
            "x86_64",
            "linux",
            &["codec:h264", "desktop:gray"],
            Some("0.59.1"),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;

        let r = agent_upgrade_handler(State(state.clone()), h, Json(json!({"session_id": sid})))
            .await;
        assert_eq!(
            r.status(),
            409,
            "CDN-only 任务绝不能发给不认新协议的旧 agent"
        );
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], "legacy_agent_needs_relay_mirror");
        let err = body["error"].as_str().unwrap();
        assert!(err.contains("--download-dir"), "必须说清怎么修: {err}");
        assert!(err.contains("shell-remote-x86_64"), "必须点名缺哪个制品: {err}");
        assert!(
            rx.try_recv().is_err(),
            "拒绝时不得下发任何指令（否则旧 agent 只会回错误帧）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归 3：老 UI 设备不能被默认当成 CLI 而误降级。未知形态 → 409
    /// `variant_unknown`，响应里带上 agent 上报的能力，便于管理员判断。
    #[tokio::test]
    async fn test_unknown_variant_refused_instead_of_defaulting_to_cli() {
        let dir = upgrade_dir_with_artifacts();
        let state = state_with_admin_artifacts(&dir);
        let h = login(&state).await;
        // 典型老 UI agent：一堆 desktop:* 能力，但没有 variant:*
        let sid = register_device(
            &state,
            "oldui1",
            "x86_64",
            "linux",
            LEGACY_AGENT_CAPS,
            Some("0.59.1"),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;

        let r = agent_upgrade_handler(State(state.clone()), h.clone(), Json(json!({"session_id": sid})))
            .await;
        assert_eq!(r.status(), 409);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(body["code"], "variant_unknown");
        assert!(
            body["error"].as_str().unwrap().contains("--upgrade-variant"),
            "必须告诉管理员显式配置: {body}"
        );
        // 能力回显：管理员能据此看出"这是台老 UI 设备"
        let caps = body["agent_capabilities"].as_array().unwrap();
        assert!(caps.iter().any(|c| c == "desktop:gray"));
        assert!(rx.try_recv().is_err(), "未知形态时不得下发，避免 UI→CLI 误降级");

        // 显式 --upgrade-variant ui 后即可正常下发 UI 制品
        let state2 = state_with_admin_artifacts_tuned(&dir, |st| {
            st.upgrade_variant_override = Some(Variant::Ui);
            st.manifest_fetch.api_base = "http://127.0.0.1:1".to_string();
            st.manifest_fetch.timeout = std::time::Duration::from_millis(400);
            st.manifest_fetch.connect_timeout = std::time::Duration::from_millis(200);
            st.allow_custom_upgrade = true;
        });
        std::fs::write(dir.join("shell-remote-ui-x86_64"), b"ui-bytes").unwrap();
        let h2 = login(&state2).await;
        let sid2 = register_device(
            &state2,
            "oldui2",
            "x86_64",
            "linux",
            LEGACY_AGENT_CAPS,
            Some("0.59.1"),
        )
        .await;
        let mut rx2 = attach_agent_channel(&state2, &sid2).await;
        let r = agent_upgrade_handler(State(state2.clone()), h2, Json(json!({"session_id": sid2})))
            .await;
        assert_eq!(r.status(), 200);
        let got = rx2.try_recv().expect("override 后应下发");
        let v: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["payload"]["filename"], "shell-remote-ui-x86_64");
        assert_eq!(v["payload"]["variant"], "ui");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// plan 端点也要给出同样的可下发判断，供 UI 提前禁用而不是点了才 409。
    #[tokio::test]
    async fn test_plan_reports_blocked_reason() {
        let dir = upgrade_dir_with_artifacts();
        // 情况 A：未知形态
        let state = state_with_admin_artifacts(&dir);
        let h = login(&state).await;
        let sid = register_device(
            &state,
            "oldui1",
            "x86_64",
            "linux",
            LEGACY_AGENT_CAPS,
            Some("0.59.1"),
        )
        .await;
        let q = axum::extract::Query(vec![("session_id".to_string(), sid.clone())]);
        let r = agent_upgrade_plan_handler(State(state.clone()), h.clone(), q).await;
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["blocked_reason"], "variant_unknown");
        assert_eq!(v["variant"], Value::Null);
        assert_eq!(v["variant_unknown"], true);
        assert_eq!(v["supports_upgrade_v2"], false);

        // 情况 B：形态已知 + 有本地镜像 + 旧 agent → 可下发（relay 兜底可用）
        let state2 = state_with_admin_artifacts_tuned(&dir, |st| {
            st.upgrade_variant_override = Some(Variant::Cli);
        });
        let h2 = login(&state2).await;
        let sid2 = register_device(
            &state2,
            "oldcli1",
            "x86_64",
            "linux",
            &["codec:h264"],
            Some("0.59.1"),
        )
        .await;
        let q = axum::extract::Query(vec![("session_id".to_string(), sid2.clone())]);
        let r = agent_upgrade_plan_handler(State(state2.clone()), h2, q).await;
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["blocked_reason"], Value::Null, "有 relay 镜像时旧 agent 也能升");
        assert_eq!(v["local_artifact_present"], true);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_agent_upgrade_plan_reports_decision_without_dispatching() {
        let dir = upgrade_dir_with_artifacts();
        let state = state_with_admin_artifacts(&dir);
        let h = login(&state).await;
        let sid = register_device(
            &state,
            "devbox9",
            "x86_64",
            "linux",
            NEW_AGENT_CAPS,
            Some("0.59.1"),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;

        let q = axum::extract::Query(vec![("session_id".to_string(), sid.clone())]);
        let r = agent_upgrade_plan_handler(State(state.clone()), h, q).await;
        assert_eq!(r.status(), 200);
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["needs_upgrade"], true);
        assert_eq!(v["decision"], "Upgrade");
        assert_eq!(v["agent_version"], "0.59.1");
        assert_eq!(v["relay_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["variant"], "cli");
        assert_eq!(v["target_filename"], "shell-remote-x86_64");
        assert_eq!(v["local_artifact_present"], true);
        assert!(rx.try_recv().is_err(), "plan 只读，不得下发任何指令");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_agent_upgrade_handler_missing_artifact_reports_clear_error() {
        let dir = std::env::temp_dir().join(format!(
            "sr-upg-none-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // only an armv7 artifact — an x86_64 device must be told explicitly.
        std::fs::write(dir.join("shell-remote-armv7"), b"x").unwrap();
        let state = state_without_manifest(&dir);
        let h = login(&state).await;
        let sid = register_device(
            &state,
            "devbox9",
            "x86_64",
            "linux",
            NEW_AGENT_CAPS,
            Some("0.59.1"),
        )
        .await;
        let mut rx = attach_agent_channel(&state, &sid).await;
        let r = agent_upgrade_handler(State(state.clone()), h, Json(json!({"session_id": sid}))).await;
        assert_eq!(r.status(), 404);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert!(
            body["error"].as_str().unwrap().contains("目标档案不存在"),
            "必须明确说明没有可用制品: {body}"
        );
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 带宽限制 admin API（MYS-2287 验收 3.2）──────────────────────

    /// 建一个带 admin 会话 + settings 文件的 state（模拟一次 relay 启动）。
    async fn bandwidth_state(settings: &std::path::Path) -> (Arc<SharedState>, HeaderMap) {
        let state = state_with_admin("admin", "s3cret");
        state
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        crate::relay::bandwidth::apply_startup_rates(
            &state.bandwidth,
            settings.to_path_buf(),
            None,
            None,
        )
        .await;
        (state, cookie_headers("tok"))
    }


    #[tokio::test]
    async fn test_bandwidth_get_post_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("relay-settings.json");
        let (state, hdr) = bandwidth_state(&settings).await;

        // 初始：不限，来源 default。
        let got = body_json(get_bandwidth_handler(State(state.clone()), hdr.clone()).await).await;
        assert_eq!(got["down_mbps"], 0.0);
        assert_eq!(got["up_mbps"], 0.0);
        assert_eq!(got["source"]["down"], "default");
        assert!(got["stats"]["down_bps_1s"].is_u64());

        // POST 8 / 2 → 返回值与 GET 一致。
        let set = body_json(
            set_bandwidth_handler(
                State(state.clone()),
                hdr.clone(),
                Json(json!({"down_mbps": 8.0, "up_mbps": 2.0})),
            )
            .await,
        )
        .await;
        assert_eq!(set["ok"], true);
        assert_eq!(set["down_mbps"], 8.0);
        assert_eq!(set["up_mbps"], 2.0);
        assert_eq!(set["source"]["down"], "file");
        assert!(set.get("warning").is_none(), "正常写入不应带 warning");

        let got2 = body_json(get_bandwidth_handler(State(state.clone()), hdr).await).await;
        assert_eq!(got2["down_mbps"], 8.0);
        assert_eq!(got2["up_mbps"], 2.0);
        assert_eq!(got2["source"]["down"], "file");
        // 内存里即时生效：令牌桶速率就是 1MB/s / 250KB/s。
        assert_eq!(state.bandwidth.down.rate(), 1_000_000);
        assert_eq!(state.bandwidth.up.rate(), 250_000);

        // settings 文件内容正确。
        let text = std::fs::read_to_string(&settings).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["bandwidth_down_mbps"], 8.0);
        assert_eq!(v["bandwidth_up_mbps"], 2.0);
    }

    #[tokio::test]
    async fn test_bandwidth_rejects_invalid_values() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("relay-settings.json");
        let (state, hdr) = bandwidth_state(&settings).await;
        state.bandwidth.set_mbps(8.0, 8.0);

        for bad in [
            json!({"down_mbps": -1.0, "up_mbps": 1.0}),
            json!({"down_mbps": 100_001.0, "up_mbps": 1.0}),
            json!({"down_mbps": "8", "up_mbps": 1.0}),
            json!({"down_mbps": 1.0, "up_mbps": -0.5}),
            json!({}), // 两个方向都缺
        ] {
            let resp =
                set_bandwidth_handler(State(state.clone()), hdr.clone(), Json(bad.clone())).await;
            assert_eq!(resp.status(), 400, "非法值应返回 400：{bad}");
        }
        // 非法请求不得改动已生效的值，也不得写文件。
        assert_eq!(state.bandwidth.down_mbps(), 8.0);
        assert!(
            !settings.exists(),
            "被拒绝的请求不应写出 settings 文件"
        );
    }

    #[tokio::test]
    async fn test_bandwidth_requires_admin() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = bandwidth_state(&dir.path().join("s.json")).await;
        let no_cookie = HeaderMap::new();
        let r = get_bandwidth_handler(State(state.clone()), no_cookie.clone()).await;
        assert_eq!(r.status(), 401);
        let r = set_bandwidth_handler(
            State(state),
            no_cookie,
            Json(json!({"down_mbps": 1.0, "up_mbps": 1.0})),
        )
        .await;
        assert_eq!(r.status(), 401);
    }

    #[tokio::test]
    async fn test_bandwidth_survives_restart() {
        // 验收 3.2 最后一条：POST 后"重启"（重新构造 state 并读文件），值保持。
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("relay-settings.json");
        let (state, hdr) = bandwidth_state(&settings).await;
        set_bandwidth_handler(
            State(state.clone()),
            hdr,
            Json(json!({"down_mbps": 12.5, "up_mbps": 3.0})),
        )
        .await;
        drop(state);

        // 模拟重启：新 state，同一 settings 文件。
        let (restarted, hdr2) = bandwidth_state(&settings).await;
        let got =
            body_json(get_bandwidth_handler(State(restarted.clone()), hdr2).await).await;
        assert_eq!(got["down_mbps"], 12.5, "重启后下行值应保持");
        assert_eq!(got["up_mbps"], 3.0, "重启后上行值应保持");
        assert_eq!(got["source"]["down"], "file");
        assert_eq!(restarted.bandwidth.down.rate(), 1_562_500);
    }

    #[tokio::test]
    async fn test_bandwidth_persist_failure_keeps_memory_value() {
        // 写文件失败（路径不可写）时：仍 500 + warning，但内存值保留并生效。
        let dir = tempfile::tempdir().unwrap();
        // 把 settings "目录"指到一个普通文件上 → create_dir_all 必失败。
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let settings = blocker.join("relay-settings.json");
        let (state, hdr) = bandwidth_state(&settings).await;
        let resp = set_bandwidth_handler(
            State(state.clone()),
            hdr,
            Json(json!({"down_mbps": 5.0, "up_mbps": 5.0})),
        )
        .await;
        let body = body_json(resp).await;
        assert_eq!(body["down_mbps"], 5.0, "内存值仍应生效");
        assert!(
            body["warning"].as_str().is_some_and(|w| !w.is_empty()),
            "写文件失败必须给出 warning，实际 {body}"
        );
        assert_eq!(state.bandwidth.down.rate(), 625_000, "内存值已生效");
    }

    #[tokio::test]
    async fn test_overview_includes_bandwidth_stats() {
        let dir = tempfile::tempdir().unwrap();
        let (state, hdr) = bandwidth_state(&dir.path().join("s.json")).await;
        state.bandwidth.down.consume(1234);
        state.bandwidth.up.consume(567);
        state.bandwidth.count_dropped_frame(3);
        let body = body_json(overview_handler(State(state), hdr).await).await;
        let bw = &body["bandwidth"];
        assert_eq!(bw["down_total_bytes"], 1234);
        assert_eq!(bw["up_total_bytes"], 567);
        assert_eq!(bw["dropped_frames"], 3);
        assert!(bw["throttled_waits"].is_u64());
        assert!(bw["down_bps_10s"].is_u64());
    }
}
