#![allow(dead_code)]

use axum::http::HeaderMap;

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
    if value.is_empty() { None } else { Some(value.to_string()) }
}

pub fn extract_token_from_headers_or_query(headers: &HeaderMap, query_token: Option<&String>) -> Option<String> {
    extract_bearer_token(headers).or_else(|| query_token.cloned())
}

fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) =
                (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
            {
                result.push(((hi << 4) | lo) as char);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            result.push(' ');
        } else {
            result.push(bytes[i] as char);
        }
        i += 1;
    }
    result
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
