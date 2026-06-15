#![allow(dead_code)]

/// Extracts the `token` parameter value from a URL query string.
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
}
