//! Every recorded HTML fixture through the real page analysis (`docs/wp/core-page.md` section 6). The oracle is
//! `fixtures/html/expected.json`; 16 of the 31 pages come from the Wayback Machine, so the assertions are structural
//! (which URL wins, which are refused, which rule and access hint apply) rather than byte-for-byte.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;

use fulltext_core::model::{
    AccessHint, CandKind, Candidate, Ids, PageClass, PageInspection, RespMeta, Route, Source,
    Vendor,
};
use fulltext_core::page::{classify_response, inspect_page};
use fulltext_core::urls::canonical_key;
use serde_json::Value;

fn inspect_with(file: &str, requested: &Ids) -> PageInspection {
    let (resp, body) = common::html_fixture(file);
    inspect_page(&resp, requested, Route::Oa, &body)
}

fn inspect(file: &str) -> PageInspection {
    inspect_with(file, &Ids::default())
}

fn class_of(file: &str) -> PageClass {
    let (resp, body) = common::html_fixture(file);
    classify_response(&resp, &body)
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn entries(kind: &str) -> Vec<Value> {
    common::html_expected()
        .into_iter()
        .filter(|e| e["kind"] == kind)
        .collect()
}

fn urls(pi: &PageInspection) -> Vec<&str> {
    pi.candidates.iter().map(|c| c.url.as_str()).collect()
}

fn first(pi: &PageInspection) -> &Candidate {
    pi.candidates.first().unwrap_or_else(|| {
        panic!(
            "no candidates; class {:?}, rejected {:?}",
            pi.class, pi.rejected
        )
    })
}

// ------------------------------------------------------------------ (a) landing pages

#[test]
fn the_oracle_is_well_formed() {
    let all = common::html_expected();
    assert!(all.len() >= 30);
    for e in &all {
        assert!(
            matches!(
                e["kind"].as_str(),
                Some("landing" | "challenge" | "blocked" | "redirect")
            ),
            "{e}"
        );
    }
    for kind in ["landing", "challenge", "blocked", "redirect"] {
        assert!(!entries(kind).is_empty(), "no {kind} entries");
    }
}

#[test]
fn landing_pages_select_the_expected_pdf_and_never_a_forbidden_one() {
    let mut checked = 0;
    for e in entries("landing") {
        let Some(want) = e["expected_pdf_url"].as_str() else {
            continue;
        };
        let file = e["file"].as_str().unwrap();
        let alternates = strings(&e, "alternates");
        let forbidden = strings(&e, "must_not_select");

        let anonymous = inspect(file);
        let page_doi = anonymous.page_ids.doi.clone();
        // Once with nothing known about the item, once with the page's own DOI (and once with a different one).
        let known = Ids {
            doi: page_doi.clone(),
            ..Default::default()
        };
        let other = Ids {
            doi: Some("10.9999/not-this-one".into()),
            ..Default::default()
        };
        for (label, requested) in [
            ("no ids", Ids::default()),
            ("page doi", known),
            ("other doi", other),
        ] {
            let pi = inspect_with(file, &requested);
            assert!(
                matches!(pi.class, PageClass::Landing | PageClass::Html),
                "{file} [{label}]: {:?}",
                pi.class
            );
            let top = first(&pi);
            assert!(
                top.url == want || alternates.contains(&top.url),
                "{file} [{label}]: picked {} (want {want} or {alternates:?}); all: {:?}",
                top.url,
                urls(&pi)
            );
            for c in &pi.candidates {
                for bad in &forbidden {
                    assert!(
                        c.url != *bad && canonical_key(&c.url) != canonical_key(bad),
                        "{file} [{label}]: forbidden URL selected: {bad}"
                    );
                }
            }
        }
        checked += 1;
    }
    // 23 landing entries; Europe PMC (the SPA shell) has no expected PDF and is covered by its own test.
    assert_eq!(checked, 22, "landing pages with an expected PDF");
}

#[test]
fn candidates_are_well_formed_sorted_and_unique() {
    for e in entries("landing") {
        let file = e["file"].as_str().unwrap();
        let pi = inspect(file);
        let mut seen = std::collections::BTreeSet::new();
        let mut last = i32::MAX;
        for c in &pi.candidates {
            assert_eq!(c.kind, CandKind::Pdf, "{file}");
            assert_eq!(c.source, Source::Extractor, "{file}");
            assert!(c.url.starts_with("https://"), "{file}: {}", c.url);
            assert!(!c.url.contains('#'), "{file}: fragment kept in {}", c.url);
            assert!(c.signal.as_deref().is_some_and(|s| !s.is_empty()), "{file}");
            assert!(c.score > 0 && c.score <= 100, "{file}: {}", c.score);
            assert!(
                c.score <= last,
                "{file}: not sorted best-first: {:?}",
                pi.candidates
                    .iter()
                    .map(|c| (c.score, &c.url))
                    .collect::<Vec<_>>()
            );
            last = c.score;
            assert!(
                seen.insert(canonical_key(&c.url)),
                "{file}: duplicate {}",
                c.url
            );
            assert!(c.landing_url.is_some(), "{file}");
            assert_eq!(
                c.record_doi, pi.page_ids.doi,
                "{file}: record_doi is the page DOI"
            );
        }
    }
}

/// `(file, url, why)`: refusals the HTML makes unambiguous. Each URL is in the page's HTML and in the oracle's
/// `must_not_select`; the `why` strings are the stable vocabulary of `Extraction.rejected`.
const REFUSALS: &[(&str, &str, &str)] = &[
    (
        "plos__article-oa.html",
        "https://journals.plos.org/plosmedicine/article/file?type=printable&id=10.1371/journal.pmed.1004449",
        "other_doi",
    ),
    (
        "plos__article-oa.html",
        "https://journals.plos.org/plosmedicine/article/file?id=10.1371/journal.pmed.1004377&type=manuscript",
        "other_format",
    ),
    (
        "plos__article-oa.html",
        "https://www.ema.europa.eu/en/documents/variation-report/eliquis-h-c-2148-x-0004-g-epar-assessment-report-extension_en.pdf",
        "off_host",
    ),
    (
        "biomedcentral__redirected-to-springerlink-oa.html",
        "https://citation-needed.springer.com/v2/references/10.1186/s12893-023-02300-3?format=refman&flavour=citation",
        "citation_export",
    ),
    (
        "springer__article-paywalled.html",
        "https://media.springernature.com/original/springer-static/esm/art%3A10.1007%2Fs00464-025-11552-4/MediaObjects/464_2025_11552_MOESM1_ESM.doc",
        "supplement",
    ),
    (
        "nature__scirep-oa.html",
        "https://media.springernature.com/original/springer-static/esm/art%3A10.1038%2Fs41598-024-73565-8/MediaObjects/41598_2024_73565_MOESM1_ESM.pdf",
        "supplement",
    ),
    (
        "frontiers__article-oa.html",
        "https://public-pages-files-2025.frontiersin.org/journals/surgery/articles/10.3389/fsurg.2025.1537318/bibTex",
        "citation_export",
    ),
    (
        "pmc__article-oa.html",
        "http://www.cihr-irsc.gc.ca/e/documents/rct_reg_e.pdf",
        "off_host",
    ),
    (
        "wiley__wjs-article.html",
        "https://onlinelibrary.wiley.com/doi/epdf/10.1002/wjs.12026",
        "reader_shell",
    ),
    (
        "tandf__article-no-citation-pdf-url.html",
        "https://www.tandfonline.com/doi/epdf/10.1080/07853890.2016.1186828?needAccess=true",
        "reader_shell",
    ),
    (
        "sage__article-ereader-only.html",
        "https://journals.sagepub.com/doi/reader/10.1177/2050312118807624",
        "reader_shell",
    ),
    (
        "sage__article-ereader-only.html",
        "http://www.apa.org/pi/women/programs/abortion/mental-health.pdf",
        "off_host",
    ),
    (
        "karger__article.html",
        "https://karger.com/Citation/Download?resourceId=87289&resourceType=3&citationFormat=0",
        "citation_export",
    ),
    (
        "thieme__article-html.html",
        "https://thomas-hochstein.de/download/rechtliche-aspekte-transportverweigerung.pdf",
        "off_host",
    ),
    (
        "thieme__article-html.html",
        "https://www.thieme-connect.com/products/ejournals/ris/10.1055/a-0495-1582/BIB",
        "citation_export",
    ),
    (
        "bmj__article-oa.html",
        "https://www.bmj.com/highwire/citation/1045551/ris",
        "citation_export",
    ),
    (
        "nejm__article-free.html",
        "https://www.nejm.org/doi/suppl/10.1056/NEJMoa2002032/suppl_file/nejmoa2002032_protocol.pdf",
        "supplement",
    ),
    (
        "nejm__article-free.html",
        "https://www.nejm.org/doi/suppl/10.1056/NEJMoa2002032/suppl_file/nejmoa2002032_appendix.pdf",
        "supplement",
    ),
    (
        "nejm__article-free.html",
        "https://www.nejm.org/doi/suppl/10.1056/NEJMoa2002032/suppl_file/nejmoa2002032_disclosures.pdf",
        "supplement",
    ),
    (
        "lancet__article-free.html",
        "https://www.thelancet.com/servlet/linkout?suffix=e_1_5_1_2_3_2&dbid=4&doi=10.1016%2FS0140-6736%2820%2930183-5&key=10.1016%2FS0140-6736%2803%2913967-0&cf=pdf&site=lancet-site",
        "other_doi",
    ),
    (
        "lancet__article-free.html",
        "https://www.thelancet.com/cms/10.1016/S0140-6736(20)30183-5/attachment/d5332ca1-83d8-4c4c-bc57-00a390bf0396/mmc1.pdf",
        "supplement",
    ),
    (
        "mdpi__article-oa.html",
        "https://www.mdpi.com/2077-0383/9/4/1225/pdf-with-cover",
        "captcha_variant",
    ),
    (
        "mdpi__article-oa.html",
        "https://www.mdpi.com/2077-0383/9/4/1225/epub",
        "other_format",
    ),
    (
        "lww__annals-of-surgery-article.html",
        "https://cdn-tp2.mozu.com/16833-m1/cms/files/Author-Document.pdf?_mzts=636519775710000000",
        "off_host",
    ),
];

#[test]
fn refused_urls_are_reported_with_stable_reasons() {
    for (file, url, why) in REFUSALS {
        let pi = inspect(file);
        assert!(
            pi.rejected.iter().any(|r| r.url == *url && r.why == *why),
            "{file}: expected rejected ({url}, {why}); got {:#?}",
            pi.rejected
        );
        assert!(
            pi.candidates
                .iter()
                .all(|c| c.url != *url && canonical_key(&c.url) != canonical_key(url)),
            "{file}: {url} became a candidate"
        );
    }
    // Every `why` in use is part of the documented vocabulary.
    let vocabulary = [
        "other_doi",
        "supplement",
        "citation_export",
        "reader_shell",
        "non_http",
        "image",
        "captcha_variant",
        "javascript_href",
        "other_format",
        "reference_link",
        "reference_list",
        "off_host",
    ];
    for e in entries("landing") {
        for r in inspect(e["file"].as_str().unwrap()).rejected {
            assert!(
                vocabulary.contains(&r.why.as_str()),
                "{}: unknown why {:?}",
                e["file"],
                r
            );
        }
    }
}

#[test]
fn the_cell_brochure_and_other_weak_pdf_links_are_not_selected() {
    // Same site, ends in .pdf, but nothing else says "this article": not a candidate (and not worth a rejection).
    let pi = inspect("cell__article-free.html");
    assert!(
        pi.candidates
            .iter()
            .all(|c| !c.url.contains("CP-RecruitmentKit")),
        "{:?}",
        urls(&pi)
    );
    let pi = inspect("lww__annals-of-surgery-article.html");
    assert!(
        pi.candidates
            .iter()
            .all(|c| !c.url.contains("JavaScript.html")),
        "the <noscript> refresh target is not a PDF"
    );
}

// ------------------------------------------------------------------ publisher-specific behaviour

#[test]
fn rule_ids_by_publisher() {
    let want = [
        ("plos__article-oa.html", "open-misc"),
        (
            "biomedcentral__redirected-to-springerlink-oa.html",
            "springer-nature",
        ),
        ("springer__article-paywalled.html", "springer-nature"),
        ("nature__scirep-oa.html", "springer-nature"),
        ("frontiers__article-oa.html", "open-misc"),
        ("cambridge__article-oa.html", "open-misc"),
        ("pmc__article-oa.html", "walled-misc"),
        ("oup__bjs-article-abstract.html", "silverchair"),
        ("wiley__wjs-article.html", "wiley"),
        ("tandf__article-no-citation-pdf-url.html", "atypon"),
        ("sage__article-ereader-only.html", "atypon"),
        ("karger__article.html", "silverchair"),
        ("thieme__article-html.html", "walled-misc"),
        ("jama__article-free.html", "silverchair"),
        ("bmj__article-oa.html", "highwire"),
        ("nejm__article-free.html", "atypon"),
        (
            "sciencedirect__article-preloaded-state.html",
            "sciencedirect",
        ),
        ("lancet__article-free.html", "elsevier-health"),
        ("cell__article-free.html", "elsevier-health"),
        ("mdpi__article-oa.html", "walled-misc"),
        ("lww__annals-of-surgery-article.html", "lww"),
        ("lww__ijs-article-oa.html", "lww"),
    ];
    for (file, rule) in want {
        let pi = inspect(file);
        assert_eq!(pi.rule.as_deref(), Some(rule), "{file}");
        assert!(
            first(&pi)
                .signal
                .as_deref()
                .unwrap()
                .starts_with(&format!("rule:{rule}:")),
            "{file}: {:?}",
            first(&pi).signal
        );
    }
    // Hosts without a rule report none.
    assert_eq!(inspect("europepmc__spa-shell.html").rule, None);
}

#[test]
fn springer_paywalled_has_the_candidate_and_no_access() {
    let pi = inspect("springer__article-paywalled.html");
    assert_eq!(
        first(&pi).url,
        "https://link.springer.com/content/pdf/10.1007/s00464-025-11552-4.pdf"
    );
    assert_eq!(
        pi.access,
        AccessHint::NoAccess,
        "citation_pdf_url without an a[data-article-pdf] button"
    );
    assert_eq!(
        pi.page_ids.doi.as_deref(),
        Some("10.1007/s00464-025-11552-4"),
        "prism.doi / dc.identifier with a doi: prefix"
    );
    // The open-access pages have the button.
    assert_eq!(
        inspect("nature__scirep-oa.html").access,
        AccessHint::Granted
    );
    assert_eq!(
        inspect("biomedcentral__redirected-to-springerlink-oa.html").access,
        AccessHint::Granted
    );
}

#[test]
fn pmc_candidate_needs_a_browser() {
    let pi = inspect("pmc__article-oa.html");
    assert_eq!(
        first(&pi).url,
        "https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/pdf/pmed.1000097.pdf"
    );
    assert!(
        pi.candidates.iter().all(|c| c.needs_browser),
        "every PMC candidate is behind the proof-of-work page"
    );
    assert_eq!(first(&pi).host_kind, fulltext_core::model::HostKind::Pmc);
}

#[test]
fn walled_hosts_flag_needs_a_browser_and_open_hosts_do_not() {
    for file in [
        "thieme__article-html.html",
        "mdpi__article-oa.html",
        "lww__annals-of-surgery-article.html",
        "lww__ijs-article-oa.html",
        "sciencedirect__article-preloaded-state.html",
    ] {
        assert!(first(&inspect(file)).needs_browser, "{file}");
    }
    for file in [
        "plos__article-oa.html",
        "frontiers__article-oa.html",
        "cambridge__article-oa.html",
        "nature__scirep-oa.html",
        "wiley__wjs-article.html",
        "nejm__article-free.html",
    ] {
        assert!(!first(&inspect(file)).needs_browser, "{file}");
    }
}

#[test]
fn thieme_candidate_is_upgraded_to_https() {
    let pi = inspect("thieme__article-html.html");
    assert_eq!(
        first(&pi).url,
        "https://www.thieme-connect.com/products/ejournals/pdf/10.1055/a-0495-1582.pdf"
    );
    assert!(pi.candidates.iter().all(|c| c.url.starts_with("https://")));
}

#[test]
fn wiley_uses_pdfdirect_and_never_epdf() {
    let pi = inspect("wiley__wjs-article.html");
    assert_eq!(
        first(&pi).url,
        "https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/wjs.12026"
    );
    assert!(urls(&pi).contains(&"https://onlinelibrary.wiley.com/doi/pdf/10.1002/wjs.12026"));
    assert!(urls(&pi).contains(
        &"https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/wjs.12026?download=true"
    ));
    assert!(pi.candidates.iter().all(|c| !c.url.contains("/epdf/")));
}

#[test]
fn atypon_sites_without_citation_pdf_url_get_the_doi_rule_url() {
    for (file, pdf) in [
        (
            "tandf__article-no-citation-pdf-url.html",
            "https://www.tandfonline.com/doi/pdf/10.1080/07853890.2016.1186828",
        ),
        (
            "sage__article-ereader-only.html",
            "https://journals.sagepub.com/doi/pdf/10.1177/2050312118807624",
        ),
        (
            "nejm__article-free.html",
            "https://www.nejm.org/doi/pdf/10.1056/NEJMoa2002032",
        ),
    ] {
        let pi = inspect(file);
        assert_eq!(first(&pi).url, pdf, "{file}");
        assert_eq!(first(&pi).score, 100, "{file}: rule URLs rank first");
        assert_eq!(pi.rule.as_deref(), Some("atypon"), "{file}");
        assert!(
            urls(&pi).contains(&format!("{pdf}?download=true").as_str()),
            "{file}: download variant"
        );
    }
    // The DOI keeps its original case in the path (NEJM paths are case-sensitive); the reported id is lower-case.
    assert_eq!(
        inspect("nejm__article-free.html").page_ids.doi.as_deref(),
        Some("10.1056/nejmoa2002032")
    );
}

#[test]
fn sage_reader_link_is_rewritten_not_followed() {
    let pi = inspect("sage__article-ereader-only.html");
    assert!(
        pi.candidates
            .iter()
            .all(|c| !c.url.contains("/doi/reader/"))
    );
    assert!(pi.candidates.iter().any(|c| {
        c.signal
            .as_deref()
            .is_some_and(|s| s.starts_with("rule:atypon"))
    }));
}

#[test]
fn mdpi_keeps_the_version_query_and_never_selects_the_cover_variant() {
    let pi = inspect("mdpi__article-oa.html");
    assert_eq!(
        first(&pi).url,
        "https://www.mdpi.com/2077-0383/9/4/1225/pdf?version=1587732386"
    );
    assert!(
        pi.candidates
            .iter()
            .all(|c| !c.url.contains("pdf-with-cover")
                && !c.url.ends_with("/xml")
                && !c.url.ends_with("/epub")),
        "{:?}",
        urls(&pi)
    );
}

#[test]
fn bmj_prefers_citation_pdf_url_then_the_alternate_link() {
    let pi = inspect("bmj__article-oa.html");
    assert_eq!(
        urls(&pi)[..2],
        [
            "https://www.bmj.com/content/bmj/372/bmj.n71.full.pdf",
            "https://www.bmj.com/content/372/bmj.n71.full.pdf"
        ]
    );
    assert_eq!(
        pi.access,
        AccessHint::Granted,
        "citation_access=all / DC.AccessRights=open-access"
    );
}

#[test]
fn lancet_and_cell_get_the_show_pdf_alternate_and_the_pii() {
    let pi = inspect("lancet__article-free.html");
    assert_eq!(
        first(&pi).url,
        "https://www.thelancet.com/pdfs/journals/lancet/PIIS0140-6736(20)30183-5.pdf"
    );
    assert!(
        urls(&pi)
            .contains(&"https://www.thelancet.com/action/showPdf?pii=S0140-6736%2820%2930183-5")
    );
    assert_eq!(pi.page_ids.pii.as_deref(), Some("S0140673620301835"));
    assert_eq!(
        pi.page_ids.doi.as_deref(),
        Some("10.1016/s0140-6736(20)30183-5")
    );
    let pi = inspect("cell__article-free.html");
    assert_eq!(
        first(&pi).url,
        "https://www.cell.com/cell/pdf/S0092-8674(20)30229-4.pdf"
    );
    assert!(
        urls(&pi).contains(&"https://www.cell.com/action/showPdf?pii=S0092-8674%2820%2930229-4")
    );
    assert_eq!(
        pi.page_ids.pii.as_deref(),
        Some("S0092867420302294"),
        "the PII comes from the canonical URL when there is no citation_pii"
    );
}

#[test]
fn karger_has_the_doi_based_alternate() {
    let pi = inspect("karger__article.html");
    assert_eq!(
        urls(&pi)[..2],
        [
            "https://karger.com/crg/article-pdf/1/1/21/2505605/000104224.pdf",
            "https://karger.com/article-pdf/doi/10.1159/000104224"
        ]
    );
    assert_eq!(
        pi.access,
        AccessHint::Granted,
        "a.article-pdfLink is present"
    );
}

#[test]
fn cambridge_offers_the_anchor_and_the_slugless_file() {
    let pi = inspect("cambridge__article-oa.html");
    let alt = "https://www.cambridge.org/core/services/aop-cambridge-core/content/view/2524DD3DAA8E8B68A1197E18196D5CB8/S2056472424006483a.pdf/community-clozapine-initiation-practice.pdf";
    assert!(urls(&pi).contains(&alt), "{:?}", urls(&pi));
    assert!(urls(&pi).contains(&"https://www.cambridge.org/core/services/aop-cambridge-core/content/view/2524DD3DAA8E8B68A1197E18196D5CB8/S2056472424006483a.pdf"));
}

#[test]
fn sciencedirect_comes_from_the_embedded_json_state() {
    let pi = inspect("sciencedirect__article-preloaded-state.html");
    let json = "https://www.sciencedirect.com/science/article/pii/S1743919121000406/pdfft?md5=b6d8b093900d49eecaf3e85a8799af17&pid=1-s2.0-S1743919121000406-main.pdf";
    assert_eq!(first(&pi).url, json);
    assert_eq!(
        first(&pi).signal.as_deref(),
        Some("rule:sciencedirect:json_state")
    );
    assert!(urls(&pi).contains(
        &"https://www.sciencedirect.com/science/article/pii/S1743919121000406/pdfft?download=true"
    ));
    assert!(urls(&pi).contains(&"https://www.sciencedirect.com/science/article/pii/S1743919121000406/pdfft?isDTMRedir=true&download=true"));
    assert_eq!(pi.page_ids.pii.as_deref(), Some("S1743919121000406"));
    assert_eq!(
        pi.page_ids.doi.as_deref(),
        Some("10.1016/j.ijsu.2021.105906")
    );
    // The anchor without an href ("View PDF") is not a link.
    assert!(
        pi.candidates
            .iter()
            .all(|c| !c.url.contains("pdf.sciencedirectassets.com"))
    );
    // Without the rule, the generic JSON-state signal still finds it (page on an unknown host).
    let (mut resp, body) = common::html_fixture("sciencedirect__article-preloaded-state.html");
    resp.final_url = "https://mirror.example.org/science/article/pii/S1743919121000406".into();
    let generic = inspect_page(&resp, &Ids::default(), Route::Oa, &body);
    assert_eq!(
        first(&generic).url,
        "https://mirror.example.org/science/article/pii/S1743919121000406/pdfft?md5=b6d8b093900d49eecaf3e85a8799af17&pid=1-s2.0-S1743919121000406-main.pdf"
    );
    assert_eq!(first(&generic).signal.as_deref(), Some("json:pdfDownload"));
}

#[test]
fn jama_comes_from_the_pdf_link_data_attribute_when_citation_pdf_url_is_missing() {
    let (resp, body) = common::html_fixture("jama__article-free.html");
    let want = "https://jamanetwork.com/journals/jama/articlepdf/1760318/jsc130006.pdf";
    assert_eq!(
        first(&inspect_page(&resp, &Ids::default(), Route::Oa, &body)).url,
        want
    );
    // Rename the meta so only `a#pdf-link[data-article-url]` (href="javascript:;") is left.
    let stripped = String::from_utf8_lossy(&body).replace("citation_pdf_url", "x_unused_meta");
    let pi = inspect_page(&resp, &Ids::default(), Route::Oa, stripped.as_bytes());
    assert_eq!(first(&pi).url, want);
    assert_eq!(
        first(&pi).signal.as_deref(),
        Some("rule:silverchair:pdf-link")
    );
    // And on an unknown host the generic anchor scoring finds it.
    let mut other = resp.clone();
    other.final_url = "https://mirror.example.org/journals/jama/fullarticle/1760318".into();
    let pi = inspect_page(&other, &Ids::default(), Route::Oa, stripped.as_bytes());
    assert_eq!(
        first(&pi).url,
        "https://mirror.example.org/journals/jama/articlepdf/1760318/jsc130006.pdf"
    );
    assert!(first(&pi).signal.as_deref().unwrap().starts_with("anchor:"));
}

#[test]
fn lww_comes_from_wkhealth_pdf_url_without_citation_pdf_url() {
    for (file, want) in [
        (
            "lww__annals-of-surgery-article.html",
            "https://journals.lww.com/annalsofsurgery/_layouts/15/oaks.journals/downloadpdf.aspx?trckng_src_pg=Other&an=00000658-202301000-00051",
        ),
        (
            "lww__ijs-article-oa.html",
            "https://journals.lww.com/international-journal-of-surgery/_layouts/15/oaks.journals/downloadpdf.aspx?trckng_src_pg=Other&an=01279778-202401000-00070",
        ),
    ] {
        let pi = inspect(file);
        assert_eq!(first(&pi).url, want, "{file}");
        assert_eq!(pi.candidates.len(), 1, "{file}: {:?}", urls(&pi));
        assert!(first(&pi).needs_browser);
        // On an unknown host the head-meta signal (not the rule) finds it.
        let (mut resp, body) = common::html_fixture(file);
        resp.final_url = format!(
            "https://mirror.example.org{}",
            resp.final_url.split_once("lww.com").unwrap().1
        );
        let generic = inspect_page(&resp, &Ids::default(), Route::Oa, &body);
        assert_eq!(
            first(&generic).signal.as_deref(),
            Some("meta:wkhealth_pdf_url"),
            "{file}"
        );
    }
    assert_eq!(
        inspect("lww__annals-of-surgery-article.html")
            .page_ids
            .doi
            .as_deref(),
        Some("10.1097/sla.0000000000004711")
    );
}

#[test]
fn page_ids_and_doi_match() {
    let page_doi = |file: &str| inspect(file).page_ids.doi;
    assert_eq!(
        page_doi("plos__article-oa.html").as_deref(),
        Some("10.1371/journal.pmed.1004377")
    );
    assert_eq!(
        page_doi("tandf__article-no-citation-pdf-url.html").as_deref(),
        Some("10.1080/07853890.2016.1186828"),
        "dc.Identifier scheme=doi, not the publisher-id one"
    );
    assert_eq!(
        page_doi("bmj__article-oa.html").as_deref(),
        Some("10.1136/bmj.n71")
    );
    assert_eq!(
        page_doi("oup__bjs-article-abstract.html").as_deref(),
        Some("10.1002/bjs.8990")
    );
    assert_eq!(page_doi("europepmc__spa-shell.html"), None);
    let ids = |doi: Option<&str>| Ids {
        doi: doi.map(str::to_owned),
        ..Default::default()
    };
    assert_eq!(
        inspect_with(
            "plos__article-oa.html",
            &ids(Some("10.1371/JOURNAL.PMED.1004377"))
        )
        .doi_match,
        Some(true)
    );
    assert_eq!(
        inspect_with(
            "plos__article-oa.html",
            &ids(Some("10.1371/journal.pmed.1000097"))
        )
        .doi_match,
        Some(false)
    );
    assert_eq!(
        inspect_with("plos__article-oa.html", &ids(None)).doi_match,
        None
    );
    assert_eq!(
        inspect_with(
            "europepmc__spa-shell.html",
            &ids(Some("10.1371/journal.pmed.1000097"))
        )
        .doi_match,
        None,
        "the page states no DOI"
    );
}

#[test]
fn access_hints_on_recorded_pages() {
    assert_eq!(inspect("karger__article.html").access, AccessHint::Granted);
    assert_eq!(inspect("bmj__article-oa.html").access, AccessHint::Granted);
    assert_eq!(
        inspect("springer__article-paywalled.html").access,
        AccessHint::NoAccess
    );
    // No evidence on the page: unknown, never a guess.
    for file in [
        "plos__article-oa.html",
        "wiley__wjs-article.html",
        "nejm__article-free.html",
        "sciencedirect__article-preloaded-state.html",
        "lww__annals-of-surgery-article.html",
    ] {
        assert_eq!(inspect(file).access, AccessHint::Unknown, "{file}");
    }
}

// ------------------------------------------------------------------ (b) the SPA shell

#[test]
fn europepmc_spa_shell_has_no_candidates() {
    let pi = inspect("europepmc__spa-shell.html");
    assert!(pi.candidates.is_empty(), "{:?}", urls(&pi));
    assert!(
        !matches!(
            pi.class,
            PageClass::Redirect { .. } | PageClass::Challenge { .. }
        ),
        "{:?}",
        pi.class
    );
    assert_eq!(pi.class, PageClass::Html);
    // The <noscript> refresh target is neither a candidate nor a rejection.
    assert!(
        pi.rejected
            .iter()
            .all(|r| !r.url.contains("javascript_support"))
    );
}

// ------------------------------------------------------------------ (c) challenges, (d) blocks, (e) redirects

#[test]
fn challenge_fixtures_are_classified_with_their_vendor() {
    let vendor_of = |file: &str| match file {
        f if f.starts_with("cloudflare__managed-challenge") => Vendor::Cloudflare,
        f if f.starts_with("pmc__pdf-proof-of-work") => Vendor::Pmc,
        f if f.starts_with("thieme__bot-challenge") => Vendor::Thieme,
        f if f.starts_with("sciencedirect__pdfft-js-challenge") => Vendor::Sciencedirect,
        f => panic!("no vendor mapped for {f}"),
    };
    let all = entries("challenge");
    assert_eq!(all.len(), 4);
    for e in all {
        let file = e["file"].as_str().unwrap();
        let want = PageClass::Challenge {
            vendor: vendor_of(file),
            interactive: false,
        };
        let pi = inspect(file);
        assert_eq!(pi.class, want, "{file}");
        assert_eq!(class_of(file), want, "{file}: classify_response agrees");
        assert!(
            pi.candidates.is_empty() && pi.rejected.is_empty() && pi.rule.is_none(),
            "{file}: challenge pages yield nothing"
        );
    }
}

#[test]
fn block_fixtures_are_classified_with_their_vendor() {
    let all = entries("blocked");
    assert_eq!(all.len(), 2);
    for e in all {
        let file = e["file"].as_str().unwrap();
        let vendor = if file.starts_with("cloudflare") {
            Vendor::Cloudflare
        } else {
            Vendor::Akamai
        };
        assert_eq!(inspect(file).class, PageClass::Blocked { vendor }, "{file}");
        assert!(inspect(file).candidates.is_empty());
    }
}

#[test]
fn redirect_fixtures_follow_the_oracle() {
    let all = entries("redirect");
    assert_eq!(all.len(), 2);
    for e in all {
        let file = e["file"].as_str().unwrap();
        let next = e["next_url"].as_str().unwrap();
        let pi = inspect(file);
        assert_eq!(
            pi.class,
            PageClass::Redirect {
                next: next.to_owned()
            },
            "{file}"
        );
        assert!(pi.candidates.is_empty(), "{file}");
    }
}

#[test]
fn oa_side_walls_with_only_a_content_type() {
    // These come from the OA manifest, so they carry no `server` / `cf-mitigated` headers: the body must be enough.
    assert_eq!(
        inspect("pmc-web__pdf-pow-interstitial.html").class,
        PageClass::Challenge {
            vendor: Vendor::Pmc,
            interactive: false
        }
    );
    assert_eq!(
        inspect("europepmc-web__pdf-render-cloudflare-challenge.html").class,
        PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive: false
        }
    );
    assert_eq!(
        inspect("medrxiv-web__pdf-403-block.html").class,
        PageClass::Blocked {
            vendor: Vendor::Cloudflare
        }
    );
}

