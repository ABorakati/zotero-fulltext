//! Zotero's open-access service. In production JS calls `Zotero.Utilities.Internal.getOpenAccessPDFURLs(doi)` (an
//! `Effect::ZoteroOa`), so the engine converts hits with [`candidates_from_hit`]; [`parse`] additionally accepts the
//! raw `POST https://services.zotero.org/oa/search` body (`[{"url","pageURL","version"}]`, `[]` for unknown / closed /
//! DataCite DOIs) so fixtures and a future direct call share one code path.
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! The service is an Unpaywall mirror: `url` is `url_for_pdf`, `pageURL` is `url_for_landing_page`. It gives no
//! licence and no OA colour. A hit with only a `pageURL` yields one `Landing` candidate; a PMC `pageURL` also teaches
//! `learned.pmcid`.
use super::util::{self, CandidateSet};
use crate::ids;
use crate::model::{
    ApiInspection, CandKind, Candidate, Ids, LearnedIds, RespMeta, Source, Version,
};
use serde_json::Value;

const SOURCE: Source = Source::ZoteroOa;

/// Parse the raw service body. `[]` -> `Empty`.
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    let _ = requested;
    if let Some(done) = super::gate(SOURCE, resp) {
        return done;
    }
    let Some(Value::Array(hits)) = util::json(body) else {
        return super::unparsed(SOURCE, "unexpected_shape");
    };
    if hits.is_empty() {
        return util::nothing(SOURCE, "no_hits");
    }
    let mut set = CandidateSet::default();
    let mut learned = LearnedIds::default();
    for hit in &hits {
        let page = util::str_of(hit, "pageURL");
        for c in candidates_from_hit(
            util::str_of(hit, "url").unwrap_or(""),
            page,
            util::str_of(hit, "version"),
        ) {
            set.push(Some(c));
        }
        if learned.pmcid.is_none() {
            learned.pmcid = page.and_then(ids::clean_pmcid);
        }
    }
    util::finish(SOURCE, learned, set.into_vec(), None, None)
}

/// Candidates for one hit: `url` as a `Pdf` candidate (version from `version`, e.g. `submittedVersion`), and
/// `page_url` as a `Landing` candidate when it differs. An empty `url` (a hit that has only a page) gives only the
/// landing candidate; URLs that are not http(s) are dropped.
pub fn candidates_from_hit(
    url: &str,
    page_url: Option<&str>,
    version: Option<&str>,
) -> Vec<Candidate> {
    let version = version
        .map(super::version_from_str)
        .unwrap_or(Version::Unknown);
    let landing = page_url.and_then(util::https);
    let mut out = Vec::new();
    if let Some(mut pdf) = util::candidate(SOURCE, CandKind::Pdf, url) {
        pdf.version = version;
        pdf.landing_url = landing.clone().filter(|l| *l != pdf.url);
        pdf.signal = Some("zotero_oa:url".to_owned());
        out.push(pdf);
    }
    if let Some(page) = page_url.and_then(|p| util::candidate(SOURCE, CandKind::Landing, p)) {
        let same_as_pdf = out.first().is_some_and(|p| p.url == page.url);
        if !same_as_pdf {
            out.push(Candidate {
                version,
                signal: Some("zotero_oa:page_url".to_owned()),
                ..page
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ApiStatus;

    #[test]
    fn hit_conversion() {
        let both = candidates_from_hit(
            "https://arxiv.org/pdf/1304.1068",
            Some("http://arxiv.org/abs/1304.1068"),
            Some("submittedVersion"),
        );
        assert_eq!(both.len(), 2);
        assert_eq!(
            (both[0].kind, both[0].version),
            (CandKind::Pdf, Version::Submitted)
        );
        assert_eq!(
            both[0].landing_url.as_deref(),
            Some("https://arxiv.org/abs/1304.1068")
        );
        assert_eq!(
            (both[1].kind, both[1].url.as_str()),
            (CandKind::Landing, "https://arxiv.org/abs/1304.1068")
        );
        let page_only = candidates_from_hit("", Some("https://doi.org/10.1/x"), None);
        assert_eq!(page_only.len(), 1);
        assert!(candidates_from_hit("", None, None).is_empty());
        assert!(candidates_from_hit("javascript:void(0)", Some("mailto:a@b.c"), None).is_empty());
    }

    #[test]
    fn garbage() {
        let resp = RespMeta {
            status: 200,
            ..Default::default()
        };
        for body in [&b""[..], b"{}", b"<html>", b"3"] {
            assert_eq!(
                parse(&resp, body, &Ids::default()).status,
                ApiStatus::Unparseable,
                "{body:?}"
            );
        }
        assert_eq!(
            parse(&resp, b"[]", &Ids::default()).status,
            ApiStatus::Empty
        );
        assert_eq!(
            parse(&resp, b"[{}]", &Ids::default()).status,
            ApiStatus::Empty
        );
    }
}
