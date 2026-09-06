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
    Json(body): Json<Value>,
) -> Response {
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
    state.agent_event_buffers.write().await.remove(&sid);
    state.last_activity.write().await.remove(&sid);
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
    let ok = state.sessions.revoke_token(&token).await;
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
    let ok = state
        .sessions
        .set_token_permission(&token, parse_perm(perm))
        .await;
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
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => (
            StatusCode::OK,
            [("content-type", "text/plain; charset=utf-8")],
            text,
        )
            .into_response(),
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
pub fn artifact_arch_key(arch: &str) -> String {
    match arch.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => "x86_64".to_string(),
        "aarch64" | "arm64" => "aarch64".to_string(),
        "armv7l" | "armv7" => "armv7".to_string(),
        other => other.to_string(),
    }
}

/// Parse a staged artifact filename into (arch key, windows). Returns `None`
/// for anything that is not `shell-remote-<arch>[.exe]`.
pub fn parse_artifact_name(name: &str) -> Option<(String, bool)> {
    let rest = name.strip_prefix("shell-remote-")?;
    let windows = rest.ends_with(".exe");
    let arch = rest.strip_suffix(".exe").unwrap_or(rest);
    if arch.is_empty() || arch.contains('.') || arch.contains('/') || arch.contains('\\') {
        return None;
    }
    Some((arch.to_string(), windows))
}

/// True when `name` is a valid staged artifact filename. The blob download
/// endpoint uses this as its only path guard — it must reject every traversal
/// or separator trick.
pub fn valid_artifact_filename(name: &str) -> bool {
    parse_artifact_name(name).is_some()
}

/// File a device with `arch`/`platform` should upgrade from, if staged.
fn artifact_file_for(
    dir: &std::path::Path,
    arch: &str,
    platform: Option<&str>,
) -> Option<std::path::PathBuf> {
    let key = artifact_arch_key(arch);
    if platform == Some("windows") {
        let p = dir.join(format!("shell-remote-{key}.exe"));
        p.is_file().then_some(p)
    } else {
        let p = dir.join(format!("shell-remote-{key}"));
        p.is_file().then_some(p)
    }
}

/// Version label for a staged artifact: the `shell-remote-<key>.version`
/// companion file if present, else a generic label.
fn artifact_version(dir: &std::path::Path, key: &str) -> String {
    std::fs::read_to_string(dir.join(format!("shell-remote-{key}.version")))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "v?".to_string())
}

/// List the staged agent upgrade artifacts (`--agent-upgrade-dir`). Purely
/// file-metadata based (no hashing) so it is cheap enough for the admin page's
/// periodic refresh.
pub async fn upgrade_artifacts_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let Some(dir) = state.upgrade_dir.read().await.clone() else {
        return Json(json!({"enabled": false, "artifacts": []})).into_response();
    };
    let mut artifacts: Vec<Value> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some((arch, windows)) = parse_artifact_name(&name) {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        artifacts.push(json!({
                            "arch": arch,
                            "filename": name,
                            "size": meta.len(),
                            "windows": windows,
                            "version": artifact_version(&dir, &arch),
                        }));
                    }
                }
            }
        }
    }
    artifacts.sort_by(|a, b| a["arch"].as_str().cmp(&b["arch"].as_str()));
    Json(json!({
        "enabled": true,
        "dir": dir.display().to_string(),
        "artifacts": artifacts,
    }))
    .into_response()
}

