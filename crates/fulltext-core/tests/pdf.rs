//! PDF sniffing: header offsets, junk prefixes, staging windows, HTML look-alikes and the recorded fixtures.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;

use fulltext_core::model::RespMeta;
use fulltext_core::pdf::{SNIFF_WINDOW, sniff, verdict};

/// A synthetic PDF: header, `body_len` filler bytes, trailer with `%%EOF`.
fn synthetic_pdf(filler: usize) -> Vec<u8> {
    let mut v = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    v.extend(std::iter::repeat_n(b'x', filler));
    v.extend_from_slice(b"\ntrailer\n<< /Root 1 0 R >>\nstartxref\n0\n%%EOF\n");
    v
}

fn resp(body_len: u64, head_len: Option<u32>) -> RespMeta {
    RespMeta {
        status: 200,
        final_url: "https://example.org/f.pdf".into(),
        body_len,
        head_len,
        ..Default::default()
    }
}

/// `pad` junk bytes (never `%`) followed by `%PDF-1.4`.
fn with_junk(pad: usize) -> Vec<u8> {
    let mut v = vec![b'j'; pad];
    v.extend_from_slice(b"%PDF-1.4\n%%EOF\n");
    v
}

#[test]
fn clean_pdf() {
    let body = synthetic_pdf(100);
    let v = sniff(&body, &body, body.len() as u64);
    assert!(v.is_pdf);
    assert_eq!(v.junk_prefix, 0);
    assert!(v.has_eof);
    assert!(!v.encrypted);
    assert!(!v.looks_html);
    assert_eq!(v.len, body.len() as u64);
}

#[test]
fn bom_before_magic_is_junk_prefix_three() {
    let mut body = b"\xEF\xBB\xBF".to_vec();
    body.extend_from_slice(b"%PDF-1.7\n");
    let v = sniff(&body, &[], 12);
    assert!(v.is_pdf);
    assert_eq!(v.junk_prefix, 3);
    assert!(!v.has_eof);
}

#[test]
fn magic_must_start_inside_the_first_1024_bytes() {
    assert_eq!(SNIFF_WINDOW, 1024);
    let at_1023 = sniff(&with_junk(1023), &[], 0);
    assert!(at_1023.is_pdf, "offset 1023 is inside the window");
    assert_eq!(at_1023.junk_prefix, 1023);
    let at_1024 = sniff(&with_junk(1024), &[], 0);
    assert!(!at_1024.is_pdf, "offset 1024 is outside the window");
    assert_eq!(at_1024.junk_prefix, 0);
    assert!(!sniff(&with_junk(1500), &[], 0).is_pdf);
}

#[test]
fn magic_needs_the_dash() {
    assert!(!sniff(b"%PDF", &[], 4).is_pdf);
    assert!(!sniff(b"%PDF1.7", &[], 7).is_pdf);
    assert!(
        !sniff(b"%pdf-1.7", &[], 8).is_pdf,
        "the magic is case-sensitive"
    );
    assert!(sniff(b"%PDF-", &[], 5).is_pdf);
}

#[test]
fn empty_and_one_byte_input() {
    let empty = sniff(&[], &[], 0);
    assert!(!empty.is_pdf && !empty.has_eof && !empty.encrypted && !empty.looks_html);
    assert_eq!((empty.junk_prefix, empty.len), (0, 0));
    for b in [b'%', b'<', b'{', 0u8, b' '] {
        let v = sniff(&[b], &[b], 1);
        assert!(!v.is_pdf && !v.has_eof && !v.looks_html, "byte {b:#x}");
        assert_eq!(v.len, 1);
    }
    let v = verdict(&resp(0, None), &[]);
    assert!(!v.is_pdf);
    assert_eq!(v.len, 0);
}

#[test]
fn eof_marker_is_searched_in_the_tail_only() {
    assert!(sniff(b"%PDF-1.7", b"..%%EOF\n", 8).has_eof);
    assert!(
        !sniff(b"%PDF-1.7 %%EOF", b"no marker", 20).has_eof,
        "an EOF in the head does not count"
    );
    assert!(!sniff(b"%PDF-1.7", b"%EOF %%EO", 8).has_eof);
}

