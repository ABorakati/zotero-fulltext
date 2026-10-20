//! OpenAlex works API: `GET https://api.openalex.org/works/doi:<doi>` (also `/works/pmid:<n>`, `filter=` batches).
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! Candidates come from EVERY open-access entry of `locations[]` (never trust `best_oa_location` alone) plus
//! `best_oa_location`, `primary_location` and `open_access.oa_url` as fallbacks: `pdf_url` -> a `Pdf` candidate,
//! `landing_page_url` -> a `Landing` candidate (a landing URL that is itself a file is a `Pdf`); `version`, `license`
//! and `source.type` (-> `host_kind` when the URL does not decide) come per location. `record_doi` is the work-level
//! `doi` (OpenAlex merges records: arXiv maps to `10.65215/...`, medRxiv to the PLOS ONE version; a `note` of
//! `merged_record:<doi>` says so). `learned` carries `ids.pmid` / `ids.pmcid` with the URL prefix stripped, plus the
//! PMC, PubMed and arXiv locations' own identifiers. `oa_status` = `open_access.oa_status`. A cached full text
//! (`content_urls.pdf`) becomes a `Pdf` candidate flagged `openalex:content_api`; it is the paid API, and `rank`
//! tiers it last. A 404 arrives as an HTML body. List answers (`results[]`) are accepted: the first result whose DOI
//! or PMID matches `requested` is used, any result when `requested` names nothing, the only result when it is alone.
//! Fixtures: `fixtures/oa/openalex__*.json`.
use super::util::{self, CandidateSet};
use crate::ids;
use crate::model::{
    ApiInspection, CandKind, Candidate, HostKind, Ids, LearnedIds, RespMeta, Source, Version,
};
use serde_json::Value;

const SOURCE: Source = Source::Openalex;

/// Parse one OpenAlex work (or the first result of a filtered list).
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    if let Some(done) = super::gate(SOURCE, resp) {
        return done;
    }
    let Some(root) = util::json(body) else {
        return super::unparsed(SOURCE, "bad_json");
    };
    let work = match pick_work(&root, requested) {
        Ok(work) => work,
        Err(Miss::Empty(note)) => return util::nothing(SOURCE, note),
        Err(Miss::Shape) => return super::unparsed(SOURCE, "unexpected_shape"),
    };

    let record_doi = util::doi_of(work, "doi");
    let oa = work.get("open_access");
    let is_closed = oa.and_then(|o| o.get("is_oa")).and_then(Value::as_bool) == Some(false);
    let oa_status = oa
        .and_then(|o| util::str_of(o, "oa_status"))
        .map(str::to_ascii_lowercase);

    let mut set = CandidateSet::default();
    for (i, location) in util::array(work, "locations").iter().enumerate() {
        add_location(
            &mut set,
            location,
            &format!("locations[{i}]"),
            record_doi.as_deref(),
        );
    }
    for name in ["best_oa_location", "primary_location"] {
        if let Some(location) = work.get(name) {
            add_location(&mut set, location, name, record_doi.as_deref());
        }
    }
    if !is_closed && let Some(url) = oa.and_then(|o| util::str_of(o, "oa_url")) {
        let kind = if util::looks_like_pdf(url) {
            CandKind::Pdf
        } else {
            CandKind::Landing
        };
        set.push(tagged(
            url,
            kind,
            "open_access.oa_url",
            record_doi.as_deref(),
        ));
    }
    if !is_closed && let Some(url) = content_url(work) {
        set.push(tagged(
            &url,
            CandKind::Pdf,
            "content_api",
            record_doi.as_deref(),
        ));
    }

    let note = match (requested.doi.as_deref(), record_doi.as_deref()) {
        (Some(want), Some(got)) if !ids::same_doi(want, got) => {
            Some(format!("merged_record:{got}"))
        }
        _ => None,
    };
    util::finish(
        SOURCE,
        learned(work, requested, record_doi.as_deref()),
        set.into_vec(),
        oa_status,
        note,
    )
}

/// Why no work could be chosen.
enum Miss {
    /// The API answered but nothing usable matches (note code).
    Empty(&'static str),
    /// Not a work and not a list of works.
    Shape,
}

fn pick_work<'a>(root: &'a Value, requested: &Ids) -> Result<&'a Value, Miss> {
    if let Some(results) = root.get("results").and_then(Value::as_array) {
        return match select(results, requested) {
            Some(work) => Ok(work),
            None if results.is_empty() => Err(Miss::Empty("no_results")),
            None => Err(Miss::Empty("no_match")),
        };
    }
    let looks_like_work = root.is_object()
        && ["locations", "open_access", "id"]
            .iter()
            .any(|k| root.get(k).is_some());
    if looks_like_work {
        Ok(root)
    } else {
        Err(Miss::Shape)
    }
}

