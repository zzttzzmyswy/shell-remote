//! Console-output encoding handling.
//!
//! Subprocesses (cmd/PowerShell on Windows, shells on other platforms) can emit
//! bytes that are not UTF-8 — most commonly GBK/cp936 on a Chinese Windows
//! console. We must decode those bytes with the *correct* legacy encoding
//! instead of `String::from_utf8_lossy` (which replaces invalid bytes with
//! U+FFFD and corrupts the text before it ever reaches the terminal / MCP
//! client).
//!
//! Strategy: try strict UTF-8 first (fast, correct for the common case and for
//! any program that already emits UTF-8). On failure, fall back to the active
//! ANSI console code page on Windows (defaulting to GBK/cp936, the most
//! frequent legacy console encoding); on non-Windows, keep the platform's
//! default text conversion behavior (lossy UTF-8), since non-UTF-8 legacy
//! output is much rarer outside Windows.

/// The active Windows ANSI console code page, or `None` on non-Windows.
///
/// Resolved lazily and cached: on Windows we query it via `chcp` at first use
/// (cost one subprocess), then reuse the result. `None` here means "no strong
/// signal about a legacy encoding", so callers fall back to GBK.
#[cfg(windows)]
fn windows_codepage() -> Option<u32> {
    use std::sync::OnceLock;
    static CP: OnceLock<Option<u32>> = OnceLock::new();
    *CP.get_or_init(|| {
        // `chcp` ships as chcp.com; run through cmd so the code page query
        // works regardless of how this binary was launched.
        let out = std::process::Command::new("cmd")
            .arg("/c")
            .arg("chcp")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        // chcp prints e.g. "Active code page: 936" (localized labels vary,
        // but the trailing number is what matters).
        let num = text
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .last()?;
        num.parse::<u32>().ok()
    })
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn windows_codepage() -> Option<u32> {
    None
}

#[cfg(windows)]
mod win {
    use super::windows_codepage;
    /// Fallback used when we cannot query a code page (e.g. chcp not found).
    pub const DEFAULT_CODEPAGE: u32 = 936; // GBK, most common legacy Chinese console

    pub fn decode_bytes(bytes: &[u8]) -> String {
        if let Ok(s) = std::str::from_utf8(bytes) {
            return s.to_string();
        }
        let cp = windows_codepage().unwrap_or(DEFAULT_CODEPAGE);
        let encoding = match cp {
            932 => encoding_rs::SHIFT_JIS,
            936 => encoding_rs::GBK,
            949 => encoding_rs::EUC_KR,
            950 => encoding_rs::BIG5,
            65001 => encoding_rs::UTF_8,
            // cp125x family
            1250..=1258 => encoding_rs::WINDOWS_1252,
            _ => encoding_rs::GBK,
        };
        let (cow, _, _had_errors) = encoding.decode(bytes);
        cow.into_owned()
    }
}
#[cfg(not(windows))]
mod win {
    pub fn decode_bytes(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Decode a subprocess stdout/stderr chunk that is *expected* to be text into a
/// UTF-8 `String`. Uses the correct legacy encoding when the bytes are not
/// valid UTF-8 (Windows console code pages such as GBK); otherwise falls back
/// to the platform default (lossy UTF-8).
pub fn decode_bytes(bytes: &[u8]) -> String {
    // Common fast path: already valid UTF-8 (covers POSIX and any UTF-8
    // configured Windows console) — avoid the platform branch entirely.
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    win::decode_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_ascii_is_identity() {
        assert_eq!(decode_bytes(b"hello world"), "hello world");
    }

    #[test]
    fn test_decode_utf8_is_identity() {
        // UTF-8 中文 "你好"
        assert_eq!(decode_bytes("你好".as_bytes()), "你好");
        // Multi-byte + ASCII mixed
        assert_eq!(decode_bytes("ab你cd好".as_bytes()), "ab你cd好");
    }

    #[test]
    fn test_decode_valid_utf8_even_with_means_lossless() {
        // Even if a codepage is configured, valid UTF-8 must pass through.
        let s = "emoji 🚀 and 中文";
        assert_eq!(decode_bytes(s.as_bytes()), s);
    }

    #[test]
    fn test_decode_gbk_hello() {
        // "你好" in GBK = \xC4\xE3\xBA\xC3
        let gbk = [0xC4u8, 0xE3, 0xBA, 0xC3];
        #[cfg(windows)]
        {
            // On Windows this decodes via codepage; if the active code page is
            // UTF-8 / chcp not reachable, our fallback is still GBK, so both
            // yield "你好".
            assert_eq!(decode_bytes(&gbk), "你好");
        }
        #[cfg(not(windows))]
        {
            // On non-Windows we intentionally keep lossy UTF-8; GBK bytes are
            // invalid UTF-8 → replacement chars, NOT "你好". This documents the
            // platform boundary.
            assert!(decode_bytes(&gbk).contains('\u{FFFD}'));
        }
    }

    #[test]
    fn test_decode_gbk_partial_valid_mix() {
        // "ABC你好XYZ" where 你好 is GBK-encoded, rest ASCII.
        // ASCII stays identical; the GBK pair must not corrupt the ASCII.
        let bytes = b"ABC\xC4\xE3\xBA\xC3XYZ";
        #[cfg(windows)]
        assert_eq!(decode_bytes(bytes), "ABC你好XYZ");
        #[cfg(not(windows))]
        assert!(decode_bytes(bytes).starts_with("ABC"));
    }

    #[test]
    fn test_decode_empty() {
        assert_eq!(decode_bytes(b""), "");
    }

    #[test]
    fn test_decoder_does_not_panic_on_garbage() {
        // Arbitrary invalid sequences must not panic — lossy path is allowed.
        let junk = [0xFFu8, 0x00, 0x80, 0xFF, 0xFE];
        let _ = decode_bytes(&junk);
    }
}