#[test]
fn encrypt_key_in_head_or_tail() {
    assert!(sniff(b"%PDF-1.7 /Encrypt 5 0 R", b"", 0).encrypted);
    assert!(sniff(b"%PDF-1.7", b"trailer << /Encrypt 5 0 R >>", 0).encrypted);
    assert!(!sniff(b"%PDF-1.7", b"trailer << /Root 1 0 R >>", 0).encrypted);
    assert!(!sniff(b"%PDF-1.7 /Encryp", b"", 0).encrypted);
}

#[test]
fn split_staging_of_a_five_megabyte_file() {
    let total = 5 * 1024 * 1024usize;
    let mut full = synthetic_pdf(total - 100);
    full.truncate(total - 64);
    full.extend_from_slice(&[b'y'; 52]);
    full.extend_from_slice(b"\n%%EOF\n\n\n\n\n");
    let head = &full[..1024];
    let tail = &full[full.len() - 1024..];
    let mut staged = head.to_vec();
    staged.extend_from_slice(tail);

    let v = verdict(&resp(full.len() as u64, Some(1024)), &staged);
    assert!(v.is_pdf && v.has_eof && !v.looks_html);
    assert_eq!(v.junk_prefix, 0);
    assert_eq!(
        v.len,
        full.len() as u64,
        "len is the full length, never the staged length"
    );
    assert_ne!(v.len, staged.len() as u64);

    // Same answer through sniff directly.
    assert_eq!(v, sniff(head, tail, full.len() as u64));

    // A truncated download: the tail has no trailer.
    let mut cut = head.to_vec();
    cut.extend_from_slice(&[b'z'; 1024]);
    let v = verdict(&resp(full.len() as u64, Some(1024)), &cut);
    assert!(v.is_pdf && !v.has_eof);
}

#[test]
fn head_len_is_clamped_to_the_body() {
    let body = synthetic_pdf(10);
    // head_len larger than the body: everything is the head, the tail falls back to its last window.
    let v = verdict(&resp(body.len() as u64, Some(100_000)), &body);
    assert!(v.is_pdf && v.has_eof);
    assert_eq!(v.len, body.len() as u64);
    // head_len == 0: the whole body is the tail, nothing can be the header.
    let v = verdict(&resp(body.len() as u64, Some(0)), &body);
    assert!(!v.is_pdf && v.has_eof);
}

#[test]
fn unsplit_body_searches_eof_in_the_last_window_only() {
    let mut body = synthetic_pdf(0);
    body.extend(std::iter::repeat_n(b'q', 3000));
    let v = verdict(&resp(0, None), &body);
    assert!(v.is_pdf);
    assert!(
        !v.has_eof,
        "the %%EOF sits more than 1024 bytes from the end"
    );
    assert_eq!(
        v.len,
        body.len() as u64,
        "body_len 0 falls back to the staged length"
    );

    let whole = synthetic_pdf(200);
    let v = verdict(&resp(whole.len() as u64, None), &whole);
    assert!(v.is_pdf && v.has_eof);
}

#[test]
fn len_never_drops_below_the_staged_length() {
    let body = synthetic_pdf(50);
    assert_eq!(verdict(&resp(1, None), &body).len, body.len() as u64);
    assert_eq!(verdict(&resp(9_999_999, None), &body).len, 9_999_999);
}

#[test]
fn looks_html_start_tags() {
    for html in [
        &b"<!doctype html><html></html>"[..],
        b"<!DOCTYPE HTML PUBLIC \"-//W3C//DTD HTML 4.01//EN\">",
        b"<HTML><HEAD>",
        b"<html lang=en>",
        b"<head><title>x</title>",
        b"<body onload=x>",
        b"\xEF\xBB\xBF<!doctype html>",
        b"  \r\n\t <html>",
        b"\xEF\xBB\xBF \n<!DocType Html>",
        b"<?xml version=\"1.0\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\">",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\" \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd\">\n<html>",
        b"<!-- saved from url=(0014)about:internet -->\n<!doctype html>",
        b"<!--a--><!--b-->\n<html>",
    ] {
        let v = sniff(html, html, html.len() as u64);
        assert!(v.looks_html, "{}", String::from_utf8_lossy(html));
        assert!(!v.is_pdf);
    }
}

