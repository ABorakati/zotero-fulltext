//! Request builders: OA API URLs (research/05 section 2) and generic GET/HEAD builders. Pure.
//! Response PARSING is `fulltext_core::inspect` (JS passes every body through it before the engine sees it); this
//! file only builds what is asked.
//!
//! One source can need more than one request to answer a question (`api_requests`): bioRxiv vs medRxiv (the DOI
//! prefix `10.1101/` is shared), and the PMC S3 metadata object vs the bucket listing (the metadata key ends in
//! `.1`, which "is not always the version that exists"). The caller tries the variants in order while the answer
//! is "not found" / "empty". `api_request` is the first variant.
//!
//! Sources that need no HTTP request return no variant: Zotero OA (an `Effect::ZoteroOa`), arXiv (the PDF URL is
//! built from the id), Alma and the page-level sources.
use crate::pipeline::pure;
use crate::proto::{HttpReq, Method, OaConfig};
use crate::sched::policy::{self, FetchClass};
use fulltext_core::model::*;
use serde_json::json;
use std::collections::BTreeMap;

/// Largest API answer the engine asks JS to read (the pure instance parses it, the engine never sees it).
pub(crate) const API_MAX_BYTES: u64 = 1024 * 1024;

const OPENALEX: &str = "https://api.openalex.org/works";
const EUROPEPMC: &str = "https://www.ebi.ac.uk/europepmc/webservices/rest/search";
const BIORXIV: &str = "https://api.biorxiv.org/details";
const PMC_S3: &str = "https://pmc-oa-opendata.s3.amazonaws.com";
const UNPAYWALL: &str = "https://api.unpaywall.org/v2";
const SEMANTIC_SCHOLAR: &str = "https://api.semanticscholar.org/graph/v1/paper";
const SEMANTIC_SCHOLAR_FIELDS: &str =
    "title,externalIds,isOpenAccess,openAccessPdf,publicationTypes";
const CORE_DISCOVER: &str = "https://api.core.ac.uk/v3/discover";
const CROSSREF: &str = "https://api.crossref.org/works";

/// A plain GET with the given UA and limits.
pub(crate) fn get(url: &str, ua: Ua, timeout_ms: u32, max_bytes: u64) -> HttpReq {
    HttpReq {
        method: Method::Get,
        url: url.to_owned(),
        headers: BTreeMap::new(),
        body: None,
        ua,
        timeout_ms,
        follow_redirects: true,
        max_bytes,
    }
}

/// A no-follow HEAD probe (EZproxy configured? OpenAthens wait poll, redirect target discovery).
pub(crate) fn probe(url: &str, ua: Ua, timeout_ms: u32) -> HttpReq {
    HttpReq {
        method: Method::Head,
        url: url.to_owned(),
        headers: BTreeMap::new(),
        body: None,
        ua,
        timeout_ms,
        follow_redirects: false,
        max_bytes: 64 * 1024,
    }
}

/// An API call: JSON `Accept`, the OA-API timeout and the 1 MiB answer cap.
fn api_call(method: Method, url: String) -> HttpReq {
    let headers = BTreeMap::from([("Accept".to_owned(), "application/json".to_owned())]);
    HttpReq {
        method,
        url,
        headers,
        body: None,
        ua: Ua::Default,
        timeout_ms: policy::timeout_ms(FetchClass::OaApi, false),
        follow_redirects: true,
        max_bytes: API_MAX_BYTES,
    }
}

fn with_header(mut req: HttpReq, name: &str, value: &str) -> HttpReq {
    req.headers.insert(name.to_owned(), value.to_owned());
    req
}

fn spec(src: Source, ids: &Ids) -> InspectSpec {
    InspectSpec::Api {
        source: src,
        requested: ids.clone(),
    }
}

fn nonempty(s: &str) -> Option<&str> {
    let s = s.trim();
    (!s.is_empty()).then_some(s)
}

/// bioRxiv or medRxiv, the two servers behind the shared `10.1101/` prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RxivServer {
    Biorxiv,
    Medrxiv,
}

