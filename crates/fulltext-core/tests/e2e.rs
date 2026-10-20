//! Cross-module acceptance through the exact wire form the wasm uses (`docs/wp/core-integration.md` section 1).
//!
//! `fx_inspect` takes a JSON `InspectMeta` plus raw body bytes and answers a JSON `Inspection`. [`run`] does the same
//! round trip natively: serialise the meta, parse it back, call [`fulltext_core::inspect::inspect`], serialise the
//! answer, parse it back and compare. Every assertion below therefore also proves that the model types survive the
//! wire unchanged.
//!
//! Inputs are the recorded fixtures (`fixtures/oa`, `fixtures/html`), the hand-made pages in `tests/data/page`, and
//! synthetic PDFs. The recordings are live captures (16 of the 31 pages come from the Wayback Machine), so the
//! assertions are structural, never byte-for-byte.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;

use fulltext_core::ids;
use fulltext_core::inspect::{MAX_BODY_BYTES, inspect, login_state};
use fulltext_core::model::{
    ApiInspection, ApiStatus, CandKind, Candidate, HostKind, Idp, Ids, InspectMeta, InspectSpec,
    Inspection, ItemFields, LoginState, PageClass, PageInspection, PdfVerdict, RespMeta, Route,
    Source, Vendor,
};
use fulltext_core::rank::{self, RankPrefs, tier};
use fulltext_core::urls::{canonical_key, host_of};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use ApiStatus::{Empty, Found, NotFound, Throttled, Unparseable};

// ---------------------------------------------------------------------------------------------------- the wire

/// What `fx_inspect` does, natively: meta through JSON and back, `inspect`, answer through JSON and back.
fn run(spec: InspectSpec, resp: RespMeta, body: &[u8]) -> Inspection {
    let meta = InspectMeta { spec, resp };
    let wire = serde_json::to_vec(&meta).unwrap();
    let parsed: InspectMeta = serde_json::from_slice(&wire).unwrap();
    assert_eq!(parsed, meta, "InspectMeta changed on the wire");
    let answer = inspect(&parsed, body);
    let json = serde_json::to_vec(&answer).unwrap();
    let back: Inspection = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        back,
        answer,
        "Inspection changed on the wire: {}",
        String::from_utf8_lossy(&json)
    );
    back
}

