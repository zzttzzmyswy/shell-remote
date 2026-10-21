#![allow(dead_code)]

use axum::http::HeaderMap;

/// Constant-time string comparison for passwords/secrets
/// Both sides are hashed first so the comparison time does not depend on the
/// secret's length either (an early length check leaks it).
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    use sha2::{Digest, Sha256};
    let (ha, hb) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    let mut diff = 0u8;
    for (x, y) in ha.iter().zip(hb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Server password (`--auth`, changeable at runtime via the admin page) gate
/// for browser-facing endpoints. NOT the admin-page login (`--admin-user` /
/// `--admin-pass`) and NOT the per-session token — three separate secrets.
///
/// Supplied via the `X-Auth` header (same as MCP) or, where headers cannot be
/// set (WebSocket upgrade), the `auth` query parameter. An empty configured
/// password disables the check (consistent with the agent/MCP paths).
pub async fn server_password_ok(
    state: &crate::relay::SharedState,
    headers: &HeaderMap,
    query_auth: Option<&String>,
) -> bool {
    let expected = state.server_auth.read().await;
    if expected.is_empty() {
        return true;
    }
    let supplied = headers
        .get("x-auth")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .or(query_auth.map(String::as_str))
        .unwrap_or("");
    constant_time_eq(supplied, &expected)
}

/// 401 body shared by all browser endpoints so the web client can tell a
/// wrong server password apart from an invalid session token.
pub fn invalid_password_response() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "error": "AUTH_INVALID_PASSWORD",
            "message": "Invalid server password"
        })),
    )
        .into_response()
}

/// 客户端 IP，用作限流 key。
///
/// `X-Forwarded-For` 由客户端可伪造——取**最后**一个非空元素：nginx 的
/// `$proxy_add_x_forwarded_for` 把直连对端追加到末尾，前面全是客户端自己
/// 填的内容。没有该头时退回 `X-Real-IP`，再没有则 `"unknown"`（与历史行为
/// 一致：无代理头时所有请求共享一个桶）。
pub fn client_ip(headers: &HeaderMap) -> String {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(last) = xff.split(',').map(str::trim).filter(|s| !s.is_empty()).next_back() {
            return last.to_string();
        }
    }
    if let Some(real) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let real = real.trim();
        if !real.is_empty() {
            return real.to_string();
        }
    }
    "unknown".to_string()
}

/// Whether a session currently has a live agent attached. "Live" = a channel
/// map entry whose `agent` sender has not been closed — a dead SSE
/// incarnation leaves a sender that `Drop` has not yet cleared, and this
/// predicate must not mistake it for an online agent.
pub async fn agent_online(state: &crate::relay::SharedState, sid: &str) -> bool {
    let broadcast = state.agent_broadcast.read().await;
    broadcast
        .get(sid)
        .and_then(|cm| cm.agent.as_ref())
        .is_some_and(|tx| !tx.is_closed())
}

/// 上行请求头的 agent 身份令牌名（新版 agent 在 `/agent/events`、
/// `/agent/ws/send`、`/agent/send` 上携带）。
pub const AGENT_TOKEN_HEADER: &str = "x-sr-agent-token";

/// 从请求头取 agent 令牌（缺失/非 UTF-8/空 → None）。
pub fn agent_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AGENT_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|t| !t.is_empty())
}

/// agent 上行通道的身份判定结果（见 [`agent_auth`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAuth {
    /// 携带了属于该 sid 的 token。
    Verified,
    /// 未携带 token 的旧版 agent：仅在非严格模式下放行，且不得顶替在线 agent。
    Legacy,
    /// 携带了错误的 token，或严格模式下未携带 token。
    Denied,
}

/// 判定一次 agent 上行请求的身份。
///
/// - 有 `X-SR-Agent-Token`：token→sid 映射命中的就是本 sid → `Verified`；
///   命中别的 sid 或未命中 → `Denied`（不做「可能是旧 agent」的降级，否则
///   带错 token 等于没带，攻击者去掉头就绕过）。
/// - 无该头：`strict_agent_auth` 开启 → `Denied`；否则 `Legacy`。
pub async fn agent_auth(
    state: &crate::relay::SharedState,
    sid: &str,
    headers: &HeaderMap,
) -> AgentAuth {
    match agent_token(headers) {
        Some(token) => match state.sessions.authenticate(token).await {
            Some((token_sid, _)) if token_sid == sid => AgentAuth::Verified,
            _ => AgentAuth::Denied,
        },
        None => {
            if state
                .strict_agent_auth
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                AgentAuth::Denied
            } else {
                AgentAuth::Legacy
            }
        }
    }
}