impl RxivServer {
    fn name(self) -> &'static str {
        match self {
            RxivServer::Biorxiv => "biorxiv",
            RxivServer::Medrxiv => "medrxiv",
        }
    }
}

/// Which server to ask first: the item's own URL usually says (`medrxiv.org/...`).
fn rxiv_order(ids: &Ids) -> [RxivServer; 2] {
    let medrxiv_first = ids
        .url
        .as_deref()
        .and_then(pure::host_of)
        .is_some_and(|h| h.contains("medrxiv"));
    if medrxiv_first {
        [RxivServer::Medrxiv, RxivServer::Biorxiv]
    } else {
        [RxivServer::Biorxiv, RxivServer::Medrxiv]
    }
}

/// Every request that answers "what does `src` know about this item?", best first, each with the `InspectSpec`
/// that tells the pure instance how to parse the answer. Empty when the source does not apply (no DOI / PMCID /
/// email / key, or toggled off in `oa`). Crossref (`Source::Doi`) has no toggle here: the caller decides whether
/// the institutional phase wants it.
pub(crate) fn api_requests(src: Source, ids: &Ids, oa: &OaConfig) -> Vec<(HttpReq, InspectSpec)> {
    let doi = ids.doi.as_deref();
    let reqs: Vec<HttpReq> = match src {
        Source::Openalex if oa.openalex => {
            let target = match (doi, ids.pmid.as_deref()) {
                (Some(doi), _) => format!("doi:{}", pure::doi_path(doi)),
                (None, Some(pmid)) => format!("pmid:{}", pure::pct_encode(pmid)),
                (None, None) => return Vec::new(),
            };
            let req = api_call(Method::Get, format!("{OPENALEX}/{target}"));
            vec![match nonempty(&oa.openalex_key) {
                Some(key) => with_header(req, "Authorization", &format!("Bearer {key}")),
                None => req,
            }]
        }
        Source::Europepmc if oa.europepmc => {
            let query = match (doi, ids.pmid.as_deref(), ids.pmcid.as_deref()) {
                (Some(doi), _, _) => format!("DOI:%22{}%22", pure::doi_path(doi)),
                (None, Some(pmid), _) => {
                    format!("EXT_ID:{}%20AND%20SRC:MED", pure::pct_encode(pmid))
                }
                (None, None, Some(pmcid)) => format!("PMCID:{}", pure::pct_encode(pmcid)),
                (None, None, None) => return Vec::new(),
            };
            vec![api_call(
                Method::Get,
                format!("{EUROPEPMC}?query={query}&resultType=core&format=json"),
            )]
        }
        Source::Biorxiv if oa.biorxiv && ids.rxiv => {
            let Some(doi) = doi else { return Vec::new() };
            rxiv_order(ids)
                .into_iter()
                .map(|server| {
                    api_call(
                        Method::Get,
                        format!(
                            "{BIORXIV}/{}/{}/na/json",
                            server.name(),
                            pure::doi_path(doi)
                        ),
                    )
                })
                .collect()
        }
        Source::PmcS3 if oa.pmc_s3 => {
            let Some(pmcid) = ids.pmcid.as_deref() else {
                return Vec::new();
            };
            let pmcid = pure::pct_encode(pmcid);
            vec![
                api_call(Method::Get, format!("{PMC_S3}/metadata/{pmcid}.1.json")),
                api_call(
                    Method::Get,
                    format!("{PMC_S3}/?list-type=2&prefix={pmcid}."),
                ),
            ]
        }
        Source::Unpaywall => {
            let (Some(doi), Some(email)) = (
                doi,
                nonempty(&oa.unpaywall_email).filter(|e| e.contains('@')),
            ) else {
                return Vec::new();
            };
            // `@` stays readable; Unpaywall accepts both forms.
            let email = pure::pct_encode(email).replace("%40", "@");
            vec![api_call(
                Method::Get,
                format!("{UNPAYWALL}/{}?email={email}", pure::doi_path(doi)),
            )]
        }
        Source::SemanticScholar => {
            let Some(key) = nonempty(&oa.semantic_scholar_key) else {
                return Vec::new();
            };
            let id = match (
                doi,
                ids.pmid.as_deref(),
                ids.pmcid.as_deref(),
                ids.arxiv.as_deref(),
            ) {
                (Some(doi), ..) => format!("DOI:{}", pure::doi_path(doi)),
                (None, Some(pmid), ..) => format!("PMID:{}", pure::pct_encode(pmid)),
                (None, None, Some(pmcid), _) => format!(
                    "PMCID:{}",
                    pure::pct_encode(pmcid.trim_start_matches("PMC"))
                ),
                (None, None, None, Some(arxiv)) => format!("ARXIV:{}", pure::pct_encode(arxiv)),
                (None, None, None, None) => return Vec::new(),
            };
            let req = api_call(
                Method::Get,
                format!("{SEMANTIC_SCHOLAR}/{id}?fields={SEMANTIC_SCHOLAR_FIELDS}"),
            );
            vec![with_header(req, "x-api-key", key)]
        }
        Source::Core => {
            let (Some(doi), Some(key)) = (doi, nonempty(&oa.core_key)) else {
                return Vec::new();
            };
            let mut req = api_call(Method::Post, CORE_DISCOVER.to_owned());
            req.body = Some(json!({ "doi": doi }).to_string());
            let req = with_header(req, "Content-Type", "application/json");
            vec![with_header(req, "Authorization", &format!("Bearer {key}"))]
        }
        Source::Doi => {
            let Some(doi) = doi else { return Vec::new() };
            vec![api_call(
                Method::Get,
                format!("{CROSSREF}/{}", pure::doi_path(doi)),
            )]
        }
        // Toggled off, or not an HTTP source.
        Source::Openalex
        | Source::Europepmc
        | Source::Biorxiv
        | Source::PmcS3
        | Source::Arxiv
        | Source::ZoteroOa
        | Source::Alma
        | Source::ItemUrl
        | Source::Translator
        | Source::Extractor
        | Source::Redirector
        | Source::Ezproxy => Vec::new(),
    };
    reqs.into_iter().map(|req| (req, spec(src, ids))).collect()
}

