//! `inspect` dispatch rules (the response-shape table in the module docs) and the `login_state` mapping.
//! Cases that need the real parsers and classifier (fixtures, `Probe` classes) belong to core-integration.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use fulltext_core::inspect::{MAX_BODY_BYTES, inspect, login_state, state_of};
use fulltext_core::model::{
    Idp, Ids, InspectMeta, InspectSpec, Inspection, LoginState, PageClass, PdfVerdict, RespMeta,
    Route, Source, Vendor,
};

fn resp(status: u16) -> RespMeta {
    RespMeta {
        status,
        final_url: "https://example.org/x".into(),
        ..Default::default()
    }
}

fn meta(spec: InspectSpec, resp: RespMeta) -> InspectMeta {
    InspectMeta { spec, resp }
}

fn pdf_spec() -> InspectSpec {
    InspectSpec::Pdf {
        requested: Ids::default(),
        route: Route::Oa,
    }
}

fn page_spec() -> InspectSpec {
    InspectSpec::Page {
        requested: Ids::default(),
        route: Route::Inst,
    }
}

fn api_spec() -> InspectSpec {
    InspectSpec::Api {
        source: Source::Unpaywall,
        requested: Ids::default(),
    }
}

fn probe_spec() -> InspectSpec {
    InspectSpec::Probe { route: Route::Inst }
}

fn synthetic_pdf(filler: usize) -> Vec<u8> {
    let mut v = b"%PDF-1.7\n".to_vec();
    v.extend(std::iter::repeat_n(b'x', filler));
    v.extend_from_slice(b"\n%%EOF\n");
    v
}

/// The `"expect"` tag of the serialised inspection.
fn tag(i: &Inspection) -> String {
    let json = serde_json::to_string(i).unwrap();
    let prefix = r#"{"expect":""#;
    let rest = json
        .strip_prefix(prefix)
        .unwrap_or_else(|| panic!("no leading expect tag: {json}"));
    rest.split('"').next().unwrap().to_owned()
}

fn pdf_verdict(i: Inspection) -> PdfVerdict {
    match i {
        Inspection::Pdf(v) => v,
        other => panic!("expected Pdf, got {other:?}"),
    }
}

// ------------------------------------------------------------------ dispatch

#[test]
fn api_with_a_pdf_body_fails() {
    for body in [synthetic_pdf(10), b"%PDF-".to_vec()] {
        let got = inspect(&meta(api_spec(), resp(200)), &body);
        assert_eq!(
            got,
            Inspection::Failed {
                reason: "api_body_is_pdf".into()
            }
        );
        assert_eq!(tag(&got), "failed");
    }
}

#[test]
fn api_with_any_other_body_goes_to_the_parser() {
    for body in [&b""[..], b"{}", b"<html>404</html>", b" %PDF-1.7", b"%PDF"] {
        match inspect(&meta(api_spec(), resp(200)), body) {
            Inspection::Api(a) => assert_eq!(a.source, Source::Unpaywall),
            other => panic!(
                "expected Api for {:?}, got {other:?}",
                String::from_utf8_lossy(body)
            ),
        }
    }
    assert_eq!(tag(&inspect(&meta(api_spec(), resp(404)), b"{}")), "api");
}

#[test]
fn pdf_spec_with_pdf_bytes_is_a_pdf_verdict() {
    let body = synthetic_pdf(5000);
    let mut r = resp(200);
    r.body_len = body.len() as u64;
    let got = inspect(&meta(pdf_spec(), r), &body);
    assert_eq!(tag(&got), "pdf");
    let v = pdf_verdict(got);
    assert!(v.is_pdf && v.has_eof && !v.looks_html);
    assert_eq!((v.junk_prefix, v.len), (0, body.len() as u64));
}

#[test]
fn pdf_spec_with_a_bom_prefixed_pdf_reports_the_junk() {
    let mut body = b"\xEF\xBB\xBF".to_vec();
    body.extend(synthetic_pdf(10));
    let v = pdf_verdict(inspect(&meta(pdf_spec(), resp(200)), &body));
    assert!(v.is_pdf);
    assert_eq!(v.junk_prefix, 3);
}

