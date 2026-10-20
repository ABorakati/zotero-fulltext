//! Semantic Scholar Graph API: `GET https://api.semanticscholar.org/graph/v1/paper/DOI:<doi>?fields=title,externalIds,isOpenAccess,openAccessPdf,publicationTypes`.
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`). Off unless the user adds a key.
//!
//! 404 (JSON `{"error": ...}`) -> `NotFound`; 429 -> `Throttled`. `openAccessPdf.url` is `""` or null when absent ->
//! `Empty`; otherwise one candidate: a `Pdf` when the URL is a file, a `Landing` when it is a page (the PLOS gold case
//! returns `https://doi.org/...` here), host_kind Aggregator unless the URL names a recognised host. `externalIds`
//! (`PubMed`, `PubMedCentral`, `ArXiv`, `DOI`) -> `learned`; `openAccessPdf.status` (`GOLD`, `BRONZE`, ...) ->
//! `oa_status`. Also accepts a `/paper/batch` array whose unknown ids are `null` entries: the entry matching
//! `requested.doi` is used, else the first non-null one. Fixtures: `fixtures/oa/semanticscholar__*`.
use super::util;
use crate::ids;
use crate::model::{ApiInspection, CandKind, HostKind, Ids, LearnedIds, RespMeta, Source, Version};
use serde_json::Value;

const SOURCE: Source = Source::SemanticScholar;

/// Parse a single-paper or batch response.
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    if let Some(done) = super::gate(SOURCE, resp) {
        return done;
    }
    let Some(root) = util::json(body) else {
        return super::unparsed(SOURCE, "bad_json");
    };
    let paper = match &root {
        Value::Array(entries) => match pick(entries, requested) {
            Some(paper) => paper,
            None => return util::nothing(SOURCE, "no_paper"),
        },
        Value::Object(_) => &root,
        _ => return super::unparsed(SOURCE, "unexpected_shape"),
    };
    if paper.get("paperId").is_none()
        && paper.get("externalIds").is_none()
        && paper.get("openAccessPdf").is_none()
    {
        return super::unparsed(SOURCE, "unexpected_shape");
    }

    let external = paper.get("externalIds");
    let from_external = |key: &str| external.and_then(|e| util::id_of(e, key));
    let record_doi = from_external("DOI").as_deref().and_then(ids::clean_doi);
    let learned = LearnedIds {
        doi: record_doi.clone().filter(|_| requested.doi.is_none()),
        pmid: from_external("PubMed").as_deref().and_then(ids::clean_pmid),
        pmcid: from_external("PubMedCentral")
            .as_deref()
            .and_then(ids::clean_pmcid),
        arxiv: from_external("ArXiv").as_deref().and_then(ids::clean_arxiv),
    };

    let open = paper.get("openAccessPdf");
    let oa_status = open
        .and_then(|o| util::str_of(o, "status"))
        .map(str::to_ascii_lowercase);
    let Some(url) = open.and_then(|o| util::str_of(o, "url")) else {
        return ApiInspection {
            learned,
            oa_status,
            ..util::nothing(SOURCE, "no_open_access_pdf")
        };
    };
    let kind = if util::looks_like_pdf(url) {
        CandKind::Pdf
    } else {
        CandKind::Landing
    };
    let candidate = util::candidate(SOURCE, kind, url).map(|mut c| {
        c.version = Version::Unknown;
        c.license = open
            .and_then(|o| util::str_of(o, "license"))
            .and_then(super::license_slug);
        c.record_doi = record_doi;
        if c.host_kind == HostKind::Other {
            c.host_kind = HostKind::Aggregator;
        }
        c.signal = Some("semanticscholar:openAccessPdf".to_owned());
        c
    });
    util::finish(
        SOURCE,
        learned,
        candidate.into_iter().collect(),
        oa_status,
        None,
    )
}

/// From a batch answer: the paper whose DOI matches, else the first one that is not `null`.
fn pick<'a>(entries: &'a [Value], requested: &Ids) -> Option<&'a Value> {
    let doi_matches = |p: &Value| {
        let doi = p.get("externalIds").and_then(|e| util::str_of(e, "DOI"));
        matches!((doi, requested.doi.as_deref()), (Some(a), Some(b)) if ids::same_doi(a, b))
    };
    let present = || entries.iter().filter(|e| e.is_object());
    present()
        .find(|p| doi_matches(p))
        .or_else(|| present().next())
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
    fn empty_url_is_empty_but_ids_are_learned() {
        let body = br#"{"paperId":"x","externalIds":{"DOI":"10.1234/abcd","PubMed":"35848743"},"isOpenAccess":false,"openAccessPdf":{"url":"","status":null,"license":null}}"#;
        let out = parse(&resp(), body, &Ids::default());
        assert_eq!(
            (out.status, out.learned.pmid.as_deref()),
            (ApiStatus::Empty, Some("35848743"))
        );
    }

    #[test]
    fn batch_of_nulls_is_empty_and_garbage_is_not_fatal() {
        assert_eq!(
            parse(&resp(), b"[null,null]", &Ids::default()).status,
            ApiStatus::Empty
        );
        for body in [&b""[..], b"{}", b"7", b"<html>"] {
            assert_eq!(
                parse(&resp(), body, &Ids::default()).status,
                ApiStatus::Unparseable,
                "{body:?}"
            );
        }
    }
}