pub fn extract_token_from_query(query: &str) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        match (parts.next(), parts.next()) {
            (Some("token"), Some(value)) => {
                return Some(url_decode(value));
            }
            _ => continue,
        }
    }
    None
}

pub fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("authorization")?.to_str().ok()?;
    let value = value.strip_prefix("Bearer ")?;
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

pub fn extract_token_from_headers_or_query(
    headers: &HeaderMap,
    query_token: Option<&String>,
) -> Option<String> {
    extract_bearer_token(headers).or_else(|| query_token.cloned())
}

fn url_decode(s: &str) -> String {
    let mut result: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                result.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            result.push(b' ');
        } else {
            result.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn test_extract_token_from_query_present() {
        let result = extract_token_from_query("token=abc123&other=foo");
        assert_eq!(result, Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_token_from_query_only_token() {
        let result = extract_token_from_query("token=abc123");
        assert_eq!(result, Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_token_from_query_empty() {
        let result = extract_token_from_query("");
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_token_from_query_no_token() {
        let result = extract_token_from_query("other=foo&bar=baz");
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_token_from_query_url_encoded() {
        let result = extract_token_from_query("token=abc%20123");
        assert_eq!(result, Some("abc 123".to_string()));
    }

    // ── extract_bearer_token ────────────────────────────────────────

    #[test]
    fn test_extract_bearer_token_valid() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer abc123".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_missing_header() {
        let headers = HeaderMap::new();
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn test_extract_bearer_token_not_bearer_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic dGVzdA==".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn test_extract_bearer_token_empty_value() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn test_extract_bearer_token_case_insensitive_header() {
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer token123".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), Some("token123".to_string()));
    }

    // ── extract_token_from_headers_or_query ────────────────────────

    #[test]
    fn test_extract_token_from_headers_or_query_header_present() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer header_token".parse().unwrap());
        let query_token = Some("query_token".to_string());
        assert_eq!(
            extract_token_from_headers_or_query(&headers, query_token.as_ref()),
            Some("header_token".to_string())
        );
    }

    #[test]
    fn test_extract_token_from_headers_or_query_fallback() {
        let headers = HeaderMap::new();
        let query_token = Some("query_token".to_string());
        assert_eq!(
            extract_token_from_headers_or_query(&headers, query_token.as_ref()),
            Some("query_token".to_string())
        );
    }

    #[test]
    fn test_extract_token_from_headers_or_query_both_missing() {
        let headers = HeaderMap::new();
        assert_eq!(extract_token_from_headers_or_query(&headers, None), None);
    }

    #[tokio::test]
    async fn test_server_password_gate() {
        let state = crate::relay::SharedState::new(
            "pw1".to_string(), 1024, None, String::new(), String::new(), None,
        );
        let mut h = HeaderMap::new();
        assert!(!server_password_ok(&state, &h, None).await, "missing password rejected");
        h.insert("x-auth", "bad".parse().unwrap());
        assert!(!server_password_ok(&state, &h, None).await, "wrong header rejected");
        h.insert("x-auth", "pw1".parse().unwrap());
        assert!(server_password_ok(&state, &h, None).await, "header accepted");
        let q = "pw1".to_string();
        assert!(server_password_ok(&state, &HeaderMap::new(), Some(&q)).await, "query accepted");
        *state.server_auth.write().await = String::new();
        assert!(server_password_ok(&state, &HeaderMap::new(), None).await, "empty password disables gate");
    }

    #[test]
    fn test_extract_token_from_headers_or_query_header_empty_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer ".parse().unwrap());
        let query_token = Some("fallback".to_string());
        assert_eq!(
            extract_token_from_headers_or_query(&headers, query_token.as_ref()),
            Some("fallback".to_string())
        );
    }

    // ── client_ip ──────────────────────────────────────────────────

    fn xff(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", v.parse().unwrap());
        h
    }

    #[test]
    fn test_client_ip_takes_last_forwarded_element() {
        // nginx 的 $proxy_add_x_forwarded_for 把直连对端追加在末尾；前面的
        // 内容全部来自客户端自己填的 XFF，不可信。
        assert_eq!(client_ip(&xff("1.1.1.1, 2.2.2.2")), "2.2.2.2");
        assert_eq!(client_ip(&xff("1.1.1.1,2.2.2.2")), "2.2.2.2");
        assert_eq!(client_ip(&xff("  evil , 3.3.3.3 ")), "3.3.3.3");
    }

    #[test]
    fn test_client_ip_single_and_trailing_comma() {
        assert_eq!(client_ip(&xff("9.9.9.9")), "9.9.9.9");
        // 末尾空元素忽略，取最后一个非空
        assert_eq!(client_ip(&xff("9.9.9.9, ") ), "9.9.9.9");
    }

    #[test]
    fn test_client_ip_falls_back_to_real_ip_then_unknown() {
        let mut h = HeaderMap::new();
        h.insert("x-real-ip", "4.4.4.4".parse().unwrap());
        assert_eq!(client_ip(&h), "4.4.4.4");

        // 空 XFF → 落到 X-Real-IP
        h.insert("x-forwarded-for", "".parse().unwrap());
        assert_eq!(client_ip(&h), "4.4.4.4");

        assert_eq!(client_ip(&HeaderMap::new()), "unknown");
    }

    #[test]
    fn test_client_ip_empty_forwarded_falls_back() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", " , ".parse().unwrap());
        h.insert("x-real-ip", "5.5.5.5".parse().unwrap());
        assert_eq!(client_ip(&h), "5.5.5.5");
    }

    // ── agent_auth ─────────────────────────────────────────────────

    fn agent_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(AGENT_TOKEN_HEADER, token.parse().unwrap());
        h
    }

    #[tokio::test]
    async fn test_agent_auth_verified_legacy_denied() {
        let state = crate::relay::SharedState::new(
            String::new(), 1024, None, String::new(), String::new(), None,
        );
        let other = state.sessions.register(None, "rw", None).await.unwrap();
        let mine = state
            .sessions
            .register(None, "rw", Some("agent01".to_string()))
            .await
            .unwrap();

        // 正确 token → Verified
        assert_eq!(
            agent_auth(&state, "agent01", &agent_headers(&mine.tokens[0].0)).await,
            AgentAuth::Verified
        );
        // 别的会话的 token → Denied（不能靠带到别处去降级成 Legacy）
        assert_eq!(
            agent_auth(&state, "agent01", &agent_headers(&other.tokens[0].0)).await,
            AgentAuth::Denied
        );
        // 乱填的 token → Denied
        assert_eq!(
            agent_auth(&state, "agent01", &agent_headers("nonsense")).await,
            AgentAuth::Denied
        );
        // 无 token + 非严格模式 → Legacy
        assert_eq!(
            agent_auth(&state, "agent01", &HeaderMap::new()).await,
            AgentAuth::Legacy
        );
        // 无 token + 严格模式 → Denied
        state.set_strict_agent_auth(true);
        assert_eq!(
            agent_auth(&state, "agent01", &HeaderMap::new()).await,
            AgentAuth::Denied
        );
    }

    #[tokio::test]
    async fn test_agent_auth_accepts_ro_token() {
        // `--token-type ro` 的 agent 只有 ro token；relay 接受该会话的任何
        // token 作为 agent 身份，否则这类 agent 永远无法通过校验。
        let state = crate::relay::SharedState::new(
            String::new(), 1024, None, String::new(), String::new(), None,
        );
        let r = state
            .sessions
            .register(None, "ro", Some("rob01".to_string()))
            .await
            .unwrap();
        assert_eq!(
            agent_auth(&state, "rob01", &agent_headers(&r.tokens[0].0)).await,
            AgentAuth::Verified
        );
    }

    #[tokio::test]
    async fn test_agent_online_requires_live_channel() {
        let state = crate::relay::SharedState::new(
            String::new(), 1024, None, String::new(), String::new(), None,
        );
        state
            .agent_broadcast
            .write()
            .await
            .insert("sid1".to_string(), crate::relay::ChannelMap::new());
        assert!(
            !agent_online(&state, "sid1").await,
            "no agent channel → not online"
        );

        let (tx, rx) = tokio::sync::mpsc::channel::<String>(4);
        state
            .agent_broadcast
            .write()
            .await
            .get_mut("sid1")
            .unwrap()
            .agent = Some(tx.clone());
        assert!(agent_online(&state, "sid1").await, "live sender → online");

        drop(rx); // 通道关闭 = agent 那一端已消失
        assert!(
            !agent_online(&state, "sid1").await,
            "closed sender must not count as a live agent"
        );
        assert!(!agent_online(&state, "nope").await);
    }
}
