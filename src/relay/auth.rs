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
}