#[test]
fn oa_side_walls_with_the_real_headers() {
    let with = |file: &str, headers: &[(&str, &str)]| {
        let (mut resp, body) = common::html_fixture(file);
        for (k, v) in headers {
            resp.headers.insert((*k).to_owned(), (*v).to_owned());
        }
        classify_response(&resp, &body)
    };
    assert_eq!(
        with(
            "europepmc-web__pdf-render-cloudflare-challenge.html",
            &[("server", "cloudflare"), ("cf-mitigated", "challenge")]
        ),
        PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive: false
        }
    );
    assert_eq!(
        with(
            "medrxiv-web__pdf-403-block.html",
            &[("server", "cloudflare")]
        ),
        PageClass::Blocked {
            vendor: Vendor::Cloudflare
        }
    );
    // Header-only: the challenge header decides even when the body is cut to nothing.
    let (mut resp, _) = common::html_fixture("europepmc-web__pdf-render-cloudflare-challenge.html");
    resp.headers
        .insert("cf-mitigated".into(), "challenge".into());
    assert_eq!(
        classify_response(&resp, b""),
        PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive: false
        }
    );
}

#[test]
fn a_landing_page_served_with_a_challenge_status_is_still_a_challenge() {
    // The same markup as the PLOS page but with a 403 and the Cloudflare header must not yield candidates.
    let (mut resp, body) = common::html_fixture("plos__article-oa.html");
    resp.status = 403;
    resp.headers
        .insert("cf-mitigated".into(), "challenge".into());
    let pi = inspect_page(&resp, &Ids::default(), Route::Oa, &body);
    assert!(matches!(pi.class, PageClass::Challenge { .. }));
    assert!(pi.candidates.is_empty());
}

