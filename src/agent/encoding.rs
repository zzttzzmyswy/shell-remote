//! Console-output encoding handling.
//!
//! Subprocesses (cmd/PowerShell on Windows, shells on other platforms) emit
//! bytes that may be UTF-8 **or** the active system ANSI code page (GBK/cp936
//! on a Chinese Windows) — and one command can mix both: PowerShell itself
//! writes UTF-8 (`[Console]::OutputEncoding`), a native program that follows
//! the console code page writes GBK, and `type` / `git` / `python` dump a
//! file's raw bytes. Decoding the whole buffer with a single choice therefore
//! corrupts whichever half loses.
//!
//! Strategy: decode **line by line**. A line that is valid UTF-8 passes
//! through untouched; only lines that are not fall back to the legacy
//! encoding. Chunks that arrive in pieces are decoded with [`StreamDecoder`],
//! which carries a small tail across reads so a multi-byte character split
//! between two reads is never torn apart.
//!
//! Non-Windows has no legacy console encoding, so the per-line fallback stays
//! lossy UTF-8 and behavior is unchanged there.

use std::sync::OnceLock;

use encoding_rs::Encoding;

/// Map a Windows code page to its `encoding_rs` decoder.
///
/// `None` means "no decoder for this code page" — the caller then falls back to
/// GBK. cp65001 (UTF-8) is deliberately absent: it is handled before this map
/// is consulted, since a UTF-8 code page means there is no legacy encoding at
/// all.
#[cfg_attr(not(windows), allow(dead_code))]
fn codepage_encoding(cp: u32) -> Option<&'static Encoding> {
    Some(match cp {
        874 => encoding_rs::WINDOWS_874,
        932 => encoding_rs::SHIFT_JIS,
        936 => encoding_rs::GBK,
        949 => encoding_rs::EUC_KR,
        950 => encoding_rs::BIG5,
        1250 => encoding_rs::WINDOWS_1250,
        1251 => encoding_rs::WINDOWS_1251,
        1252 => encoding_rs::WINDOWS_1252,
        1253 => encoding_rs::WINDOWS_1253,
        1254 => encoding_rs::WINDOWS_1254,
        1255 => encoding_rs::WINDOWS_1255,
        1256 => encoding_rs::WINDOWS_1256,
        1257 => encoding_rs::WINDOWS_1257,
        1258 => encoding_rs::WINDOWS_1258,
        54936 => encoding_rs::GB18030,
        866 => encoding_rs::IBM866,
        _ => return None,
    })
}

/// Pick the legacy encoding from the ANSI and OEM code pages.
///
/// Split out from [`platform_legacy_encoding`] so the rule is testable without
/// an OS that reports code pages. Never returns `None`: a silently wrong guess
/// only costs replacement characters on lines that really were legacy-encoded,
/// whereas giving up would make *every* such line lossy.
#[cfg_attr(not(windows), allow(dead_code))]
fn legacy_from_codepages(acp: u32, oem: u32) -> &'static Encoding {
    if acp == 65001 {
        // ANSI is already UTF-8, so the OEM code page is the only remaining
        // candidate for text that is not.
        if oem != 65001 {
            return codepage_encoding(oem).unwrap_or(encoding_rs::GBK);
        }
        return encoding_rs::GBK;
    }
    codepage_encoding(acp).unwrap_or(encoding_rs::GBK)
}

/// The system's legacy (non-UTF-8) console encoding, or `None` when the
/// platform has none.
///
/// `GetACP` is the ANSI code page that native programs target when writing to
/// a pipe — this is the authoritative signal, unlike the console *input* code
/// page that `chcp` reports (and which an earlier PowerShell call can leave
/// behind).
#[cfg(windows)]
fn platform_legacy_encoding() -> Option<&'static Encoding> {
    let acp = unsafe { windows_sys::Win32::Globalization::GetACP() };
    let oem = unsafe { windows_sys::Win32::Globalization::GetOEMCP() };
    Some(legacy_from_codepages(acp, oem))
}

/// Non-Windows platforms have no legacy console encoding: keep lossy UTF-8.
#[cfg(not(windows))]
fn platform_legacy_encoding() -> Option<&'static Encoding> {
    None
}