#[test]
fn pdf_spec_with_html_bytes_is_classified_as_a_page() {
    for body in [
        &b"<html><body>Please log in</body></html>"[..],
        b"<!doctype html><title>Just a moment...</title>",
        b"  \n<HTML>",
    ] {
        let got = inspect(&meta(pdf_spec(), resp(200)), body);
        assert_eq!(tag(&got), "page", "{}", String::from_utf8_lossy(body));
    }
}

#[test]
fn pdf_spec_with_an_error_status_and_a_text_body_is_classified_as_a_page() {
    for status in [401, 403, 404, 429, 503] {
        let got = inspect(&meta(pdf_spec(), resp(status)), b"Access denied");
        assert_eq!(tag(&got), "page", "status {status}");
    }
    // JSON error bodies count as text too.
    assert_eq!(
        tag(&inspect(
            &meta(pdf_spec(), resp(429)),
            br#"{"error":"slow down"}"#
        )),
        "page"
    );
}

#[test]
fn pdf_spec_with_other_bytes_is_a_non_pdf_verdict() {
    for (status, body) in [
        (200, &b"plain text, not a document"[..]),
        (200, b"{\"a\":1}"),
        (200, b""),
        (404, b""),
        (500, b"\x00\x01\x02 binary garbage"),
        (302, b"moved"),
    ] {
        let got = inspect(&meta(pdf_spec(), resp(status)), body);
        let v = pdf_verdict(got);
        assert!(!v.is_pdf && !v.looks_html, "status {status} body {body:?}");
    }
}

#[test]
fn page_spec_with_pdf_bytes_is_a_pdf_verdict() {
    let body = synthetic_pdf(100);
    let got = inspect(&meta(page_spec(), resp(200)), &body);
    assert_eq!(tag(&got), "pdf");
    assert!(pdf_verdict(got).is_pdf);
}

#[test]
fn page_spec_with_other_bytes_is_a_page() {
    for body in [
        &b"<html><head><title>x</title></head></html>"[..],
        b"",
        b"{\"x\":1}",
        b"just text",
    ] {
        let got = inspect(&meta(page_spec(), resp(200)), body);
        assert_eq!(tag(&got), "page", "{}", String::from_utf8_lossy(body));
    }
}

#[test]
fn probe_with_a_pdf_body_is_class_pdf() {
    let mut body = b"\xEF\xBB\xBF".to_vec();
    body.extend(synthetic_pdf(10));
    let got = inspect(&meta(probe_spec(), resp(200)), &body);
    assert_eq!(
        got,
        Inspection::Probe {
            class: PageClass::Pdf { junk_prefix: 3 }
        }
    );
    assert_eq!(tag(&got), "probe");
}

#[test]
fn probe_and_none_are_tagged() {
    // Probe on a non-PDF body is the classifier's business; here only the dispatch and tag are checked.
    for body in [&b""[..], b"<html></html>", b"hello"] {
        assert_eq!(tag(&inspect(&meta(probe_spec(), resp(200)), body)), "probe");
    }
    let none = inspect(&meta(InspectSpec::None, resp(200)), b"%PDF-1.7 anything");
    assert_eq!(none, Inspection::None);
    assert_eq!(tag(&none), "none");
}

#[test]
fn oversized_bodies_fail_before_any_analysis() {
    assert_eq!(MAX_BODY_BYTES, 16 * 1024 * 1024);
    let big = vec![b'a'; 17 * 1024 * 1024];
    for spec in [
        api_spec(),
        page_spec(),
        pdf_spec(),
        probe_spec(),
        InspectSpec::None,
    ] {
        let got = inspect(&meta(spec, resp(200)), &big);
        assert_eq!(
            got,
            Inspection::Failed {
                reason: "body_too_large".into()
            }
        );
    }
    // Exactly at the limit is still analysed.
    let at_limit = vec![b'a'; MAX_BODY_BYTES];
    assert_eq!(
        tag(&inspect(&meta(pdf_spec(), resp(200)), &at_limit)),
        "pdf"
    );
}