// ------------------------------------------------------------------ debugging aid

/// Prints the class, rule, access hint, candidates and refusals of every fixture. Not part of the normal run:
/// `cargo test -p fulltext-core --test page_fixtures dump_every_fixture -- --ignored --nocapture`.
#[test]
#[ignore = "debugging aid: prints the analysis of every fixture"]
fn dump_every_fixture() {
    for file in all_fixture_files() {
        let pi = inspect(&file);
        println!(
            "== {file}\n   class {:?} rule {:?} access {:?} ids {:?} match {:?}",
            pi.class, pi.rule, pi.access, pi.page_ids, pi.doi_match
        );
        for c in &pi.candidates {
            println!(
                "   + {:>3} {}{} [{}]",
                c.score,
                c.url,
                if c.needs_browser { " (browser)" } else { "" },
                c.signal.as_deref().unwrap_or("")
            );
        }
        for r in &pi.rejected {
            println!("   - {:<16} {}", r.why, r.url);
        }
    }
}

// ------------------------------------------------------------------ totality

fn all_fixture_files() -> Vec<String> {
    let mut files: Vec<String> = common::html_expected()
        .iter()
        .map(|e| e["file"].as_str().unwrap().to_owned())
        .collect();
    files.extend(
        [
            "pmc-web__pdf-pow-interstitial.html",
            "europepmc-web__pdf-render-cloudflare-challenge.html",
            "medrxiv-web__pdf-403-block.html",
        ]
        .map(String::from),
    );
    files
}