fn select<'a>(results: &'a [Value], requested: &Ids) -> Option<&'a Value> {
    let matches = |work: &Value| {
        let doi_hit = requested
            .doi
            .as_deref()
            .is_some_and(|d| util::str_of(work, "doi").is_some_and(|x| ids::same_doi(x, d)));
        let pmid_hit = requested.pmid.is_some()
            && work_ids(work)
                .and_then(|i| util::str_of(i, "pmid"))
                .and_then(ids::clean_pmid)
                == requested.pmid;
        doi_hit || pmid_hit
    };
    let names_something = requested.doi.is_some() || requested.pmid.is_some();
    results.iter().find(|w| matches(w)).or_else(|| {
        (!names_something || results.len() == 1)
            .then(|| results.first())
            .flatten()
    })
}

fn work_ids(work: &Value) -> Option<&Value> {
    work.get("ids")
}

/// Identifiers the work states: `ids.*` first, then the PubMed, PMC and arXiv locations' own ids and URLs.
fn learned(work: &Value, requested: &Ids, record_doi: Option<&str>) -> LearnedIds {
    let ids_obj = work_ids(work);
    let from_ids = |key: &str| ids_obj.and_then(|i| util::str_of(i, key));
    let mut out = LearnedIds {
        pmid: from_ids("pmid").and_then(ids::clean_pmid),
        pmcid: from_ids("pmcid").and_then(ids::clean_pmcid),
        doi: if requested.doi.is_none() {
            record_doi.map(str::to_owned)
        } else {
            None
        },
        arxiv: None,
    };
    for location in util::array(work, "locations") {
        let id = util::str_of(location, "id").unwrap_or("");
        let lower = id.to_ascii_lowercase();
        if out.pmid.is_none() {
            out.pmid = lower.strip_prefix("pmid:").and_then(ids::clean_pmid);
        }
        if out.pmcid.is_none() {
            out.pmcid = lower
                .strip_prefix("pmh:oai:pubmedcentral.nih.gov:")
                .and_then(ids::clean_pmcid)
                .or_else(|| {
                    ["landing_page_url", "pdf_url"]
                        .iter()
                        .filter_map(|k| util::str_of(location, k))
                        .find_map(ids::clean_pmcid)
                });
        }
        if out.arxiv.is_none() && requested.arxiv.is_none() {
            out.arxiv = lower
                .strip_prefix("pmh:oai:arxiv.org:")
                .and_then(ids::clean_arxiv)
                .or_else(|| lower.strip_prefix("doi:").and_then(ids::arxiv_from_doi));
        }
    }
    out
}

/// A candidate tagged `openalex:<label>`.
fn tagged(url: &str, kind: CandKind, label: &str, record_doi: Option<&str>) -> Option<Candidate> {
    let mut c = util::candidate(SOURCE, kind, url)?;
    c.record_doi = record_doi.map(str::to_owned);
    c.signal = Some(format!("openalex:{label}"));
    Some(c)
}

/// Candidates of one location. Locations that are not open access are skipped.
fn add_location(set: &mut CandidateSet, location: &Value, label: &str, record_doi: Option<&str>) {
    if location.get("is_oa").and_then(Value::as_bool) != Some(true) {
        return;
    }
    let version = util::str_of(location, "version")
        .map(super::version_from_str)
        .unwrap_or(Version::Unknown);
    let license = util::str_of(location, "license").and_then(super::license_slug);
    let source_type = location.get("source").and_then(|s| util::str_of(s, "type"));
    let landing = util::str_of(location, "landing_page_url").and_then(util::https);

    let build = |url: &str, kind: CandKind, field: &str| -> Option<Candidate> {
        let mut c = tagged(url, kind, &format!("{label}.{field}"), record_doi)?;
        c.version = version;
        c.license = license.clone();
        c.host_kind = refine_host_kind(c.host_kind, source_type);
        Some(c)
    };
    if let Some(pdf) = util::str_of(location, "pdf_url") {
        let mut c = build(pdf, CandKind::Pdf, "pdf_url");
        if let Some(c) = c.as_mut() {
            c.landing_url = landing.clone().filter(|l| *l != c.url);
        }
        set.push(c);
    }
    if let Some(page) = util::str_of(location, "landing_page_url") {
        let kind = if util::looks_like_pdf(page) {
            CandKind::Pdf
        } else {
            CandKind::Landing
        };
        set.push(build(page, kind, "landing_page_url"));
    }
}

/// The URL decides when it names a recognised host; otherwise `source.type` does (`journal` -> publisher).
fn refine_host_kind(by_url: HostKind, source_type: Option<&str>) -> HostKind {
    if by_url != HostKind::Other {
        return by_url;
    }
    match source_type {
        Some("journal" | "ebook platform" | "book series" | "conference") => HostKind::Publisher,
        Some("repository") => HostKind::Repository,
        _ => HostKind::Other,
    }
}

