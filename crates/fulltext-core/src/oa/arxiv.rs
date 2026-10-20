//! arXiv export API: `GET https://export.arxiv.org/api/query?id_list=<id>` (Atom XML).
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! `totalResults` 0 (a 200) -> `Empty`. Otherwise read the `<entry>` that matches `requested.arxiv` (the only or first
//! one when nothing is requested): emit `https://arxiv.org/pdf/<id>` (version-less) as a `Pdf` candidate (host_kind
//! Preprint, version Submitted, `landing_url` the abstract page, licence from `<arxiv:license>` when present);
//! `<arxiv:doi>` -> `learned.doi` when the request had none, and the entry's id -> `learned.arxiv`. An API error
//! (arXiv reports malformed requests as an `Error` entry in a 200) is `Unparseable`. A tolerant tag scanner reads the
//! feed, not a full XML parser. Fixtures: `fixtures/oa/arxiv__*.xml`.
use super::util::{self, CandidateSet};
use super::xml;
use crate::ids;
use crate::model::{ApiInspection, CandKind, HostKind, Ids, LearnedIds, RespMeta, Source, Version};

const SOURCE: Source = Source::Arxiv;

/// Parse an arXiv Atom feed.
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    if let Some(done) = super::gate(SOURCE, resp) {
        return done;
    }
    let text = crate::text::decode(body);
    if !text.contains("<feed") {
        return super::unparsed(SOURCE, "not_atom");
    }
    let entries = xml::elements(&text, "entry");
    if entries.is_empty() {
        return util::nothing(SOURCE, "no_entries");
    }
    // Each entry: (arXiv id from `<id>`, the entry).
    let described: Vec<(Option<String>, &xml::Element)> = entries
        .iter()
        .map(|e| {
            (
                xml::first_text(e.inner, "id").and_then(|id| ids::clean_arxiv(&id)),
                e,
            )
        })
        .collect();
    if entries
        .iter()
        .any(|e| xml::first_text(e.inner, "id").is_some_and(|id| id.contains("/api/errors")))
    {
        return super::unparsed(SOURCE, "api_error");
    }
    let chosen = match requested.arxiv.as_deref() {
        Some(want) => described.iter().find(|(id, _)| id.as_deref() == Some(want)),
        None => described.first(),
    };
    let Some((Some(id), entry)) = chosen else {
        return util::nothing(SOURCE, "no_match");
    };

    let journal_doi = xml::first_text(entry.inner, "arxiv:doi")
        .as_deref()
        .and_then(ids::clean_doi);
    let license = xml::first_text(entry.inner, "arxiv:license")
        .or_else(|| xml::first_text(entry.inner, "rights"))
        .as_deref()
        .and_then(license_from_text);
    let learned = LearnedIds {
        doi: journal_doi.clone().filter(|_| requested.doi.is_none()),
        arxiv: Some(id.clone()).filter(|_| requested.arxiv.is_none()),
        ..Default::default()
    };
    let mut set = CandidateSet::default();
    set.push(
        util::candidate(
            SOURCE,
            CandKind::Pdf,
            &format!("https://arxiv.org/pdf/{id}"),
        )
        .map(|mut c| {
            c.version = Version::Submitted;
            c.license = license;
            c.host_kind = HostKind::Preprint;
            c.record_doi = journal_doi;
            c.landing_url = Some(format!("https://arxiv.org/abs/{id}"));
            c.signal = Some("arxiv:api".to_owned());
            c
        }),
    );
    util::finish(SOURCE, learned, set.into_vec(), None, None)
}

/// A licence from free text or a licence URL: only Creative Commons forms are recognised.
fn license_from_text(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let rest = lower.split_once("creativecommons.org/licenses/")?.1;
    let slug = rest.split('/').next()?;
    super::license_slug(&format!("cc-{slug}"))
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
    fn licence_urls() {
        assert_eq!(
            license_from_text("http://creativecommons.org/licenses/by-nc-sa/4.0/").as_deref(),
            Some("cc-by-nc-sa")
        );
        assert_eq!(license_from_text("arXiv.org perpetual license"), None);
    }

    #[test]
    fn error_entry_and_garbage() {
        let err = "<feed><entry><id>http://arxiv.org/api/errors#incorrect_id_format_for_1</id><title>Error</title></entry></feed>";
        assert_eq!(
            parse(&resp(), err.as_bytes(), &Ids::default())
                .note
                .as_deref(),
            Some("api_error")
        );
        for body in [&b""[..], b"<html></html>", b"{}", b"\xff"] {
            assert_eq!(
                parse(&resp(), body, &Ids::default()).status,
                ApiStatus::Unparseable,
                "{body:?}"
            );
        }
        assert_eq!(
            parse(&resp(), b"<feed></feed>", &Ids::default()).status,
            ApiStatus::Empty
        );
    }

    #[test]
    fn matches_the_requested_entry() {
        let feed = "<feed><entry><id>http://arxiv.org/abs/1706.03762v7</id></entry><entry><id>http://arxiv.org/abs/hep-th/9901001v2</id><arxiv:doi>10.1234/abcd.5</arxiv:doi></entry></feed>";
        let want = Ids {
            arxiv: Some("hep-th/9901001".to_owned()),
            ..Default::default()
        };
        let out = parse(&resp(), feed.as_bytes(), &want);
        assert_eq!(
            out.candidates[0].url,
            "https://arxiv.org/pdf/hep-th/9901001"
        );
        assert_eq!(out.learned.doi.as_deref(), Some("10.1234/abcd.5"));
    }
}
