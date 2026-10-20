//! Tiny byte/text helpers shared by the parsers. Owned by core-pdf-urls-inspect; signatures are frozen.
use std::borrow::Cow;

/// Largest text body the host stages for inspection (matches `RespMeta::truncated` at 2 MiB).
pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

/// Decode a response body as UTF-8, replacing invalid sequences, after dropping a UTF-8 byte-order mark.
/// Charset sniffing (`<meta charset>`, `Content-Type; charset=`) is deliberately not done: every target API and
/// publisher page we parse is UTF-8, and invalid bytes only affect text the heuristics ignore.
pub fn decode(body: &[u8]) -> Cow<'_, str> {
    let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    String::from_utf8_lossy(body)
}

/// The first `n` bytes of `body` (all of it when shorter).
pub fn head(body: &[u8], n: usize) -> &[u8] {
    body.get(..n).unwrap_or(body)
}

/// The last `n` bytes of `body` (all of it when shorter).
pub fn tail(body: &[u8], n: usize) -> &[u8] {
    body.len()
        .checked_sub(n)
        .and_then(|start| body.get(start..))
        .unwrap_or(body)
}

/// ASCII case-insensitive substring test (`needle` must already be lower-case).
pub fn contains_ci(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    haystack.to_ascii_lowercase().contains(needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_strips_bom_and_survives_garbage() {
        assert_eq!(decode(b"\xEF\xBB\xBFabc"), "abc");
        assert!(decode(&[0xff, 0xfe, b'a']).contains('a'));
    }

    #[test]
    fn head_and_tail_clamp() {
        assert_eq!(head(b"abcdef", 3), b"abc");
        assert_eq!(head(b"ab", 9), b"ab");
        assert_eq!(tail(b"abcdef", 2), b"ef");
        assert_eq!(tail(b"ab", 9), b"ab");
    }

    #[test]
    fn head_and_tail_zero_and_empty() {
        assert_eq!(head(b"abc", 0), b"");
        assert_eq!(tail(b"abc", 0), b"");
        assert_eq!(head(b"", 4), b"");
        assert_eq!(tail(b"", 4), b"");
        assert_eq!(tail(b"abc", usize::MAX), b"abc");
    }

    #[test]
    fn decode_keeps_valid_utf8_and_replaces_invalid_bytes() {
        assert_eq!(decode("caf\u{e9}".as_bytes()), "caf\u{e9}");
        assert_eq!(decode(b"a\xFFb"), "a\u{FFFD}b");
        assert_eq!(decode(b""), "");
        assert_eq!(decode(b"\xEF\xBB\xBF"), "", "a lone BOM decodes to nothing");
        // Only a leading BOM is dropped.
        assert_eq!(decode("a\u{feff}b".as_bytes()), "a\u{feff}b");
    }

    #[test]
    fn contains_ci_needs_a_lower_case_needle() {
        assert!(contains_ci("Just a Moment...", "just a moment"));
        assert!(contains_ci("anything", ""));
        assert!(!contains_ci("abc", "abd"));
        assert!(
            !contains_ci("abc", "ABC"),
            "the needle must already be lower-case"
        );
    }

    #[test]
    fn max_text_bytes_is_two_mebibytes() {
        assert_eq!(MAX_TEXT_BYTES, 2 * 1024 * 1024);
    }
}