#[test]
fn no_fixture_can_panic_the_analysis_however_it_is_cut_or_corrupted() {
    for file in all_fixture_files() {
        let (resp, body) = common::html_fixture(&file);
        let len = body.len();
        let mut variants: Vec<(String, Vec<u8>)> = [0, 1, 100, len / 2, len.saturating_sub(1), len]
            .into_iter()
            .map(|n| {
                (
                    format!("truncated to {n}"),
                    body.get(..n.min(len)).unwrap_or(&body).to_vec(),
                )
            })
            .collect();
        let mut nul = body.clone();
        for b in nul.iter_mut().skip(100) {
            *b = 0;
        }
        variants.push(("NUL after 100".into(), nul));
        let mut ff = body.clone();
        for b in ff.iter_mut().skip(50).step_by(3) {
            *b = 0xff;
        }
        variants.push(("0xff every third byte".into(), ff));
        for (label, bytes) in variants {
            for route in [Route::Oa, Route::Inst] {
                let pi = inspect_page(&resp, &Ids::default(), route, &bytes);
                // Candidate URLs are always absolute https URLs, whatever the input.
                assert!(
                    pi.candidates.iter().all(|c| c.url.starts_with("https://")),
                    "{file} [{label}]"
                );
            }
            let _ = classify_response(&resp, &bytes);
            let mut probe = RespMeta {
                status: 302,
                ..resp.clone()
            };
            probe.headers.insert("location".into(), "/next".into());
            let _ = classify_response(&probe, &bytes);
        }
    }
}

#[test]
fn headers_with_upper_case_names_are_tolerated() {
    let (mut resp, body) = common::html_fixture("cloudflare__managed-challenge.html");
    resp.headers = resp
        .headers
        .iter()
        .map(|(k, v)| (k.to_uppercase(), v.clone()))
        .collect();
    assert_eq!(
        classify_response(&resp, &body),
        PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive: false
        }
    );
}