#[test]
fn head_len_staging_goes_through_inspect() {
    let total = 5 * 1024 * 1024usize;
    let full = {
        let mut v = synthetic_pdf(total - 64);
        v.truncate(total);
        v.extend_from_slice(b"\n%%EOF\n");
        v
    };
    let mut staged = full[..1024].to_vec();
    staged.extend_from_slice(&full[full.len() - 1024..]);
    let r = RespMeta {
        body_len: full.len() as u64,
        head_len: Some(1024),
        ..resp(200)
    };

    for spec in [pdf_spec(), page_spec()] {
        let v = pdf_verdict(inspect(&meta(spec, r.clone()), &staged));
        assert!(v.is_pdf && v.has_eof);
        assert_eq!(v.len, full.len() as u64);
    }
    let probe = inspect(&meta(probe_spec(), r), &staged);
    assert_eq!(
        probe,
        Inspection::Probe {
            class: PageClass::Pdf { junk_prefix: 0 }
        }
    );
}

#[test]
fn every_result_serialises_with_its_expect_tag_first() {
    let cases = [
        (inspect(&meta(api_spec(), resp(200)), b"{}"), "api"),
        (
            inspect(&meta(page_spec(), resp(200)), b"<html></html>"),
            "page",
        ),
        (inspect(&meta(pdf_spec(), resp(200)), b"%PDF-1.7"), "pdf"),
        (inspect(&meta(probe_spec(), resp(200)), b""), "probe"),
        (inspect(&meta(api_spec(), resp(200)), b"%PDF-1.7"), "failed"),
        (inspect(&meta(InspectSpec::None, resp(200)), b""), "none"),
    ];
    for (got, want) in cases {
        assert_eq!(tag(&got), want);
        // And it round-trips, because the host reads it back as an Envelope payload.
        let back: Inspection = serde_json::from_str(&serde_json::to_string(&got).unwrap()).unwrap();
        assert_eq!(back, got);
    }
}

#[test]
fn inspect_is_total_on_hostile_bytes() {
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for round in 0..60 {
        let len = (next() % 3000) as usize;
        let mut body: Vec<u8> = match round % 4 {
            0 => b"%PDF-".to_vec(),
            1 => b"<html>".to_vec(),
            _ => Vec::new(),
        };
        body.extend((0..len).map(|_| (next() >> 24) as u8));
        for spec in [
            api_spec(),
            page_spec(),
            pdf_spec(),
            probe_spec(),
            InspectSpec::None,
        ] {
            for status in [0u16, 200, 302, 404, 599] {
                let head_len = if round % 3 == 0 {
                    Some((next() % 5000) as u32)
                } else {
                    None
                };
                let r = RespMeta {
                    body_len: next() % 10_000_000,
                    head_len,
                    ..resp(status)
                };
                let _ = inspect(&meta(spec.clone(), r), &body);
            }
        }
    }
}

// ------------------------------------------------------------------ login_state

fn refused(code: &str) -> LoginState {
    LoginState::Refused {
        code: Some(code.to_owned()),
    }
}

#[test]
fn state_of_maps_every_page_class() {
    let cases: Vec<(PageClass, LoginState)> = vec![
        // Pending
        (
            PageClass::IdpLogin {
                idp: Idp::Entra,
                ca_error: None,
            },
            LoginState::Pending,
        ),
        (
            PageClass::IdpLogin {
                idp: Idp::Openathens,
                ca_error: None,
            },
            LoginState::Pending,
        ),
        (PageClass::EzproxyNeedLogin, LoginState::Pending),
        (PageClass::SamlAutoPost, LoginState::Pending),
        (PageClass::OpenathensWait, LoginState::Pending),
        (PageClass::PublisherSsoHop, LoginState::Pending),
        (
            PageClass::Redirect {
                next: "https://x.org/".into(),
            },
            LoginState::Pending,
        ),
        // Challenge
        (
            PageClass::Challenge {
                vendor: Vendor::Cloudflare,
                interactive: true,
            },
            LoginState::ChallengeShown,
        ),
        (
            PageClass::Challenge {
                vendor: Vendor::Pmc,
                interactive: false,
            },
            LoginState::ChallengeShown,
        ),
        // Refused
        (
            PageClass::IdpLogin {
                idp: Idp::Entra,
                ca_error: Some("AADSTS53003".into()),
            },
            refused("AADSTS53003"),
        ),
        (
            PageClass::IdpLogin {
                idp: Idp::Kcl,
                ca_error: Some("AADSTS50011".into()),
            },
            refused("AADSTS50011"),
        ),
        (PageClass::OpenathensBadOrg, refused("openathens_bad_org")),
        (
            PageClass::OpenathensUnknownResource,
            refused("openathens_unknown_resource"),
        ),
        (
            PageClass::EzproxyHostNotConfigured,
            refused("ezproxy_host_not_configured"),
        ),
        (
            PageClass::EzproxyError { suspend: true },
            refused("ezproxy_suspended"),
        ),
        (
            PageClass::EzproxyError { suspend: false },
            refused("ezproxy_error"),
        ),
        (
            PageClass::Blocked {
                vendor: Vendor::Akamai,
            },
            refused("blocked"),
        ),
        (
            PageClass::RateLimited {
                retry_after_s: Some(30),
            },
            refused("rate_limited"),
        ),
        (
            PageClass::RateLimited {
                retry_after_s: None,
            },
            refused("rate_limited"),
        ),
        (PageClass::LicenceStop, refused("licence_stop")),
        // Landed
        (PageClass::Pdf { junk_prefix: 0 }, LoginState::Landed),
        (PageClass::Pdf { junk_prefix: 3 }, LoginState::Landed),
        (PageClass::Landing, LoginState::Landed),
        (PageClass::Html, LoginState::Landed),
        (PageClass::ReaderShell { rewrite: None }, LoginState::Landed),
        (
            PageClass::ReaderShell {
                rewrite: Some("https://x.org/pdf".into()),
            },
            LoginState::Landed,
        ),
        (PageClass::NotFound, LoginState::Landed),
        (PageClass::AccessDenied, LoginState::Landed),
    ];
    for (class, want) in cases {
        assert_eq!(state_of(&class), want, "{class:?}");
    }
}

