//! Every recorded OA-API response through the real parser (spec: docs/wp/core-ids-oa.md §2).
//!
//! The fixtures are live captures, so the assertions are structural (status, which kind of candidate exists on which
//! kind of host, which ids were learned) rather than whole URL lists.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;

use fulltext_core::model::{
    ApiInspection, ApiStatus, CandKind, Candidate, HostKind, Ids, RespMeta, Source, Version,
};
use fulltext_core::urls::{canonical_key, host_of};
use fulltext_core::{ids, oa};
use std::collections::{BTreeMap, BTreeSet};

use ApiStatus::{Empty, Found, NotFound, Throttled, Unparseable};
use Source::{
    Alma, Arxiv, Biorxiv, Core, Doi, Europepmc, Openalex, PmcS3, SemanticScholar, Unpaywall,
    ZoteroOa,
};

/// file -> (parser, expected status when parsed with no particular request in mind).
#[rustfmt::skip]
const TABLE: &[(&str, Source, ApiStatus)] = &[
    ("openalex__arxiv.json", Openalex, Found),
    ("openalex__by-pmcid-404.html", Openalex, NotFound),
    ("openalex__by-pmcid-filter.json", Openalex, Empty),
    ("openalex__by-pmid.json", Openalex, Found),
    ("openalex__filter-doi-select.json", Openalex, Found),
    ("openalex__gold.json", Openalex, Found),
    ("openalex__green-select.json", Openalex, Found),
    ("openalex__green.json", Openalex, Found),
    ("openalex__medrxiv.json", Openalex, Found),
    ("openalex__nonexistent-404.html", Openalex, NotFound),
    ("openalex__paywalled.json", Openalex, Empty),
    ("openalex__title-search.json", Openalex, Found),
    ("openalex-content__no-key.json", Openalex, Unparseable),
    ("europepmc__batch-or-lite.json", Europepmc, Empty),
    ("europepmc__by-extid-pmid.json", Europepmc, Found),
    ("europepmc__by-pmcid.json", Europepmc, Found),
    ("europepmc__fulltext-xml-gold.xml", Europepmc, Unparseable),
    ("europepmc__gold.json", Europepmc, Found),
    ("europepmc__green.json", Europepmc, Found),
    ("europepmc__medrxiv.json", Europepmc, Found),
    ("europepmc__nonexistent.json", Europepmc, Empty),
    ("europepmc__paywalled.json", Europepmc, Empty),
    ("europepmc-web__pdf-render-cloudflare-challenge.html", Europepmc, Unparseable),
    ("biorxiv-api__medrxiv-details.json", Biorxiv, Found),
    ("biorxiv-api__medrxiv-pubs.json", Biorxiv, Found),
    ("biorxiv-api__nonexistent.json", Biorxiv, Empty),
    ("medrxiv-web__pdf-403-block.html", Biorxiv, Unparseable),
    ("pmc-s3__list-PMC2707599.xml", PmcS3, Found),
    ("pmc-s3__list-PMC4221854.xml", PmcS3, Empty),
    ("pmc-s3__meta-gold.json", PmcS3, Found),
    ("pmc-s3__meta-green-aam.json", PmcS3, Empty),
    ("pmc-s3__meta-nonexistent.xml", PmcS3, NotFound),
    ("pmc-web__pdf-pow-interstitial.html", PmcS3, Unparseable),
    ("ncbi-idconv__gold-green-paywalled.json", PmcS3, Found),
    ("ncbi-idconv__nonexistent.json", PmcS3, Empty),
    ("ncbi-idconv__pmid.json", PmcS3, Found),
    ("ncbi-pmc-oa__retired-404.html", PmcS3, NotFound),
    ("arxiv__export-api-nonexistent.xml", Arxiv, Empty),
    ("arxiv__export-api.xml", Arxiv, Found),
    ("unpaywall__arxiv-404.html", Unpaywall, NotFound),
    ("unpaywall__error-invalid-doi-404.html", Unpaywall, NotFound),
    ("unpaywall__error-no-email.json", Unpaywall, Unparseable),
    ("unpaywall__error-test-example-email.json", Unpaywall, Unparseable),
    ("unpaywall__gold.json", Unpaywall, Found),
    ("unpaywall__green.json", Unpaywall, Found),
    ("unpaywall__medrxiv.json", Unpaywall, Found),
    ("unpaywall__nonexistent-404.html", Unpaywall, NotFound),
    ("unpaywall__paywalled.json", Unpaywall, Empty),
    ("semanticscholar__arxiv.json", SemanticScholar, NotFound),
    ("semanticscholar__batch-other-ids.json", SemanticScholar, Empty),
    ("semanticscholar__batch.json", SemanticScholar, Found),
    ("semanticscholar__error-429-unauthenticated.json", SemanticScholar, Throttled),
    ("semanticscholar__gold.json", SemanticScholar, Found),
    ("semanticscholar__green.json", SemanticScholar, Found),
    ("semanticscholar__medrxiv.json", SemanticScholar, NotFound),
    ("semanticscholar__nonexistent.json", SemanticScholar, NotFound),
    ("semanticscholar__paywalled.json", SemanticScholar, Empty),
    ("core__discover-gold-no-key.json", Core, Found),
    ("core__discover-green-no-key.json", Core, Found),
    ("core__discover-paywalled-no-key.json", Core, Found),
    ("core__output-green-no-key.json", Core, Found),
    ("core__output-paywalled-hit-no-key.json", Core, NotFound),
    ("core__search-no-key-500.json", Core, Unparseable),
    ("crossref__gold.json", Doi, Found),
    ("crossref__green.json", Doi, Found),
    ("crossref__medrxiv.json", Doi, Found),
    ("crossref__nonexistent-404.txt", Doi, NotFound),
    ("crossref__paywalled.json", Doi, Found),
    ("zotero-oa__arxiv.json", ZoteroOa, Empty),
    ("zotero-oa__gold.json", ZoteroOa, Found),
    ("zotero-oa__green.json", ZoteroOa, Found),
    ("zotero-oa__invalid-doi.json", ZoteroOa, Empty),
    ("zotero-oa__medrxiv.json", ZoteroOa, Found),
    ("zotero-oa__nonexistent.json", ZoteroOa, Empty),
    ("zotero-oa__paywalled.json", ZoteroOa, Empty),
];

/// Manifest prefixes the parsers own. Every manifest file with one of these must be in `TABLE`.
const PARSED_PREFIXES: &[&str] = &[
    "openalex__",
    "europepmc__",
    "biorxiv-api__",
    "pmc-s3__",
    "ncbi-idconv__",
    "arxiv__",
    "unpaywall__",
    "semanticscholar__",
    "core__",
    "crossref__",
    "zotero-oa__",
];

// ---------------------------------------------------------------------------------------------------- helpers

fn source_of(file: &str) -> Source {
    TABLE
        .iter()
        .find(|(f, _, _)| *f == file)
        .map(|(_, s, _)| *s)
        .unwrap_or_else(|| panic!("{file} is not in TABLE"))
}

fn parse_with(file: &str, requested: &Ids) -> ApiInspection {
    let (resp, body) = common::oa_fixture(file);
    oa::parse(source_of(file), &resp, &body, requested)
}

fn parse_file(file: &str) -> ApiInspection {
    parse_with(file, &Ids::default())
}

fn want_doi(doi: &str) -> Ids {
    Ids {
        doi: Some(doi.to_owned()),
        ..Default::default()
    }
}

fn host(c: &Candidate) -> String {
    host_of(&c.url).unwrap_or_default()
}

fn on_host<'a>(a: &'a ApiInspection, host_name: &str) -> Vec<&'a Candidate> {
    a.candidates
        .iter()
        .filter(|c| host(c) == host_name)
        .collect()
}

fn find(a: &ApiInspection, pred: impl Fn(&Candidate) -> bool) -> Option<&Candidate> {
    a.candidates.iter().find(|c| pred(c))
}

fn pdfs(a: &ApiInspection) -> Vec<&Candidate> {
    a.candidates
        .iter()
        .filter(|c| c.kind == CandKind::Pdf)
        .collect()
}

