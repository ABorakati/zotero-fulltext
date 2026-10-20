#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fulltext_core::abi::respond_bytes;
use fulltext_core::logic::*;

#[test]
fn doi_normalisation() {
    let d = |s: &str| normalize_doi(DoiIn { text: s.into() }).unwrap();
    assert_eq!(d("https://doi.org/10.1371/journal.pmed.1000097"), Some("10.1371/journal.pmed.1000097".into()));
    assert_eq!(d("doi:10.1186/1741-7015-8-18."), Some("10.1186/1741-7015-8-18".into()));
    assert_eq!(d("https://doi.org/10.1002%2F%28SICI%291097-4636"), Some("10.1002/(sici)1097-4636".into()));
    assert_eq!(d("no doi here"), None);
}

#[test]
fn ezproxy_hostname_mode() {
    let r = proxy_url(ProxyIn::Hostname {
        url: "https://www.nature.com/articles/s41586-020-2649-2".into(),
        suffix: "kcl.idm.oclc.org".into(),
    })
    .unwrap();
    assert_eq!(r, "https://www-nature-com.kcl.idm.oclc.org/articles/s41586-020-2649-2");
    let r = proxy_url(ProxyIn::Hostname { url: "https://onlinelibrary.wiley-x.com/a".into(), suffix: ".p.example".into() })
        .unwrap();
    assert_eq!(r, "https://onlinelibrary-wiley--x-com.p.example/a");
}

#[test]
fn unpaywall_ranking_prefers_published_pdf() {
    let j = r#"{"is_oa":true,
      "best_oa_location":{"url":"https://x/landing","url_for_pdf":null,"host_type":"repository","version":"acceptedVersion"},
      "oa_locations":[{"url_for_pdf":"https://pub/x.pdf","url":"https://pub/x","host_type":"publisher","version":"publishedVersion"}]}"#;
    let input: UnpaywallIn = serde_json::from_str(j).unwrap();
    let c = rank_unpaywall(input).unwrap();
    assert_eq!(c[0].url, "https://pub/x.pdf");
}

#[test]
fn envelope_shape() {
    let ok = String::from_utf8(respond_bytes::<u32>(Ok(7))).unwrap();
    assert_eq!(ok, r#"{"ok":7}"#);
    let e = String::from_utf8(respond_bytes::<u32>(Err(fulltext_core::abi::err("bad_url", "x")))).unwrap();
    assert_eq!(e, r#"{"err":{"code":"bad_url","message":"x"}}"#);
}

#[test]
fn pdf_sniff() {
    assert!(looks_like_pdf(b"%PDF-1.7\n..."));
    assert!(looks_like_pdf(b"\xef\xbb\xbf\r\n%PDF-1.4"));
    assert!(!looks_like_pdf(b"<!DOCTYPE html><html>"));
}

#[test]
fn landing_page_candidates_messy() {
    let html = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/messy.html")).unwrap();
    let out = landing_pdf_candidates(LandingIn { html, url: "https://example.org/a/123".into(), raw: false }).unwrap();
    let urls: Vec<_> = out.candidates.iter().map(|c| c.url.as_str()).collect();
    assert_eq!(urls[0], "https://example.org/pdf/123.pdf?download=1&x=2");
    assert!(urls.contains(&"https://example.org/articles/relative/full.pdf")); // <base href> honoured
    assert!(!urls.iter().any(|u| u.starts_with("javascript:")));
}
