//! PDF byte sniffing. Owned by core-pdf-urls-inspect (spec: `docs/wp/core-pdf-urls-inspect.md`).
//!
//! The host never hands the core a whole binary body: it stages `head ++ tail` (see [`verdict`]) so a 40 MB PDF costs
//! a few KB of wasm memory. Everything here is therefore a byte-pattern test on those two windows.
use crate::model::{PdfVerdict, RespMeta};
use crate::text;

/// How far into the body `%PDF-` may start for our own save path (Zotero itself needs offset 0, so
/// `PdfVerdict.junk_prefix > 0` makes the engine ask JS to trim before handing the file to Zotero).
pub const SNIFF_WINDOW: usize = 1024;

/// The PDF header magic. Also used by [`crate::inspect`] to refuse a PDF where an API body was expected.
pub const MAGIC: &[u8] = b"%PDF-";

const EOF_MARKER: &[u8] = b"%%EOF";
const ENCRYPT_KEY: &[u8] = b"/Encrypt";
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// Bytes (after BOM, whitespace and leading comments) compared against the HTML start tags.
const HTML_WINDOW: usize = 256;
/// Leading `<!-- ... -->` comments skipped before the HTML test (saved-page banners, fixture notes).
const MAX_LEADING_COMMENTS: usize = 4;

/// Sniff a PDF from the first and last bytes of a body.
///
/// * `is_pdf`: `%PDF-` starts within the first [`SNIFF_WINDOW`] bytes of `head`; `junk_prefix` is its offset (0 = clean).
/// * `has_eof`: `%%EOF` present anywhere in `tail` (callers stage the last 1024 bytes or more).
/// * `encrypted`: a `/Encrypt` key in `head` or `tail` (informational; encrypted PDFs still save).
/// * `looks_html`: after an optional BOM, whitespace and leading comments the body starts with `<!doctype html`,
///   `<html`, `<head`, `<body`, or `<?xml` followed by `<html` (case-insensitive, first 256 bytes). Independent of
///   `is_pdf`; a PDF never matches because it starts with `%`.
/// * `len`: the full body length (`total_len`), never the staged length.
pub fn sniff(head: &[u8], tail: &[u8], total_len: u64) -> PdfVerdict {
    let offset = pdf_offset(head);
    PdfVerdict {
        is_pdf: offset.is_some(),
        junk_prefix: offset.and_then(|o| u32::try_from(o).ok()).unwrap_or(0),
        has_eof: find(tail, EOF_MARKER).is_some(),
        encrypted: find(head, ENCRYPT_KEY).is_some() || find(tail, ENCRYPT_KEY).is_some(),
        looks_html: looks_html(head),
        len: total_len,
    }
}

/// Verdict for a staged response body.
///
/// * `resp.head_len == Some(n)`: the body is `head(n bytes) ++ tail` (binary staging). `n` is clamped to the body; when
///   nothing is left for the tail the whole body was staged as the head and its last [`SNIFF_WINDOW`] bytes serve as
///   the tail.
/// * otherwise `body` is the whole (possibly 2 MiB-truncated) body: it is the head, and its last [`SNIFF_WINDOW`] bytes
///   are the tail, so `%%EOF` is only found near the end.
///
/// `len` is `resp.body_len`, raised to the staged length when the host reported 0 (or less than it staged).
pub fn verdict(resp: &RespMeta, body: &[u8]) -> PdfVerdict {
    let staged = u64::try_from(body.len()).unwrap_or(u64::MAX);
    let len = resp.body_len.max(staged);
    match resp.head_len {
        Some(n) => {
            let n = usize::try_from(n).map_or(body.len(), |n| n.min(body.len()));
            let (head, tail) = body.split_at_checked(n).unwrap_or((body, &[]));
            let tail = if tail.is_empty() {
                text::tail(head, SNIFF_WINDOW)
            } else {
                tail
            };
            sniff(head, tail, len)
        }
        None => sniff(body, text::tail(body, SNIFF_WINDOW), len),
    }
}

/// Offset of `%PDF-` when it starts inside the first [`SNIFF_WINDOW`] bytes.
fn pdf_offset(head: &[u8]) -> Option<usize> {
    head.windows(MAGIC.len())
        .take(SNIFF_WINDOW)
        .position(|w| w == MAGIC)
}

/// First occurrence of `needle` in `haystack`. Scans for the first needle byte, so it is fast on large heads.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    let mut from = 0;
    while let Some(step) = haystack.get(from..)?.iter().position(|&b| b == first) {
        let at = from + step;
        if haystack.get(at + 1..).is_some_and(|t| t.starts_with(rest)) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

fn looks_html(head: &[u8]) -> bool {
    let mut rest = head.strip_prefix(UTF8_BOM).unwrap_or(head);
    rest = text::head(rest, SNIFF_WINDOW + HTML_WINDOW);
    for _ in 0..MAX_LEADING_COMMENTS {
        rest = rest.trim_ascii_start();
        let Some(after_open) = rest.strip_prefix(b"<!--") else {
            break;
        };
        // An unterminated comment inside the window is not a document we can call HTML.
        let Some(end) = find(after_open, b"-->") else {
            return false;
        };
        rest = after_open.get(end + 3..).unwrap_or_default();
    }
    let window = text::head(rest.trim_ascii_start(), HTML_WINDOW).to_ascii_lowercase();
    if [&b"<!doctype html"[..], b"<html", b"<head", b"<body"]
        .iter()
        .any(|tag| window.starts_with(tag))
    {
        return true;
    }
    window.starts_with(b"<?xml") && find(&window, b"<html").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_handles_edges() {
        assert_eq!(find(b"", b"a"), None);
        assert_eq!(find(b"abc", b""), None);
        assert_eq!(find(b"abc", b"abcd"), None);
        assert_eq!(find(b"xxabcxx", b"abc"), Some(2));
        assert_eq!(find(b"aab", b"ab"), Some(1));
        assert_eq!(find(b"ab", b"b"), Some(1));
    }

    #[test]
    fn clean_pdf_header() {
        let v = sniff(b"%PDF-1.7\n", b"%%EOF\n", 9);
        assert!(v.is_pdf && v.has_eof && !v.looks_html && v.junk_prefix == 0 && v.len == 9);
    }
}
