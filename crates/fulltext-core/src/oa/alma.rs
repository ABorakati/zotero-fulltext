//! Alma Call-To-Action entitlement oracle and full-text link source:
//! `GET https://librarysearch.kcl.ac.uk/view/uresolver/44KCL_INST/openurl?ctx_ver=Z39.88-2004&url_ver=Z39.88-2004&rft_id=info:doi/<DOI>&svc_dat=CTO`
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`; live URL shapes: `docs/design/core.md`, Alma section).
//!
//! XML. One `<context_service service_type="getFullTxt">` per entitled portfolio, whose `<key id="...">` children
//! include `package_public_name`, `Availability`, `Is_free`, `current_access` and a `resolution_url` (an element
//! after `<keys>`, or a key; research/04 section 4.5).
//!
//! Result: `Found` when at least one `getFullTxt` service has an http(s) `resolution_url`, else `Empty` ("not
//! held", note `not_held`). Each service becomes a candidate to open in the user's own browser session: the
//! `resolution_url` is an Alma redirector that 302s to the publisher or aggregator (never EZproxy-wrapped), so it
//! needs the user's cookies (`needs_browser`). Candidate fields: `kind` `Pdf` only when the URL is evidently a file,
//! else `Landing`; `source` `Alma`; `version` `Published`; `host_kind` `Publisher`; `record_doi` the requested DOI;
//! `signal = "alma:<package_public_name>"` (80 chars at most). Services whose `current_access` is `false` (the
//! coverage window ended before this article) are dropped when another service is current. Order: current before
//! lapsed, paid before free packages, then document order; at most [`MAX_CANDIDATES`]. `oa_status` = `free` when
//! a retained service has `Is_free` 1. The engine caches held / not-held per DOI prefix + ISSN in `Persisted.alma`.
//! Inputs: real captures in `tests/data/oa/alma-*.xml`.
use super::util::{self, CandidateSet};
use super::xml;
use crate::ids;
use crate::model::{ApiInspection, CandKind, HostKind, Ids, LearnedIds, RespMeta, Source, Version};

const SOURCE: Source = Source::Alma;

/// Most candidates one response yields.
const MAX_CANDIDATES: usize = 5;

/// Longest `signal` (characters).
const MAX_SIGNAL: usize = 80;

/// Parse an Alma CTO response.
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
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
    let record_doi = requested.doi.as_deref().and_then(ids::clean_doi);
    // (candidate, current access, free package)
    let mut found = Vec::new();
    for service in xml::elements(&text, "context_service") {
        if xml::attr(service.attrs, "service_type").as_deref() != Some("getFullTxt") {
            continue;
        }
        let is_free = key(service.inner, "Is_free").as_deref() == Some("1");
        let current = key(service.inner, "current_access").as_deref() != Some("false");
        let url = xml::first_text(service.inner, "resolution_url")
            .or_else(|| key(service.inner, "resolution_url"));
        let Some(mut c) = url.and_then(|u| util::candidate(SOURCE, CandKind::Landing, &u)) else {
            continue;
        };
        if util::looks_like_pdf(&c.url) {
            c.kind = CandKind::Pdf;
        }
        c.version = Version::Published;
        c.host_kind = HostKind::Publisher;
        c.record_doi.clone_from(&record_doi);
        c.signal = Some(format!("alma:{}", package_name(service.inner)));
        c.needs_browser = true;
        found.push((c, current, is_free));
    }
    if found.iter().any(|(_, current, _)| *current) {
        found.retain(|(_, current, _)| *current);
    }
    // Stable: paid packages before free ones (the free "bulk" targets are archives, rarely this article).
    found.sort_by_key(|(_, _, is_free)| *is_free);
    let free = found.iter().any(|(_, _, is_free)| *is_free);
    let mut set = CandidateSet::default();
    for (c, _, _) in found {
        set.push(Some(c));
    }
    if set.is_empty() {
        return util::nothing(SOURCE, "not_held");
    }
    let mut candidates = set.into_vec();
    candidates.truncate(MAX_CANDIDATES);
    util::finish(
        SOURCE,
        LearnedIds::default(),
        candidates,
        free.then(|| "free".to_owned()),
        None,
    )
}

/// The package (or service) name for the `signal`, trimmed to [`MAX_SIGNAL`] characters.
fn package_name(inner: &str) -> String {
    let name = [
        "package_public_name",
        "package_display_name",
        "package_name",
        "interface_name",
    ]
    .iter()
    .find_map(|id| key(inner, id))
    .unwrap_or_else(|| "getFullTxt".to_owned());
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_SIGNAL)
        .collect::<String>()
        .trim_end()
        .to_owned()
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

    fn service(name: &str, url: &str, extra: &str) -> String {
        format!(
            r#"<context_service context_service_id="1" service_type="getFullTxt"><keys>
              <key id="package_public_name">{name}</key>{extra}</keys>
              <resolution_url>{url}</resolution_url></context_service>"#
        )
    }

    fn doc(services: &str) -> String {
        format!(
            "<uresolver_content><context_object/><context_services>{services}</context_services></uresolver_content>"
        )
    }

    fn want(doi: &str) -> Ids {
        Ids {
            doi: Some(doi.to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn held_candidate_follows_the_pinned_contract() {
        let held = r#"<uresolver_content><context_object><context_services>
            <context_service context_service_id="1" service_type="getFullTxt"><keys>
              <key id="package_public_name">Nature &amp; Co</key><key id="Is_free">1</key>
              <key id="resolution_url">https://kcl.alma.exlibrisgroup.com/view/action/uresolver.do?operation=resolveService&amp;package_service_id=1</key>
            </keys></context_service>
            <context_service context_service_id="2" service_type="getHoldings"><keys/></context_service>
            </context_services></context_object></uresolver_content>"#;
        let out = parse(&resp(200), held.as_bytes(), &want("10.1038/NATURE12373"));
        assert_eq!(
            (out.status, out.oa_status.as_deref(), out.candidates.len()),
            (ApiStatus::Found, Some("free"), 1)
        );
        let c = &out.candidates[0];
        assert_eq!(c.signal.as_deref(), Some("alma:Nature & Co"));
        assert!(c.url.ends_with("package_service_id=1"));
        assert_eq!(
            (c.kind, c.source, c.version, c.host_kind),
            (
                CandKind::Landing,
                Source::Alma,
                Version::Published,
                HostKind::Publisher
            )
        );
        assert_eq!(c.record_doi.as_deref(), Some("10.1038/nature12373"));
        assert_eq!((c.landing_url.clone(), c.score, c.tier), (None, 0, 0));
        assert!(c.needs_browser);

        let none = "<uresolver_content><context_object><context_services/></context_object></uresolver_content>";
        let out = parse(&resp(200), none.as_bytes(), &Ids::default());
        assert_eq!(
            (out.status, out.note.as_deref()),
            (ApiStatus::Empty, Some("not_held"))
        );
    }

    #[test]
    fn pdf_url_becomes_a_pdf_candidate_and_no_doi_means_none() {
        let body = doc(&service(
            "Direct",
            "https://publisher.example/files/a.pdf",
            "",
        ));
        let out = parse(&resp(200), body.as_bytes(), &Ids::default());
        assert_eq!(out.candidates[0].kind, CandKind::Pdf);
        assert_eq!(out.candidates[0].record_doi, None);
    }

    #[test]
    fn non_http_targets_are_not_candidates() {
        let body = doc(&(service("FTP", "ftp://x.example/a", "")
            + &service("Rel", "/view/action/uresolver.do?x=1", "")
            + &service("Empty", "", "")));
        let out = parse(&resp(200), body.as_bytes(), &Ids::default());
        assert_eq!(
            (out.status, out.note.as_deref()),
            (ApiStatus::Empty, Some("not_held"))
        );
    }

    #[test]
    fn dedupes_caps_and_orders() {
        let mut s = String::new();
        s += &service(
            "Lapsed",
            "https://a.example/lapsed",
            r#"<key id="current_access">false</key>"#,
        );
        s += &service(
            "Free",
            "https://a.example/free",
            r#"<key id="Is_free">1</key><key id="current_access">true</key>"#,
        );
        s += &service("Paid", "https://a.example/paid", "");
        s += &service("Paid again", "https://a.example/paid", "");
        for i in 0..6 {
            s += &service(&format!("P{i}"), &format!("https://a.example/p{i}"), "");
        }
        let out = parse(&resp(200), doc(&s).as_bytes(), &Ids::default());
        let urls: Vec<&str> = out.candidates.iter().map(|c| c.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://a.example/paid",
                "https://a.example/p0",
                "https://a.example/p1",
                "https://a.example/p2",
                "https://a.example/p3"
            ]
        );
        assert_eq!(out.oa_status.as_deref(), Some("free"));
    }

    #[test]
    fn lapsed_services_are_kept_when_nothing_is_current() {
        let body = doc(&service(
            "Backfile",
            "https://a.example/old",
            r#"<key id="current_access">false</key>"#,
        ));
        let out = parse(&resp(200), body.as_bytes(), &Ids::default());
        assert_eq!((out.status, out.candidates.len()), (ApiStatus::Found, 1));
    }

    #[test]
    fn signal_is_trimmed_to_80_characters() {
        let long = "Oxford University Press:Jisc Collections:OUP Read and Publish 2024-2025 (2025 collection) (Reading list)";
        let body = doc(&service(long, "https://a.example/x", ""));
        let out = parse(&resp(200), body.as_bytes(), &Ids::default());
        let signal = out.candidates[0].signal.clone().unwrap();
        assert_eq!(signal.chars().count(), "alma:".len() + 80);
        assert!(signal.starts_with("alma:Oxford University Press:Jisc"));
        // Falls back to other names, then to the service type.
        let body = doc(
            r#"<context_service service_type="getFullTxt"><keys><key id="interface_name">Iface</key></keys><resolution_url>https://a.example/y</resolution_url></context_service>"#,
        );
        let out = parse(&resp(200), body.as_bytes(), &Ids::default());
        assert_eq!(out.candidates[0].signal.as_deref(), Some("alma:Iface"));
        let body = doc(
            r#"<context_service service_type="getFullTxt"><resolution_url>https://a.example/z</resolution_url></context_service>"#,
        );
        let out = parse(&resp(200), body.as_bytes(), &Ids::default());
        assert_eq!(out.candidates[0].signal.as_deref(), Some("alma:getFullTxt"));
    }

    #[test]
    fn hostile_input_never_panics() {
        for body in [
            "<uresolver_content><context_service service_type=\"getFullTxt\"><resolution_url>",
            "<context_services><context_service service_type='getFullTxt'",
            "<context_service service_type=\"getFullTxt\"><keys><key id=\"package_public_name\"><![CDATA[",
            "<context_service service_type=\"getFullTxt\"><resolution_url>http://</resolution_url></context_service>",
            "<context_service service_type=\"getFullTxt\"><resolution_url>https://a\u{0}b</resolution_url></context_service>",
            "<uresolver_content>\u{feff}&#99999999999;&bogus;</uresolver_content>",
        ] {
            let _ = parse(&resp(200), body.as_bytes(), &Ids::default());
        }
        let _ = parse(&resp(200), &[0xff, 0xfe, 0x3c, 0x00], &Ids::default());
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