fn resp(status: u16) -> RespMeta {
    RespMeta {
        status,
        final_url: "https://api.example.org/x".into(),
        ..Default::default()
    }
}

const GOLD: &str = "10.1371/journal.pmed.1000097";
const GREEN: &str = "10.1038/nature12373";
const ARXIV_DOI: &str = "10.48550/arxiv.1706.03762";
const MEDRXIV: &str = "10.1101/2023.05.31.23290770";
const PAYWALLED: &str = "10.1097/sla.0000000000005603";

// ---------------------------------------------------------------------------------------------------- coverage

#[test]
fn every_parsed_manifest_file_has_a_table_row() {
    let manifest: Vec<String> = common::oa_manifest()
        .iter()
        .map(|e| e["file"].as_str().unwrap().to_owned())
        .collect();
    let missing: Vec<&String> = manifest
        .iter()
        .filter(|f| {
            PARSED_PREFIXES.iter().any(|p| f.starts_with(p))
                && !TABLE.iter().any(|(t, _, _)| t == f)
        })
        .collect();
    assert!(
        missing.is_empty(),
        "manifest files not covered by TABLE: {missing:?}"
    );
    let stale: Vec<&str> = TABLE
        .iter()
        .map(|(f, _, _)| *f)
        .filter(|f| !manifest.iter().any(|m| m == f))
        .collect();
    assert!(
        stale.is_empty(),
        "TABLE rows without a manifest file: {stale:?}"
    );
    let unique: BTreeSet<&str> = TABLE.iter().map(|(f, _, _)| *f).collect();
    assert_eq!(unique.len(), TABLE.len(), "duplicate TABLE rows");
}

#[test]
fn every_table_row_goes_through_oa_parse_with_the_expected_status() {
    for (file, source, want) in TABLE {
        let (meta, body) = common::oa_fixture(file);
        let got = oa::parse(*source, &meta, &body, &Ids::default());
        assert_eq!(got.status, *want, "{file}: note={:?}", got.note);
        assert_eq!(got.source, *source, "{file}");
    }
}

/// Invariants of every candidate any parser emits.
fn assert_candidate_invariants(file: &str, a: &ApiInspection) {
    let mut seen = BTreeSet::new();
    for c in &a.candidates {
        assert!(
            c.url.starts_with("https://"),
            "{file}: not https: {}",
            c.url
        );
        assert_eq!(c.source, a.source, "{file}: {}", c.url);
        assert_eq!(
            (c.score, c.tier),
            (0, 0),
            "{file}: parsers leave score and tier to rank: {}",
            c.url
        );
        assert!(
            seen.insert(canonical_key(&c.url)),
            "{file}: duplicate candidate {}",
            c.url
        );
        if let Some(l) = &c.license {
            assert!(
                !l.is_empty()
                    && *l == l.to_ascii_lowercase()
                    && !l.contains(' ')
                    && !l.contains('_'),
                "{file}: licence {l:?}"
            );
        }
        if let Some(d) = &c.record_doi {
            assert_eq!(
                ids::clean_doi(d).as_deref(),
                Some(d.as_str()),
                "{file}: record_doi {d:?}"
            );
        }
        if let Some(l) = &c.landing_url {
            assert!(
                l.starts_with("https://") && *l != c.url,
                "{file}: landing_url {l:?} for {}",
                c.url
            );
        }
        let signal = c.signal.as_deref().unwrap_or_default();
        assert!(
            !signal.is_empty(),
            "{file}: candidate without a signal: {}",
            c.url
        );
    }
}

#[test]
fn every_fixture_yields_well_formed_candidates() {
    for (file, _, _) in TABLE {
        let a = parse_file(file);
        if a.status == Found && a.candidates.is_empty() {
            // Only the ID converter (learned ids) and bioRxiv `pubs` (published DOI in the note) may do this.
            assert!(
                a.note.is_some() || a.learned != Default::default(),
                "{file}"
            );
            continue;
        }
        assert_candidate_invariants(file, &a);
        if a.status != Found {
            assert!(
                a.candidates.is_empty(),
                "{file}: {:?} with candidates",
                a.status
            );
        }
        assert_eq!(a.retry_after_s, None, "{file}");
    }
}

#[test]
fn truncated_and_corrupted_fixtures_never_panic() {
    for (file, source, _) in TABLE {
        let (meta, body) = common::oa_fixture(file);
        let req = Ids::default();
        for cut in [
            0,
            1,
            2,
            7,
            64,
            body.len() / 3,
            body.len() / 2,
            body.len().saturating_sub(1),
        ] {
            let cut = cut.min(body.len());
            let _ = oa::parse(*source, &meta, &body[..cut], &req);
        }
        // Flip bytes at a few positions and drop a UTF-8 lead byte in the middle.
        let mut mutated = body.clone();
        for i in (0..mutated.len()).step_by(97) {
            mutated[i] ^= 0x5a;
        }
        let _ = oa::parse(*source, &meta, &mutated, &req);
        // Every parser, every fixture body, whatever the body's real origin.
        for other in [
            Openalex,
            Europepmc,
            Biorxiv,
            PmcS3,
            Arxiv,
            Unpaywall,
            SemanticScholar,
            Core,
            Doi,
            Alma,
            ZoteroOa,
        ] {
            let _ = oa::parse(other, &meta, &body, &req);
        }
    }
}

