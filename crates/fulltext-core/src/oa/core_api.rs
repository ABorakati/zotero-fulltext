//! CORE API v3: `POST https://api.core.ac.uk/v3/discover` and `GET /v3/outputs/<id>`. Off unless the user adds a key.
//! (Named `core_api` because `core` would shadow the `core` crate.)
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! `/discover` returns a link even for paywalled works (a false positive: the output id then 404s), so every candidate
//! from here is host_kind Aggregator, version Unknown, and ranks below real publisher and repository copies.
//! `/discover` yields the wrapped reader URL (`fullTextLink?url=` is unwrapped) as a `Landing` candidate; output
//! records contribute `downloadUrl` (`Pdf`) and `sourceFulltextUrls` (`Landing`), and a `/search/works` answer
//! (`results[]`) is read like a list of outputs. An output whose DOI differs from the requested one is `Empty`
//! (note `doi_mismatch`). 404 / 401 / 500 without a key -> `NotFound` / `Unparseable` through the status gate.
//! Fixtures: `fixtures/oa/core__*`.
use super::util::{self, CandidateSet};
use crate::ids;
use crate::model::{ApiInspection, CandKind, HostKind, Ids, LearnedIds, RespMeta, Source};
use crate::urls;
use serde_json::Value;

const SOURCE: Source = Source::Core;

/// Parse a CORE discover or output response.
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    if let Some(done) = super::gate(SOURCE, resp) {
        return done;
    }
    let Some(root) = util::json(body).filter(Value::is_object) else {
        return super::unparsed(SOURCE, "bad_json");
    };
    if let Some(link) = util::str_of(&root, "fullTextLink") {
        return discover(link);
    }
    if let Some(results) = root.get("results").and_then(Value::as_array) {
        let hit = results
            .iter()
            .find(|r| doi_matches(r, requested))
            .or_else(|| results.first().filter(|_| requested.doi.is_none()));
        return match hit {
            Some(output) => output_record(output, requested),
            None => util::nothing(SOURCE, "no_results"),
        };
    }
    if root.get("downloadUrl").is_some()
        || root.get("sourceFulltextUrls").is_some()
        || root.get("id").is_some()
    {
        return output_record(&root, requested);
    }
    super::unparsed(SOURCE, "unexpected_shape")
}

/// `{"fullTextLink": "http://core.ac.uk/labs/oadiscovery/redirect?url=<reader url>&key=...", "source": "From CORE"}`.
fn discover(link: &str) -> ApiInspection {
    let wrapped = urls::parse_http(link)
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "url")
                .map(|(_, v)| v.into_owned())
        })
        .and_then(|inner| util::https(&inner));
    let url = wrapped.or_else(|| util::https(link));
    let mut set = CandidateSet::default();
    set.push(url.and_then(|u| aggregator(&u, CandKind::Landing, "core:discover")));
    util::finish(
        SOURCE,
        LearnedIds::default(),
        set.into_vec(),
        None,
        Some("unverified".to_owned()),
    )
}

fn aggregator(url: &str, kind: CandKind, signal: &str) -> Option<crate::model::Candidate> {
    util::candidate(SOURCE, kind, url).map(|mut c| {
        c.host_kind = HostKind::Aggregator;
        c.signal = Some(signal.to_owned());
        c
    })
}

fn doi_matches(output: &Value, requested: &Ids) -> bool {
    matches!((output_doi(output), requested.doi.as_deref()), (Some(got), Some(want)) if ids::same_doi(&got, want))
}

fn output_doi(output: &Value) -> Option<String> {
    util::doi_of(output, "doi").or_else(|| {
        output
            .get("identifiers")
            .and_then(|i| util::doi_of(i, "doi"))
    })
}

/// One CORE output record.
fn output_record(output: &Value, requested: &Ids) -> ApiInspection {
    let record_doi = output_doi(output);
    if let (Some(got), Some(want)) = (record_doi.as_deref(), requested.doi.as_deref())
        && !ids::same_doi(got, want)
    {
        return util::nothing(SOURCE, "doi_mismatch");
    }
    let learned = LearnedIds {
        doi: record_doi.clone().filter(|_| requested.doi.is_none()),
        ..Default::default()
    };
    let license = util::str_of(output, "license").and_then(super::license_slug);
    let mut set = CandidateSet::default();
    let pdf = util::str_of(output, "downloadUrl")
        .and_then(|u| aggregator(u, CandKind::Pdf, "core:downloadUrl"));
    set.push(pdf);
    for url in util::array(output, "sourceFulltextUrls")
        .iter()
        .filter_map(Value::as_str)
    {
        let kind = if util::looks_like_pdf(url) {
            CandKind::Pdf
        } else {
            CandKind::Landing
        };
        set.push(aggregator(url, kind, "core:sourceFulltextUrls"));
    }
    let mut candidates = set.into_vec();
    for c in &mut candidates {
        c.license = license.clone();
        c.record_doi = record_doi.clone();
    }
    util::finish(SOURCE, learned, candidates, None, None)
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
    fn discover_link_is_unwrapped() {
        let body = br#"{"fullTextLink":"http://core.ac.uk/labs/oadiscovery/redirect?url=https%3A%2F%2Fcore.ac.uk%2Freader%2F29786010&key=ABC","source":"From CORE"}"#;
        let out = parse(&resp(), body, &Ids::default());
        assert_eq!(out.status, ApiStatus::Found);
        assert_eq!(out.candidates[0].url, "https://core.ac.uk/reader/29786010");
        assert_eq!(out.candidates[0].kind, CandKind::Landing);
    }

    #[test]
    fn mismatching_output_is_empty() {
        let body =
            br#"{"id":1,"doi":"10.1234/other","downloadUrl":"https://core.ac.uk/download/1.pdf"}"#;
        let want = Ids {
            doi: Some("10.1234/wanted".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            parse(&resp(), body, &want).note.as_deref(),
            Some("doi_mismatch")
        );
    }

    #[test]
    fn garbage() {
        for body in [&b""[..], b"[]", b"{}", b"<html>", b"null"] {
            assert_eq!(
                parse(&resp(), body, &Ids::default()).status,
                ApiStatus::Unparseable,
                "{body:?}"
            );
        }
    }
}
