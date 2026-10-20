//! Unpaywall v2: `GET https://api.unpaywall.org/v2/<doi>?email=<email>`. Only used when the user configured an email.
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! 404 (an HTML body) -> `NotFound`; 422 (JSON body, missing or placeholder email) -> `Unparseable` with note
//! `bad_email`. `oa_locations[]` (+ `best_oa_location`): `url_for_pdf` -> `Pdf`, `url_for_landing_page` / `url` ->
//! `Landing`; `version` (`publishedVersion` ...), `license`, `host_type` (-> host_kind when the URL does not decide);
//! `oa_status` = `oa_status`; `is_oa == false` -> `Empty`. `evidence` and `updated` may be the string `"deprecated"`
//! and are never read. A PMC location teaches `learned.pmcid`. Fixtures: `fixtures/oa/unpaywall__*`.
use super::util::{self, CandidateSet};
use crate::ids;
use crate::model::{ApiInspection, CandKind, HostKind, Ids, LearnedIds, RespMeta, Source, Version};
use serde_json::Value;

const SOURCE: Source = Source::Unpaywall;

/// Parse an Unpaywall v2 record.
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    if let Some(mut done) = super::gate(SOURCE, resp) {
        if resp.status == 422 {
            done.note = Some("bad_email".to_owned());
        }
        return done;
    }
    let Some(root) = util::json(body).filter(Value::is_object) else {
        return super::unparsed(SOURCE, "bad_json");
    };
    if root.get("error").and_then(Value::as_bool) == Some(true) {
        let bad_email = root.get("HTTP_status_code").and_then(Value::as_u64) == Some(422);
        return super::unparsed(SOURCE, if bad_email { "bad_email" } else { "api_error" });
    }
    if root.get("oa_locations").is_none() && root.get("is_oa").is_none() {
        return super::unparsed(SOURCE, "unexpected_shape");
    }

    let record_doi = util::doi_of(&root, "doi");
    let oa_status = util::str_of(&root, "oa_status").map(str::to_ascii_lowercase);
    let locations = util::array(&root, "oa_locations");
    let mut learned = LearnedIds {
        doi: record_doi.clone().filter(|_| requested.doi.is_none()),
        ..Default::default()
    };
    learned.pmcid = locations.iter().find_map(pmcid_of_location);
    if root.get("is_oa").and_then(Value::as_bool) == Some(false) {
        return ApiInspection {
            learned,
            oa_status,
            ..util::nothing(SOURCE, "closed")
        };
    }

    let mut set = CandidateSet::default();
    for location in locations.iter().chain(root.get("best_oa_location")) {
        add_location(&mut set, location, record_doi.as_deref());
    }
    util::finish(SOURCE, learned, set.into_vec(), oa_status, None)
}

/// `oai:pubmedcentral.nih.gov:4221854` or a PMC landing page names the article's PMCID.
fn pmcid_of_location(location: &Value) -> Option<String> {
    let pmh = util::str_of(location, "pmh_id").map(str::to_ascii_lowercase);
    pmh.as_deref()
        .and_then(|p| p.strip_prefix("oai:pubmedcentral.nih.gov:"))
        .and_then(ids::clean_pmcid)
        .or_else(|| {
            ["url_for_landing_page", "url_for_pdf", "url"]
                .iter()
                .filter_map(|k| util::str_of(location, k))
                .find_map(ids::clean_pmcid)
        })
}

fn add_location(set: &mut CandidateSet, location: &Value, record_doi: Option<&str>) {
    let version = util::str_of(location, "version")
        .map(super::version_from_str)
        .unwrap_or(Version::Unknown);
    let license = util::str_of(location, "license").and_then(super::license_slug);
    let host_type = util::str_of(location, "host_type");
    let landing =
        util::str_of(location, "url_for_landing_page").or_else(|| util::str_of(location, "url"));

    let build = |url: &str, kind: CandKind, field: &str| {
        util::candidate(SOURCE, kind, url).map(|mut c| {
            c.version = version;
            c.license = license.clone();
            c.record_doi = record_doi.map(str::to_owned);
            c.signal = Some(format!("unpaywall:{field}"));
            if c.host_kind == HostKind::Other {
                c.host_kind = match host_type {
                    Some("publisher") => HostKind::Publisher,
                    Some("repository") => HostKind::Repository,
                    _ => HostKind::Other,
                };
            }
            c
        })
    };
    if let Some(pdf) = util::str_of(location, "url_for_pdf") {
        let mut c = build(pdf, CandKind::Pdf, "url_for_pdf");
        if let (Some(c), Some(page)) = (c.as_mut(), landing.and_then(util::https)) {
            c.landing_url = Some(page).filter(|p| *p != c.url);
        }
        set.push(c);
    }
    if let Some(page) = landing {
        let kind = if util::looks_like_pdf(page) {
            CandKind::Pdf
        } else {
            CandKind::Landing
        };
        set.push(build(page, kind, "url_for_landing_page"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ApiStatus;

    #[test]
    fn placeholder_email_is_reported() {
        let resp = RespMeta {
            status: 422,
            ..Default::default()
        };
        let out = parse(
            &resp,
            br#"{"HTTP_status_code":422,"error":true,"message":"x"}"#,
            &Ids::default(),
        );
        assert_eq!(
            (out.status, out.note.as_deref()),
            (ApiStatus::Unparseable, Some("bad_email"))
        );
    }

    #[test]
    fn closed_record_is_empty() {
        let resp = RespMeta {
            status: 200,
            ..Default::default()
        };
        let out = parse(
            &resp,
            br#"{"doi":"10.1234/abcd","is_oa":false,"oa_status":"closed","oa_locations":[]}"#,
            &Ids::default(),
        );
        assert_eq!(
            (out.status, out.oa_status.as_deref()),
            (ApiStatus::Empty, Some("closed"))
        );
    }

    #[test]
    fn garbage() {
        let resp = RespMeta {
            status: 200,
            ..Default::default()
        };
        for body in [&b""[..], b"<html>", b"[]", b"{}", b"null"] {
            assert_eq!(
                parse(&resp, body, &Ids::default()).status,
                ApiStatus::Unparseable,
                "{body:?}"
            );
        }
    }
}
