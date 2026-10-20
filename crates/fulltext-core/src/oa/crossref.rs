//! Crossref works API: `GET https://api.crossref.org/works/<doi>`. Reported with `Source::Doi` (the frozen `Source`
//! enum has no Crossref variant; `Source::Doi` means "DOI registry metadata").
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! 404 is plain text `Resource not found.` -> `NotFound`. `message.link[]` entries with `content-type: application/pdf`
//! -> `Pdf` candidates (links whose `intended-application` is `text-mining` carry `signal = "crossref:link_tdm"` so
//! they rank below normal ones; `content-version` `vor` / `am` gives the version); `message.resource.primary.URL` ->
//! `Landing`; `message.license[]` entries with a Creative Commons URL give the licence slug of the candidates.
//! `type` `posted-content` or `subtype` `preprint` set `oa_status` to `preprint`. Never claims open access by itself:
//! these are the publisher's own URLs, useful for the institutional route. `learned.doi` is only offered when the
//! request had no DOI. Fixtures: `fixtures/oa/crossref__*`.
use super::util::{self, CandidateSet};
use crate::model::{ApiInspection, CandKind, HostKind, Ids, LearnedIds, RespMeta, Source, Version};
use serde_json::Value;

const SOURCE: Source = Source::Doi;

/// Parse a Crossref work record.
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    if let Some(done) = super::gate(SOURCE, resp) {
        return done;
    }
    let Some(root) = util::json(body).filter(Value::is_object) else {
        return super::unparsed(SOURCE, "bad_json");
    };
    let Some(message) = root.get("message").filter(|m| m.is_object()) else {
        return super::unparsed(SOURCE, "unexpected_shape");
    };

    let record_doi = util::doi_of(message, "DOI");
    let learned = LearnedIds {
        doi: record_doi.clone().filter(|_| requested.doi.is_none()),
        ..Default::default()
    };
    let preprint = util::str_of(message, "type") == Some("posted-content")
        || util::str_of(message, "subtype") == Some("preprint");
    let oa_status = preprint.then(|| "preprint".to_owned());
    let license = util::array(message, "license")
        .iter()
        .find_map(|l| util::str_of(l, "URL"))
        .and_then(license_from_url);

    let build = |url: &str, kind: CandKind, version: Version, signal: &str| {
        util::candidate(SOURCE, kind, url).map(|mut c| {
            c.version = version;
            c.license = license.clone();
            c.record_doi = record_doi.clone();
            if c.host_kind == HostKind::Other {
                c.host_kind = HostKind::Publisher;
            }
            c.signal = Some(signal.to_owned());
            c
        })
    };

    let mut set = CandidateSet::default();
    for link in util::array(message, "link") {
        let is_pdf = util::str_of(link, "content-type")
            .is_some_and(|t| t.eq_ignore_ascii_case("application/pdf"));
        let Some(url) = util::str_of(link, "URL").filter(|_| is_pdf) else {
            continue;
        };
        let text_mining = util::str_of(link, "intended-application") == Some("text-mining");
        let version = util::str_of(link, "content-version")
            .map(super::version_from_str)
            .unwrap_or(Version::Unknown);
        let signal = if text_mining {
            "crossref:link_tdm"
        } else {
            "crossref:link"
        };
        set.push(build(url, CandKind::Pdf, version, signal));
    }
    let primary = message
        .get("resource")
        .and_then(|r| r.get("primary"))
        .and_then(|p| util::str_of(p, "URL"));
    if let Some(url) = primary {
        set.push(build(
            url,
            CandKind::Landing,
            Version::Unknown,
            "crossref:resource_primary",
        ));
    }
    util::finish(SOURCE, learned, set.into_vec(), oa_status, None)
}

/// `http://creativecommons.org/licenses/by/4.0/` -> `cc-by`; `.../publicdomain/zero/1.0/` -> `cc0`; other URLs
/// (publisher TDM terms, `www.springer.com/tdm`) are not open licences and give `None`.
fn license_from_url(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let rest = lower.split_once("creativecommons.org/")?.1;
    if rest.starts_with("publicdomain/zero") {
        return Some("cc0".to_owned());
    }
    let slug = rest.strip_prefix("licenses/")?.split('/').next()?;
    super::license_slug(&format!("cc-{slug}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ApiStatus;

    #[test]
    fn licence_urls() {
        assert_eq!(
            license_from_url("http://creativecommons.org/licenses/by/4.0/").as_deref(),
            Some("cc-by")
        );
        assert_eq!(
            license_from_url("https://creativecommons.org/licenses/by-nc-nd/4.0").as_deref(),
            Some("cc-by-nc-nd")
        );
        assert_eq!(
            license_from_url("https://creativecommons.org/publicdomain/zero/1.0/").as_deref(),
            Some("cc0")
        );
        assert_eq!(license_from_url("http://www.springer.com/tdm"), None);
    }

    #[test]
    fn garbage() {
        let resp = RespMeta {
            status: 200,
            ..Default::default()
        };
        for body in [
            &b""[..],
            b"Resource not found.",
            b"{}",
            b"[]",
            b"{\"message\":3}",
        ] {
            assert_eq!(
                parse(&resp, body, &Ids::default()).status,
                ApiStatus::Unparseable,
                "{body:?}"
            );
        }
    }
}