/// The `"expect"` tag of an answer as JS sees it.
fn tag(i: &Inspection) -> String {
    serde_json::to_value(i).unwrap()["expect"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn as_api(i: Inspection) -> ApiInspection {
    match i {
        Inspection::Api(a) => a,
        other => panic!("expected Api, got {other:?}"),
    }
}

fn as_page(i: Inspection) -> PageInspection {
    match i {
        Inspection::Page(p) => p,
        other => panic!("expected Page, got {other:?}"),
    }
}

fn as_pdf(i: Inspection) -> PdfVerdict {
    match i {
        Inspection::Pdf(v) => v,
        other => panic!("expected Pdf, got {other:?}"),
    }
}

fn as_probe(i: Inspection) -> PageClass {
    match i {
        Inspection::Probe { class } => class,
        other => panic!("expected Probe, got {other:?}"),
    }
}

fn resp(status: u16, url: &str) -> RespMeta {
    RespMeta {
        status,
        final_url: url.to_owned(),
        body_len: 0,
        ..Default::default()
    }
}

fn resp_for(status: u16, url: &str, content_type: &str, body: &[u8]) -> RespMeta {
    let mut r = resp(status, url);
    r.headers
        .insert("content-type".to_owned(), content_type.to_owned());
    r.body_len = body.len() as u64;
    r
}

fn page_spec(requested: &Ids, route: Route) -> InspectSpec {
    InspectSpec::Page {
        requested: requested.clone(),
        route,
    }
}

fn pdf_spec(requested: &Ids, route: Route) -> InspectSpec {
    InspectSpec::Pdf {
        requested: requested.clone(),
        route,
    }
}

fn read_data(name: &str) -> String {
    let path = common::root()
        .join("crates/fulltext-core/tests/data/page")
        .join(name);
    String::from_utf8(std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .unwrap()
}

// ---------------------------------------------------------------------------------------------------- cases

/// The five test works of `fixtures/oa/manifest.json`, plus the nonexistent DOI. `none` is a request without a DOI
/// (a title search).
fn case_doi(case: &str) -> &'static str {
    match case {
        "gold" => "10.1371/journal.pmed.1000097",
        "green" => "10.1038/nature12373",
        "arxiv" => "10.48550/arXiv.1706.03762",
        "medrxiv" => "10.1101/2023.05.31.23290770",
        "paywalled" => "10.1097/SLA.0000000000005603",
        "nonexistent" => "10.9999/zft.nonexistent.000001",
        "none" => "",
        other => panic!("unknown case {other}"),
    }
}

/// `Ids` exactly as phase 0 of a run builds them: `ids::normalise` over an item that only has a DOI.
fn requested(case: &str) -> Ids {
    ids::normalise(&ItemFields {
        doi: case_doi(case).to_owned(),
        ..Default::default()
    })
}

/// The parser a manifest file belongs to (the prefix before `__`).
fn source_of(file: &str) -> Source {
    match file.split("__").next().unwrap_or_default() {
        "openalex" | "openalex-content" => Source::Openalex,
        "europepmc" | "europepmc-web" => Source::Europepmc,
        "biorxiv-api" | "medrxiv-web" => Source::Biorxiv,
        "pmc-s3" | "ncbi-idconv" | "ncbi-pmc-oa" | "pmc-web" => Source::PmcS3,
        "arxiv" => Source::Arxiv,
        "unpaywall" => Source::Unpaywall,
        "semanticscholar" => Source::SemanticScholar,
        "core" => Source::Core,
        "crossref" => Source::Doi,
        "zotero-oa" => Source::ZoteroOa,
        other => panic!("no parser for fixture prefix {other}"),
    }
}

fn api(file: &str, req: &Ids) -> ApiInspection {
    let (resp, body) = common::oa_fixture(file);
    as_api(run(
        InspectSpec::Api {
            source: source_of(file),
            requested: req.clone(),
        },
        resp,
        &body,
    ))
}

// ---------------------------------------------------------------------------------------------------- 1. API fixtures

/// `(file, case the request is built from, expected status)`. The case decides the DOI that `ids::normalise` turns
/// into the request. Files that no case names (list answers, error pages) use the work they were recorded for.
#[rustfmt::skip]
const API_TABLE: &[(&str, &str, ApiStatus)] = &[
    ("arxiv__export-api-nonexistent.xml", "nonexistent", Empty),
    ("arxiv__export-api.xml", "arxiv", Found),
    ("biorxiv-api__medrxiv-details.json", "medrxiv", Found),
    ("biorxiv-api__medrxiv-pubs.json", "medrxiv", Found),
    ("biorxiv-api__nonexistent.json", "nonexistent", Empty),
    ("core__discover-gold-no-key.json", "gold", Found),
    ("core__discover-green-no-key.json", "green", Found),
    ("core__discover-paywalled-no-key.json", "paywalled", Found),
    ("core__output-green-no-key.json", "green", Found),
    ("core__output-paywalled-hit-no-key.json", "paywalled", NotFound),
    ("core__search-no-key-500.json", "gold", Unparseable),
    ("crossref__gold.json", "gold", Found),
    ("crossref__green.json", "green", Found),
    ("crossref__medrxiv.json", "medrxiv", Found),
    ("crossref__nonexistent-404.txt", "nonexistent", NotFound),
    ("crossref__paywalled.json", "paywalled", Found),
    ("europepmc__batch-or-lite.json", "gold", Found),
    ("europepmc__by-extid-pmid.json", "gold", Found),
    ("europepmc__by-pmcid.json", "gold", Found),
    ("europepmc__fulltext-xml-gold.xml", "gold", Unparseable),
    ("europepmc__gold.json", "gold", Found),
    ("europepmc__green.json", "green", Found),
    ("europepmc__medrxiv.json", "medrxiv", Found),
    ("europepmc__nonexistent.json", "nonexistent", Empty),
    ("europepmc__paywalled.json", "paywalled", Empty),
    ("europepmc-web__pdf-render-cloudflare-challenge.html", "gold", Unparseable),
    ("medrxiv-web__pdf-403-block.html", "medrxiv", Unparseable),
    ("ncbi-idconv__gold-green-paywalled.json", "gold", Found),
    ("ncbi-idconv__nonexistent.json", "nonexistent", Empty),
    ("ncbi-idconv__pmid.json", "gold", Found),
    ("ncbi-pmc-oa__retired-404.html", "gold", NotFound),
    ("openalex__arxiv.json", "arxiv", Found),
    ("openalex__by-pmcid-404.html", "gold", NotFound),
    ("openalex__by-pmcid-filter.json", "gold", Empty),
    ("openalex__by-pmid.json", "gold", Found),
    ("openalex__filter-doi-select.json", "gold", Found),
    ("openalex__gold.json", "gold", Found),
    ("openalex__green-select.json", "green", Found),
    ("openalex__green.json", "green", Found),
    ("openalex__medrxiv.json", "medrxiv", Found),
    ("openalex__nonexistent-404.html", "nonexistent", NotFound),
    ("openalex__paywalled.json", "paywalled", Empty),
    ("openalex__title-search.json", "none", Found),
    ("openalex-content__no-key.json", "gold", Unparseable),
    ("pmc-s3__list-PMC2707599.xml", "gold", Found),
    ("pmc-s3__list-PMC4221854.xml", "green", Empty),
    ("pmc-s3__meta-gold.json", "gold", Found),
    ("pmc-s3__meta-green-aam.json", "green", Empty),
    ("pmc-s3__meta-nonexistent.xml", "nonexistent", NotFound),
    ("pmc-web__pdf-pow-interstitial.html", "gold", Unparseable),
    ("semanticscholar__arxiv.json", "arxiv", NotFound),
    ("semanticscholar__batch-other-ids.json", "gold", Found),
    ("semanticscholar__batch.json", "gold", Found),
    ("semanticscholar__error-429-unauthenticated.json", "gold", Throttled),
    ("semanticscholar__gold.json", "gold", Found),
    ("semanticscholar__green.json", "green", Found),
    ("semanticscholar__medrxiv.json", "medrxiv", NotFound),
    ("semanticscholar__nonexistent.json", "nonexistent", NotFound),
    ("semanticscholar__paywalled.json", "paywalled", Empty),
    ("unpaywall__arxiv-404.html", "arxiv", NotFound),
    ("unpaywall__error-invalid-doi-404.html", "gold", NotFound),
    ("unpaywall__error-no-email.json", "gold", Unparseable),
    ("unpaywall__error-test-example-email.json", "gold", Unparseable),
    ("unpaywall__gold.json", "gold", Found),
    ("unpaywall__green.json", "green", Found),
    ("unpaywall__medrxiv.json", "medrxiv", Found),
    ("unpaywall__nonexistent-404.html", "nonexistent", NotFound),
    ("unpaywall__paywalled.json", "paywalled", Empty),
    ("zotero-oa__arxiv.json", "arxiv", Empty),
    ("zotero-oa__gold.json", "gold", Found),
    ("zotero-oa__green.json", "green", Found),
    ("zotero-oa__invalid-doi.json", "gold", Empty),
    ("zotero-oa__medrxiv.json", "medrxiv", Found),
    ("zotero-oa__nonexistent.json", "nonexistent", Empty),
    ("zotero-oa__paywalled.json", "paywalled", Empty),
];

/// Manifest prefixes that have a parser (DataCite and DOAJ captures are not parsed in v1).
const PARSED_PREFIXES: &[&str] = &[
    "openalex",
    "europepmc",
    "biorxiv-api",
    "pmc-s3",
    "ncbi-idconv",
    "arxiv",
    "unpaywall",
    "semanticscholar",
    "core",
    "crossref",
    "zotero-oa",
];

fn is_parsed(file: &str) -> bool {
    let prefix = file.split("__").next().unwrap_or_default();
    PARSED_PREFIXES.contains(&prefix)
        || [
            "openalex-content",
            "europepmc-web",
            "medrxiv-web",
            "ncbi-pmc-oa",
            "pmc-web",
        ]
        .contains(&prefix)
}

#[test]
fn the_api_table_covers_the_manifest_exactly() {
    let manifest: BTreeSet<String> = common::oa_manifest()
        .iter()
        .map(|e| e["file"].as_str().unwrap().to_owned())
        .collect();
    let table: BTreeSet<&str> = API_TABLE.iter().map(|(f, _, _)| *f).collect();
    assert_eq!(table.len(), API_TABLE.len(), "duplicate rows");
    let missing: Vec<&String> = manifest
        .iter()
        .filter(|f| is_parsed(f) && !table.contains(f.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "manifest files with a parser but no row: {missing:?}"
    );
    let stale: Vec<&&str> = table.iter().filter(|f| !manifest.contains(**f)).collect();
    assert!(stale.is_empty(), "rows without a manifest file: {stale:?}");
}

#[test]
fn every_api_fixture_goes_through_the_wire_with_the_expected_status() {
    for (file, case, want) in API_TABLE {
        let (resp, body) = common::oa_fixture(file);
        let source = source_of(file);
        let got = run(
            InspectSpec::Api {
                source,
                requested: requested(case),
            },
            resp,
            &body,
        );
        assert_eq!(tag(&got), "api", "{file}");
        let a = as_api(got);
        assert_eq!(a.source, source, "{file}");
        assert_eq!(a.status, *want, "{file} [{case}]: note={:?}", a.note);
        match a.status {
            // The API said nothing usable: no candidates, and the note says why.
            Empty | NotFound | Throttled | Unparseable => assert!(
                a.candidates.is_empty(),
                "{file}: {:?} with candidates",
                a.status
            ),
            Found => assert!(
                !a.candidates.is_empty() || a.note.is_some() || a.learned != Default::default(),
                "{file}: Found but nothing found"
            ),
        }
        assert!(
            a.candidates.iter().all(|c| c.url.starts_with("https://")),
            "{file}"
        );
    }
}

#[test]
fn http_errors_and_throttles_are_reported_with_their_notes() {
    let note = |file: &str, case: &str| api(file, &requested(case)).note;
    assert_eq!(
        note("openalex__nonexistent-404.html", "nonexistent").as_deref(),
        Some("http_404")
    );
    assert_eq!(
        note("unpaywall__error-no-email.json", "gold").as_deref(),
        Some("bad_email")
    );
    assert_eq!(
        note("semanticscholar__error-429-unauthenticated.json", "gold").as_deref(),
        Some("http_429")
    );
    assert_eq!(
        note(
            "europepmc-web__pdf-render-cloudflare-challenge.html",
            "gold"
        )
        .as_deref(),
        Some("http_403")
    );
    // Retry-After comes from the headers the host passes along.
    let (mut r, body) = common::oa_fixture("semanticscholar__error-429-unauthenticated.json");
    r.headers.insert("retry-after".to_owned(), "17".to_owned());
    let a = as_api(run(
        InspectSpec::Api {
            source: Source::SemanticScholar,
            requested: requested("gold"),
        },
        r,
        &body,
    ));
    assert_eq!((a.status, a.retry_after_s), (Throttled, Some(17)));
}

#[test]
fn a_200_with_nothing_to_offer_is_empty_and_still_teaches_ids() {
    // OpenAlex knows the closed work and its PMID; the plugin needs that PMID for the next request.
    let a = api("openalex__paywalled.json", &requested("paywalled"));
    assert_eq!((a.status, a.oa_status.as_deref()), (Empty, Some("closed")));
    assert_eq!(a.learned.pmid.as_deref(), Some("35848743"));
    // bioRxiv `pubs`: the preprint was published; the engine reads the published DOI from the note.
    let pubs = api("biorxiv-api__medrxiv-pubs.json", &requested("medrxiv"));
    assert_eq!(pubs.status, Found);
    assert!(
        pubs.note
            .as_deref()
            .is_some_and(|n| n.starts_with("published_doi:10.")),
        "{:?}",
        pubs.note
    );
}

#[test]
fn learned_ids_flow_into_the_next_request() {
    // Phase 0: an item with a DOI only.
    let mut ids = requested("gold");
    assert_eq!((ids.pmid.as_deref(), ids.pmcid.as_deref()), (None, None));
    // Phase 1: OpenAlex and Europe PMC answer; their learned ids are folded in without overwriting anything.
    let openalex = api("openalex__gold.json", &ids);
    assert!(ids::apply_learned(&mut ids, &openalex.learned, "openalex"));
    assert_eq!(
        (ids.pmid.as_deref(), ids.pmcid.as_deref()),
        (Some("19621072"), Some("PMC2707599"))
    );
    assert_eq!(
        ids.sources.get("pmcid").map(String::as_str),
        Some("openalex")
    );
    let europepmc = api("europepmc__gold.json", &ids);
    assert!(
        !ids::apply_learned(&mut ids, &europepmc.learned, "europepmc"),
        "same ids again: nothing changes"
    );
    assert_eq!(
        ids.sources.get("pmcid").map(String::as_str),
        Some("openalex"),
        "the first source keeps the credit"
    );
    // Phase 2: the PMC listing is now addressable, and its candidate is the open-data PDF of that PMCID.
    let s3 = api("pmc-s3__list-PMC2707599.xml", &ids);
    assert_eq!(s3.status, Found);
    assert!(s3.candidates.iter().all(|c| c.url.contains("PMC2707599")
        && host_of(&c.url).as_deref() == Some("pmc-oa-opendata.s3.amazonaws.com")));
    // The wire form of the enriched `Ids` is what the next InspectSpec carries.
    let again: Ids = serde_json::from_value(serde_json::to_value(&ids).unwrap()).unwrap();
    assert_eq!(again, ids);
}

// ---------------------------------------------------------------------------------------------------- 2. source pools

/// The fixtures that exist for each test work, with their parser implied by the file name.
const POOLS: &[(&str, &[&str])] = &[
    (
        "gold",
        &[
            "openalex__gold.json",
            "europepmc__gold.json",
            "unpaywall__gold.json",
            "semanticscholar__gold.json",
            "crossref__gold.json",
            "zotero-oa__gold.json",
            "pmc-s3__meta-gold.json",
            "pmc-s3__list-PMC2707599.xml",
            "core__discover-gold-no-key.json",
        ],
    ),
    (
        "green",
        &[
            "openalex__green.json",
            "europepmc__green.json",
            "unpaywall__green.json",
            "semanticscholar__green.json",
            "crossref__green.json",
            "zotero-oa__green.json",
            "core__discover-green-no-key.json",
            "core__output-green-no-key.json",
            "pmc-s3__meta-green-aam.json",
            "pmc-s3__list-PMC4221854.xml",
        ],
    ),
    (
        "arxiv",
        &[
            "arxiv__export-api.xml",
            "openalex__arxiv.json",
            "semanticscholar__arxiv.json",
            "zotero-oa__arxiv.json",
            "unpaywall__arxiv-404.html",
        ],
    ),
    (
        "medrxiv",
        &[
            "biorxiv-api__medrxiv-details.json",
            "openalex__medrxiv.json",
            "europepmc__medrxiv.json",
            "unpaywall__medrxiv.json",
            "crossref__medrxiv.json",
            "zotero-oa__medrxiv.json",
            "semanticscholar__medrxiv.json",
        ],
    ),
    (
        "paywalled",
        &[
            "openalex__paywalled.json",
            "europepmc__paywalled.json",
            "unpaywall__paywalled.json",
            "semanticscholar__paywalled.json",
            "crossref__paywalled.json",
            "zotero-oa__paywalled.json",
            "core__discover-paywalled-no-key.json",
        ],
    ),
];

/// Candidates of every available source for `case`, merged the way the engine pools them.
fn pool_of(case: &str, files: &[&str]) -> Vec<Candidate> {
    let req = requested(case);
    let mut pool = Vec::new();
    for file in files {
        rank::merge(&mut pool, api(file, &req).candidates);
    }
    pool
}

fn ranked_of(case: &str) -> Vec<Candidate> {
    let files = POOLS
        .iter()
        .find(|(c, _)| *c == case)
        .map(|(_, f)| *f)
        .unwrap();
    rank::rank(
        pool_of(case, files),
        &RankPrefs {
            requested: requested(case),
            ..Default::default()
        },
    )
}

fn index_of(ranked: &[Candidate], pred: impl Fn(&Candidate) -> bool) -> Option<usize> {
    ranked.iter().position(pred)
}

/// The attempt families of the plan, in the order they must be tried.
fn family(c: &Candidate) -> Option<u8> {
    let host = host_of(&c.url).unwrap_or_default();
    if host == "pmc-oa-opendata.s3.amazonaws.com" {
        Some(0)
    } else if host == "europepmc.org" && c.url.contains("/api/getPdf") {
        Some(1)
    } else if host == "arxiv.org" && c.kind == CandKind::Pdf {
        Some(2)
    } else if c.kind == CandKind::Pdf
        && c.host_kind == HostKind::Publisher
        && c.tier == tier::DIRECT_PUBLISHER
    {
        Some(3)
    } else if c.kind == CandKind::Landing {
        Some(4)
    } else {
        None
    }
}

#[test]
fn every_pool_fixture_exists_and_pools_are_not_trivial() {
    for (case, files) in POOLS {
        assert!(!files.is_empty());
        for f in *files {
            assert!(
                API_TABLE.iter().any(|(t, _, _)| t == f),
                "{f} is not in API_TABLE"
            );
        }
        assert!(!ranked_of(case).is_empty(), "{case}: empty pool");
    }
}

#[test]
fn ranked_pools_obey_the_attempt_order_and_stay_unique() {
    for (case, _) in POOLS {
        let ranked = ranked_of(case);
        let urls: Vec<&str> = ranked.iter().map(|c| c.url.as_str()).collect();
        // Tiers never decrease, and inside a tier the score never increases.
        for w in ranked.windows(2) {
            assert!(
                w[0].tier <= w[1].tier,
                "{case}: tiers out of order: {urls:?}"
            );
            if w[0].tier == w[1].tier {
                assert!(
                    w[0].score >= w[1].score,
                    "{case}: scores out of order inside tier {}: {urls:?}",
                    w[0].tier
                );
            }
        }
        // PMC S3 < Europe PMC getPdf < arXiv < publisher < landing, for the families that are present.
        let fams: Vec<(u8, &str)> = ranked
            .iter()
            .filter_map(|c| family(c).map(|f| (f, c.url.as_str())))
            .collect();
        for w in fams.windows(2) {
            assert!(
                w[0].0 <= w[1].0,
                "{case}: family order broken between {} and {}",
                w[0].1,
                w[1].1
            );
        }
        // No duplicates, every candidate usable as given.
        let mut seen = BTreeSet::new();
        for c in &ranked {
            assert!(
                seen.insert(canonical_key(&c.url)),
                "{case}: duplicate {}",
                c.url
            );
            assert!(
                c.url.starts_with("https://") && !c.url.contains('#'),
                "{case}: {}",
                c.url
            );
            assert!(
                (1..=9).contains(&c.tier),
                "{case}: tier {} for {}",
                c.tier,
                c.url
            );
            assert!(c.score > 0, "{case}: score {} for {}", c.score, c.url);
            assert!(
                c.signal.as_deref().is_some_and(|s| !s.is_empty()),
                "{case}: no signal on {}",
                c.url
            );
        }
    }
}

#[test]
fn gold_starts_with_the_pmc_s3_pdf_then_europe_pmc() {
    let ranked = ranked_of("gold");
    let first = &ranked[0];
    assert_eq!(
        (first.kind, first.tier, host_of(&first.url).as_deref()),
        (
            CandKind::Pdf,
            tier::PMC_S3,
            Some("pmc-oa-opendata.s3.amazonaws.com")
        ),
        "{first:?}"
    );
    assert!(first.url.contains("PMC2707599"));
    assert_eq!(ranked[1].tier, tier::EUROPEPMC_GETPDF, "{:?}", ranked[1]);
    assert!(
        ranked[1]
            .url
            .starts_with("https://europepmc.org/api/getPdf?pmcid=PMC2707599")
    );
    // The publisher landing page (doi.org, which carries citation_pdf_url) is still there for the extractor.
    assert!(
        index_of(&ranked, |c| c.kind == CandKind::Landing
            && c.url == "https://doi.org/10.1371/journal.pmed.1000097")
        .is_some()
    );
}

#[test]
fn green_prefers_arxiv_then_the_publisher_pdf_and_keeps_the_paid_cache_last() {
    let ranked = ranked_of("green");
    let arxiv =
        index_of(&ranked, |c| c.url == "https://arxiv.org/pdf/1304.1068").expect("arXiv copy");
    let publisher = index_of(&ranked, |c| {
        c.url == "https://www.nature.com/articles/nature12373.pdf"
    })
    .expect("publisher PDF");
    let landing = index_of(&ranked, |c| c.kind == CandKind::Landing).expect("a landing page");
    assert!(
        arxiv < publisher && publisher < landing,
        "{arxiv} {publisher} {landing}"
    );
    let paid = ranked.last().unwrap();
    assert_eq!(
        (paid.tier, paid.signal.as_deref()),
        (tier::PAID_API, Some("openalex:content_api"))
    );
    // The walled PMC website PDF only appears when the open PMC routes are absent; here Europe PMC `?pdf=render` is browser-only.
    let walled = ranked
        .iter()
        .filter(|c| c.tier == tier::BROWSER_ONLY)
        .count();
    assert!(walled <= 1, "{walled}");
}

#[test]
fn arxiv_has_the_real_preprint_before_the_merged_work_mirror() {
    let ranked = ranked_of("arxiv");
    let arxiv =
        index_of(&ranked, |c| c.url == "https://arxiv.org/pdf/1706.03762").expect("arxiv.org PDF");
    assert_eq!(ranked[arxiv].tier, tier::ARXIV);
    let mirrors: Vec<usize> = ranked
        .iter()
        .enumerate()
        // The mirror is identified by its own URLs; every candidate of this OpenAlex record carries the merged work's
        // DOI as `record_doi`, including the arXiv one.
        .filter(|(_, c)| c.url.contains("10.65215/") || c.url.contains("langtaosha"))
        .map(|(i, _)| i)
        .collect();
    assert!(
        !mirrors.is_empty(),
        "the 10.65215 mirror should be in the pool"
    );
    assert!(
        mirrors.iter().all(|m| arxiv < *m),
        "arXiv at {arxiv}, mirror candidates at {mirrors:?}"
    );
    // The mirror's PDF is an aggregator copy: far below the real one.
    let mirror_pdf = ranked
        .iter()
        .find(|c| c.url.contains("langtaosha") && c.kind == CandKind::Pdf)
        .expect("mirror pdf");
    assert_eq!(mirror_pdf.host_kind, HostKind::Aggregator);
    assert!(mirror_pdf.score < ranked[arxiv].score);
}

#[test]
fn medrxiv_tries_the_early_path_before_the_canonical_pdf_and_the_published_landing() {
    let ranked = ranked_of("medrxiv");
    let early = index_of(&ranked, |c| {
        c.url
            .contains("/early/2023/06/01/2023.05.31.23290770.full.pdf")
    })
    .expect("early path");
    let canonical = index_of(&ranked, |c| {
        c.url.ends_with("2023.05.31.23290770v1.full.pdf")
    })
    .expect("canonical");
    let landing = index_of(&ranked, |c| c.kind == CandKind::Landing).expect("landing");
    assert!(
        early < canonical && canonical < landing,
        "{early} {canonical} {landing}"
    );
    assert_eq!(ranked[early].tier, tier::RXIV_EARLY);
    // The PMC website PDF of the published version is behind a proof-of-work page: browser-only, after the landing pages.
    let pmc = ranked
        .iter()
        .find(|c| c.url.contains("pmc.ncbi.nlm.nih.gov") && c.kind == CandKind::Pdf)
        .expect("PMC pdf");
    assert!(pmc.needs_browser && pmc.tier == tier::BROWSER_ONLY);
}

#[test]
fn paywalled_offers_no_direct_pdf_and_ranks_aggregator_guesses_last() {
    let ranked = ranked_of("paywalled");
    assert!(!ranked.is_empty());
    // Nothing is tried before the landing tier, except aggregator false positives, which would have to rank last.
    for c in ranked
        .iter()
        .filter(|c| c.kind == CandKind::Pdf && c.tier < tier::LANDING)
    {
        assert_eq!(
            c.host_kind,
            HostKind::Aggregator,
            "a direct PDF for a closed work: {c:?}"
        );
    }
    // CORE `discover` is an unverified guess: after everything else.
    let first_core = index_of(&ranked, |c| c.source == Source::Core);
    if let Some(i) = first_core {
        assert!(
            ranked[i..].iter().all(|c| c.source == Source::Core),
            "non-CORE candidate after a CORE one: {:?}",
            ranked.iter().map(|c| &c.url).collect::<Vec<_>>()
        );
        assert_eq!(ranked[i].host_kind, HostKind::Aggregator);
    }
    // The publisher landing page from Crossref is the way in.
    assert!(ranked.iter().any(|c| c.kind == CandKind::Landing
        && c.source == Source::Doi
        && c.url.contains("journals.lww.com")));
}

#[test]
fn ranking_depends_on_the_set_of_candidates_not_on_their_arrival_order() {
    for (case, files) in POOLS {
        let req = requested(case);
        let prefs = RankPrefs {
            requested: req.clone(),
            ..Default::default()
        };
        let pool = pool_of(case, files);
        let want = rank::rank(pool.clone(), &prefs);
        // Idempotent.
        assert_eq!(
            rank::rank(want.clone(), &prefs),
            want,
            "{case}: rank(rank(x)) != rank(x)"
        );
        // Reversed, rotated and shuffled arrival orders give the same attempt order.
        let mut reversed = pool.clone();
        reversed.reverse();
        assert_eq!(rank::rank(reversed, &prefs), want, "{case}: reversed");
        let mut rotated = pool.clone();
        rotated.rotate_left(pool.len() / 2);
        assert_eq!(rank::rank(rotated, &prefs), want, "{case}: rotated");
        let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ case.len() as u64;
        for round in 0..8 {
            let mut shuffled = pool.clone();
            for i in (1..shuffled.len()).rev() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                shuffled.swap(i, (state % (i as u64 + 1)) as usize);
            }
            assert_eq!(
                rank::rank(shuffled, &prefs),
                want,
                "{case}: shuffle {round}"
            );
        }
        // Splitting the pool across several `merge` calls (answers arriving one by one) changes nothing either.
        let mut piecewise = Vec::new();
        for c in pool.iter().rev() {
            rank::merge(&mut piecewise, vec![c.clone()]);
        }
        assert_eq!(
            rank::rank(piecewise, &prefs),
            want,
            "{case}: piecewise merge"
        );
    }
}

#[test]
fn candidate_wire_form_survives_json() {
    for (case, _) in POOLS {
        let ranked = ranked_of(case);
        let back: Vec<Candidate> =
            serde_json::from_slice(&serde_json::to_vec(&ranked).unwrap()).unwrap();
        assert_eq!(back, ranked, "{case}");
    }
}

// ---------------------------------------------------------------------------------------------------- 3. HTML fixtures

fn html_files() -> Vec<String> {
    let mut files: Vec<String> = common::html_expected()
        .iter()
        .map(|e| e["file"].as_str().unwrap().to_owned())
        .collect();
    // OA-side walls: recorded next to the API responses, so they carry only a content-type.
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

fn page(file: &str, requested: &Ids, route: Route) -> PageInspection {
    let (resp, body) = common::html_fixture(file);
    as_page(run(page_spec(requested, route), resp, &body))
}

#[test]
fn every_html_fixture_follows_the_oracle_through_the_page_spec() {
    let all = common::html_expected();
    assert!(all.len() >= 30, "{}", all.len());
    let mut landing_checked = 0;
    for e in &all {
        let file = e["file"].as_str().unwrap();
        let kind = e["kind"].as_str().unwrap();
        for route in [Route::Oa, Route::Inst] {
            let p = page(file, &Ids::default(), route);
            match kind {
                "landing" => {
                    assert!(
                        matches!(p.class, PageClass::Landing | PageClass::Html),
                        "{file}: {:?}",
                        p.class
                    );
                    let forbidden = strings(e, "must_not_select");
                    let alternates = strings(e, "alternates");
                    match e["expected_pdf_url"].as_str() {
                        Some(want) => {
                            let top = p.candidates.first().unwrap_or_else(|| {
                                panic!("{file}: no candidates; rejected {:?}", p.rejected)
                            });
                            assert!(
                                top.url == want || alternates.contains(&top.url),
                                "{file}: picked {} (want {want} or {alternates:?})",
                                top.url
                            );
                            landing_checked += 1;
                        }
                        None => assert!(
                            p.candidates.is_empty(),
                            "{file}: expected no candidate, got {:?}",
                            p.candidates
                        ),
                    }
                    for c in &p.candidates {
                        assert_eq!(c.kind, CandKind::Pdf, "{file}");
                        assert!(c.url.starts_with("https://"), "{file}: {}", c.url);
                        for bad in &forbidden {
                            assert!(
                                c.url != *bad && canonical_key(&c.url) != canonical_key(bad),
                                "{file}: forbidden URL selected: {bad}"
                            );
                        }
                    }
                }
                "challenge" => {
                    assert!(
                        matches!(p.class, PageClass::Challenge { .. }),
                        "{file}: {:?}",
                        p.class
                    );
                    assert!(p.candidates.is_empty(), "{file}");
                }
                "blocked" => {
                    assert!(
                        matches!(p.class, PageClass::Blocked { .. }),
                        "{file}: {:?}",
                        p.class
                    );
                    assert!(p.candidates.is_empty(), "{file}");
                }
                "redirect" => {
                    assert_eq!(
                        p.class,
                        PageClass::Redirect {
                            next: e["next_url"].as_str().unwrap().to_owned()
                        },
                        "{file}"
                    );
                    assert!(p.candidates.is_empty(), "{file}");
                }
                other => panic!("{file}: unknown kind {other}"),
            }
        }
    }
    assert_eq!(
        landing_checked,
        2 * 22,
        "landing pages with an expected PDF, on both routes"
    );
}

#[test]
fn the_page_doi_makes_no_difference_to_the_winner() {
    // The same oracle once more, but with the request the engine would hold after phase 1: the page's own DOI.
    for e in common::html_expected()
        .iter()
        .filter(|e| e["kind"] == "landing" && e["expected_pdf_url"].is_string())
    {
        let file = e["file"].as_str().unwrap();
        let anonymous = page(file, &Ids::default(), Route::Oa);
        let known = Ids {
            doi: anonymous.page_ids.doi.clone(),
            ..Default::default()
        };
        let with_doi = page(file, &known, Route::Oa);
        assert_eq!(
            with_doi.candidates.first().map(|c| &c.url),
            anonymous.candidates.first().map(|c| &c.url),
            "{file}"
        );
        if anonymous.page_ids.doi.is_some() {
            assert_eq!(with_doi.doi_match, Some(true), "{file}");
        }
        let other = Ids {
            doi: Some("10.9999/not-this-one".to_owned()),
            ..Default::default()
        };
        let mismatched = page(file, &other, Route::Oa);
        assert_eq!(
            mismatched.candidates.first().map(|c| &c.url),
            anonymous.candidates.first().map(|c| &c.url),
            "{file}"
        );
        if anonymous.page_ids.doi.is_some() {
            assert_eq!(mismatched.doi_match, Some(false), "{file}");
        }
    }
}

#[test]
fn html_bodies_under_the_pdf_spec_come_back_as_pages_never_as_a_bare_verdict() {
    for file in html_files() {
        let (resp, body) = common::html_fixture(&file);
        for route in [Route::Oa, Route::Inst] {
            let requested = Ids::default();
            let got = run(pdf_spec(&requested, route), resp.clone(), &body);
            assert_eq!(
                tag(&got),
                "page",
                "{file}: an HTML body must not be reported as a PDF verdict"
            );
            // The same analysis as the Page spec, byte for byte.
            let via_page = run(page_spec(&requested, route), resp.clone(), &body);
            assert_eq!(
                got, via_page,
                "{file}: Pdf and Page specs disagree about the same HTML"
            );
        }
    }
}

#[test]
fn probes_report_the_class_of_the_page_analysis() {
    for file in html_files() {
        let (resp, body) = common::html_fixture(&file);
        let want = page(&file, &Ids::default(), Route::Oa).class;
        for route in [Route::Oa, Route::Inst] {
            let got = as_probe(run(InspectSpec::Probe { route }, resp.clone(), &body));
            assert_eq!(got, want, "{file}");
        }
        // A probe sent without a body (JS settle detection) must not panic and must still answer.
        let mut headless = resp.clone();
        headless.body_len = 0;
        let _ = as_probe(run(InspectSpec::Probe { route: Route::Oa }, headless, b""));
    }
}

#[test]
fn url_only_probes_classify_the_sso_hops_the_browser_settle_loop_asks_about() {
    // The settle detector sends `Probe` with an empty body, status 200 and the window's current URL.
    let probe = |url: &str| {
        as_probe(run(
            InspectSpec::Probe { route: Route::Inst },
            resp(200, url),
            b"",
        ))
    };
    assert_eq!(
        probe(ENTRA),
        PageClass::IdpLogin {
            idp: Idp::Entra,
            ca_error: None
        }
    );
    assert_eq!(probe(EZPROXY_LOGIN), PageClass::EzproxyNeedLogin);
    assert_eq!(
        probe("https://www.thelancet.com/action/ssostart?redirectUri=%2Fdoi%2Ffull%2F10.1016%2Fx"),
        PageClass::PublisherSsoHop
    );
    assert_eq!(
        probe("https://www.nature.com/articles/nature12373"),
        PageClass::Html,
        "a publisher URL with no body says nothing"
    );
    // A no-follow probe of a proxied host that bounces to the login page.
    let mut bounce = resp(
        302,
        "https://www-nature-com.kcl.idm.oclc.org/articles/nature12373",
    );
    bounce
        .headers
        .insert("server".to_owned(), "EZproxy".to_owned());
    bounce.headers.insert("location".to_owned(), "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fwww.nature.com%2Farticles%2Fnature12373".to_owned());
    assert_eq!(
        as_probe(run(InspectSpec::Probe { route: Route::Inst }, bounce, b"")),
        PageClass::EzproxyNeedLogin
    );
}

#[test]
fn oa_side_walls_are_recognised_from_the_body_alone() {
    let vendor = |file: &str| {
        as_probe(run(
            InspectSpec::Probe { route: Route::Oa },
            common::html_fixture(file).0,
            &common::html_fixture(file).1,
        ))
    };
    assert_eq!(
        vendor("pmc-web__pdf-pow-interstitial.html"),
        PageClass::Challenge {
            vendor: Vendor::Pmc,
            interactive: false
        }
    );
    assert_eq!(
        vendor("europepmc-web__pdf-render-cloudflare-challenge.html"),
        PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive: false
        }
    );
    assert_eq!(
        vendor("medrxiv-web__pdf-403-block.html"),
        PageClass::Blocked {
            vendor: Vendor::Cloudflare
        }
    );
}

// ---------------------------------------------------------------------------------------------------- synthetic PDFs

fn synthetic_pdf(filler: usize) -> Vec<u8> {
    let mut v = b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog >>\nendobj\n".to_vec();
    v.extend(std::iter::repeat_n(b'x', filler));
    v.extend_from_slice(b"\ntrailer\n<< /Root 1 0 R >>\nstartxref\n0\n%%EOF\n");
    v
}

fn pdf_resp(body: &[u8]) -> RespMeta {
    resp_for(
        200,
        "https://example.org/paper.pdf",
        "application/pdf",
        body,
    )
}

/// Every spec that may legitimately receive PDF bytes, with the check each must satisfy.
fn check_pdf_everywhere(resp: &RespMeta, body: &[u8], junk: u32, len: u64, eof: bool) {
    let requested = Ids::default();
    for route in [Route::Oa, Route::Inst] {
        for (label, spec) in [
            ("page", page_spec(&requested, route)),
            ("pdf", pdf_spec(&requested, route)),
        ] {
            let v = as_pdf(run(spec, resp.clone(), body));
            assert!(v.is_pdf && !v.looks_html, "{label}/{route:?}: {v:?}");
            assert_eq!(
                (v.junk_prefix, v.len, v.has_eof),
                (junk, len, eof),
                "{label}/{route:?}: {v:?}"
            );
        }
        assert_eq!(
            as_probe(run(InspectSpec::Probe { route }, resp.clone(), body)),
            PageClass::Pdf { junk_prefix: junk },
            "probe/{route:?}"
        );
    }
}

#[test]
fn synthetic_pdfs_are_pdfs_under_every_spec() {
    let clean = synthetic_pdf(4000);
    check_pdf_everywhere(&pdf_resp(&clean), &clean, 0, clean.len() as u64, true);

    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(&clean);
    check_pdf_everywhere(&pdf_resp(&bom), &bom, 3, bom.len() as u64, true);

    // No PDF content type at all: the magic bytes decide.
    let mut bare = pdf_resp(&clean);
    bare.headers.clear();
    check_pdf_everywhere(&bare, &clean, 0, clean.len() as u64, true);
    // A server that labels the PDF as HTML does not change what the bytes are.
    let lying = resp_for(
        200,
        "https://example.org/paper.pdf",
        "text/html; charset=utf-8",
        &clean,
    );
    check_pdf_everywhere(&lying, &clean, 0, clean.len() as u64, true);

    // A truncated download is still a PDF but has no end-of-file marker.
    let cut = &clean[..clean.len() - 30];
    let v = as_pdf(run(
        pdf_spec(&Ids::default(), Route::Oa),
        pdf_resp(cut),
        cut,
    ));
    assert!(v.is_pdf && !v.has_eof, "{v:?}");
}

#[test]
fn staged_binary_bodies_report_the_full_length_and_the_tail_marker() {
    // How JS stages a large file: head ++ tail, `head_len` set, `body_len` the real size.
    for (head_len, tail_len) in [(1024_usize, 1024_usize), (4096, 2048)] {
        let full = synthetic_pdf(5_000_000);
        let mut staged = full[..head_len].to_vec();
        staged.extend_from_slice(&full[full.len() - tail_len..]);
        let mut r = pdf_resp(&staged);
        r.body_len = full.len() as u64;
        r.head_len = Some(head_len as u32);
        check_pdf_everywhere(&r, &staged, 0, full.len() as u64, true);

        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(&full);
        let mut staged = with_bom[..head_len].to_vec();
        staged.extend_from_slice(&with_bom[with_bom.len() - tail_len..]);
        let mut r = pdf_resp(&staged);
        r.body_len = with_bom.len() as u64;
        r.head_len = Some(head_len as u32);
        check_pdf_everywhere(&r, &staged, 3, with_bom.len() as u64, true);
    }
    // The tail of a file that is missing its end marker is reported as such.
    let mut full = synthetic_pdf(1_000_000);
    let end = full.len();
    full.truncate(end - 10);
    let mut staged = full[..1024].to_vec();
    staged.extend_from_slice(&full[full.len() - 1024..]);
    let mut r = pdf_resp(&staged);
    r.body_len = full.len() as u64;
    r.head_len = Some(1024);
    check_pdf_everywhere(&r, &staged, 0, full.len() as u64, false);
    // A staged HTML error page (head_len set by mistake) is still HTML.
    let html = b"<html><body>Access denied</body></html>";
    let mut r = resp_for(403, "https://example.org/x.pdf", "text/html", html);
    r.head_len = Some(10);
    assert_eq!(
        tag(&run(pdf_spec(&Ids::default(), Route::Oa), r, html)),
        "page"
    );
}

// ---------------------------------------------------------------------------------------------------- 4. login_state

/// `login_state` as the `fx_login_state` export runs it, with the answer pushed through JSON.
fn login(url: &str, html: Option<&str>) -> LoginState {
    let state = login_state(url, html);
    let back: LoginState = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert_eq!(back, state);
    state
}

const ENTRA: &str = "https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/oauth2/v2.0/authorize?client_id=x&redirect_uri=y";
const REDIRECTOR: &str = "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1038%2Fnature12373";
const EZPROXY_LOGIN: &str =
    "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2F10.1038%2Fnature12373";

#[test]
fn login_state_from_urls_alone() {
    assert_eq!(login(ENTRA, None), LoginState::Pending);
    assert_eq!(
        login(ENTRA, Some("")),
        LoginState::Pending,
        "an empty document counts as none"
    );
    assert_eq!(login(REDIRECTOR, None), LoginState::Pending);
    assert_eq!(login(EZPROXY_LOGIN, None), LoginState::Pending);
    assert_eq!(
        login("https://login.kcl.idm.oclc.org/connect?session=abc", None),
        LoginState::Pending
    );
    // Not an SSO hop: the window has landed.
    assert_eq!(
        login("https://www.nature.com/articles/nature12373", None),
        LoginState::Landed
    );
    assert_eq!(
        login(
            "https://www-nature-com.kcl.idm.oclc.org/articles/nature12373.pdf",
            None
        ),
        LoginState::Landed
    );
    assert_eq!(
        login("", None),
        LoginState::Landed,
        "nothing known is not a reason to keep waiting"
    );
}

#[test]
fn login_state_from_documents() {
    // The EZproxy "Shibboleth Authentication Request" page at the login host.
    let shib = read_data("ezproxy_login_page.html");
    assert!(shib.contains("Shibboleth Authentication Request"));
    assert_eq!(login(EZPROXY_LOGIN, Some(&shib)), LoginState::Pending);
    // Entra's own pages.
    assert_eq!(
        login(ENTRA, Some(&read_data("entra_signin.html"))),
        LoginState::Pending
    );
    assert_eq!(
        login(REDIRECTOR, Some(&read_data("openathens_wait.html"))),
        LoginState::Pending
    );
    assert_eq!(
        login(
            "https://login.microsoftonline.com/common/saml2",
            Some(&read_data("saml_autopost.html"))
        ),
        LoginState::Pending
    );

    // A Cloudflare managed challenge on the publisher the window is heading to.
    let (_, challenge) = common::html_fixture("cloudflare__managed-challenge.html");
    let challenge = String::from_utf8(challenge).unwrap();
    assert_eq!(
        login(
            "https://www.sciencedirect.com/science/article/pii/S1743919121000406",
            Some(&challenge)
        ),
        LoginState::ChallengeShown
    );

    // A conditional-access refusal names its code.
    let denied = read_data("entra_ca_error.html");
    assert!(denied.contains("AADSTS53000"));
    assert_eq!(
        login(ENTRA, Some(&denied)),
        LoginState::Refused {
            code: Some("AADSTS53000".to_owned())
        }
    );

    // Other refusals that the document alone can tell. (The 404 / 403 refusals of EZproxy and OpenAthens depend on
    // the HTTP status, which the window poll does not have: the engine sees those through its own requests.)
    for (file, url, code) in [
        (
            "ezproxy_error_suspend.html",
            "https://login.kcl.idm.oclc.org/login",
            "ezproxy_suspended",
        ),
        (
            "licence_stop.html",
            "https://www.sciencedirect.com/science/article/pii/S1",
            "licence_stop",
        ),
    ] {
        match login(url, Some(&read_data(file))) {
            LoginState::Refused { code: Some(c) } => assert_eq!(c, code, "{file}"),
            other => panic!("{file}: expected Refused({code}), got {other:?}"),
        }
    }
}

#[test]
fn a_cloudflare_interstitial_is_a_challenge_without_a_status_but_a_normal_page_behind_cloudflare_is_not()
 {
    let (_, challenge) = common::html_fixture("cloudflare__managed-challenge.html");
    // The window only has the document: no 403, no `cf-mitigated`. The same bytes at 200 are a challenge too.
    let at_200 = RespMeta {
        headers: BTreeMap::from([("content-type".to_owned(), "text/html".to_owned())]),
        ..resp(200, "https://academic.oup.com/bjs/article/1")
    };
    assert_eq!(
        as_probe(run(
            InspectSpec::Probe { route: Route::Inst },
            at_200,
            &challenge
        )),
        PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive: false
        }
    );
    // An article served by a Cloudflare-fronted publisher also loads the challenge-platform script (bot detection
    // `jsd`), and its title may even say "Just a moment" in prose; neither makes it a challenge.
    let article = br#"<html><head><title>Cold-stored blood | Journal</title><meta name="citation_title" content="T">
        <script src="/cdn-cgi/challenge-platform/scripts/jsd/main.js"></script></head><body>Just a moment of silence.</body></html>"#;
    assert_eq!(
        login(
            "https://www.thelancet.com/x",
            Some(std::str::from_utf8(article).unwrap())
        ),
        LoginState::Landed
    );
    let titled = "<html><head><title>Just a moment in surgery</title><meta name=\"citation_title\" content=\"T\"></head><body>text</body></html>";
    assert_eq!(
        login("https://www.thelancet.com/x", Some(titled)),
        LoginState::Landed,
        "title alone, no challenge options object"
    );
}

#[test]
fn login_state_lands_on_publisher_pages_and_pdfs() {
    // Recorded publisher pages at their own URL: landed.
    for file in [
        "plos__article-oa.html",
        "nature__scirep-oa.html",
        "wiley__wjs-article.html",
        "bmj__article-oa.html",
        "nejm__article-free.html",
    ] {
        let (resp, body) = common::html_fixture(file);
        let html = String::from_utf8(body).unwrap();
        assert_eq!(
            login(&resp.final_url, Some(&html)),
            LoginState::Landed,
            "{file}"
        );
    }
    // The same page served through the proxy host.
    let (_, body) = common::html_fixture("bmj__article-oa.html");
    let html = String::from_utf8(body).unwrap();
    assert_eq!(
        login(
            "https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71",
            Some(&html)
        ),
        LoginState::Landed
    );
    // A PDF: by URL, and with PDF bytes as the document.
    let pdf = String::from_utf8(synthetic_pdf(100)).unwrap();
    assert_eq!(
        login("https://example.org/paper.pdf", None),
        LoginState::Landed
    );
    assert_eq!(
        login("https://example.org/paper.pdf", Some(&pdf)),
        LoginState::Landed
    );
    assert_eq!(
        login(
            "https://www-bmj-com.kcl.idm.oclc.org/content/bmj/372/bmj.n71.full.pdf",
            Some(&pdf)
        ),
        LoginState::Landed
    );
    // The article is not hidden by words that look like a block page.
    assert_eq!(
        login(
            "https://www.nature.com/articles/x",
            Some(&read_data("article_mentions_blockwords.html"))
        ),
        LoginState::Landed
    );
    // A landing document shown at an SSO-looking URL is not a landing yet.
    assert_eq!(
        login(ENTRA, Some("<html><body><p>Loading...</p></body></html>")),
        LoginState::Pending
    );
}

#[test]
fn login_state_is_decided_by_the_first_two_mebibytes_only() {
    // 5 MiB document: cut at 2 MiB, never rejected.
    let big = format!(
        "<html><body>{}</body></html>",
        "<p>text</p>".repeat(500_000)
    );
    assert!(big.len() > 5 * 1024 * 1024);
    assert_eq!(
        login("https://www.nature.com/articles/x", Some(&big)),
        LoginState::Landed
    );
    assert_eq!(login(ENTRA, Some(&big)), LoginState::Pending);
}

// ---------------------------------------------------------------------------------------------------- 5. response-shape rule

const HTML: &[u8] = b"<!doctype html><html><head><title>Just some page</title></head><body><p>hello</p></body></html>";
const OTHER: &[u8] = b"plain text, neither markup nor a document";

fn specs() -> Vec<(&'static str, InspectSpec)> {
    vec![
        (
            "api",
            InspectSpec::Api {
                source: Source::Unpaywall,
                requested: Ids::default(),
            },
        ),
        ("page", page_spec(&Ids::default(), Route::Oa)),
        ("pdf", pdf_spec(&Ids::default(), Route::Oa)),
        ("probe", InspectSpec::Probe { route: Route::Oa }),
        ("none", InspectSpec::None),
    ]
}

fn shape(label: &str, body: &[u8], status: u16) -> Inspection {
    let spec = specs().into_iter().find(|(l, _)| *l == label).unwrap().1;
    let content_type = if body.starts_with(b"%PDF-") {
        "application/pdf"
    } else if body == HTML {
        "text/html; charset=utf-8"
    } else {
        "text/plain"
    };
    run(
        spec,
        resp_for(status, "https://example.org/x", content_type, body),
        body,
    )
}

#[test]
fn response_shape_api_row() {
    let pdf = synthetic_pdf(50);
    assert_eq!(
        shape("api", &pdf, 200),
        Inspection::Failed {
            reason: "api_body_is_pdf".to_owned()
        },
        "api / PDF"
    );
    // HTML: the parser decides by status first.
    assert_eq!(
        as_api(shape("api", HTML, 200)).status,
        Unparseable,
        "api / HTML 200"
    );
    assert_eq!(
        as_api(shape("api", HTML, 404)).status,
        NotFound,
        "api / HTML 404"
    );
    assert_eq!(
        as_api(shape("api", HTML, 429)).status,
        Throttled,
        "api / HTML 429"
    );
    // Anything else goes to the parser, which reads real JSON.
    let (resp, body) = common::oa_fixture("unpaywall__gold.json");
    assert_eq!(
        as_api(run(
            InspectSpec::Api {
                source: Source::Unpaywall,
                requested: requested("gold")
            },
            resp,
            &body
        ))
        .status,
        Found,
        "api / JSON"
    );
    assert_eq!(
        as_api(shape("api", OTHER, 200)).status,
        Unparseable,
        "api / text"
    );
}

#[test]
fn response_shape_page_row() {
    let pdf = synthetic_pdf(50);
    assert_eq!(tag(&shape("page", &pdf, 200)), "pdf", "page / PDF");
    assert!(as_pdf(shape("page", &pdf, 200)).is_pdf);
    assert_eq!(tag(&shape("page", HTML, 200)), "page", "page / HTML");
    // Anything else: a page with class Html and nothing to extract.
    let p = as_page(shape("page", OTHER, 200));
    assert_eq!(p.class, PageClass::Html, "page / text");
    assert!(p.candidates.is_empty());
}

#[test]
fn response_shape_pdf_row() {
    let pdf = synthetic_pdf(50);
    assert_eq!(tag(&shape("pdf", &pdf, 200)), "pdf", "pdf / PDF");
    assert!(as_pdf(shape("pdf", &pdf, 200)).is_pdf);
    assert_eq!(tag(&shape("pdf", HTML, 200)), "page", "pdf / HTML");
    let other = as_pdf(shape("pdf", OTHER, 200));
    assert!(!other.is_pdf && !other.looks_html, "pdf / text: {other:?}");
    assert_eq!(other.len, OTHER.len() as u64);
}

#[test]
fn response_shape_probe_row() {
    let pdf = synthetic_pdf(50);
    assert_eq!(
        as_probe(shape("probe", &pdf, 200)),
        PageClass::Pdf { junk_prefix: 0 },
        "probe / PDF"
    );
    let html_class = as_probe(shape("probe", HTML, 200));
    assert!(
        matches!(html_class, PageClass::Html | PageClass::Landing),
        "probe / HTML: {html_class:?}"
    );
    assert_eq!(
        as_probe(shape("probe", OTHER, 200)),
        PageClass::Html,
        "probe / text"
    );
    assert_eq!(
        as_probe(shape("probe", b"", 404)),
        PageClass::NotFound,
        "probe / empty 404"
    );
}

#[test]
fn response_shape_none_row() {
    let pdf = synthetic_pdf(50);
    for body in [&pdf[..], HTML, OTHER, b""] {
        assert_eq!(shape("none", body, 200), Inspection::None);
    }
    assert_eq!(tag(&Inspection::None), "none");
}

#[test]
fn response_shape_refinements() {
    // A Pdf-spec response that is an error with a text body is classified as a page, not as a failed download.
    for status in [401, 403, 404, 429, 503] {
        assert_eq!(
            tag(&shape("pdf", b"Access denied", status)),
            "page",
            "pdf / text {status}"
        );
    }
    // ... but a binary error body is a verdict.
    assert_eq!(tag(&shape("pdf", &[0u8; 64], 500)), "pdf");
    // Over the body limit nothing looks at the bytes, whatever the spec says.
    let huge = vec![b'a'; MAX_BODY_BYTES + 1];
    for (label, _) in specs() {
        assert_eq!(
            shape(label, &huge, 200),
            Inspection::Failed {
                reason: "body_too_large".to_owned()
            },
            "{label}"
        );
    }
    // Exactly at the limit it is still inspected.
    let edge = vec![b'a'; MAX_BODY_BYTES];
    assert_eq!(tag(&shape("pdf", &edge, 200)), "pdf");
}

#[test]
fn the_table_and_the_answers_agree_on_every_cell() {
    // Summary of the whole table in one place, so a change to the rule shows up as one diff.
    let pdf = synthetic_pdf(50);
    let mut got: BTreeMap<(&str, &str), String> = BTreeMap::new();
    for (label, _) in specs() {
        for (name, body) in [("pdf", &pdf[..]), ("html", HTML), ("other", OTHER)] {
            let answer = shape(label, body, 200);
            let class = match &answer {
                Inspection::Page(p) => format!("page:{:?}", p.class),
                Inspection::Probe { class } => format!("probe:{class:?}"),
                Inspection::Api(a) => format!("api:{:?}", a.status),
                Inspection::Pdf(v) => format!("pdf:{}", v.is_pdf),
                Inspection::Failed { reason } => format!("failed:{reason}"),
                Inspection::None => "none".to_owned(),
            };
            got.insert((label, name), class);
        }
    }
    let want: [((&str, &str), &str); 15] = [
        (("api", "pdf"), "failed:api_body_is_pdf"),
        (("api", "html"), "api:Unparseable"),
        (("api", "other"), "api:Unparseable"),
        (("page", "pdf"), "pdf:true"),
        (("page", "html"), "page:Html"),
        (("page", "other"), "page:Html"),
        (("pdf", "pdf"), "pdf:true"),
        (("pdf", "html"), "page:Html"),
        (("pdf", "other"), "pdf:false"),
        (("probe", "pdf"), "probe:Pdf { junk_prefix: 0 }"),
        (("probe", "html"), "probe:Html"),
        (("probe", "other"), "probe:Html"),
        (("none", "pdf"), "none"),
        (("none", "html"), "none"),
        (("none", "other"), "none"),
    ];
    for (cell, expected) in want {
        let actual = got
            .get(&cell)
            .unwrap_or_else(|| panic!("no answer for {cell:?}"));
        assert_eq!(actual, expected, "cell {cell:?}");
    }
}

// ---------------------------------------------------------------------------------------------------- debugging aid

/// Prints the ranked pool of every test work. Not part of the normal run:
/// `cargo test -p fulltext-core --test e2e dump_pools -- --ignored --nocapture`.
#[test]
#[ignore = "debugging aid: prints the ranked pool of every test work"]
fn dump_pools() {
    for (case, _) in POOLS {
        println!("=== {case}");
        for c in ranked_of(case) {
            println!(
                "  t{} s{:>4} {:?} {:?} {:?} {} [{}]",
                c.tier,
                c.score,
                c.kind,
                c.source,
                c.host_kind,
                c.url,
                c.signal.as_deref().unwrap_or("")
            );
        }
    }
}