#[test]
fn every_parser_survives_every_status_code_and_hostile_bodies() {
    let bodies: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"null".to_vec(),
        b"[]".to_vec(),
        b"{}".to_vec(),
        b"{\"results\":null,\"collection\":3,\"records\":\"x\",\"message\":[]}".to_vec(),
        b"<html><body>Just a moment...</body></html>".to_vec(),
        b"<feed><entry>".to_vec(),
        vec![0u8; 4096],
        vec![0xff; 4096],
        "{\"locations\":".repeat(500).into_bytes(),
        "[".repeat(5_000).into_bytes(),
        format!("{{\"results\":[{}]}}", "{},".repeat(2_000)).into_bytes(),
    ];
    let sources = [
        Openalex,
        Europepmc,
        Biorxiv,
        PmcS3,
        Arxiv,
        Unpaywall,
        SemanticScholar,
        Core,
        Doi,
        Alma,
        ZoteroOa,
        Source::ItemUrl,
        Source::Ezproxy,
    ];
    let requested = Ids {
        doi: Some(GOLD.into()),
        pmid: Some("19621072".into()),
        pmcid: Some("PMC2707599".into()),
        arxiv: Some("1706.03762".into()),
        ..Default::default()
    };
    for status in [
        0u16, 100, 200, 201, 204, 301, 302, 400, 401, 403, 404, 410, 422, 429, 500, 502, 503, 504,
        599, 999,
    ] {
        for source in sources {
            for body in &bodies {
                for req in [&Ids::default(), &requested] {
                    let a = oa::parse(source, &resp(status), body, req);
                    assert_eq!(a.source, source);
                    if !(200..300).contains(&status) {
                        assert!(
                            a.candidates.is_empty() && a.learned == Default::default(),
                            "{source:?} {status}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn http_status_decides_before_the_body_is_read() {
    let json = br#"{"results":[{"doi":"https://doi.org/10.1371/journal.pmed.1000097"}]}"#;
    for source in [
        Openalex,
        Europepmc,
        Biorxiv,
        PmcS3,
        Arxiv,
        Unpaywall,
        SemanticScholar,
        Core,
        Doi,
        Alma,
        ZoteroOa,
    ] {
        let a = |status: u16, retry: Option<&str>| {
            let mut r = resp(status);
            if let Some(v) = retry {
                r.headers = BTreeMap::from([("retry-after".to_owned(), v.to_owned())]);
            }
            oa::parse(source, &r, json, &Ids::default())
        };
        assert_eq!(a(404, None).status, NotFound, "{source:?}");
        assert_eq!(a(410, None).status, NotFound, "{source:?}");
        assert_eq!(a(500, None).status, Unparseable, "{source:?}");
        assert_eq!(a(403, None).status, Unparseable, "{source:?}");
        let throttled = a(429, Some("17"));
        assert_eq!(
            (throttled.status, throttled.retry_after_s),
            (Throttled, Some(17)),
            "{source:?}"
        );
        let busy = a(503, Some("5"));
        assert_eq!(
            (busy.status, busy.retry_after_s),
            (Throttled, Some(5)),
            "{source:?}"
        );
        assert_eq!(a(503, None).status, Unparseable, "{source:?}");
        assert_eq!(
            a(429, Some("Wed, 21 Oct 2026 07:28:00 GMT")).retry_after_s,
            None,
            "{source:?}: date form is ignored"
        );
    }
    for source in [
        Source::ItemUrl,
        Source::Translator,
        Source::Extractor,
        Source::Redirector,
        Source::Ezproxy,
    ] {
        assert_eq!(
            oa::parse(source, &resp(200), json, &Ids::default()).status,
            Unparseable,
            "{source:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------------- OpenAlex

#[test]
fn openalex_gold_keeps_open_access_locations_only() {
    let a = parse_with("openalex__gold.json", &want_doi(GOLD));
    assert_eq!(
        (a.status, a.oa_status.as_deref(), a.note.as_deref()),
        (Found, Some("gold"), None)
    );
    assert_eq!(a.learned.pmcid.as_deref(), Some("PMC2707599"));
    assert_eq!(a.learned.pmid.as_deref(), Some("19621072"));
    assert_eq!(a.learned.doi, None, "the request already had a DOI");
    // The publisher location is a landing page on doi.org: the extractor finds citation_pdf_url there.
    let publisher =
        find(&a, |c| c.url == format!("https://doi.org/{GOLD}")).expect("publisher landing page");
    assert_eq!(
        (publisher.kind, publisher.host_kind, publisher.version),
        (CandKind::Landing, HostKind::Publisher, Version::Published)
    );
    assert_eq!(publisher.license.as_deref(), Some("cc-by"));
    assert_eq!(publisher.record_doi.as_deref(), Some(GOLD));
    // Repository copies are present, PubMed (is_oa = false) and RePEc (is_oa = false) are not.
    assert!(
        find(&a, |c| c.kind == CandKind::Pdf
            && host(c) == "iris.unimore.it")
        .is_some()
    );
    assert!(
        a.candidates
            .iter()
            .all(|c| host(c) != "pubmed.ncbi.nlm.nih.gov" && host(c) != "journals.plos.org")
    );
    assert!(
        find(&a, |c| c.host_kind == HostKind::Pmc
            && c.kind == CandKind::Landing)
        .is_some()
    );
    // The Pdf candidate remembers the landing page of its location.
    let iris = find(&a, |c| host(c) == "iris.unimore.it").unwrap();
    assert_eq!(
        iris.landing_url.as_deref(),
        Some("https://hdl.handle.net/11380/1368022")
    );
    assert_eq!(
        iris.signal.as_deref(),
        Some("openalex:locations[3].pdf_url")
    );
    // No paid content cache for this work.
    assert!(
        a.candidates
            .iter()
            .all(|c| c.signal.as_deref() != Some("openalex:content_api"))
    );
    assert_eq!(a.candidates.iter().filter(|c| c.needs_browser).count(), 0);
}

#[test]
fn openalex_green_has_publisher_pdf_arxiv_pmc_and_the_paid_cache() {
    let a = parse_with("openalex__green.json", &want_doi(GREEN));
    assert_eq!((a.status, a.oa_status.as_deref()), (Found, Some("bronze")));
    let nature = find(&a, |c| host(c) == "www.nature.com").unwrap();
    assert_eq!(
        (nature.kind, nature.host_kind, nature.version),
        (CandKind::Pdf, HostKind::Publisher, Version::Published)
    );
    assert_eq!(
        nature.landing_url.as_deref(),
        Some("https://doi.org/10.1038/nature12373")
    );
    let arxiv = find(&a, |c| c.url == "https://arxiv.org/pdf/1304.1068").unwrap();
    assert_eq!(
        (arxiv.kind, arxiv.host_kind, arxiv.version),
        (CandKind::Pdf, HostKind::Preprint, Version::Submitted)
    );
    // `http://arxiv.org/abs/..` was upgraded.
    assert!(
        find(&a, |c| c.url == "https://arxiv.org/abs/1304.1068"
            && c.kind == CandKind::Landing)
        .is_some()
    );
    let dash = find(&a, |c| host(c) == "nrs.harvard.edu").unwrap();
    assert_eq!(
        (dash.license.as_deref(), dash.host_kind),
        (Some("cc-by"), HostKind::Repository)
    );
    assert_eq!(
        (
            a.learned.pmid.as_deref(),
            a.learned.pmcid.as_deref(),
            a.learned.arxiv.as_deref()
        ),
        (Some("23903748"), Some("PMC4221854"), Some("1304.1068"))
    );
    let paid = on_host(&a, "content.openalex.org");
    assert_eq!(paid.len(), 1);
    assert_eq!(
        (paid[0].kind, paid[0].signal.as_deref()),
        (CandKind::Pdf, Some("openalex:content_api"))
    );
    assert_eq!(
        paid[0].url,
        "https://content.openalex.org/works/W2159974629.pdf"
    );
}

#[test]
fn openalex_arxiv_keeps_the_merged_work_mirror_and_flags_it() {
    let requested = Ids {
        doi: Some(ARXIV_DOI.into()),
        arxiv: Some("1706.03762".into()),
        ..Default::default()
    };
    let a = parse_with("openalex__arxiv.json", &requested);
    assert_eq!(a.status, Found);
    assert_eq!(a.note.as_deref(), Some("merged_record:10.65215/2q58a426"));
    assert!(
        a.candidates
            .iter()
            .all(|c| c.record_doi.as_deref() == Some("10.65215/2q58a426")),
        "the work-level DOI differs from the request"
    );
    let arxiv = find(&a, |c| c.url == "https://arxiv.org/pdf/1706.03762").unwrap();
    assert_eq!(
        (arxiv.kind, arxiv.host_kind),
        (CandKind::Pdf, HostKind::Preprint)
    );
    // The third-party mirror is there, as an aggregator.
    let mirror = on_host(&a, "langtaosha.org.cn");
    assert_eq!(mirror.len(), 1);
    assert_eq!(
        (mirror[0].kind, mirror[0].host_kind),
        (CandKind::Pdf, HostKind::Aggregator)
    );
    // `locations[]` entries with is_oa = false (the other 10.65215 records) are not candidates.
    assert!(
        a.candidates
            .iter()
            .all(|c| !c.url.contains("10.65215/ctdc8e75"))
    );
    // Learned: nothing contradicts the request.
    assert_eq!(
        (a.learned.doi.as_deref(), a.learned.arxiv.as_deref()),
        (None, None)
    );
    // Without a request the work DOI is offered.
    assert_eq!(
        parse_file("openalex__arxiv.json").learned.doi.as_deref(),
        Some("10.65215/2q58a426")
    );
}

#[test]
fn openalex_medrxiv_resolves_to_the_published_work() {
    let a = parse_with("openalex__medrxiv.json", &want_doi(MEDRXIV));
    assert_eq!(a.status, Found);
    assert_eq!(
        a.note.as_deref(),
        Some("merged_record:10.1371/journal.pone.0298402")
    );
    let early = find(&a, |c| {
        c.url
            .ends_with("/content/medrxiv/early/2023/06/01/2023.05.31.23290770.full.pdf")
    })
    .unwrap();
    assert_eq!(
        (early.kind, early.host_kind, early.license.as_deref()),
        (CandKind::Pdf, HostKind::Preprint, Some("cc-by"))
    );
    let plos = find(&a, |c| {
        c.url == "https://doi.org/10.1371/journal.pone.0298402"
    })
    .unwrap();
    assert_eq!(
        (plos.kind, plos.host_kind),
        (CandKind::Landing, HostKind::Publisher)
    );
    // The PMC website PDF is flagged as browser-only.
    let pmc = find(&a, |c| host(c) == "pmc.ncbi.nlm.nih.gov").unwrap();
    assert!(pmc.needs_browser && pmc.kind == CandKind::Pdf);
    assert_eq!(a.learned.pmcid.as_deref(), Some("PMC11156280"));
    assert_eq!(a.learned.pmid.as_deref(), Some("38843138"));
}

#[test]
fn openalex_closed_work_is_empty_with_status_and_ids() {
    let a = parse_with(
        "openalex__paywalled.json",
        &want_doi("10.1097/SLA.0000000000005603"),
    );
    assert_eq!(
        (a.status, a.oa_status.as_deref(), a.candidates.len()),
        (Empty, Some("closed"), 0)
    );
    assert_eq!(a.learned.pmid.as_deref(), Some("35848743"));
    assert_eq!(
        a.note, None,
        "same work as requested, DOIs compare case-insensitively"
    );
}

#[test]
fn openalex_errors() {
    for file in [
        "openalex__nonexistent-404.html",
        "openalex__by-pmcid-404.html",
    ] {
        let a = parse_file(file);
        assert_eq!(
            (a.status, a.note.as_deref()),
            (NotFound, Some("http_404")),
            "{file}"
        );
    }
    let content = parse_file("openalex-content__no-key.json");
    assert_eq!(
        (content.status, content.note.as_deref()),
        (Unparseable, Some("http_401"))
    );
    let filtered = parse_file("openalex__by-pmcid-filter.json");
    assert_eq!(
        (filtered.status, filtered.note.as_deref()),
        (Empty, Some("no_results"))
    );
}

#[test]
fn openalex_list_answers_pick_the_matching_work() {
    // Default request: the first result.
    let first = parse_file("openalex__filter-doi-select.json");
    assert_eq!(first.candidates[0].record_doi.as_deref(), Some(GOLD));

    let green = parse_with("openalex__filter-doi-select.json", &want_doi(GREEN));
    assert!(
        green
            .candidates
            .iter()
            .all(|c| c.record_doi.as_deref() == Some(GREEN))
    );
    assert_eq!(green.oa_status.as_deref(), Some("bronze"));
    // The paid content cache is not requested by `select`ing only some fields.
    assert!(
        green
            .candidates
            .iter()
            .all(|c| c.signal.as_deref() != Some("openalex:content_api"))
    );

    let closed = parse_with("openalex__filter-doi-select.json", &want_doi(PAYWALLED));
    assert_eq!(
        (closed.status, closed.oa_status.as_deref()),
        (Empty, Some("closed"))
    );

    // A PMID request matches `ids.pmid`.
    let by_pmid = Ids {
        pmid: Some("23903748".into()),
        ..Default::default()
    };
    assert_eq!(
        parse_with("openalex__filter-doi-select.json", &by_pmid)
            .oa_status
            .as_deref(),
        Some("bronze")
    );

    // Several results, none about the request: the answer is not about it either.
    let other = parse_with(
        "openalex__filter-doi-select.json",
        &want_doi("10.1234/not-in-the-list"),
    );
    assert_eq!(
        (other.status, other.note.as_deref()),
        (Empty, Some("no_match"))
    );

    // A title search has no request to match: first result (it has best_oa_location, no locations[]).
    let title = parse_file("openalex__title-search.json");
    assert_eq!(title.status, Found);
    assert!(pdfs(&title).iter().any(|c| host(c) == "langtaosha.org.cn"));

    // A single work for a PMID lookup teaches the DOI.
    let by_pmid = parse_file("openalex__by-pmid.json");
    assert_eq!(by_pmid.learned.doi.as_deref(), Some(GOLD));
}

// ---------------------------------------------------------------------------------------------------- Europe PMC

#[test]
fn europepmc_gold_offers_get_pdf_and_never_the_walled_render_url() {
    let a = parse_with("europepmc__gold.json", &want_doi(GOLD));
    assert_eq!(a.status, Found);
    assert_eq!(
        (a.learned.pmcid.as_deref(), a.learned.pmid.as_deref()),
        (Some("PMC2707599"), Some("19621072"))
    );
    let get_pdf = find(&a, |c| {
        c.url == "https://europepmc.org/api/getPdf?pmcid=PMC2707599"
    })
    .unwrap();
    assert_eq!(
        (get_pdf.kind, get_pdf.host_kind, get_pdf.license.as_deref()),
        (CandKind::Pdf, HostKind::Pmc, Some("cc-by"))
    );
    assert_eq!(get_pdf.signal.as_deref(), Some("europepmc:getpdf"));
    assert!(!get_pdf.needs_browser);
    assert!(a.candidates.iter().all(|c| !c.url.contains("pdf=render")));
    // The OA html entry is a landing page; the subscription DOI entry is dropped.
    assert!(
        find(&a, |c| c.url == "https://europepmc.org/articles/PMC2707599"
            && c.kind == CandKind::Landing)
        .is_some()
    );
    assert!(find(&a, |c| c.url.starts_with("https://doi.org/")).is_none());
}

#[test]
fn europepmc_free_but_not_open_access_falls_back_to_the_browser_only_pdf() {
    let a = parse_with("europepmc__green.json", &want_doi(GREEN));
    assert_eq!(a.status, Found);
    assert_eq!(a.learned.pmcid.as_deref(), Some("PMC4221854"));
    assert!(
        find(&a, |c| c.url.contains("api/getPdf")).is_none(),
        "isOpenAccess is N"
    );
    let render = find(&a, |c| c.url.contains("pdf=render")).expect("nothing else to offer");
    assert!(render.needs_browser && render.kind == CandKind::Pdf);
    // Availability F ("Free") entries are kept, including the DOI one.
    assert!(find(&a, |c| c.url == "https://doi.org/10.1038/nature12373").is_some());
}

#[test]
fn europepmc_preprint_record_is_a_preprint_host() {
    let a = parse_with("europepmc__medrxiv.json", &want_doi(MEDRXIV));
    assert_eq!(a.status, Found);
    assert!(
        a.candidates
            .iter()
            .all(|c| c.host_kind == HostKind::Preprint && c.version == Version::Submitted)
    );
    let pdf = pdfs(&a);
    assert_eq!(pdf.len(), 1);
    assert!(
        pdf[0]
            .url
            .ends_with("/early/2023/06/01/2023.05.31.23290770.full.pdf")
    );
    assert_eq!(a.learned.pmcid, None);
}

#[test]
fn europepmc_no_hits_and_subscription_only_records_are_empty() {
    let none = parse_file("europepmc__nonexistent.json");
    assert_eq!(
        (none.status, none.note.as_deref()),
        (Empty, Some("no_hits"))
    );
    let paywalled = parse_with(
        "europepmc__paywalled.json",
        &want_doi("10.1097/SLA.0000000000005603"),
    );
    assert_eq!((paywalled.status, paywalled.candidates.len()), (Empty, 0));
    assert_eq!(
        paywalled.learned.pmid.as_deref(),
        Some("35848743"),
        "the record still teaches its PMID"
    );
}

#[test]
fn europepmc_lite_and_batch_answers_pick_the_requested_record() {
    // `lite` results carry no fullTextUrlList but still give the getPdf candidate and ids.
    let lite = parse_with(
        "europepmc__by-extid-pmid.json",
        &Ids {
            pmid: Some("19621072".into()),
            ..Default::default()
        },
    );
    assert_eq!((lite.status, lite.candidates.len()), (Found, 1));
    assert_eq!(lite.learned.pmcid.as_deref(), Some("PMC2707599"));
    assert_eq!(parse_file("europepmc__by-pmcid.json").status, Found);

    // Four DOIs OR-ed in one query: each request gets its own record.
    let gold = parse_with("europepmc__batch-or-lite.json", &want_doi(GOLD));
    assert_eq!(
        (gold.status, gold.learned.pmcid.as_deref()),
        (Found, Some("PMC2707599"))
    );
    let green = parse_with("europepmc__batch-or-lite.json", &want_doi(GREEN));
    assert_eq!(
        (green.status, green.learned.pmcid.as_deref()),
        (Empty, Some("PMC4221854"))
    );
    let paywalled = parse_with("europepmc__batch-or-lite.json", &want_doi(PAYWALLED));
    assert_eq!(paywalled.learned.pmid.as_deref(), Some("35848743"));
    let medrxiv = parse_with("europepmc__batch-or-lite.json", &want_doi(MEDRXIV));
    assert!(medrxiv.candidates.is_empty() && medrxiv.learned.pmcid.is_none());
    let nobody = parse_with(
        "europepmc__batch-or-lite.json",
        &want_doi("10.1234/not-there"),
    );
    assert_eq!(
        (nobody.status, nobody.note.as_deref()),
        (Empty, Some("no_match"))
    );
    // Matching by PMCID works too.
    let by_pmcid = Ids {
        pmcid: Some("PMC4221854".into()),
        ..Default::default()
    };
    assert_eq!(
        parse_with("europepmc__batch-or-lite.json", &by_pmcid)
            .learned
            .pmid
            .as_deref(),
        Some("23903748")
    );
}

#[test]
fn europepmc_html_and_xml_bodies_are_unparseable() {
    let xml = parse_file("europepmc__fulltext-xml-gold.xml");
    assert_eq!(
        (xml.status, xml.note.as_deref()),
        (Unparseable, Some("bad_json"))
    );
    let challenge = parse_file("europepmc-web__pdf-render-cloudflare-challenge.html");
    assert_eq!(
        (challenge.status, challenge.note.as_deref()),
        (Unparseable, Some("http_403"))
    );
}

// ---------------------------------------------------------------------------------------------------- bioRxiv / medRxiv

#[test]
fn biorxiv_details_builds_the_early_path_and_canonical_urls() {
    let a = parse_with("biorxiv-api__medrxiv-details.json", &want_doi(MEDRXIV));
    assert_eq!(
        (a.status, a.note.as_deref()),
        (Found, None),
        "published is NA"
    );
    assert_eq!(a.candidates.len(), 2);
    let early = &a.candidates[0];
    assert_eq!(
        early.url,
        "https://www.medrxiv.org/content/medrxiv/early/2023/06/01/2023.05.31.23290770.full.pdf"
    );
    assert_eq!(early.signal.as_deref(), Some("biorxiv:early_path"));
    let canonical = &a.candidates[1];
    assert_eq!(
        canonical.url,
        "https://www.medrxiv.org/content/10.1101/2023.05.31.23290770v1.full.pdf"
    );
    assert_eq!(canonical.signal.as_deref(), Some("biorxiv:canonical"));
    for c in &a.candidates {
        assert_eq!(
            (c.kind, c.host_kind, c.version, c.license.as_deref()),
            (
                CandKind::Pdf,
                HostKind::Preprint,
                Version::Submitted,
                Some("cc-by")
            )
        );
        assert_eq!(c.record_doi.as_deref(), Some(MEDRXIV));
        assert_eq!(
            c.landing_url.as_deref(),
            Some("https://www.medrxiv.org/content/10.1101/2023.05.31.23290770v1")
        );
    }
}

#[test]
fn biorxiv_pubs_reports_the_published_doi() {
    let a = parse_file("biorxiv-api__medrxiv-pubs.json");
    assert_eq!((a.status, a.candidates.len()), (Found, 0));
    assert_eq!(
        a.note.as_deref(),
        Some("published_doi:10.1016/j.jpainsymman.2024.03.027")
    );
    let named = parse_with(
        "biorxiv-api__medrxiv-pubs.json",
        &want_doi("10.1101/2024.02.26.24303409"),
    );
    assert_eq!(named.note, a.note);
}

#[test]
fn biorxiv_unknown_preprint_is_empty() {
    let a = parse_file("biorxiv-api__nonexistent.json");
    assert_eq!((a.status, a.note.as_deref()), (Empty, Some("no_posts")));
    let web = parse_file("medrxiv-web__pdf-403-block.html");
    assert_eq!(
        (web.status, web.note.as_deref()),
        (Unparseable, Some("http_403"))
    );
}

#[test]
fn biorxiv_uses_the_latest_version_date() {
    let body = br#"{"messages":[{"status":"ok"}],"collection":[
        {"doi":"10.1101\/2024.01.30.24301991","date":"2024-02-01","version":"1","server":"medRxiv","license":"cc_by_nc_nd","published":"NA"},
        {"doi":"10.1101\/2024.01.30.24301991","date":"2024-03-12","version":"2","server":"medRxiv","license":"cc_by_nc_nd","published":"NA"}]}"#;
    let a = oa::parse(
        Biorxiv,
        &resp(200),
        body,
        &want_doi("10.1101/2024.01.30.24301991"),
    );
    assert_eq!(
        a.candidates[0].url,
        "https://www.medrxiv.org/content/medrxiv/early/2024/03/12/2024.01.30.24301991.full.pdf"
    );
    assert_eq!(
        a.candidates[1].url,
        "https://www.medrxiv.org/content/10.1101/2024.01.30.24301991v2.full.pdf"
    );
    assert_eq!(a.candidates[0].license.as_deref(), Some("cc-by-nc-nd"));
    // bioRxiv uses its own host.
    let bio = br#"{"messages":[{"status":"ok"}],"collection":[{"doi":"10.1101\/2020.01.01.123456","date":"2020-01-02","version":"1","server":"bioRxiv","license":"cc_no","published":"10.1234\/journal.1"}]}"#;
    let b = oa::parse(Biorxiv, &resp(200), bio, &Ids::default());
    assert!(b.candidates[0].url.starts_with(
        "https://www.biorxiv.org/content/biorxiv/early/2020/01/02/2020.01.01.123456.full.pdf"
    ));
    assert_eq!(
        (b.candidates[0].license.as_deref(), b.note.as_deref()),
        (None, Some("published_doi:10.1234/journal.1"))
    );
}

// ---------------------------------------------------------------------------------------------------- PMC S3 and the ID converter

#[test]
fn pmc_s3_metadata_gives_an_https_pdf() {
    let a = parse_file("pmc-s3__meta-gold.json");
    assert_eq!(a.status, Found);
    assert_eq!(a.candidates.len(), 1);
    let c = &a.candidates[0];
    assert_eq!(
        c.url,
        "https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf"
    );
    assert_eq!(
        (c.kind, c.host_kind, c.version, c.license.as_deref()),
        (
            CandKind::Pdf,
            HostKind::Pmc,
            Version::Published,
            Some("cc-by")
        )
    );
    assert_eq!(c.record_doi.as_deref(), Some(GOLD));
    assert_eq!(
        c.landing_url.as_deref(),
        Some("https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/")
    );
    assert!(!c.needs_browser);
    assert_eq!(
        (
            a.learned.pmcid.as_deref(),
            a.learned.pmid.as_deref(),
            a.learned.doi.as_deref()
        ),
        (Some("PMC2707599"), Some("19621072"), Some(GOLD))
    );
}

#[test]
fn pmc_s3_author_manuscript_without_a_pdf_is_empty() {
    let a = parse_file("pmc-s3__meta-green-aam.json");
    assert_eq!(
        (a.status, a.note.as_deref(), a.candidates.len()),
        (Empty, Some("no_pdf"), 0)
    );
    assert_eq!(
        (a.learned.pmcid.as_deref(), a.learned.pmid.as_deref()),
        (Some("PMC4221854"), Some("23903748"))
    );
    // TDM licence wins even if a pdf_url is present.
    let tdm = br#"{"pmcid":"PMC1","license_code":"TDM","pdf_url":"s3://pmc-oa-opendata/PMC1.1/PMC1.1.pdf?md5=x","is_manuscript":true}"#;
    assert_eq!(
        oa::parse(PmcS3, &resp(200), tdm, &Ids::default()).status,
        Empty
    );
    // A manuscript with a Creative Commons licence and a PDF is an accepted version.
    let cc = br#"{"pmcid":"PMC1","license_code":"CC BY-NC","pdf_url":"s3://pmc-oa-opendata/PMC1.3/PMC1.3.pdf?md5=x","is_manuscript":true}"#;
    let a = oa::parse(PmcS3, &resp(200), cc, &Ids::default());
    assert_eq!(
        (a.candidates[0].version, a.candidates[0].license.as_deref()),
        (Version::Accepted, Some("cc-by-nc"))
    );
    assert!(
        a.candidates[0].url.ends_with("/PMC1.3/PMC1.3.pdf"),
        "the key's version is kept"
    );
}

#[test]
fn pmc_s3_listing_picks_the_pdf_or_reports_none() {
    let a = parse_file("pmc-s3__list-PMC2707599.xml");
    assert_eq!(a.status, Found);
    assert_eq!(
        a.candidates[0].url,
        "https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf"
    );
    let aam = parse_file("pmc-s3__list-PMC4221854.xml");
    assert_eq!((aam.status, aam.note.as_deref()), (Empty, Some("no_pdf")));
}

#[test]
fn pmc_s3_missing_objects_and_walled_pages() {
    let missing = parse_file("pmc-s3__meta-nonexistent.xml");
    assert_eq!(
        (missing.status, missing.note.as_deref()),
        (NotFound, Some("http_404"))
    );
    // The same S3 error document served with a 200 is still NoSuchKey.
    let (_, body) = common::oa_fixture("pmc-s3__meta-nonexistent.xml");
    let ok = oa::parse(PmcS3, &resp(200), &body, &Ids::default());
    assert_eq!(
        (ok.status, ok.note.as_deref()),
        (NotFound, Some("no_such_key"))
    );
    let denied = oa::parse(
        PmcS3,
        &resp(200),
        b"<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code></Error>",
        &Ids::default(),
    );
    assert_eq!(
        (denied.status, denied.note.as_deref()),
        (Unparseable, Some("s3_error"))
    );
    // The proof-of-work interstitial is HTML served with a 200: nothing to read.
    let pow = parse_file("pmc-web__pdf-pow-interstitial.html");
    assert_eq!(
        (pow.status, pow.note.as_deref()),
        (Unparseable, Some("not_s3_xml"))
    );
    assert_eq!(parse_file("ncbi-pmc-oa__retired-404.html").status, NotFound);
}

#[test]
fn ncbi_idconv_teaches_ids_only() {
    let gold = parse_with("ncbi-idconv__gold-green-paywalled.json", &want_doi(GOLD));
    assert_eq!(gold.status, Found);
    assert!(gold.candidates.is_empty());
    assert_eq!(
        (gold.learned.pmcid.as_deref(), gold.learned.pmid.as_deref()),
        (Some("PMC2707599"), Some("19621072"))
    );
    let green = parse_with(
        "ncbi-idconv__gold-green-paywalled.json",
        &want_doi("10.1038/NATURE12373"),
    );
    assert_eq!(
        (
            green.learned.pmcid.as_deref(),
            green.learned.pmid.as_deref()
        ),
        (Some("PMC4221854"), Some("23903748"))
    );
    let paywalled = parse_with(
        "ncbi-idconv__gold-green-paywalled.json",
        &want_doi("10.1097/SLA.0000000000005603"),
    );
    assert_eq!(
        (
            paywalled.status,
            paywalled.note.as_deref(),
            paywalled.learned == Default::default()
        ),
        (Empty, Some("not_in_pmc"), true)
    );
    // No request: the first record, with all three ids.
    let first = parse_file("ncbi-idconv__pmid.json");
    assert_eq!(
        (
            first.learned.pmcid.as_deref(),
            first.learned.pmid.as_deref(),
            first.learned.doi.as_deref()
        ),
        (Some("PMC2707599"), Some("19621072"), Some(GOLD))
    );
    let none = parse_file("ncbi-idconv__nonexistent.json");
    assert_eq!(
        (none.status, none.note.as_deref()),
        (Empty, Some("not_in_pmc"))
    );
    // An unrelated request finds no record in a batch answer.
    let other = parse_with(
        "ncbi-idconv__gold-green-paywalled.json",
        &want_doi("10.1234/unrelated"),
    );
    assert_eq!(
        (other.status, other.note.as_deref()),
        (Empty, Some("no_match"))
    );
}

// ---------------------------------------------------------------------------------------------------- arXiv

#[test]
fn arxiv_feed_gives_the_versionless_pdf() {
    let a = parse_file("arxiv__export-api.xml");
    assert_eq!(a.status, Found);
    assert_eq!(a.candidates.len(), 1);
    let c = &a.candidates[0];
    assert_eq!(c.url, "https://arxiv.org/pdf/1706.03762");
    assert_eq!(
        (c.kind, c.host_kind, c.version),
        (CandKind::Pdf, HostKind::Preprint, Version::Submitted)
    );
    assert_eq!(
        c.landing_url.as_deref(),
        Some("https://arxiv.org/abs/1706.03762")
    );
    assert_eq!(a.learned.arxiv.as_deref(), Some("1706.03762"));
    let requested = Ids {
        arxiv: Some("1706.03762".into()),
        ..Default::default()
    };
    assert_eq!(
        parse_with("arxiv__export-api.xml", &requested)
            .learned
            .arxiv,
        None
    );
    // The wrong entry is not used.
    let other = Ids {
        arxiv: Some("hep-th/9901001".into()),
        ..Default::default()
    };
    assert_eq!(parse_with("arxiv__export-api.xml", &other).status, Empty);
}

#[test]
fn arxiv_unknown_id_is_empty() {
    let a = parse_file("arxiv__export-api-nonexistent.xml");
    assert_eq!((a.status, a.candidates.len()), (Empty, 0));
}

// ---------------------------------------------------------------------------------------------------- Unpaywall

#[test]
fn unpaywall_gold_and_green() {
    let gold = parse_with("unpaywall__gold.json", &want_doi(GOLD));
    assert_eq!(
        (gold.status, gold.oa_status.as_deref()),
        (Found, Some("gold"))
    );
    assert_eq!(gold.learned.pmcid.as_deref(), Some("PMC2707599"));
    let publisher = find(&gold, |c| c.url == format!("https://doi.org/{GOLD}")).unwrap();
    assert_eq!(
        (
            publisher.kind,
            publisher.host_kind,
            publisher.license.as_deref()
        ),
        (CandKind::Landing, HostKind::Publisher, Some("cc-by"))
    );
    let iris = find(&gold, |c| c.kind == CandKind::Pdf).unwrap();
    assert_eq!(
        (host(iris).as_str(), iris.version, iris.host_kind),
        ("iris.unimore.it", Version::Submitted, HostKind::Repository)
    );
    assert_eq!(
        iris.landing_url.as_deref(),
        Some("https://hdl.handle.net/11380/1368022")
    );

    let green = parse_with("unpaywall__green.json", &want_doi(GREEN));
    assert_eq!(
        (green.status, green.oa_status.as_deref()),
        (Found, Some("bronze"))
    );
    let nature = find(&green, |c| host(c) == "www.nature.com").unwrap();
    assert_eq!(
        (nature.kind, nature.host_kind, nature.version),
        (CandKind::Pdf, HostKind::Publisher, Version::Published)
    );
    assert!(find(&green, |c| c.url == "https://arxiv.org/pdf/1304.1068").is_some());
    assert!(
        find(&green, |c| c.url
            == "https://nrs.harvard.edu/urn-3:HUL.InstRepos:12285462")
        .is_some(),
        "http upgraded"
    );
    assert_eq!(green.learned.pmcid.as_deref(), Some("PMC4221854"));
}

#[test]
fn unpaywall_medrxiv_closed_and_errors() {
    let med = parse_with("unpaywall__medrxiv.json", &want_doi(MEDRXIV));
    assert_eq!(
        (med.status, med.oa_status.as_deref()),
        (Found, Some("green"))
    );
    let pdf = find(&med, |c| c.kind == CandKind::Pdf).unwrap();
    assert!(
        pdf.url
            .ends_with("/early/2023/06/01/2023.05.31.23290770.full.pdf")
    );
    assert_eq!(
        (pdf.license.as_deref(), pdf.host_kind),
        (Some("cc-by"), HostKind::Preprint)
    );

    let closed = parse_with("unpaywall__paywalled.json", &want_doi(PAYWALLED));
    assert_eq!(
        (
            closed.status,
            closed.oa_status.as_deref(),
            closed.candidates.len()
        ),
        (Empty, Some("closed"), 0)
    );

    for file in [
        "unpaywall__arxiv-404.html",
        "unpaywall__nonexistent-404.html",
        "unpaywall__error-invalid-doi-404.html",
    ] {
        assert_eq!(parse_file(file).status, NotFound, "{file}");
    }
    for file in [
        "unpaywall__error-no-email.json",
        "unpaywall__error-test-example-email.json",
    ] {
        let a = parse_file(file);
        assert_eq!(
            (a.status, a.note.as_deref()),
            (Unparseable, Some("bad_email")),
            "{file}"
        );
    }
}

// ---------------------------------------------------------------------------------------------------- Semantic Scholar

#[test]
fn semantic_scholar_single_papers() {
    let gold = parse_with("semanticscholar__gold.json", &want_doi(GOLD));
    assert_eq!(
        (
            gold.status,
            gold.oa_status.as_deref(),
            gold.candidates.len()
        ),
        (Found, Some("gold"), 1)
    );
    // `openAccessPdf.url` is the DOI landing page for this paper: a page, not a file.
    let c = &gold.candidates[0];
    assert_eq!(
        (c.kind, c.host_kind, c.license.as_deref(), c.version),
        (
            CandKind::Landing,
            HostKind::Aggregator,
            Some("cc-by"),
            Version::Unknown
        )
    );
    assert_eq!(
        (
            gold.learned.pmid.as_deref(),
            gold.learned.pmcid.as_deref(),
            gold.learned.doi.as_deref()
        ),
        (Some("19621072"), Some("PMC2707599"), None)
    );

    let green = parse_with("semanticscholar__green.json", &want_doi(GREEN));
    let nature = &green.candidates[0];
    assert_eq!(
        (nature.kind, nature.host_kind, nature.url.as_str()),
        (
            CandKind::Pdf,
            HostKind::Publisher,
            "https://www.nature.com/articles/nature12373.pdf"
        )
    );
    assert_eq!(
        (green.oa_status.as_deref(), green.learned.arxiv.as_deref()),
        (Some("bronze"), Some("1304.1068"))
    );

    let closed = parse_with("semanticscholar__paywalled.json", &want_doi(PAYWALLED));
    assert_eq!(
        (
            closed.status,
            closed.candidates.len(),
            closed.learned.pmid.as_deref()
        ),
        (Empty, 0, Some("35848743"))
    );
}

#[test]
fn semantic_scholar_errors_and_throttling() {
    for file in [
        "semanticscholar__arxiv.json",
        "semanticscholar__medrxiv.json",
        "semanticscholar__nonexistent.json",
    ] {
        assert_eq!(parse_file(file).status, NotFound, "{file}");
    }
    let limited = parse_file("semanticscholar__error-429-unauthenticated.json");
    assert_eq!((limited.status, limited.retry_after_s), (Throttled, None));
    let (mut meta, body) = common::oa_fixture("semanticscholar__error-429-unauthenticated.json");
    meta.headers.insert("retry-after".into(), "30".into());
    let with_header = oa::parse(SemanticScholar, &meta, &body, &Ids::default());
    assert_eq!(
        (with_header.status, with_header.retry_after_s),
        (Throttled, Some(30))
    );
}

#[test]
fn semantic_scholar_batch_skips_null_entries_and_matches_the_request() {
    let first = parse_file("semanticscholar__batch.json");
    assert_eq!(
        first.learned.pmid.as_deref(),
        Some("19621072"),
        "first non-null entry"
    );
    let green = parse_with("semanticscholar__batch.json", &want_doi(GREEN));
    assert_eq!(
        green.candidates[0].url,
        "https://www.nature.com/articles/nature12373.pdf"
    );
    let other_ids = parse_with("semanticscholar__batch-other-ids.json", &want_doi(GREEN));
    assert_eq!(other_ids.status, Found);
    // Without a request the first entry is the arXiv paper, which is not open access.
    let arxiv_first = parse_file("semanticscholar__batch-other-ids.json");
    assert_eq!(
        (arxiv_first.status, arxiv_first.learned.arxiv.as_deref()),
        (Empty, Some("1706.03762"))
    );
}

// ---------------------------------------------------------------------------------------------------- CORE

#[test]
fn core_discover_is_an_unverified_aggregator_landing_page() {
    for file in [
        "core__discover-gold-no-key.json",
        "core__discover-green-no-key.json",
        "core__discover-paywalled-no-key.json",
    ] {
        let a = parse_file(file);
        assert_eq!(
            (a.status, a.candidates.len(), a.note.as_deref()),
            (Found, 1, Some("unverified")),
            "{file}"
        );
        let c = &a.candidates[0];
        assert_eq!(
            (c.kind, c.host_kind, c.version),
            (CandKind::Landing, HostKind::Aggregator, Version::Unknown),
            "{file}"
        );
        assert!(
            c.url.starts_with("https://core.ac.uk/reader/"),
            "{file}: {}",
            c.url
        );
        assert!(!c.url.contains("key="), "the redirect key is not kept");
    }
}

#[test]
fn core_output_gives_download_and_source_links() {
    let a = parse_with("core__output-green-no-key.json", &want_doi(GREEN));
    assert_eq!(a.status, Found);
    let pdf = find(&a, |c| c.kind == CandKind::Pdf).unwrap();
    assert_eq!(pdf.url, "https://core.ac.uk/download/28948651.pdf");
    let source = find(&a, |c| c.kind == CandKind::Landing).unwrap();
    assert_eq!(
        source.url,
        "https://nrs.harvard.edu/urn-3:HUL.InstRepos:12285462"
    );
    assert!(
        a.candidates
            .iter()
            .all(|c| c.host_kind == HostKind::Aggregator && c.version == Version::Unknown)
    );
    // A different work than the one requested is not offered.
    let mismatch = parse_with("core__output-green-no-key.json", &want_doi(GOLD));
    assert_eq!(
        (mismatch.status, mismatch.note.as_deref()),
        (Empty, Some("doi_mismatch"))
    );
}

#[test]
fn core_errors() {
    let missing = parse_file("core__output-paywalled-hit-no-key.json");
    assert_eq!(
        (missing.status, missing.note.as_deref()),
        (NotFound, Some("http_404"))
    );
    let server = parse_file("core__search-no-key-500.json");
    assert_eq!(
        (server.status, server.note.as_deref()),
        (Unparseable, Some("http_500"))
    );
}

// ---------------------------------------------------------------------------------------------------- Crossref

#[test]
fn crossref_gold_has_a_landing_page_and_a_cc_licence() {
    let a = parse_with("crossref__gold.json", &want_doi(GOLD));
    assert_eq!((a.status, a.oa_status.as_deref()), (Found, None));
    assert!(
        pdfs(&a).is_empty(),
        "the only link is an unspecified syndication link"
    );
    let landing = &a.candidates[0];
    assert_eq!(
        landing.url,
        "https://dx.plos.org/10.1371/journal.pmed.1000097"
    );
    assert_eq!(
        (
            landing.kind,
            landing.license.as_deref(),
            landing.host_kind,
            landing.source
        ),
        (CandKind::Landing, Some("cc-by"), HostKind::Publisher, Doi)
    );
    assert_eq!(landing.signal.as_deref(), Some("crossref:resource_primary"));
    assert_eq!(a.learned.doi, None, "the request already had a DOI");
    assert_eq!(
        parse_file("crossref__gold.json").learned.doi.as_deref(),
        Some(GOLD)
    );
}

#[test]
fn crossref_green_flags_the_text_mining_pdf() {
    let a = parse_with("crossref__green.json", &want_doi(GREEN));
    let pdf = find(&a, |c| c.kind == CandKind::Pdf).unwrap();
    assert_eq!(pdf.url, "https://www.nature.com/articles/nature12373.pdf");
    assert_eq!(pdf.signal.as_deref(), Some("crossref:link_tdm"));
    assert_eq!(
        (pdf.version, pdf.license.as_deref()),
        (Version::Published, None),
        "springer.com/tdm is not an open licence"
    );
    assert_eq!(
        pdfs(&a).len(),
        1,
        "the similarity-checking link to the same URL is merged"
    );
    let landing = find(&a, |c| c.kind == CandKind::Landing).unwrap();
    assert_eq!(landing.url, "https://www.nature.com/articles/nature12373");
}

#[test]
fn crossref_preprints_and_closed_articles() {
    let med = parse_with("crossref__medrxiv.json", &want_doi(MEDRXIV));
    assert_eq!(
        (med.status, med.oa_status.as_deref()),
        (Found, Some("preprint"))
    );
    assert_eq!(
        med.candidates[0].url, "https://medrxiv.org/lookup/doi/10.1101/2023.05.31.23290770",
        "http upgraded"
    );
    assert!(pdfs(&med).is_empty());
    let paywalled = parse_with("crossref__paywalled.json", &want_doi(PAYWALLED));
    assert_eq!(
        (paywalled.status, paywalled.oa_status.as_deref()),
        (Found, None),
        "Crossref never claims open access"
    );
    assert_eq!(paywalled.candidates[0].host_kind, HostKind::Publisher);
    assert_eq!(paywalled.candidates[0].license, None);
    let missing = parse_file("crossref__nonexistent-404.txt");
    assert_eq!(
        (missing.status, missing.note.as_deref()),
        (NotFound, Some("http_404"))
    );
}

// ---------------------------------------------------------------------------------------------------- Alma

fn alma_data(name: &str) -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/oa")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn alma_held_item_has_one_landing_candidate_per_full_text_service() {
    let a = oa::parse(
        Alma,
        &resp(200),
        &alma_data("alma-held.xml"),
        &want_doi(GREEN),
    );
    assert_eq!((a.status, a.oa_status.as_deref()), (Found, None));
    assert_eq!(
        a.candidates.len(),
        2,
        "the getHoldings service is not a full-text service"
    );
    for c in &a.candidates {
        assert_eq!(
            (c.kind, c.source, c.record_doi.clone(), c.needs_browser),
            (CandKind::Landing, Alma, None, false)
        );
        assert!(c.url.starts_with("https://kcl.alma.exlibrisgroup.com/view/action/uresolver.do?operation=resolveService&package_service_id="), "{}", c.url);
        assert!(
            c.url.contains("&institutionId=6881&customerId=6880"),
            "entities are decoded: {}",
            c.url
        );
    }
    assert_eq!(a.candidates[0].signal.as_deref(), Some("alma:Nature"));
    assert_eq!(
        a.candidates[1].signal.as_deref(),
        Some("alma:ProQuest Central UK/Ireland")
    );
}

#[test]
fn alma_free_and_not_held() {
    let free = oa::parse(
        Alma,
        &resp(200),
        &alma_data("alma-free.xml"),
        &Ids::default(),
    );
    assert_eq!(
        (
            free.status,
            free.oa_status.as_deref(),
            free.candidates.len()
        ),
        (Found, Some("free"), 1)
    );
    assert_eq!(
        free.candidates[0].signal.as_deref(),
        Some("alma:Free E-journals & open archives")
    );
    assert!(
        free.candidates[0]
            .url
            .contains("package_service_id=61300000010006881"),
        "resolution_url given as a <key>"
    );

    let none = oa::parse(
        Alma,
        &resp(200),
        &alma_data("alma-not-held.xml"),
        &Ids::default(),
    );
    assert_eq!(
        (none.status, none.note.as_deref(), none.candidates.len()),
        (Empty, Some("not_held"), 0)
    );

    let login = oa::parse(
        Alma,
        &resp(200),
        b"<!DOCTYPE html><html><body>Sign in</body></html>",
        &Ids::default(),
    );
    assert_eq!(
        (login.status, login.note.as_deref()),
        (Unparseable, Some("not_alma"))
    );
}

// ---------------------------------------------------------------------------------------------------- Zotero OA

#[test]
fn zotero_oa_gold_hits() {
    let a = parse_file("zotero-oa__gold.json");
    assert_eq!(a.status, Found);
    let iris = find(&a, |c| c.kind == CandKind::Pdf).unwrap();
    assert_eq!(
        (host(iris).as_str(), iris.version),
        ("iris.unimore.it", Version::Submitted)
    );
    assert_eq!(
        iris.landing_url.as_deref(),
        Some("https://hdl.handle.net/11380/1368022")
    );
    let doi_page = find(&a, |c| c.url == format!("https://doi.org/{GOLD}")).unwrap();
    assert_eq!(
        (doi_page.kind, doi_page.version),
        (CandKind::Landing, Version::Published)
    );
    // The legacy PMC page URL teaches the PMCID.
    assert_eq!(a.learned.pmcid.as_deref(), Some("PMC2707599"));
    assert_eq!(pdfs(&a).len(), 1, "every other hit is a page only");
}

#[test]
fn zotero_oa_green_and_medrxiv_hits() {
    let green = parse_file("zotero-oa__green.json");
    let nature = find(&green, |c| host(c) == "www.nature.com").unwrap();
    assert_eq!(
        (nature.kind, nature.version),
        (CandKind::Pdf, Version::Published)
    );
    let arxiv = find(&green, |c| c.url == "https://arxiv.org/pdf/1304.1068").unwrap();
    assert_eq!(
        (arxiv.version, arxiv.host_kind),
        (Version::Submitted, HostKind::Preprint)
    );
    assert!(
        find(&green, |c| c.url == "https://arxiv.org/abs/1304.1068").is_some(),
        "http upgraded"
    );
    assert_eq!(green.learned.pmcid.as_deref(), Some("PMC4221854"));

    let med = parse_file("zotero-oa__medrxiv.json");
    assert_eq!(med.candidates.len(), 2);
    assert_eq!(
        (med.candidates[0].kind, med.candidates[0].version),
        (CandKind::Pdf, Version::Accepted)
    );
    assert_eq!(
        med.candidates[0].landing_url.as_deref(),
        Some("https://doi.org/10.1101/2023.05.31.23290770")
    );
}

#[test]
fn zotero_oa_empty_arrays_are_empty() {
    for file in [
        "zotero-oa__arxiv.json",
        "zotero-oa__paywalled.json",
        "zotero-oa__nonexistent.json",
        "zotero-oa__invalid-doi.json",
    ] {
        let a = parse_file(file);
        assert_eq!((a.status, a.candidates.len()), (Empty, 0), "{file}");
    }
}

#[test]
fn zotero_oa_hit_conversion() {
    use oa::zotero_oa::candidates_from_hit;
    let both = candidates_from_hit(
        "https://example.org/a.pdf",
        Some("http://example.org/a"),
        Some("acceptedVersion"),
    );
    assert_eq!(both.len(), 2);
    assert_eq!(
        (both[0].kind, both[0].version, both[0].source),
        (CandKind::Pdf, Version::Accepted, ZoteroOa)
    );
    assert_eq!(
        both[0].landing_url.as_deref(),
        Some("https://example.org/a")
    );
    assert_eq!(
        (both[1].kind, both[1].url.as_str()),
        (CandKind::Landing, "https://example.org/a")
    );
    // Same URL twice: one candidate. Page only: one landing candidate. Nothing: nothing.
    assert_eq!(
        candidates_from_hit(
            "https://example.org/a.pdf",
            Some("https://example.org/a.pdf"),
            None
        )
        .len(),
        1
    );
    assert_eq!(
        candidates_from_hit("", Some("https://doi.org/10.1/x"), None).len(),
        1
    );
    assert!(candidates_from_hit("", None, None).is_empty());
    assert!(candidates_from_hit("ftp://example.org/a.pdf", Some(""), Some("weird")).is_empty());
    assert_eq!(
        candidates_from_hit("https://example.org/a.pdf", None, Some("weird"))[0].version,
        Version::Unknown
    );
}
