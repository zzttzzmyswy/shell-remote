//! Web admin panel: hidden sub-path + login, then session/token/runtime
//! management. Routes are registered dynamically in `relay::start` under the
//! configured `--admin-path`; when that flag is unset, no admin route exists
//! and the panel is completely inaccessible.
#![allow(clippy::too_many_arguments)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
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
            "browser_count": browser_count,
            "tokens": tokens,
            "tags": info.tags,
        }));
    }
    drop(broadcasts);
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "agent_count": sessions.len(),
        "agent_online": agent_online,
        "browser_count": browser_total,
        "sessions": sess_json,
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

pub async fn add_tag_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let sid = body["session_id"].as_str().unwrap_or("").to_string();
    let tag = body["tag"].as_str().unwrap_or("").to_string();
    if sid.is_empty() || tag.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing session_id or tag"})),
        )
            .into_response();
    }
    let ok = state.sessions.add_tag(&sid, &tag).await;
    Json(json!({"ok": ok})).into_response()
}

pub async fn remove_tag_handler(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_admin(&state, &headers).await {
        return unauthorized();
    }
    let sid = body["session_id"].as_str().unwrap_or("").to_string();
    let tag = body["tag"].as_str().unwrap_or("").to_string();
    if sid.is_empty() || tag.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing session_id or tag"})),
        )
            .into_response();
    }
    let ok = state.sessions.remove_tag(&sid, &tag).await;
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
        let (sid, _t) = state.sessions.register(None, "rw", None).await.unwrap();
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
    async fn test_kick_removes_session() {
        let state = state_with_admin("admin", "s3cret");
        let (sid, tokens) = state.sessions.register(None, "rw", None).await.unwrap();
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
        let (sid, tokens) = state.sessions.register(None, "both", None).await.unwrap();
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
}