#[test]
fn login_state_without_a_document_is_decided_by_the_url() {
    for url in [
        "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2Fx",
        "https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/saml2?SAMLRequest=x",
        "https://login.live.com/login.srf",
        "https://kclidp.kcl.ac.uk/idp/profile/SAML2/Redirect/SSO",
        "https://login.openathens.net/auth/kcl.ac.uk/x",
        "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2Fx",
        "https://login.kcl.idm.oclc.org/connect",
        "https://onlinelibrary.wiley.com/action/ssostart?idp=E&redirectUri=T",
        "https://auth.elsevier.com/ShibAuth/institutionLogin?entityID=E",
    ] {
        assert_eq!(login_state(url, None), LoginState::Pending, "{url}");
        // An empty document counts as no document.
        assert_eq!(
            login_state(url, Some("")),
            LoginState::Pending,
            "{url} (empty html)"
        );
    }
}

#[test]
fn login_state_is_total() {
    for (url, html) in [
        ("", None),
        ("", Some("")),
        ("not a url", Some("<html></html>")),
        ("https://www.nature.com/articles/x", None),
        (
            "https://www.nature.com/articles/x",
            Some("<!doctype html><title>Article</title>"),
        ),
        (
            "https://login.microsoftonline.com/x",
            Some("<html><body>Sign in to your account</body></html>"),
        ),
        (
            "https://login.kcl.idm.oclc.org/login",
            Some("\u{0}\u{1}binary"),
        ),
    ] {
        let _ = login_state(url, html);
    }
    // A huge document is cut, not rejected.
    let huge = "a".repeat(3 * 1024 * 1024);
    let _ = login_state("https://www.nature.com/x", Some(&huge));
}

#[test]
fn login_state_never_reports_landed_at_an_sso_url() {
    // Whatever the classifier makes of the document, a page shown at an IdP or redirector URL has not landed.
    for url in [
        "https://login.microsoftonline.com/common/oauth2/authorize",
        "https://go.openathens.net/redirector/kcl.ac.uk?url=x",
        "https://login.kcl.idm.oclc.org/login?qurl=x",
        "https://jamanetwork.com/Shibboleth.sso/Login?entityID=E",
    ] {
        let got = login_state(
            url,
            Some(
                "<!doctype html><html><head><title>Something unrecognisable</title></head><body><p>hello</p></body></html>",
            ),
        );
        assert_ne!(got, LoginState::Landed, "{url}");
    }
}

#[test]
fn login_state_serialises_with_a_state_tag() {
    assert_eq!(
        serde_json::to_string(&LoginState::Pending).unwrap(),
        r#"{"state":"pending"}"#
    );
    assert_eq!(
        serde_json::to_string(&refused("blocked")).unwrap(),
        r#"{"state":"refused","code":"blocked"}"#
    );
}