/// [`platform_legacy_encoding`], resolved once per process (it cannot change
/// while we run, and the Windows path calls into the OS).
fn legacy_encoding() -> Option<&'static Encoding> {
    static LEGACY: OnceLock<Option<&'static Encoding>> = OnceLock::new();
    *LEGACY.get_or_init(platform_legacy_encoding)
}

/// Decode one complete line with the legacy encoding (`None` → lossy UTF-8).
fn decode_legacy(bytes: &[u8], legacy: Option<&'static Encoding>) -> String {
    match legacy {
        Some(enc) => {
            let (cow, _, _) = enc.decode(bytes);
            cow.into_owned()
        }
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Decode one complete line (it already contains its `\n`, if any): valid
/// UTF-8 passes through, anything else is legacy-encoded.
///
/// A `\n` byte can never be part of a multi-byte sequence, so a line that ends
/// in a newline is either wholly valid UTF-8 or wholly not — there is no
/// "truncated tail" case to consider here.
fn decode_line(line: &[u8], legacy: Option<&'static Encoding>) -> String {
    match std::str::from_utf8(line) {
        Ok(s) => s.to_string(),
        Err(_) => decode_legacy(line, legacy),
    }
}

/// Decode the final line of a buffer, which may end mid-character.
///
/// When the *only* problem is an incomplete UTF-8 sequence at the very end,
/// the valid prefix is kept verbatim and just the truncated tail goes lossy.
/// Decoding the whole line as GBK instead would garble the prefix — this is
/// exactly the case a 4096-byte read boundary produces.
fn decode_trailing_line(line: &[u8], legacy: Option<&'static Encoding>) -> String {
    match std::str::from_utf8(line) {
        Ok(s) => s.to_string(),
        // Only split when the prefix really is UTF-8; a line with no valid
        // prefix at all (e.g. GBK text that merely ends mid-pair) is legacy.
        Err(e) if e.error_len().is_none() && e.valid_up_to() > 0 => {
            let cut = e.valid_up_to();
            let mut s = String::with_capacity(line.len());
            s.push_str(&String::from_utf8_lossy(&line[..cut]));
            s.push_str(&String::from_utf8_lossy(&line[cut..]));
            s
        }
        Err(_) => decode_legacy(line, legacy),
    }
}

/// Line-by-line decoder with an injectable legacy encoding.
///
/// The injectable parameter exists so the GBK path is unit-testable on Linux;
/// production callers go through [`decode_bytes`] / [`StreamDecoder::new`],
/// which pass the platform's encoding.
fn decode_bytes_with(bytes: &[u8], legacy: Option<&'static Encoding>) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // Fast path: one encoding for the whole buffer (the overwhelmingly common
    // case) — no need to split lines at all.
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let mut out = String::with_capacity(bytes.len());
    let mut start = 0;
    while start < bytes.len() {
        match bytes[start..].iter().position(|&b| b == b'\n') {
            Some(off) => {
                let end = start + off + 1;
                out.push_str(&decode_line(&bytes[start..end], legacy));
                start = end;
            }
            None => {
                out.push_str(&decode_trailing_line(&bytes[start..], legacy));
                break;
            }
        }
    }
    out
}

/// Decode a subprocess stdout/stderr chunk that is *expected* to be text into
/// a UTF-8 `String`.
///
/// Decoding is per line, so a single buffer that mixes UTF-8 and legacy
/// (GBK/cp936) lines yields correct text for both. See the module docs.
pub fn decode_bytes(bytes: &[u8]) -> String {
    decode_bytes_with(bytes, legacy_encoding())
}

/// Returns `1` when `line` ends with a double-byte lead byte that has no trail
/// byte yet, else `0`.
///
/// Pairing is scanned GBK-style from the start of the line (`< 0x80` consumes
/// one byte, otherwise two), which is what tells a genuinely unpaired lead
/// apart from a trail byte that merely *looks* like a lead.
fn unpaired_lead_tail_len(line: &[u8]) -> usize {
    let mut i = 0;
    while i < line.len() {
        if line[i] < 0x80 {
            i += 1;
        } else if i + 1 >= line.len() {
            return 1;
        } else {
            i += 2;
        }
    }
    0
}

/// Streaming byte→text decoder for reads that can split a character.
///
/// `push` emits every complete line immediately and withholds only what it
/// must: an incomplete UTF-8 sequence at the end (≤3 bytes), or an unpaired
/// legacy double-byte lead byte. `finish` releases whatever is still withheld
/// at EOF so no output is ever swallowed.
pub struct StreamDecoder {
    /// Bytes withheld from the previous `push` (up to 3).
    tail: Vec<u8>,
    legacy: Option<&'static Encoding>,
}

impl StreamDecoder {
    /// Decoder using the platform's legacy encoding.
    pub fn new() -> Self {
        Self::new_with(legacy_encoding())
    }

    /// Decoder with an injectable legacy encoding (tests exercise GBK here).
    fn new_with(legacy: Option<&'static Encoding>) -> Self {
        Self {
            tail: Vec::new(),
            legacy,
        }
    }

    /// Feed one read's worth of bytes; returns the text that is now complete.
    pub fn push(&mut self, chunk: &[u8]) -> String {
        if chunk.is_empty() {
            return String::new();
        }
        self.tail.extend_from_slice(chunk);
        let Some(nl) = self.tail.iter().rposition(|&b| b == b'\n') else {
            // No full line yet — only the pending line can be examined.
            return self.drain_pending_line();
        };
        let complete = nl + 1;
        let mut out = decode_bytes_with(&self.tail[..complete], self.legacy);
        self.tail.drain(..complete);
        out.push_str(&self.drain_pending_line());
        out
    }

    /// Decode as much of the pending (newline-free) line as is safe, keeping
    /// the undecidable tail for the next `push`.
    fn drain_pending_line(&mut self) -> String {
        if self.tail.is_empty() {
            return String::new();
        }
        match std::str::from_utf8(&self.tail) {
            Ok(_) => {
                let s = String::from_utf8_lossy(&self.tail).into_owned();
                self.tail.clear();
                s
            }
            // Missing continuation bytes at the end: emit the valid prefix and
            // hold the (at most 3) incomplete bytes.
            Err(e) if e.error_len().is_none() => {
                let cut = e.valid_up_to();
                let s = String::from_utf8_lossy(&self.tail[..cut]).into_owned();
                self.tail.drain(..cut);
                s
            }
            // Genuinely legacy bytes. Decode them now, but keep a trailing lead
            // byte whose trail byte is still in flight.
            Err(_) => {
                let hold = match self.legacy {
                    Some(_) => unpaired_lead_tail_len(&self.tail),
                    None => 0,
                };
                let upto = self.tail.len() - hold;
                let s = decode_legacy(&self.tail[..upto], self.legacy);
                self.tail.drain(..upto);
                s
            }
        }
    }

    /// Release everything still withheld (process exit / pipe EOF).
    pub fn finish(&mut self) -> String {
        if self.tail.is_empty() {
            return String::new();
        }
        // For `StreamDecoder::new` this is exactly `decode_bytes(&tail)`: the
        // injected encoding is the same one `decode_bytes` resolves.
        let s = decode_bytes_with(&self.tail, self.legacy);
        self.tail.clear();
        s
    }
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GBK: Option<&'static Encoding> = Some(encoding_rs::GBK);

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
        // The GBK path itself is covered on every platform by the injectable
        // tests below; here we only check that `decode_bytes` routes to the
        // *platform's* legacy encoding, whatever it is. Asserting "你好" would
        // bake in an assumption this change removes: a Windows whose ANSI code
        // page is 1252 (or UTF-8) must not decode GBK bytes as GBK.
        let out = decode_bytes(&gbk);
        match legacy_encoding() {
            // A legacy code page that is not GBK (e.g. cp1252) maps the two
            // GBK pairs to its own characters, never to 你好.
            Some(enc) if enc != encoding_rs::GBK => {
                let (expected, _, _) = enc.decode(&gbk);
                assert_eq!(out, expected);
            }
            // GBK (Chinese Windows, and the fallback when the code page is
            // unmappable) or no legacy encoding at all (non-Windows: lossy).
            _ => {
                assert!(
                    out == "你好" || out.contains('\u{FFFD}'),
                    "unexpected decode of GBK bytes: {out:?}"
                );
            }
        }
    }

    #[test]
    fn test_decode_gbk_partial_valid_mix() {
        // "ABC你好XYZ" where 你好 is GBK-encoded, rest ASCII.
        // ASCII stays identical; the GBK pair must not corrupt the ASCII.
        let bytes = b"ABC\xC4\xE3\xBA\xC3XYZ";
        let out = decode_bytes(bytes);
        assert!(out.starts_with("ABC"), "ASCII prefix corrupted: {out:?}");
        assert!(out.ends_with("XYZ"), "ASCII suffix corrupted: {out:?}");
        if legacy_encoding().is_some() {
            // A real legacy code page decodes the non-UTF-8 bytes losslessly
            // (你好 under GBK, "ÄãºÃ" under cp1252) — the one thing it must
            // never produce is U+FFFD. The char count differs by code page
            // (GBK is double-byte, cp1252 single-byte), so it is not asserted.
            let middle = &out[3..out.len() - 3];
            assert!(!middle.is_empty(), "middle dropped: {out:?}");
            assert!(!middle.contains('\u{FFFD}'), "middle: {middle:?}");
        }
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
        let _ = decode_bytes_with(&junk, GBK);
    }

    // ---- 按行混合解码（可注入 GBK，Linux 上也真正验证 GBK） ----

    #[test]
    fn test_mixed_utf8_and_gbk_lines() {
        // 复现实机现象：PowerShell 写的 UTF-8 行 + `type` 输出文件的 GBK 行。
        let bytes =
            b"UTF8\xe6\x96\x87\xe4\xbb\xb6\r\nGBK\xce\xc4\xbc\xfe\r\n\xe7\xbb\x93\xe6\x9d\x9f\r\n";
        assert_eq!(
            decode_bytes_with(bytes, GBK),
            "UTF8文件\r\nGBK文件\r\n结束\r\n"
        );
    }

    #[test]
    fn test_pure_gbk_lines_with_legacy() {
        // "GBK文件内容\r\n结束\r\n" 全部 GBK，逐行解码后应还原。
        let bytes = b"GBK\xce\xc4\xbc\xfe\xc4\xda\xc8\xdd\r\n\xbd\xe1\xca\xf8\r\n";
        assert_eq!(decode_bytes_with(bytes, GBK), "GBK文件内容\r\n结束\r\n");
    }

    #[test]
    fn test_pure_utf8_with_legacy_is_identity() {
        // 即使指定了 GBK，纯 UTF-8 也必须原样返回。
        let s = "第一行\n第二行\n没有换行结尾";
        assert_eq!(decode_bytes_with(s.as_bytes(), GBK), s);
    }

    #[test]
    fn test_trailing_incomplete_utf8_keeps_prefix() {
        // 最后一行结尾是半个 UTF-8 字符：合法前缀必须保留，不能被整行当 GBK。
        let mut bytes = b"GBK\xce\xc4\xbc\xfe\r\n".to_vec();
        // 「好」3 字节 + 「中」的前 2 字节：前缀「好」完整，尾部残缺。
        bytes.extend_from_slice(&"好中".as_bytes()[..5]);
        let out = decode_bytes_with(&bytes, GBK);
        assert_eq!(out, "GBK文件\r\n好\u{FFFD}", "prefix must survive: {out:?}");
    }

    #[test]
    fn test_trailing_line_with_no_valid_prefix_is_legacy() {
        // 整行都不像 UTF-8（连合法前缀都没有）：整行走 legacy 解码，
        // 末尾那个孤立的 GBK 前导字节（BA）变成 U+FFFD。
        let bytes = b"GBK\xce\xc4\xbc\xfe\r\n\xc4\xe3\xba";
        assert_eq!(decode_bytes_with(bytes, GBK), "GBK文件\r\n你\u{FFFD}");
    }

    #[test]
    fn test_trailing_line_without_valid_prefix_is_legacy() {
        // 「中」的 UTF-8 被截成前 2 字节（error_len == None 但 valid_up_to == 0）：
        // 没有合法前缀，整行必须走 legacy，而不是按 UTF-8 补一个 U+FFFD。
        let line = &"中".as_bytes()[..2];
        let (expected, _, _) = encoding_rs::GBK.decode(line);
        assert_eq!(decode_bytes_with(line, GBK), expected);
        assert_ne!(
            decode_bytes_with(line, GBK),
            String::from_utf8_lossy(line),
            "must not be decoded as truncated UTF-8"
        );
    }

    #[test]
    fn test_stream_finish_matches_decode_bytes() {
        // finish 必须与 decode_bytes 等价（设计约束）：单测注入的 GBK 与
        // decode_bytes 解析出的平台编码在此都用显式参数对齐。
        for tail in [
            &b"\xc4"[..],
            &b"\xc4\xe3"[..],
            &b"\xe4\xb8"[..],
            &b"AB\xC4"[..],
            &b""[..],
        ] {
            let mut d = StreamDecoder::new_with(GBK);
            let mut all = d.push(tail);
            all.push_str(&d.finish());
            assert_eq!(all, decode_bytes_with(tail, GBK), "tail={tail:?}");
        }
    }

    #[test]
    fn test_newline_terminated_line_is_never_treated_as_truncated() {
        // 完整行（含 \n）末尾不可能"缺字节"：即使前面有非法字节也整行走 GBK。
        let bytes = b"\xc4\xe3\n";
        assert_eq!(decode_bytes_with(bytes, GBK), "你\n");
    }

    #[test]
    fn test_codepage_mapping() {
        assert_eq!(codepage_encoding(936), Some(encoding_rs::GBK));
        assert_eq!(codepage_encoding(65001), None);
        assert_eq!(codepage_encoding(1251), Some(encoding_rs::WINDOWS_1251));
        assert_eq!(codepage_encoding(932), Some(encoding_rs::SHIFT_JIS));
        assert_eq!(codepage_encoding(949), Some(encoding_rs::EUC_KR));
        assert_eq!(codepage_encoding(950), Some(encoding_rs::BIG5));
        assert_eq!(codepage_encoding(874), Some(encoding_rs::WINDOWS_874));
        assert_eq!(codepage_encoding(54936), Some(encoding_rs::GB18030));
        assert_eq!(codepage_encoding(866), Some(encoding_rs::IBM866));
        for cp in 1250..=1258u32 {
            assert!(codepage_encoding(cp).is_some(), "cp{cp} must map");
        }
        assert_eq!(codepage_encoding(437), None, "unmapped → caller falls back");
    }

    #[test]
    fn test_legacy_fallback_rule() {
        assert_eq!(legacy_from_codepages(936, 936), encoding_rs::GBK);
        assert_eq!(legacy_from_codepages(1252, 437), encoding_rs::WINDOWS_1252);
        // ANSI is UTF-8 → the OEM code page is the remaining candidate.
        assert_eq!(legacy_from_codepages(65001, 936), encoding_rs::GBK);
        assert_eq!(
            legacy_from_codepages(65001, 437),
            encoding_rs::GBK,
            "unmapped OEM code page falls back to GBK"
        );
        assert_eq!(
            legacy_from_codepages(65001, 65001),
            encoding_rs::GBK,
            "both UTF-8 → GBK fallback rather than giving up"
        );
        assert_eq!(
            legacy_from_codepages(437, 437),
            encoding_rs::GBK,
            "unmapped ANSI code page falls back to GBK"
        );
    }

    #[test]
    fn test_legacy_encoding_matches_platform() {
        #[cfg(windows)]
        assert!(legacy_encoding().is_some(), "Windows always has a fallback");
        #[cfg(not(windows))]
        assert_eq!(legacy_encoding(), None, "no legacy console encoding");
        // Cached: a second call returns the same value, not a fresh probe.
        assert_eq!(legacy_encoding(), legacy_encoding());
    }

    // ---- StreamDecoder ----

    #[test]
    fn test_stream_utf8_split_at_every_position() {
        // 「你好世界」12 字节，在每个可能的位置切成两块 push。
        let bytes = "你好世界".as_bytes();
        for split in 0..=bytes.len() {
            let mut d = StreamDecoder::new_with(GBK);
            let mut out = d.push(&bytes[..split]);
            out.push_str(&d.push(&bytes[split..]));
            out.push_str(&d.finish());
            assert_eq!(out, "你好世界", "split at byte {split}");
        }
    }

    #[test]
    fn test_stream_gbk_split_at_every_position() {
        // GBK「你好」= C4 E3 BA C3，含"前导字节被单独留在块尾"的情形。
        let gbk: [u8; 4] = [0xC4, 0xE3, 0xBA, 0xC3];
        for split in 0..=gbk.len() {
            let mut d = StreamDecoder::new_with(GBK);
            let mut out = d.push(&gbk[..split]);
            out.push_str(&d.push(&gbk[split..]));
            out.push_str(&d.finish());
            assert_eq!(out, "你好", "GBK split at byte {split}");
        }
    }

    #[test]
    fn test_stream_withholds_unpaired_gbk_lead_byte() {
        // 单独一个 GBK 前导字节：先扣留，等尾字节到了再一起解。
        let mut d = StreamDecoder::new_with(GBK);
        assert_eq!(d.push(&[0xC4]), "", "lead byte alone must be withheld");
        assert_eq!(d.push(&[0xE3]), "你");
        assert_eq!(d.finish(), "");
    }

    #[test]
    fn test_stream_mixed_lines_across_chunks() {
        // 混合多行跨块：UTF-8 行 + GBK 行，切在 GBK 字符与换行之间。
        let bytes = b"UTF8\xe6\x96\x87\xe4\xbb\xb6\r\nGBK\xce\xc4\xbc\xfe\r\n";
        for split in 0..=bytes.len() {
            let mut d = StreamDecoder::new_with(GBK);
            let mut out = d.push(&bytes[..split]);
            // 乱序无关：一次性把剩余字节全部喂进去。
            out.push_str(&d.push(&bytes[split..]));
            out.push_str(&d.finish());
            assert_eq!(out, "UTF8文件\r\nGBK文件\r\n", "split at byte {split}");
        }
    }

    #[test]
    fn test_stream_emits_complete_lines_and_valid_partial_lines() {
        // 完整行立即输出；末尾那段即使没有换行，只要自身是合法 UTF-8 也立即
        // 输出——只有无法判定的残缺尾部才扣留。
        let mut d = StreamDecoder::new_with(GBK);
        assert_eq!(d.push(b"line1\npart"), "line1\npart");
        assert_eq!(d.push(b"ial\n"), "ial\n");
    }

    #[test]
    fn test_stream_finish_emits_withheld_bytes() {
        // 进程在字符中途退出：扣留的字节必须在 finish 时吐出来，不能吞掉。
        let mut d = StreamDecoder::new_with(GBK);
        assert_eq!(d.push(&"中".as_bytes()[..2]), "");
        let flushed = d.finish();
        assert!(!flushed.is_empty(), "withheld bytes must not be dropped");
        assert_eq!(d.finish(), "", "finish is idempotent");
    }

    #[test]
    fn test_stream_does_not_hold_invalid_bytes() {
        // 真正非法的字节不能被当成"跨块字符"无限缓存，必须立刻交给解码器。
        let mut d = StreamDecoder::new_with(GBK);
        let out = d.push(b"AB\xFFCD");
        assert!(
            out.starts_with("AB"),
            "invalid bytes emit immediately: {out:?}"
        );
        assert_eq!(d.finish(), "", "nothing withheld");
        assert_eq!(d.push(b""), "");
    }

    #[test]
    fn test_stream_empty_chunk_is_noop() {
        let mut d = StreamDecoder::new_with(GBK);
        assert_eq!(d.push(b""), "");
        assert_eq!(d.finish(), "");
        assert_eq!(d.push(b"x"), "x");
    }

    #[test]
    fn test_unpaired_lead_scan() {
        assert_eq!(unpaired_lead_tail_len(b"abc"), 0);
        assert_eq!(unpaired_lead_tail_len(b"\xc4"), 1, "lone lead");
        assert_eq!(unpaired_lead_tail_len(b"\xc4\xe3"), 0, "paired");
        assert_eq!(unpaired_lead_tail_len(b"a\xc4\xe3\xba"), 1, "trailing lead");
        assert_eq!(unpaired_lead_tail_len(b""), 0);
    }
}
