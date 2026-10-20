//! Correctness check of every compiled-in HTML backend against a hand-written
//! messy page. Run: cargo test -p fulltext-core --features tl,lolhtml,html5gum,scraper
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fulltext_core::html::{Anchor, Extracted, Link, Meta};

fn expected() -> Extracted {
    let m = |n: &str, c: &str| Meta { name: n.into(), content: c.into() };
    let a = |h: &str, t: &str| Anchor { href: h.into(), text: t.into() };
    Extracted {
        metas: vec![
            m("citation_title", "Messy & Real"),
            m("citation_pdf_url", "https://example.org/pdf/123.pdf?download=1&x=2"),
            m("citation_doi", "10.1234/abc.def"),
            m("og:url", "https://example.org/a/123"),
        ],
        links: vec![
            Link { rel: "alternate".into(), r#type: "application/pdf".into(), href: "/pdf/123.pdf".into() },
            Link { rel: "Alternate".into(), r#type: "application/pdf".into(), href: "../other.pdf".into() },
        ],
        anchors: vec![
            a("/doi/pdf/10.1234/abc.def", "Download PDF (1.2 MB)"),
            a("relative/full.pdf", "Full text"),
            a("javascript:void(0)", "PDF"),
            a("https://example.org/epdf/123", "View ePDF"),
            a("/caf\u{e9}.pdf", "caf\u{e9} \u{2014} PDF"),
        ],
        iframes: vec!["/pdfviewer?file=%2Fpdf%2F123.pdf".into(), "https://cdn.example.org/x.pdf#view=fit".into()],
        base_href: Some("https://example.org/articles/".into()),
    }
}

fn html() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/messy.html")).unwrap()
}

/// Returns a list of human-readable mismatches (empty = perfect).
fn diff(got: &Extracted) -> Vec<String> {
    let exp = expected();
    let mut d = Vec::new();
    for m in &exp.metas {
        if !got.metas.contains(m) {
            d.push(format!("missing meta {m:?}"));
        }
    }
    for m in &got.metas {
        if !exp.metas.contains(m) {
            d.push(format!("extra meta {m:?}"));
        }
    }
    for l in &exp.links {
        if !got.links.iter().any(|g| g.href == l.href && g.r#type.eq_ignore_ascii_case(&l.r#type)) {
            d.push(format!("missing link {l:?}"));
        }
    }
    for a in &exp.anchors {
        if !got.anchors.contains(a) {
            d.push(format!("missing/garbled anchor {a:?}"));
        }
    }
    for a in &got.anchors {
        if !exp.anchors.iter().any(|e| e.href == a.href) {
            d.push(format!("extra anchor {a:?}"));
        }
    }
    if got.iframes != exp.iframes {
        d.push(format!("iframes {:?}", got.iframes));
    }
    if got.base_href != exp.base_href {
        d.push(format!("base {:?}", got.base_href));
    }
    d
}

fn report(name: &str, got: &Extracted) -> usize {
    let d = diff(got);
    println!("[{name}] {} mismatches", d.len());
    for x in &d {
        println!("    {x}");
    }
    d.len()
}

#[cfg(feature = "hand")]
#[test]
fn messy_hand() {
    assert_eq!(report("hand", &fulltext_core::html::hand::extract(&html())), 0);
}

#[cfg(feature = "html5gum")]
#[test]
fn messy_html5gum() {
    assert_eq!(report("html5gum", &fulltext_core::html::gum::extract(&html())), 0);
}

#[cfg(feature = "scraper")]
#[test]
fn messy_scraper() {
    assert_eq!(report("scraper", &fulltext_core::html::scrap::extract(&html())), 0);
}

// tl and lol_html are expected to have known gaps; report but do not fail.
#[cfg(feature = "tl")]
#[test]
fn messy_tl() {
    report("tl", &fulltext_core::html::tl_impl::extract(&html()));
}

#[cfg(feature = "lolhtml")]
#[test]
fn messy_lolhtml() {
    report("lol_html", &fulltext_core::html::lol::extract(&html()));
}