/// The lookup request for one OA source and the `InspectSpec` that tells the pure instance how to parse the
/// answer (the first of [`api_requests`]). `None` when the source does not apply.
#[cfg(test)]
pub(crate) fn api_request(src: Source, ids: &Ids, oa: &OaConfig) -> Option<(HttpReq, InspectSpec)> {
    api_requests(src, ids, oa).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oa() -> OaConfig {
        crate::sim::fixtures::test_config().oa
    }

    fn doi_ids(doi: &str) -> Ids {
        Ids {
            doi: Some(doi.to_owned()),
            rxiv: doi.starts_with("10.1101/"),
            ..Ids::default()
        }
    }

    fn url_of(src: Source, ids: &Ids, oa: &OaConfig) -> Option<String> {
        api_request(src, ids, oa).map(|(req, _)| req.url)
    }

    #[test]
    fn get_and_probe_shapes() {
        let g = get("https://example.org/a", Ua::Plain, 9_000, 1_000);
        assert_eq!(
            (g.method, g.follow_redirects, g.ua),
            (Method::Get, true, Ua::Plain)
        );
        let p = probe("https://example.org/a", Ua::Default, 8_000);
        assert_eq!((p.method, p.follow_redirects), (Method::Head, false));
    }

    /// The URLs of `fixtures/oa/manifest.json`, one per source, for the test set DOIs.
    #[test]
    fn golden_urls_match_the_fixture_manifest() {
        let oa = oa();
        let gold = doi_ids("10.1371/journal.pmed.1000097");
        assert_eq!(
            url_of(Source::Openalex, &gold, &oa).as_deref(),
            Some("https://api.openalex.org/works/doi:10.1371/journal.pmed.1000097")
        );
        assert_eq!(
            url_of(Source::Europepmc, &gold, &oa).as_deref(),
            Some(
                "https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=DOI:%2210.1371/journal.pmed.1000097%22&resultType=core&format=json"
            )
        );
        let medrxiv = doi_ids("10.1101/2023.05.31.23290770");
        assert_eq!(
            url_of(Source::Biorxiv, &medrxiv, &oa).as_deref(),
            Some("https://api.biorxiv.org/details/biorxiv/10.1101/2023.05.31.23290770/na/json"),
            "bioRxiv is asked first when the item URL does not say medRxiv"
        );
        let with_pmcid = Ids {
            pmcid: Some("PMC2707599".into()),
            ..gold.clone()
        };
        let pmc: Vec<String> = api_requests(Source::PmcS3, &with_pmcid, &oa)
            .into_iter()
            .map(|(r, _)| r.url)
            .collect();
        assert_eq!(
            pmc,
            vec![
                "https://pmc-oa-opendata.s3.amazonaws.com/metadata/PMC2707599.1.json",
                "https://pmc-oa-opendata.s3.amazonaws.com/?list-type=2&prefix=PMC2707599."
            ]
        );
        let mut keyed = oa.clone();
        keyed.unpaywall_email = "zotero-fulltext-fixtures@example.org".into();
        keyed.semantic_scholar_key = "s2key".into();
        keyed.core_key = "corekey".into();
        assert_eq!(
            url_of(Source::Unpaywall, &gold, &keyed).as_deref(),
            Some(
                "https://api.unpaywall.org/v2/10.1371/journal.pmed.1000097?email=zotero-fulltext-fixtures@example.org"
            )
        );
        assert_eq!(
            url_of(Source::SemanticScholar, &gold, &keyed).as_deref(),
            Some(
                "https://api.semanticscholar.org/graph/v1/paper/DOI:10.1371/journal.pmed.1000097?fields=title,externalIds,isOpenAccess,openAccessPdf,publicationTypes"
            )
        );
        assert_eq!(
            url_of(Source::Doi, &gold, &oa).as_deref(),
            Some("https://api.crossref.org/works/10.1371/journal.pmed.1000097")
        );
        assert_eq!(
            url_of(Source::Core, &gold, &keyed).as_deref(),
            Some("https://api.core.ac.uk/v3/discover")
        );
    }

    /// Every URL this module builds for the manifest's test DOIs was seen working when the fixtures were captured.
    #[test]
    fn built_urls_were_captured_in_the_fixture_manifest() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/oa/manifest.json"
        );
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let manifest: serde_json::Value =
            serde_json::from_str(&text).expect("the manifest is JSON");
        let captured: Vec<&str> = manifest["fixtures"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["url"].as_str())
            .collect();
        let mut keyed = oa();
        keyed.unpaywall_email = "zotero-fulltext-fixtures@example.org".into();
        keyed.semantic_scholar_key = "k".into();
        let cases = [
            (
                "gold",
                Ids {
                    pmcid: Some("PMC2707599".into()),
                    ..doi_ids("10.1371/journal.pmed.1000097")
                },
            ),
            ("green", doi_ids("10.1038/nature12373")),
            ("medrxiv", doi_ids("10.1101/2023.05.31.23290770")),
            ("paywalled", doi_ids("10.1097/SLA.0000000000005603")),
        ];
        for (name, ids) in cases {
            for src in [
                Source::Openalex,
                Source::Europepmc,
                Source::Biorxiv,
                Source::PmcS3,
                Source::Unpaywall,
                Source::SemanticScholar,
                Source::Doi,
            ] {
                for (req, _) in api_requests(src, &ids, &keyed) {
                    let medrxiv_only = src == Source::Biorxiv && req.url.contains("/biorxiv/");
                    // The captured test set only has the medRxiv server's answers for its preprint.
                    assert!(
                        medrxiv_only || captured.contains(&req.url.as_str()),
                        "{name} {src:?}: {} was not captured",
                        req.url
                    );
                }
            }
        }
        // By PMID: OpenAlex as captured; Europe PMC was captured with `resultType=lite`, the pipeline asks for `core`.
        let by_pmid = Ids {
            pmid: Some("19621072".into()),
            ..Ids::default()
        };
        let openalex = url_of(Source::Openalex, &by_pmid, &keyed).unwrap_or_default();
        assert!(captured.contains(&openalex.as_str()), "{openalex}");
        let europepmc = url_of(Source::Europepmc, &by_pmid, &keyed).unwrap_or_default();
        assert!(
            captured.contains(
                &europepmc
                    .replace("resultType=core", "resultType=lite")
                    .as_str()
            ),
            "{europepmc}"
        );
    }

    #[test]
    fn identifiers_other_than_the_doi_are_used_when_there_is_no_doi() {
        let oa = oa();
        let pmid = Ids {
            pmid: Some("19621072".into()),
            ..Ids::default()
        };
        assert_eq!(
            url_of(Source::Openalex, &pmid, &oa).as_deref(),
            Some("https://api.openalex.org/works/pmid:19621072")
        );
        assert_eq!(
            url_of(Source::Europepmc, &pmid, &oa).as_deref(),
            Some(
                "https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=EXT_ID:19621072%20AND%20SRC:MED&resultType=core&format=json"
            )
        );
        let pmcid = Ids {
            pmcid: Some("PMC2707599".into()),
            ..Ids::default()
        };
        assert_eq!(
            url_of(Source::Europepmc, &pmcid, &oa).as_deref(),
            Some(
                "https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=PMCID:PMC2707599&resultType=core&format=json"
            )
        );
        assert_eq!(
            url_of(Source::Openalex, &pmcid, &oa),
            None,
            "OpenAlex cannot look a PMCID up"
        );
        let mut keyed = oa;
        keyed.semantic_scholar_key = "k".into();
        assert!(
            url_of(Source::SemanticScholar, &pmcid, &keyed)
                .is_some_and(|u| u.contains("/paper/PMCID:2707599?"))
        );
        let arxiv = Ids {
            arxiv: Some("1706.03762".into()),
            ..Ids::default()
        };
        assert!(
            url_of(Source::SemanticScholar, &arxiv, &keyed)
                .is_some_and(|u| u.contains("/paper/ARXIV:1706.03762?"))
        );
        assert_eq!(url_of(Source::Europepmc, &arxiv, &keyed), None);
    }

    #[test]
    fn keys_and_emails_gate_the_optional_sources() {
        let ids = doi_ids("10.1038/nature12373");
        let mut oa = oa();
        for src in [Source::Unpaywall, Source::SemanticScholar, Source::Core] {
            assert!(
                api_request(src, &ids, &oa).is_none(),
                "{src:?} needs the user's email / key"
            );
        }
        oa.unpaywall_email = "not-an-email".into();
        assert!(
            api_request(Source::Unpaywall, &ids, &oa).is_none(),
            "an email needs an @"
        );
        oa.unpaywall_email = " me@kcl.ac.uk ".into();
        assert_eq!(
            url_of(Source::Unpaywall, &ids, &oa).as_deref(),
            Some("https://api.unpaywall.org/v2/10.1038/nature12373?email=me@kcl.ac.uk")
        );
        oa.openalex_key = "  ".into();
        let (req, _) = api_request(Source::Openalex, &ids, &oa).expect("openalex");
        assert!(
            !req.headers.contains_key("Authorization"),
            "a blank key sends nothing"
        );
        oa.openalex_key = "abc".into();
        let (req, _) = api_request(Source::Openalex, &ids, &oa).expect("openalex");
        assert_eq!(
            req.headers.get("Authorization").map(String::as_str),
            Some("Bearer abc")
        );
        oa.core_key = "ck".into();
        let (req, _) = api_request(Source::Core, &ids, &oa).expect("core");
        assert_eq!(
            (req.method, req.body.as_deref()),
            (Method::Post, Some(r#"{"doi":"10.1038/nature12373"}"#))
        );
        assert_eq!(
            req.headers.get("Authorization").map(String::as_str),
            Some("Bearer ck")
        );
        assert_eq!(
            req.headers.get("Content-Type").map(String::as_str),
            Some("application/json")
        );
    }

    #[test]
    fn toggles_switch_the_free_sources_off() {
        let ids = Ids {
            pmcid: Some("PMC1".into()),
            ..doi_ids("10.1101/2023.05.31.23290770")
        };
        let mut oa = oa();
        for src in [
            Source::Openalex,
            Source::Europepmc,
            Source::Biorxiv,
            Source::PmcS3,
        ] {
            assert!(api_request(src, &ids, &oa).is_some(), "{src:?}");
        }
        oa.openalex = false;
        oa.europepmc = false;
        oa.biorxiv = false;
        oa.pmc_s3 = false;
        for src in [
            Source::Openalex,
            Source::Europepmc,
            Source::Biorxiv,
            Source::PmcS3,
        ] {
            assert!(api_request(src, &ids, &oa).is_none(), "{src:?} is off");
        }
    }

    #[test]
    fn biorxiv_needs_a_preprint_doi_and_follows_the_item_url() {
        let oa = oa();
        assert!(
            api_request(
                Source::Biorxiv,
                &doi_ids("10.1371/journal.pmed.1000097"),
                &oa
            )
            .is_none()
        );
        let ids = Ids {
            url: Some("https://www.medrxiv.org/content/10.1101/2023.05.31.23290770v2".into()),
            ..doi_ids("10.1101/2023.05.31.23290770")
        };
        let servers: Vec<String> = api_requests(Source::Biorxiv, &ids, &oa)
            .into_iter()
            .map(|(r, _)| r.url)
            .collect();
        assert_eq!(
            servers,
            vec![
                "https://api.biorxiv.org/details/medrxiv/10.1101/2023.05.31.23290770/na/json",
                "https://api.biorxiv.org/details/biorxiv/10.1101/2023.05.31.23290770/na/json"
            ]
        );
    }

    #[test]
    fn sources_without_an_http_request_return_nothing() {
        let ids = Ids {
            arxiv: Some("1706.03762".into()),
            ..doi_ids("10.1038/nature12373")
        };
        let oa = oa();
        for src in [
            Source::ZoteroOa,
            Source::Arxiv,
            Source::Alma,
            Source::ItemUrl,
            Source::Translator,
            Source::Extractor,
        ] {
            assert!(api_requests(src, &ids, &oa).is_empty(), "{src:?}");
        }
    }

    #[test]
    fn every_request_is_a_json_call_with_the_api_limits_and_the_right_spec() {
        let ids = Ids {
            pmcid: Some("PMC2707599".into()),
            ..doi_ids("10.1371/journal.pmed.1000097")
        };
        let oa = oa();
        for src in [
            Source::Openalex,
            Source::Europepmc,
            Source::PmcS3,
            Source::Doi,
        ] {
            for (req, spec) in api_requests(src, &ids, &oa) {
                assert_eq!(
                    req.headers.get("Accept").map(String::as_str),
                    Some("application/json"),
                    "{src:?}"
                );
                assert_eq!(
                    (req.max_bytes, req.timeout_ms, req.ua, req.follow_redirects),
                    (API_MAX_BYTES, 9_000, Ua::Default, true)
                );
                assert_eq!(
                    spec,
                    InspectSpec::Api {
                        source: src,
                        requested: ids.clone()
                    }
                );
            }
        }
    }

    #[test]
    fn dois_with_odd_characters_are_encoded_per_segment() {
        let ids = doi_ids("10.1002/(sici)1097-0258(19980430)17:8<873::aid-sim780>3.0.co;2-i");
        let url = url_of(Source::Openalex, &ids, &oa()).expect("url");
        assert_eq!(
            url,
            "https://api.openalex.org/works/doi:10.1002/(sici)1097-0258(19980430)17%3A8%3C873%3A%3Aaid-sim780%3E3.0.co%3B2-i"
        );
    }
}
