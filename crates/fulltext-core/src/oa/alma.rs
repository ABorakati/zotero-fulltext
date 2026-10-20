//! Alma Call-To-Action entitlement oracle:
//! `GET https://librarysearch.kcl.ac.uk/view/uresolver/44KCL_INST/openurl?ctx_ver=Z39.88-2004&url_ver=Z39.88-2004&rft_id=info:doi/<DOI>&svc_dat=CTO`
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! XML. One `<context_service service_type="getFullTxt">` per entitled portfolio, whose `<key id="...">` children
//! include `package_public_name`, `Availability`, `Is_free` and a `resolution_url` (an element or a key,
//! research/04 section 4.5). Result: `Found` when at least one `getFullTxt` service exists, else `Empty` ("not
//! held", note `not_held`); each service becomes a `Landing` candidate (`Source::Alma`, `resolution_url`) with
//! `signal = "alma:<package_public_name>"`; `oa_status` = `free` when any service has `Is_free` 1. The engine caches
//! held / not-held per DOI prefix + ISSN in `Persisted.alma`. The resolver is an entitlement oracle, not a file host:
//! its candidates have no `record_doi` and never `needs_browser`. Hand-made inputs: `tests/data/oa/alma-*.xml`.
use super::util::{self, CandidateSet};
use super::xml;
use crate::model::{ApiInspection, CandKind, Ids, LearnedIds, RespMeta, Source};

const SOURCE: Source = Source::Alma;

/// Parse an Alma CTO response.
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    let _ = requested;
    if let Some(done) = super::gate(SOURCE, resp) {
        return done;
    }
    let text = crate::text::decode(body);
    let alma_markers = [
        "<uresolver_content",
        "<context_object",
        "<context_services",
        "<context_service",
    ];
    if !alma_markers.iter().any(|m| text.contains(m)) {
        return super::unparsed(SOURCE, "not_alma");
    }
    let mut set = CandidateSet::default();
    let mut free = false;
    for service in xml::elements(&text, "context_service") {
        if xml::attr(service.attrs, "service_type").as_deref() != Some("getFullTxt") {
            continue;
        }
        free |= key(service.inner, "Is_free").as_deref() == Some("1");
        let url = xml::first_text(service.inner, "resolution_url")
            .or_else(|| key(service.inner, "resolution_url"));
        let name =
            key(service.inner, "package_public_name").unwrap_or_else(|| "getFullTxt".to_owned());
        set.push(
            url.and_then(|u| util::candidate(SOURCE, CandKind::Landing, &u))
                .map(|mut c| {
                    c.signal = Some(format!("alma:{name}"));
                    c
                }),
        );
    }
    if set.is_empty() {
        return util::nothing(SOURCE, "not_held");
    }
    util::finish(
        SOURCE,
        LearnedIds::default(),
        set.into_vec(),
        free.then(|| "free".to_owned()),
        None,
    )
}

/// The text of `<key id="id">...</key>` inside a service.
fn key(inner: &str, id: &str) -> Option<String> {
    xml::elements(inner, "key")
        .into_iter()
        .filter(|k| xml::attr(k.attrs, "id").as_deref() == Some(id))
        .map(|k| xml::text(k.inner))
        .find(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ApiStatus;

    fn resp(status: u16) -> RespMeta {
        RespMeta {
            status,
            ..Default::default()
        }
    }

    #[test]
    fn held_and_not_held() {
        let held = r#"<uresolver_content><context_object><context_services>
            <context_service context_service_id="1" service_type="getFullTxt"><keys>
              <key id="package_public_name">Nature &amp; Co</key><key id="Is_free">1</key>
              <key id="resolution_url">https://kcl.alma.exlibrisgroup.com/view/action/uresolver.do?operation=resolveService&amp;package_service_id=1</key>
            </keys></context_service>
            <context_service context_service_id="2" service_type="getHoldings"><keys/></context_service>
            </context_services></context_object></uresolver_content>"#;
        let out = parse(&resp(200), held.as_bytes(), &Ids::default());
        assert_eq!(
            (out.status, out.oa_status.as_deref(), out.candidates.len()),
            (ApiStatus::Found, Some("free"), 1)
        );
        assert_eq!(
            out.candidates[0].signal.as_deref(),
            Some("alma:Nature & Co")
        );
        assert!(out.candidates[0].url.ends_with("package_service_id=1"));

        let none = "<uresolver_content><context_object><context_services/></context_object></uresolver_content>";
        assert_eq!(
            parse(&resp(200), none.as_bytes(), &Ids::default()).status,
            ApiStatus::Empty
        );
    }

    #[test]
    fn not_alma_and_error_statuses() {
        assert_eq!(
            parse(
                &resp(200),
                b"<html><body>Login</body></html>",
                &Ids::default()
            )
            .status,
            ApiStatus::Unparseable
        );
        assert_eq!(
            parse(&resp(200), b"", &Ids::default()).status,
            ApiStatus::Unparseable
        );
        assert_eq!(
            parse(&resp(500), b"<uresolver_content/>", &Ids::default()).status,
            ApiStatus::Unparseable
        );
    }
}