/// Trigger an atomic self-upgrade for one device: hash the staged artifact for
/// the device's arch and deliver `agent:upgrade` over the session's agent
/// channel. The agent then downloads, verifies and atomically replaces itself.
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
    let Some(info) = state.sessions.get(&sid).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "session not found"})),
        )
            .into_response();
    };
    let (arch, platform) = match info.device.as_ref() {
        Some(d) => (d.arch.clone().unwrap_or_default(), d.platform.clone()),
        None => (String::new(), None),
    };
    if arch.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "device arch unknown — cannot pick an artifact"})),
        )
            .into_response();
    }
    let Some(dir) = state.upgrade_dir.read().await.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "agent upgrades disabled (--agent-upgrade-dir unset)"})),
        )
            .into_response();
    };
    let Some(path) = artifact_file_for(&dir, &arch, platform.as_deref()) else {
        let key = artifact_arch_key(&arch);
        let want = if platform.as_deref() == Some("windows") {
            format!("shell-remote-{key}.exe")
        } else {
            format!("shell-remote-{key}")
        };
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": format!("no upgrade artifact for arch {arch} (expected {want} in {})", dir.display())})),
        )
            .into_response();
    };
    // Hash + version at trigger time (one-shot, per click).
    let sha256 = match crate::agent::upgrade::sha256_hex_file(&path) {
        Ok(h) => h,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": format!("cannot hash artifact: {e}")})),
            )
                .into_response();
        }
    };
    let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let arch_key = artifact_arch_key(&arch);
    let version = artifact_version(&dir, &arch_key);

    let text = serde_json::to_string(&json!({
        "type": "agent:upgrade",
        "session_id": sid,
        "payload": {
            "version": version,
            "url": format!("/agent/upgrade/blob/{}", filename),
            "sha256": sha256,
        }
    }))
    .unwrap_or_default();
    {
        let broadcast = state.agent_broadcast.read().await;
        match broadcast.get(&sid).and_then(|cm| cm.agent.as_ref()) {
            Some(tx) => crate::relay::ws::deliver(tx, "agent:upgrade", text),
            None => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"ok": false, "error": "agent offline"})),
                )
                    .into_response();
            }
        }
    }
    // Reflect the trigger immediately in the overview until the agent's first
    // progress frame replaces this entry.
    state.agent_upgrades.write().await.insert(
        sid.clone(),
        json!({"stage": "started", "version": version, "detail": "upgrade triggered"}),
    );
    Json(json!({"ok": true, "version": version, "arch": arch_key})).into_response()
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
        let resp = login_handler(State(state.clone()), body).await;
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
        let resp = login_handler(State(state), body).await;
        assert_eq!(resp.status(), 401);
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

    #[tokio::test]
    async fn test_upgrade_artifacts_handler_lists_staged_binaries() {
        let dir = upgrade_dir_with_artifacts();
        let state = state_with_admin("admin", "s3cret");
        *state.upgrade_dir.write().await = Some(dir.clone());
        state
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        let h = cookie_headers("tok");

        let r = upgrade_artifacts_handler(State(state.clone()), h.clone()).await;
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["enabled"], true);
        let arts = v["artifacts"].as_array().unwrap();
        assert_eq!(arts.len(), 3, "noise files must be excluded");
        let x86 = arts
            .iter()
            .find(|a| a["arch"] == "x86_64")
            .expect("x86_64 artifact listed");
        assert_eq!(x86["filename"], "shell-remote-x86_64");
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

    #[tokio::test]
    async fn test_agent_upgrade_handler_offline_agent_is_conflict() {
        let dir = upgrade_dir_with_artifacts();
        let state = state_with_admin("admin", "s3cret");
        *state.upgrade_dir.write().await = Some(dir.clone());
        state
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        let h = cookie_headers("tok");

        let reg = state
            .sessions
            .register(None, "rw", Some("devbox9".to_string()))
            .await
            .unwrap();
        state
            .sessions
            .set_device(
                &reg.session_id,
                Some(crate::proto::DeviceInfo {
                    hostname: None,
                    platform: Some("linux".to_string()),
                    arch: Some("x86_64".to_string()),
                    os: None,
                    kernel: None,
                    cpu_model: None,
                }),
            )
            .await;

        // No agent channel → offline → 409.
        let r = agent_upgrade_handler(
            State(state.clone()),
            h.clone(),
            Json(json!({"session_id": reg.session_id})),
        )
        .await;
        assert_eq!(r.status(), 409);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_agent_upgrade_handler_delivers_upgrade_message() {
        let dir = upgrade_dir_with_artifacts();
        let expected_digest = crate::agent::upgrade::sha256_hex_file(&dir.join("shell-remote-x86_64"))
            .unwrap();
        let state = state_with_admin("admin", "s3cret");
        *state.upgrade_dir.write().await = Some(dir.clone());
        state
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        let h = cookie_headers("tok");

        let reg = state
            .sessions
            .register(None, "rw", Some("devbox9".to_string()))
            .await
            .unwrap();
        state
            .sessions
            .set_device(
                &reg.session_id,
                Some(crate::proto::DeviceInfo {
                    hostname: None,
                    platform: Some("linux".to_string()),
                    arch: Some("x86_64".to_string()),
                    os: None,
                    kernel: None,
                    cpu_model: None,
                }),
            )
            .await;

        // A live agent SSE channel so the relay can deliver the message.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
        state.agent_broadcast.write().await.insert(
            reg.session_id.clone(),
            crate::relay::ChannelMap {
                agent: Some(tx.clone()),
                agent_bulk: None,
                browser_sessions: std::collections::HashMap::new(),
            },
        );

        let r = agent_upgrade_handler(
            State(state.clone()),
            h.clone(),
            Json(json!({"session_id": reg.session_id})),
        )
        .await;
        assert_eq!(r.status(), 200);
        let got = rx.try_recv().expect("agent channel must receive the upgrade msg");
        let v: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["type"], "agent:upgrade");
        assert_eq!(v["session_id"], reg.session_id);
        let payload = &v["payload"];
        assert_eq!(payload["version"], "0.20.0");
        assert_eq!(payload["url"], "/agent/upgrade/blob/shell-remote-x86_64");
        assert_eq!(payload["sha256"], expected_digest);

        // Trigger is reflected in the overview immediately.
        let resp = overview_handler(State(state.clone()), h.clone()).await;
        let ov: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap();
        let sess = ov["sessions"]
            .as_array()
            .unwrap()
            .into_iter()
            .find(|s| s["session_id"] == reg.session_id)
            .unwrap();
        assert_eq!(sess["upgrade"]["stage"], "started");
        assert_eq!(sess["upgrade"]["version"], "0.20.0");
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
        let state = state_with_admin("admin", "s3cret");
        *state.upgrade_dir.write().await = Some(dir.clone());
        state
            .admin_sessions
            .write()
            .await
            .insert("tok".to_string(), Instant::now() + ADMIN_SESSION_TTL);
        let reg = state
            .sessions
            .register(None, "rw", Some("devbox9".to_string()))
            .await
            .unwrap();
        state
            .sessions
            .set_device(
                &reg.session_id,
                Some(crate::proto::DeviceInfo {
                    hostname: None,
                    platform: Some("linux".to_string()),
                    arch: Some("x86_64".to_string()),
                    os: None,
                    kernel: None,
                    cpu_model: None,
                }),
            )
            .await;
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(8);
        state.agent_broadcast.write().await.insert(
            reg.session_id.clone(),
            crate::relay::ChannelMap {
                agent: Some(tx),
                agent_bulk: None,
                browser_sessions: std::collections::HashMap::new(),
            },
        );
        let r = agent_upgrade_handler(
            State(state.clone()),
            cookie_headers("tok"),
            Json(json!({"session_id": reg.session_id})),
        )
        .await;
        assert_eq!(r.status(), 404);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