/// OpenAlex's cached full text: `content_urls.pdf`, else built from the work id when `has_content.pdf` is true.
fn content_url(work: &Value) -> Option<String> {
    if let Some(url) = work
        .get("content_urls")
        .and_then(|c| util::str_of(c, "pdf"))
    {
        return Some(url.to_owned());
    }
    let has_pdf = work
        .get("has_content")
        .and_then(|h| h.get("pdf"))
        .and_then(Value::as_bool)
        == Some(true);
    let id = util::str_of(work, "id")?.rsplit('/').next()?;
    let valid = id
        .strip_prefix('W')
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
    (has_pdf && valid).then(|| format!("https://content.openalex.org/works/{id}.pdf"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ApiStatus;

    fn resp() -> RespMeta {
        RespMeta {
            status: 200,
            ..Default::default()
        }
    }

    #[test]
    fn garbage_is_unparseable_not_a_panic() {
        for body in [
            &b""[..],
            b"null",
            b"[]",
            b"42",
            b"{\"x\":1}",
            b"<html>",
            b"\xff\xfe",
        ] {
            let out = parse(&resp(), body, &Ids::default());
            assert_eq!(out.status, ApiStatus::Unparseable, "{body:?}");
        }
    }

    #[test]
    fn empty_list_is_empty() {
        let out = parse(
            &resp(),
            br#"{"meta":{"count":0},"results":[]}"#,
            &Ids::default(),
        );
        assert_eq!(
            (out.status, out.note.as_deref()),
            (ApiStatus::Empty, Some("no_results"))
        );
    }

    #[test]
    fn oa_url_and_cached_content_are_fallbacks() {
        let body = br#"{"id":"https://openalex.org/W42","doi":"https://doi.org/10.1234/abcd","has_content":{"pdf":true},
            "open_access":{"is_oa":true,"oa_status":"green","oa_url":"http://repo.example.org/files/a.pdf"},"locations":[]}"#;
        let out = parse(&resp(), body, &Ids::default());
        assert_eq!(out.status, ApiStatus::Found);
        let urls: Vec<(&str, CandKind, Option<&str>)> = out
            .candidates
            .iter()
            .map(|c| (c.url.as_str(), c.kind, c.signal.as_deref()))
            .collect();
        assert_eq!(
            urls,
            [
                (
                    "https://repo.example.org/files/a.pdf",
                    CandKind::Pdf,
                    Some("openalex:open_access.oa_url")
                ),
                (
                    "https://content.openalex.org/works/W42.pdf",
                    CandKind::Pdf,
                    Some("openalex:content_api")
                ),
            ]
        );
    }

    #[test]
    fn locations_without_open_access_or_urls_are_skipped() {
        let body = br#"{"id":"https://openalex.org/W1","open_access":{"is_oa":true},"locations":[
            {"is_oa":false,"pdf_url":"https://a.example.org/closed.pdf"},
            {"is_oa":true,"pdf_url":null,"landing_page_url":""},
            {"is_oa":true,"pdf_url":"javascript:void(0)","landing_page_url":"ftp://x.example.org/f"},
            {"pdf_url":"https://a.example.org/no-flag.pdf"},
            {"is_oa":true,"landing_page_url":"https://a.example.org/page","version":"acceptedVersion","license":"CC BY-NC 4.0","source":{"type":"repository"}}]}"#;
        let out = parse(&resp(), body, &Ids::default());
        assert_eq!(out.candidates.len(), 1);
        let c = &out.candidates[0];
        assert_eq!(
            (c.kind, c.version, c.license.as_deref(), c.host_kind),
            (
                CandKind::Landing,
                Version::Accepted,
                Some("cc-by-nc"),
                HostKind::Repository
            )
        );
    }

    #[test]
    fn closed_work_keeps_learned_ids_but_offers_nothing() {
        let body = br#"{"id":"https://openalex.org/W1","doi":"https://doi.org/10.1/abcd","ids":{"pmid":"https://pubmed.ncbi.nlm.nih.gov/35848743"},
            "open_access":{"is_oa":false,"oa_status":"closed","oa_url":"https://x.org/a.pdf"},
            "locations":[{"id":"doi:10.1/abcd","is_oa":false,"pdf_url":"https://x.org/a.pdf","landing_page_url":"https://x.org/a"}]}"#;
        let out = parse(&resp(), body, &Ids::default());
        assert_eq!(
            (
                out.status,
                out.oa_status.as_deref(),
                out.learned.pmid.as_deref()
            ),
            (ApiStatus::Empty, Some("closed"), Some("35848743"))
        );
        assert!(out.candidates.is_empty());
    }
}