#[test]
fn looks_html_rejects_other_bodies() {
    for other in [
        &b"{\"is_oa\": true}"[..],
        b"[1,2,3]",
        b"Resource not found.",
        b"plain text <html> later",
        b"<?xml version='1.0'?><feed xmlns='http://www.w3.org/2005/Atom'></feed>",
        b"<?xml version='1.0'?><article><body>jats</body></article>",
        b"<div>fragment</div>",
        b"<!-- never closed <html>",
        b"%PDF-1.7\n<html>",
        b"\x00\x01\x02",
        b"",
    ] {
        assert!(
            !sniff(other, other, other.len() as u64).looks_html,
            "{}",
            String::from_utf8_lossy(other)
        );
    }
}

#[test]
fn a_pdf_never_looks_like_html() {
    let body = synthetic_pdf(10);
    assert!(!sniff(&body, &body, 0).looks_html);
    // Even a PDF that embeds HTML text after the header.
    let mut tricky = b"%PDF-1.4\n<html><body>".to_vec();
    tricky.extend_from_slice(&body);
    let v = sniff(&tricky, &tricky, 0);
    assert!(v.is_pdf && !v.looks_html);
}

#[test]
fn looks_html_and_is_pdf_are_independent() {
    // Junk that is itself HTML, then a PDF header inside the window: both facts are reported.
    let both = b"<html>\n<!-- x -->%PDF-1.4";
    let v = sniff(both, both, 0);
    assert!(v.looks_html && v.is_pdf);
    assert_eq!(v.junk_prefix, 17);
}

#[test]
fn every_html_fixture_looks_html_and_is_not_a_pdf() {
    let mut count = 0;
    for e in common::html_expected() {
        let file = e["file"].as_str().unwrap();
        let (resp, body) = common::html_fixture(file);
        let v = verdict(&resp, &body);
        assert!(v.looks_html, "{file}");
        assert!(!v.is_pdf, "{file}");
        assert_eq!(v.len, body.len() as u64, "{file}");
        count += 1;
    }
    assert!(count >= 30);
}

#[test]
fn bot_wall_pages_in_the_oa_fixtures_look_html() {
    for file in [
        "pmc-web__pdf-pow-interstitial.html",
        "europepmc-web__pdf-render-cloudflare-challenge.html",
        "medrxiv-web__pdf-403-block.html",
    ] {
        let (resp, body) = common::html_fixture(file);
        let v = verdict(&resp, &body);
        assert!(v.looks_html && !v.is_pdf, "{file}");
    }
}

#[test]
fn json_text_and_html_error_fixtures_are_classified_by_shape() {
    for e in common::oa_manifest() {
        let file = e["file"].as_str().unwrap();
        let (resp, body) = common::oa_fixture(file);
        let v = verdict(&resp, &body);
        assert!(!v.is_pdf, "{file}");
        let is_html_file = file.ends_with(".html");
        if is_html_file {
            assert!(v.looks_html, "{file}");
        } else {
            // .json, .xml (Atom, JATS, S3 listings) and .txt bodies are never HTML.
            assert!(!v.looks_html, "{file}");
        }
    }
}

#[test]
fn staged_html_under_a_binary_head_len_is_still_html() {
    // A tiny HTML error page fetched as a "PDF": JS stages it as head ++ tail with head_len set.
    let page = b"<!doctype html><title>Access denied</title><p>no</p>".to_vec();
    let v = verdict(&resp(page.len() as u64, Some(page.len() as u32)), &page);
    assert!(v.looks_html && !v.is_pdf);
}
