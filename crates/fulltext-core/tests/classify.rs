//! One case per classifier rule and per sub-signal (`docs/wp/core-page.md` section 3). Rule order is normative:
//! the "order" tests at the end pin which rule wins when several would match.
//!
//! Inputs: recorded fixtures through `common`, small hand-made pages in `tests/data/page/` (each cites the research
//! section whose shape it copies), and inline bodies for the header- or URL-only cases.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;

use fulltext_core::model::{Idp, PageClass, RespMeta, Vendor};
use fulltext_core::page::classify_response;

const LOGIN: &str = "https://login.kcl.idm.oclc.org";

fn data(name: &str) -> Vec<u8> {
    let path = common::root()
        .join("crates/fulltext-core/tests/data/page")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn meta(status: u16, url: &str, headers: &[(&str, &str)]) -> RespMeta {
    RespMeta {
        status,
        final_url: url.to_owned(),
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        ..Default::default()
    }
}

fn class(status: u16, url: &str, headers: &[(&str, &str)], body: &[u8]) -> PageClass {
    classify_response(&meta(status, url, headers), body)
}

fn html(body: &str) -> Vec<u8> {
    body.as_bytes().to_vec()
}

fn cloudflare(interactive: bool) -> PageClass {
    PageClass::Challenge {
        vendor: Vendor::Cloudflare,
        interactive,
    }
}

// ------------------------------------------------------------------ 1. Pdf

#[test]
fn pdf_by_content_type_with_or_without_a_body() {
    let ct = [("content-type", "application/pdf")];
    assert_eq!(
        class(200, "https://e.org/a", &ct, b""),
        PageClass::Pdf { junk_prefix: 0 },
        "HEAD-style probe: type alone is enough"
    );
    assert_eq!(
        class(200, "https://e.org/a", &ct, b"%PDF-1.7\n..."),
        PageClass::Pdf { junk_prefix: 0 }
    );
    assert_eq!(
        class(
            200,
            "https://e.org/a",
            &[("content-type", "application/PDF; charset=binary")],
            b"%PDF-1.4"
        ),
        PageClass::Pdf { junk_prefix: 0 }
    );
    assert_eq!(
        class(
            200,
            "https://e.org/a",
            &[("content-type", "application/x-pdf")],
            b""
        ),
        PageClass::Pdf { junk_prefix: 0 }
    );
}

#[test]
fn pdf_by_magic_with_junk_prefix() {
    let ct = [("content-type", "application/octet-stream")];
    assert_eq!(
        class(200, "https://e.org/a", &ct, b"%PDF-1.4\n"),
        PageClass::Pdf { junk_prefix: 0 }
    );
    assert_eq!(
        class(200, "https://e.org/a", &ct, b"\n\r\n%PDF-1.4\n"),
        PageClass::Pdf { junk_prefix: 3 },
        "junk_prefix is the offset of %PDF-"
    );
    let mut late = vec![b' '; 1500];
    late.extend_from_slice(b"%PDF-1.4");
    assert_ne!(
        class(200, "https://e.org/a", &ct, &late),
        PageClass::Pdf { junk_prefix: 1500 },
        "magic beyond the first KiB does not count without a PDF type"
    );
    assert_eq!(
        class(
            200,
            "https://e.org/a",
            &[("content-type", "application/pdf")],
            &late
        ),
        PageClass::Pdf { junk_prefix: 1500 },
        "but a PDF content type searches further"
    );
}

#[test]
fn a_pdf_content_type_without_pdf_bytes_is_not_a_pdf() {
    let ct = [("content-type", "application/pdf")];
    assert_eq!(
        class(
            200,
            "https://e.org/a",
            &ct,
            b"<html><body>error page served as pdf</body></html>"
        ),
        PageClass::Html
    );
}

// ------------------------------------------------------------------ 2. EZproxy

#[test]
fn ezproxy_need_login_a_redirect_from_a_proxied_host() {
    let loc_q = format!("{LOGIN}/login?qurl=https://doi.org%2f10.1038%2fnature12373");
    let r = class(
        302,
        "https://doi-org.kcl.idm.oclc.org/10.1038/nature12373",
        &[("location", &loc_q), ("server", "EZproxy")],
        b"To access this site, go <a href='x'>here</a>",
    );
    assert_eq!(r, PageClass::EzproxyNeedLogin);
    let loc_u = format!("{LOGIN}/login?url=https://www.bmj.com/content/372/bmj.n71");
    assert_eq!(
        class(
            302,
            "https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71",
            &[("location", &loc_u)],
            b""
        ),
        PageClass::EzproxyNeedLogin
    );
    let loc_http = "http://login.kcl.idm.oclc.org/login?qurl=http://doi.org%2fx";
    assert_eq!(
        class(
            302,
            "https://doi-org.kcl.idm.oclc.org/x",
            &[("location", loc_http)],
            b""
        ),
        PageClass::EzproxyNeedLogin,
        "http login URLs count too"
    );
}

#[test]
fn ezproxy_need_login_b_the_login_page_itself() {
    let page = data("ezproxy_login_page.html");
    let url = format!("{LOGIN}/login?url=https://doi.org/10.1038/nature12373");
    assert_eq!(
        class(
            200,
            &url,
            &[("server", "EZproxy"), ("content-type", "text/html")],
            &page
        ),
        PageClass::EzproxyNeedLogin
    );
    // Each of the three signals alone is enough.
    for body in [
        "<form name='EZproxyForm' method='POST'></form>",
        "<form name=\"EZproxyForm\" method=\"POST\"></form>",
        "<title>Shibboleth Authentication Request</title>",
        "<input type='hidden' name='RelayState' value='ezp.2abc'>",
        "<input type=\"hidden\" name=\"RelayState\" value=\"ezp.2abc\">",
    ] {
        assert_eq!(
            class(200, &url, &[], body.as_bytes()),
            PageClass::EzproxyNeedLogin,
            "{body}"
        );
    }
    // URL-only probe (the JS settle detector sends an empty body).
    assert_eq!(class(200, &url, &[], b""), PageClass::EzproxyNeedLogin);
}

#[test]
fn ezproxy_need_login_c_the_login_host_bouncing_to_login() {
    let loc = format!("{LOGIN}/login?url=https://login.kcl.idm.oclc.org/menu");
    assert_eq!(
        class(302, &format!("{LOGIN}/menu"), &[("location", &loc)], b""),
        PageClass::EzproxyNeedLogin
    );
}

#[test]
fn ezproxy_host_not_configured() {
    let loc = format!("{LOGIN}/articles/nature12373");
    let r = class(
        302,
        "https://www-nature-com.kcl.idm.oclc.org/articles/nature12373",
        &[("location", &loc)],
        b"",
    );
    assert_eq!(r, PageClass::EzproxyHostNotConfigured);
    let loc = format!("{LOGIN}/search?q=a%20b");
    assert_eq!(
        class(
            302,
            "https://www-sciencedirect-com.kcl.idm.oclc.org/search?q=a%20b",
            &[("location", &loc)],
            b""
        ),
        PageClass::EzproxyHostNotConfigured,
        "path AND query must match"
    );
    // The 404 the bounced request ends in.
    let page = data("ezproxy_404_not_configured.html");
    assert_eq!(
        class(
            404,
            &format!("{LOGIN}/articles/nature12373"),
            &[("server", "EZproxy")],
            &page
        ),
        PageClass::EzproxyHostNotConfigured
    );
    // A plain 404 from the login host without that body is just a 404.
    assert_eq!(
        class(404, &format!("{LOGIN}/nope"), &[], b"<p>other</p>"),
        PageClass::NotFound
    );
}

#[test]
fn ezproxy_error_page_with_and_without_suspension_words() {
    let url = format!("{LOGIN}/login?url=https://doi.org/10.1/x");
    assert_eq!(
        class(
            200,
            &url,
            &[("content-type", "text/html")],
            &data("ezproxy_error_suspend.html")
        ),
        PageClass::EzproxyError { suspend: true }
    );
    assert_eq!(
        class(
            200,
            &url,
            &[("content-type", "text/html; charset=utf-8")],
            &data("ezproxy_error_needhost.html")
        ),
        PageClass::EzproxyError { suspend: false }
    );
    // Other login-host paths are not the login page.
    assert_eq!(
        class(
            200,
            &format!("{LOGIN}/menu"),
            &[("content-type", "text/html")],
            &data("ezproxy_error_needhost.html")
        ),
        PageClass::Html
    );
}

#[test]
fn ezproxy_is_recognised_generically_for_other_institutions() {
    let page = data("ezproxy_login_page.html");
    assert_eq!(
        class(
            200,
            "https://login.other.idm.oclc.org/login?url=https://x.org/",
            &[],
            &page
        ),
        PageClass::EzproxyNeedLogin,
        "any login.*.idm.oclc.org"
    );
    let loc = "https://login.other.idm.oclc.org/login?qurl=https%3A%2F%2Fx.org%2F";
    assert_eq!(
        class(
            302,
            "https://x-org.other.idm.oclc.org/",
            &[("location", loc)],
            b""
        ),
        PageClass::EzproxyNeedLogin,
        "any *.idm.oclc.org proxied host"
    );
    // `Server: EZproxy` on a host that does not follow the naming convention.
    assert_eq!(
        class(
            200,
            "https://ezproxy.lib.example.ac.uk/login?url=https://x.org/",
            &[("server", "EZproxy")],
            &page
        ),
        PageClass::EzproxyNeedLogin
    );
    // The same page from a non-EZproxy server is just a SAML auto-post.
    assert_eq!(
        class(
            200,
            "https://ezproxy.lib.example.ac.uk/login?url=https://x.org/",
            &[("server", "nginx")],
            &page
        ),
        PageClass::SamlAutoPost
    );
}

// ------------------------------------------------------------------ 3. SSO

#[test]
fn idp_login_hosts() {
    let idp = |idp: Idp| PageClass::IdpLogin {
        idp,
        ca_error: None,
    };
    let tenant = "8370cf14-16f3-4c16-b83c-724071654356";
    for url in [
        format!("https://login.microsoftonline.com/{tenant}/saml2?SAMLRequest=abc"),
        "https://login.microsoftonline.com/common/login".to_owned(),
        "https://login.microsoftonline.com/kmsi".to_owned(),
        "https://login.live.com/login.srf".to_owned(),
        "https://login.microsoft.com/".to_owned(),
        "https://device.login.microsoftonline.com/".to_owned(),
    ] {
        assert_eq!(class(200, &url, &[], b""), idp(Idp::Entra), "{url}");
    }
    assert_eq!(
        class(
            200,
            "https://login.openathens.net/auth/kcl.ac.uk/o/71807903",
            &[],
            b""
        ),
        idp(Idp::Openathens)
    );
    assert_eq!(
        class(
            200,
            "https://kclidp.kcl.ac.uk/idp/profile/SAML2/Redirect/SSO",
            &[],
            b""
        ),
        idp(Idp::Kcl)
    );
    // The SAML hop endpoints of OpenAthens are not the interactive page: a redirect is a redirect.
    let loc = "https://login.openathens.net/auth/kcl.ac.uk/o/71807903?t=%2Fsaml";
    assert_eq!(
        class(
            307,
            "https://login.openathens.net/saml/2/sso/kcl.ac.uk/o/71807903/c/oafed",
            &[("location", loc)],
            b""
        ),
        PageClass::Redirect {
            next: loc.to_owned()
        }
    );
    // A 3xx from the IdP host is still "at the IdP".
    assert_eq!(
        class(
            302,
            "https://login.microsoftonline.com/x",
            &[("location", "https://login.microsoftonline.com/y")],
            b""
        ),
        idp(Idp::Entra)
    );
}

#[test]
fn idp_login_page_and_conditional_access_codes() {
    let url = "https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/saml2";
    assert_eq!(
        class(200, url, &[], &data("entra_signin.html")),
        PageClass::IdpLogin {
            idp: Idp::Entra,
            ca_error: None
        }
    );
    // The first AADSTS code wins.
    assert_eq!(
        class(200, url, &[], &data("entra_ca_error.html")),
        PageClass::IdpLogin {
            idp: Idp::Entra,
            ca_error: Some("AADSTS53000".into())
        }
    );
    for (text, code) in [
        ("AADSTS530003: device must be managed", "AADSTS530003"),
        ("error AADSTS50105.", "AADSTS50105"),
        ("AADSTS", ""),
        ("AADSTS x AADSTS70001", "AADSTS70001"),
    ] {
        let want = (!code.is_empty()).then(|| code.to_owned());
        assert_eq!(
            class(200, url, &[], text.as_bytes()),
            PageClass::IdpLogin {
                idp: Idp::Entra,
                ca_error: want
            },
            "{text}"
        );
    }
}

fn other_idp() -> PageClass {
    PageClass::IdpLogin {
        idp: Idp::Other,
        ca_error: None,
    }
}

#[test]
fn generic_shibboleth_idp_by_url() {
    // The Shibboleth IdP v4/v5 profile endpoints, whatever the institution.
    for url in [
        "https://idp.shibboleth.ox.ac.uk/idp/profile/SAML2/Redirect/SSO?execution=e1s1",
        "https://idp.shibboleth.ox.ac.uk/idp/profile/SAML2/POST/SSO",
        "https://shibboleth.imperial.ac.uk/idp/profile/Authn/SAML2/POST/SSO/start?conversation=e1s1",
        "https://sso.example.ac.uk/idp/profile/Shibboleth/SSO",
        "https://sso.example.ac.uk/idp/profile/cas/login?service=x",
        "https://sso.example.ac.uk/idp/Authn/UserPassword",
        "https://sso.example.ac.uk/adfs/ls/?SAMLRequest=x",
        "https://login.example.ac.uk/simplesaml/saml2/idp/SSOService.php?spentityid=x",
        // `idp`, `idp1`, `idp2` as the first host label.
        "https://idp.warwick.ac.uk/",
        "https://idp2.example.ac.uk/any/path",
    ] {
        assert_eq!(class(200, url, &[], b""), other_idp(), "{url}");
    }
    // A redirect issued BY the IdP (to an upstream Entra tenant) is still "at the IdP".
    assert_eq!(
        class(
            302,
            "https://shibboleth.imperial.ac.uk/idp/profile/Authn/SAML2/POST/SSO/start?conversation=e1s1",
            &[(
                "location",
                "https://login.microsoftonline.com/2b897507-ee8c-4575-830b-4f8267c3d307/saml2?SAMLRequest=x"
            )],
            b""
        ),
        other_idp()
    );
    // The KCL and OpenAthens IdPs keep their own labels (rule order).
    assert_eq!(
        class(
            200,
            "https://kclidp.kcl.ac.uk/idp/profile/SAML2/Redirect/SSO",
            &[],
            b""
        ),
        idp(Idp::Kcl)
    );
    // An IdP that answers with an error is an error, not a sign-in page.
    assert_eq!(
        class(404, "https://idp.warwick.ac.uk/nothing", &[], b"Not found"),
        PageClass::NotFound
    );
    assert_ne!(
        class(
            503,
            "https://idp.warwick.ac.uk/",
            &[],
            b"Service unavailable"
        ),
        other_idp()
    );
    // Look-alikes are not IdPs: a publisher page whose path merely mentions "idp", a host that only contains it.
    for url in [
        "https://www.nature.com/articles/idp-binding-protein",
        "https://example.org/idp",
        "https://idpath.example.org/x",
        "https://rapid.example.org/idp/x",
        "https://idp/x",
    ] {
        assert_ne!(class(200, url, &[], b""), other_idp(), "{url}");
    }
}

fn idp(idp: Idp) -> PageClass {
    PageClass::IdpLogin {
        idp,
        ca_error: None,
    }
}

#[test]
fn generic_shibboleth_idp_by_markup() {
    // The default login view of a Shibboleth IdP v4/v5: recognised on a host and path that say nothing.
    let login = data("shibboleth_idp_login.html");
    for url in [
        "https://sso.example.ac.uk/login",
        "https://idp.example.ac.uk/idp/profile/SAML2/Redirect/SSO?execution=e1s2",
        "https://campus.example.ac.uk/auth?session=1",
    ] {
        assert_eq!(class(200, url, &[], &login), other_idp(), "{url}");
    }
    // The "Loading Session Information" interstitial (JavaScript submit, no SAMLRequest field).
    let loading = data("shibboleth_idp_session_loading.html");
    assert_eq!(
        class(200, "https://sso.example.ac.uk/login", &[], &loading),
        other_idp()
    );
    // Flow fields alone are enough; so is a form that posts to an IdP profile endpoint.
    assert_eq!(
        class(
            200,
            "https://sso.example.ac.uk/x",
            &[],
            &html("<form method=post action=/n><input name=_eventId_proceed type=hidden></form>")
        ),
        other_idp()
    );
    assert_eq!(
        class(
            200,
            "https://sso.example.ac.uk/x",
            &[],
            &html("<form method=post action='/idp/profile/SAML2/POST/SSO'><input name=x></form>")
        ),
        other_idp()
    );
    // A generic username / password form (a publisher's own login) is NOT a Shibboleth IdP.
    assert_eq!(
        class(
            200,
            "https://pub.example.org/login",
            &[],
            &html(
                "<title>Sign in</title><form method=post action=/login><input name=username><input type=password name=password></form>"
            )
        ),
        PageClass::Html
    );
    // An error status never classifies as a sign-in page.
    assert_ne!(
        class(500, "https://sso.example.ac.uk/login", &[], &login),
        other_idp()
    );
    // SAML auto-post to an IdP host that has no profile path keeps its own class.
    let post = html(
        "<form method=POST action=https://sso.example.org/x><input name=SAMLRequest value=x></form>",
    );
    assert_eq!(
        class(200, "https://pub.example.org/x", &[], &post),
        PageClass::SamlAutoPost
    );
}

#[test]
fn shibboleth_sp_paths_are_publisher_sso_hops() {
    for url in [
        "https://pubs.acs.org/Shibboleth.sso/Login?entityID=x&target=y",
        "https://www.sciencedirect.com/Shibboleth.sso/SAML2/POST",
        "https://publisher.example.org/Shibboleth.sso/Session",
        "https://publisher.example.org/shibboleth.sso/DS",
    ] {
        assert_eq!(
            class(200, url, &[], b""),
            PageClass::PublisherSsoHop,
            "{url}"
        );
    }
}

#[test]
fn saml_auto_post_needs_a_post_form_with_a_saml_field() {
    let page = data("saml_autopost.html");
    assert_eq!(
        class(
            200,
            "https://auth.elsevier.com/ShibAuth/institutionLogin?entityID=x",
            &[],
            &page
        ),
        PageClass::SamlAutoPost,
        "input names are matched case-insensitively"
    );
    let response = html(
        "<form method=POST action=/acs><input name=SAMLResponse value=x><input name=RelayState value=y></form>",
    );
    assert_eq!(
        class(
            200,
            "https://onlinelibrary.wiley.com/saml/acs",
            &[],
            &response
        ),
        PageClass::SamlAutoPost
    );
    let get_form = html("<form method=GET action=/acs><input name=SAMLRequest value=x></form>");
    assert_eq!(
        class(200, "https://pub.example.org/x", &[], &get_form),
        PageClass::Html
    );
    let other_post = html("<form method=POST action=/login><input name=username></form>");
    assert_eq!(
        class(200, "https://pub.example.org/x", &[], &other_post),
        PageClass::Html
    );
}

#[test]
fn openathens_redirector_states() {
    let url = "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fwww.nature.com%2Farticles%2Fnature12373";
    assert_eq!(
        class(200, url, &[], &data("openathens_wait.html")),
        PageClass::OpenathensWait
    );
    assert_eq!(
        class(404, url, &[], &data("openathens_404.html")),
        PageClass::OpenathensUnknownResource
    );
    assert_eq!(
        class(403, url, &[], b"Forbidden"),
        PageClass::OpenathensBadOrg
    );
    // A 200 from the redirector without the wait title is not "please wait".
    assert_eq!(
        class(200, url, &[], b"<title>Something else</title>"),
        PageClass::Html
    );
    // The successful answer is a 302 to the publisher's SSO endpoint.
    let loc =
        "https://sp.nature.com/saml/login?idp=https%3A%2F%2Fkclidp.kcl.ac.uk%2Fidp%2Fshibboleth";
    assert_eq!(
        class(302, url, &[("location", loc)], b""),
        PageClass::PublisherSsoHop
    );
}

#[test]
fn publisher_sso_hops_by_url() {
    let e = "https://kclidp.kcl.ac.uk/idp/shibboleth";
    let t = "https://x.org/a";
    for url in [
        format!("https://jamanetwork.com/Shibboleth.sso/Login?entityID={e}&target={t}"),
        format!("https://shibboleth.cambridge.org/Shibboleth.sso/discovery?SAMLDS=1&target={t}"),
        format!("https://onlinelibrary.wiley.com/action/ssostart?idp={e}&redirectUri={t}"),
        format!(
            "https://auth.elsevier.com/ShibAuth/institutionLogin?entityID={e}&appReturnURL={t}"
        ),
        format!("https://fsso.springer.com/federation/init?entityId={e}&returnUrl={t}"),
        format!("https://sp.nature.com/saml/login?idp={e}&targetUrl={t}"),
        "https://secure.nature.com/saml2/authenticate/abc?targetUrl=x".to_owned(),
        format!("https://oup-sp.sams-sigma.com/Shibboleth.sso/Login?entityID={e}&target={t}"),
        format!("https://shibboleth.highwire.org/session/init?entityID={e}&hw-shib-return-uri={t}"),
        format!(
            "https://profile.thieme.de/HTML/sso/ejournals/shibboleth.htm?federationid=x&entityid={e}"
        ),
        format!(
            "https://karger.com/ExternalAuthentication/KeystoneDeeplink?entityId={e}&destination={t}"
        ),
        "https://shibboleth.cambridge.org/CJOShibb2/index".to_owned(),
    ] {
        assert_eq!(
            class(200, &url, &[], b""),
            PageClass::PublisherSsoHop,
            "{url}"
        );
        assert_eq!(
            class(302, "https://x.org/start", &[("location", &url)], b""),
            PageClass::PublisherSsoHop,
            "location: {url}"
        );
    }
    assert_eq!(
        class(
            200,
            "https://onlinelibrary.wiley.com/doi/full/10.1002/wjs.12026",
            &[],
            b""
        ),
        PageClass::Html
    );
}

// ------------------------------------------------------------------ 4. Bot walls

#[test]
fn cloudflare_challenge_signals() {
    // header alone, any status
    assert_eq!(
        class(
            200,
            "https://e.org/a",
            &[("cf-mitigated", "challenge")],
            b""
        ),
        cloudflare(false)
    );
    assert_eq!(
        class(
            403,
            "https://e.org/a",
            &[("cf-mitigated", "Challenge")],
            b""
        ),
        cloudflare(false)
    );
    // server + status + body marker
    for body in [
        "<script>window._cf_chl_opt = {}</script>",
        "<script src='/cdn-cgi/challenge-platform/h/b/orchestrate'></script>",
        "<title>Just a moment...</title>",
    ] {
        assert_eq!(
            class(
                403,
                "https://e.org/a",
                &[("server", "cloudflare")],
                body.as_bytes()
            ),
            cloudflare(false),
            "{body}"
        );
        assert_eq!(
            class(
                503,
                "https://e.org/a",
                &[("server", "cloudflare")],
                body.as_bytes()
            ),
            cloudflare(false),
            "503: {body}"
        );
    }
    // no server header (callers that pass only the content type) still works on the Cloudflare-only markers
    assert_eq!(
        class(
            403,
            "https://e.org/a",
            &[("content-type", "text/html")],
            b"<script>window._cf_chl_opt={}</script>"
        ),
        cloudflare(false)
    );
    // not Cloudflare: a different server, or a status that is not a wall
    assert_ne!(
        class(
            403,
            "https://e.org/a",
            &[("server", "AkamaiGHost")],
            b"<script>window._cf_chl_opt={}</script>"
        ),
        cloudflare(false)
    );
    assert_ne!(
        class(
            200,
            "https://e.org/a",
            &[("server", "cloudflare")],
            b"<title>Just a moment...</title>"
        ),
        cloudflare(false)
    );
}

#[test]
fn cloudflare_challenge_interactive_widgets() {
    let page = data("cloudflare_turnstile.html");
    assert_eq!(
        class(
            403,
            "https://e.org/a",
            &[("server", "cloudflare"), ("cf-mitigated", "challenge")],
            &page
        ),
        cloudflare(true)
    );
    for marker in ["cf-turnstile", "turnstile", "g-recaptcha", "h-captcha"] {
        let body = format!("<title>Just a moment...</title><div class='{marker}'></div>");
        assert_eq!(
            class(
                403,
                "https://e.org/a",
                &[("server", "cloudflare")],
                body.as_bytes()
            ),
            cloudflare(true),
            "{marker}"
        );
    }
}

#[test]
fn pmc_proof_of_work_signals() {
    let pmc = PageClass::Challenge {
        vendor: Vendor::Pmc,
        interactive: false,
    };
    let url = "https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/pdf/pmed.1000097.pdf";
    assert_eq!(
        class(
            200,
            url,
            &[],
            &html(
                "<title>Preparing to download ...</title><script>const POW_CHALLENGE = \"abc\"</script>"
            )
        ),
        pmc
    );
    assert_eq!(
        class(
            200,
            url,
            &[],
            &html(
                "<title>Preparing to download ...</title><script type=module src=\"https://cdn.ncbi.nlm.nih.gov/x/pow-o51sQKbL.js\"></script>"
            )
        ),
        pmc
    );
    // The title alone is not enough.
    assert_eq!(
        class(
            200,
            url,
            &[],
            &html("<title>Preparing to download ...</title>")
        ),
        PageClass::Html
    );
    assert_eq!(
        class(
            200,
            url,
            &[],
            &html("<title>PMC article</title><script>const POW_CHALLENGE = 1</script>")
        ),
        PageClass::Html
    );
}

#[test]
fn thieme_script_challenge_signals() {
    let thieme = PageClass::Challenge {
        vendor: Vendor::Thieme,
        interactive: false,
    };
    let url = "https://www.thieme-connect.de/DOI/DOI?10.1055/a-2459-0064";
    assert_eq!(
        class(
            200,
            url,
            &[],
            &html(
                "<title>Please wait</title><script src=\"/products/assets/challenge-55fc59.js\"></script>"
            )
        ),
        thieme
    );
    assert_eq!(
        class(200, url, &[], &html("<title>Please wait</title>")),
        PageClass::Html,
        "needs the challenge script"
    );
    assert_eq!(
        class(
            200,
            url,
            &[],
            &html(
                "<title>Thieme</title><script src=\"/products/assets/challenge-55fc59.js\"></script>"
            )
        ),
        PageClass::Html,
        "needs the title"
    );
}

#[test]
fn sciencedirect_download_challenge_signals() {
    let sd = PageClass::Challenge {
        vendor: Vendor::Sciencedirect,
        interactive: false,
    };
    let url = "https://www.sciencedirect.com/science/article/pii/S0140673600023941/pdfft";
    for body in [
        "<h1>Preparing your download</h1>",
        "<script src=\"/craft/challenge/pdf/abc.js\"></script>",
        "<noscript><meta http-equiv=refresh content=\"0; url=/x?ref=cra_js_challenge&amp;fr=njs\"></noscript>",
    ] {
        assert_eq!(class(200, url, &[], body.as_bytes()), sd, "{body}");
    }
}

#[test]
fn akamai_bot_manager_refresh() {
    let r = class(
        200,
        "https://www.mdpi.com/2077-0383/13/5/1296",
        &[],
        &data("akamai_bm_verify.html"),
    );
    assert_eq!(
        r,
        PageClass::Challenge {
            vendor: Vendor::Akamai,
            interactive: false
        }
    );
    // An ordinary refresh to a normal URL is a redirect, not a challenge.
    assert_eq!(
        class(
            200,
            "https://www.mdpi.com/a",
            &[],
            b"<meta http-equiv=refresh content=\"0;url=/b\">"
        ),
        PageClass::Redirect {
            next: "https://www.mdpi.com/b".into()
        }
    );
}

#[test]
fn cloudflare_block_signals() {
    let blocked = PageClass::Blocked {
        vendor: Vendor::Cloudflare,
    };
    let cf = [("server", "cloudflare")];
    for body in [
        "<div id=\"cf-error-details\"></div>",
        "<h1>Sorry, you have been blocked</h1>",
        "<title>Attention Required! | Cloudflare</title>",
    ] {
        assert_eq!(
            class(403, "https://bmjopen.bmj.com/x", &cf, body.as_bytes()),
            blocked,
            "{body}"
        );
    }
    // Without a server header the body must still say Cloudflare.
    assert_eq!(
        class(
            403,
            "https://www.medrxiv.org/x",
            &[],
            b"<title>Attention Required! | Cloudflare</title>"
        ),
        blocked
    );
    assert_eq!(
        class(
            403,
            "https://x.org/x",
            &[],
            b"<h1>Sorry, you have been blocked</h1> by the site firewall"
        ),
        PageClass::AccessDenied
    );
    // Status must be 403; a different server rules it out.
    assert_ne!(
        class(
            200,
            "https://x.org/x",
            &cf,
            b"<h1>Sorry, you have been blocked</h1>"
        ),
        blocked
    );
    assert_eq!(
        class(
            403,
            "https://x.org/x",
            &[("server", "nginx")],
            b"<h1>Sorry, you have been blocked</h1>"
        ),
        PageClass::AccessDenied
    );
}

#[test]
fn akamai_block_signals() {
    let blocked = PageClass::Blocked {
        vendor: Vendor::Akamai,
    };
    assert_eq!(
        class(
            403,
            "https://www.mdpi.com/x",
            &[("akamai-grn", "0.1a2b.1792496400.1")],
            b"nope"
        ),
        blocked,
        "Akamai-GRN header"
    );
    assert_eq!(
        class(
            403,
            "https://www.mdpi.com/x",
            &[],
            b"<p>https&#58;&#47;&#47;errors&#46;edgesuite&#46;net&#47;18&#46;a4c</p>"
        ),
        blocked,
        "errors.edgesuite.net, even entity-encoded"
    );
    assert_eq!(
        class(
            403,
            "https://www.mdpi.com/x",
            &[],
            b"<TITLE>Access Denied</TITLE><H1>Access Denied</H1>"
        ),
        blocked,
        "upper-case tags"
    );
    // 'Access Denied' on a normal page is not a block.
    assert_eq!(
        class(
            200,
            "https://www.mdpi.com/x",
            &[],
            b"<title>Access Denied</title>"
        ),
        PageClass::Html
    );
    assert_eq!(
        class(
            403,
            "https://www.mdpi.com/x",
            &[],
            b"<title>Not allowed</title>"
        ),
        PageClass::AccessDenied
    );
}

// ------------------------------------------------------------------ 5. status classes

#[test]
fn rate_limited() {
    assert_eq!(
        class(429, "https://e.org/a", &[], b""),
        PageClass::RateLimited {
            retry_after_s: None
        }
    );
    assert_eq!(
        class(429, "https://e.org/a", &[("retry-after", "120")], b""),
        PageClass::RateLimited {
            retry_after_s: Some(120)
        }
    );
    assert_eq!(
        class(429, "https://e.org/a", &[("retry-after", " 7 ")], b""),
        PageClass::RateLimited {
            retry_after_s: Some(7)
        }
    );
    assert_eq!(
        class(
            429,
            "https://e.org/a",
            &[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")],
            b""
        ),
        PageClass::RateLimited {
            retry_after_s: None
        },
        "HTTP-date form is not parsed"
    );
    assert_eq!(
        class(
            403,
            "https://e.org/a",
            &[("retry-after", "30")],
            b"<p>We have detected UNUSUAL TRAFFIC from your network</p>"
        ),
        PageClass::RateLimited {
            retry_after_s: Some(30)
        }
    );
    assert_eq!(
        class(503, "https://e.org/a", &[], b"<h1>Too Many Requests</h1>"),
        PageClass::RateLimited {
            retry_after_s: None
        }
    );
    // The Silverchair page can come back with status 200.
    assert_eq!(
        class(
            200,
            "https://academic.oup.com/bjs/x",
            &[],
            &data("silverchair_unusual_traffic.html")
        ),
        PageClass::RateLimited {
            retry_after_s: None
        }
    );
    // The words alone on a 200 article do not count.
    assert_eq!(
        class(
            200,
            "https://e.org/a",
            &[],
            b"<meta name=citation_title content=T><p>unusual traffic and too many requests</p>"
        ),
        PageClass::Landing
    );
}

#[test]
fn licence_stop_phrases() {
    assert_eq!(
        class(
            200,
            "https://pub.example.org/x",
            &[],
            &data("licence_stop.html")
        ),
        PageClass::LicenceStop
    );
    for phrase in [
        "Excessive download",
        "SYSTEMATIC DOWNLOAD",
        "Your access has been suspended",
        "Usage policy violation",
        "access has been temporarily suspended",
    ] {
        let body = format!("<html><body><p>Notice: {phrase} detected.</p></body></html>");
        assert_eq!(
            class(200, "https://pub.example.org/x", &[], body.as_bytes()),
            PageClass::LicenceStop,
            "{phrase}"
        );
        assert_eq!(
            class(403, "https://pub.example.org/x", &[], body.as_bytes()),
            PageClass::LicenceStop,
            "403: {phrase}"
        );
    }
    // On an article page the same words are prose.
    let article = html(
        "<meta name=citation_title content=T><p>excessive downloading and systematic download studies</p>",
    );
    assert_eq!(
        class(200, "https://pub.example.org/x", &[], &article),
        PageClass::Landing
    );
}

#[test]
fn not_found_and_access_denied() {
    for status in [404, 410] {
        assert_eq!(
            class(status, "https://e.org/a", &[], b"<h1>Gone</h1>"),
            PageClass::NotFound,
            "{status}"
        );
    }
    for status in [401, 403] {
        assert_eq!(
            class(status, "https://e.org/a", &[], b"<h1>Forbidden</h1>"),
            PageClass::AccessDenied,
            "{status}"
        );
    }
    // A Cloudflare challenge is a 403 but not an access-denied page (rule 4 beats rule 5).
    assert_eq!(
        class(
            403,
            "https://e.org/a",
            &[("server", "cloudflare"), ("cf-mitigated", "challenge")],
            b""
        ),
        cloudflare(false)
    );
    assert_eq!(
        class(500, "https://e.org/a", &[], b"<h1>oops</h1>"),
        PageClass::Html
    );
}

// ------------------------------------------------------------------ 6. Redirect

#[test]
fn redirect_via_a_location_header() {
    assert_eq!(
        class(302, "https://e.org/a/b", &[("location", "/c?x=1")], b""),
        PageClass::Redirect {
            next: "https://e.org/c?x=1".into()
        }
    );
    assert_eq!(
        class(
            301,
            "https://e.org/a/b",
            &[("location", "https://other.org/z")],
            b""
        ),
        PageClass::Redirect {
            next: "https://other.org/z".into()
        }
    );
    assert_eq!(
        class(307, "https://e.org/a/b", &[("location", "../up")], b""),
        PageClass::Redirect {
            next: "https://e.org/up".into()
        }
    );
    assert_eq!(
        class(302, "https://e.org/a", &[], b""),
        PageClass::Html,
        "a 3xx without Location has nowhere to go"
    );
    assert_eq!(
        class(302, "https://e.org/a", &[("location", "mailto:a@b.c")], b""),
        PageClass::Html,
        "non-web targets are ignored"
    );
}

#[test]
fn redirect_via_input_redirect_url() {
    let (resp, body) = common::html_fixture("elsevier-linkinghub__redirect-S0140673620301835.html");
    assert_eq!(
        classify_response(&resp, &body),
        PageClass::Redirect {
            next: "https://www.thelancet.com/retrieve/pii/S0140673620301835".into()
        },
        "decoded once, http upgraded"
    );
    let (resp, body) = common::html_fixture("elsevier-linkinghub__redirect-S1743919121000406.html");
    assert_eq!(
        classify_response(&resp, &body),
        PageClass::Redirect {
            next: "https://www.sciencedirect.com/science/article/pii/S1743919121000406?via%3Dihub"
                .into()
        }
    );
    // input#redirectURL beats the refresh; relative values resolve against the page.
    let both = html(
        "<meta http-equiv=refresh content=\"2;url=/refresh\"><input id=redirectURL value=\"%2Fnext%3Fa%3D1\">",
    );
    assert_eq!(
        class(
            200,
            "https://linkinghub.elsevier.com/retrieve/pii/S1",
            &[],
            &both
        ),
        PageClass::Redirect {
            next: "https://linkinghub.elsevier.com/next?a=1".into()
        }
    );
}

#[test]
fn redirect_via_meta_refresh() {
    let url = "https://pub.example.org/a/b";
    let go = |content: &str| {
        class(
            200,
            url,
            &[],
            format!("<meta http-equiv=\"refresh\" content=\"{content}\">").as_bytes(),
        )
    };
    assert_eq!(
        go("0;url=/next"),
        PageClass::Redirect {
            next: "https://pub.example.org/next".into()
        }
    );
    assert_eq!(
        go("2; URL='https://other.org/x?y=1'"),
        PageClass::Redirect {
            next: "https://other.org/x?y=1".into()
        }
    );
    assert_eq!(
        go("15;url=/edge"),
        PageClass::Redirect {
            next: "https://pub.example.org/edge".into()
        }
    );
    assert_eq!(
        go("16;url=/late"),
        PageClass::Html,
        "longer than 15 s is not an interstitial"
    );
    assert_eq!(
        go("360"),
        PageClass::Html,
        "a refresh without a URL (Cloudflare) is ignored"
    );
    assert_eq!(
        go("0;url=https://pub.example.org/a/b#top"),
        PageClass::Html,
        "refreshing to the same page is not a redirect"
    );
    assert_eq!(go("0;url=javascript:alert(1)"), PageClass::Html);
}

#[test]
fn noscript_refreshes_never_redirect() {
    for (file, expected) in [
        ("lww__annals-of-surgery-article.html", PageClass::Landing),
        ("lww__ijs-article-oa.html", PageClass::Landing),
        ("europepmc__spa-shell.html", PageClass::Html),
    ] {
        let (resp, body) = common::html_fixture(file);
        assert_eq!(classify_response(&resp, &body), expected, "{file}");
    }
    let (resp, body) = common::html_fixture("sciencedirect__pdfft-js-challenge-interstitial.html");
    assert_eq!(
        classify_response(&resp, &body),
        PageClass::Challenge {
            vendor: Vendor::Sciencedirect,
            interactive: false
        },
        "the challenge wins; the <noscript> refresh is invisible"
    );
}

// ------------------------------------------------------------------ 7. ReaderShell

#[test]
fn reader_shells_and_their_rewrites() {
    let shell = |url: &str| class(200, url, &[], b"<html><body>reader</body></html>");
    let rw = |u: &str| PageClass::ReaderShell {
        rewrite: Some(u.to_owned()),
    };
    assert_eq!(
        shell("https://onlinelibrary.wiley.com/doi/epdf/10.1002/wjs.12026"),
        rw("https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/wjs.12026")
    );
    assert_eq!(
        shell("https://www.tandfonline.com/doi/epdf/10.1080/07853890.2016.1186828?needAccess=true"),
        rw("https://www.tandfonline.com/doi/pdf/10.1080/07853890.2016.1186828")
    );
    assert_eq!(
        shell("https://journals.sagepub.com/doi/reader/10.1177/2050312118807624"),
        rw("https://journals.sagepub.com/doi/pdf/10.1177/2050312118807624")
    );
    assert_eq!(
        shell("https://x.org/doi/epdf/10.1/y?a=1&needAccess=true"),
        rw("https://x.org/doi/pdf/10.1/y?a=1"),
        "other query pairs survive"
    );
    assert_eq!(
        shell("https://www.bmj.com/content/372/bmj.n71.full.pdf+html"),
        rw("https://www.bmj.com/content/372/bmj.n71.full.pdf")
    );
    assert_eq!(
        shell("https://x.org/core/epdf/abc"),
        PageClass::ReaderShell { rewrite: None },
        "a bare /epdf/ is a shell without a known rewrite"
    );
    assert_eq!(
        shell("https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/wjs.12026"),
        PageClass::Html,
        "pdfdirect is the target, not a shell"
    );
}

// ------------------------------------------------------------------ 8. Landing / Html

#[test]
fn landing_signals_one_by_one() {
    for meta in [
        "<meta name=citation_title content=T>",
        "<meta name=citation_doi content=10.1234/x>",
        "<meta name=citation_pdf_url content=/a.pdf>",
        "<meta name=\"DC.Identifier\" content=\"10.1234/x\">",
        "<meta name=prism.doi content=\"doi:10.1234/x\">",
        "<meta name=wkhealth_pdf_url content=/a.pdf>",
        "<meta property=og:type content=Article>",
    ] {
        assert_eq!(
            class(200, "https://pub.example.org/x", &[], meta.as_bytes()),
            PageClass::Landing,
            "{meta}"
        );
    }
    for plain in [
        "",
        "<title>Home</title>",
        "<meta property=og:type content=website>",
        "<meta name=citation_title content=\"\">",
        "<p>citation_title</p>",
    ] {
        assert_eq!(
            class(200, "https://pub.example.org/x", &[], plain.as_bytes()),
            PageClass::Html,
            "{plain}"
        );
    }
}

#[test]
fn recorded_fixtures_land_or_stay_plain() {
    let (resp, body) = common::html_fixture("europepmc__spa-shell.html");
    assert_eq!(classify_response(&resp, &body), PageClass::Html);
    for e in common::html_expected() {
        if e["kind"] == "landing" && e["expected_pdf_url"].is_string() {
            let file = e["file"].as_str().unwrap();
            let (resp, body) = common::html_fixture(file);
            let c = classify_response(&resp, &body);
            assert!(
                matches!(
                    c,
                    PageClass::Landing | PageClass::Html | PageClass::ReaderShell { .. }
                ),
                "{file}: {c:?}"
            );
            assert_eq!(c, PageClass::Landing, "{file}");
        }
    }
}

// ------------------------------------------------------------------ negative cases and rule order

#[test]
fn prose_that_mentions_wall_words_stays_a_landing_page() {
    let page = data("article_mentions_blockwords.html");
    assert_eq!(
        class(200, "https://pub.example.org/article/1", &[], &page),
        PageClass::Landing
    );
    assert_eq!(
        class(
            200,
            "https://pub.example.org/article/1",
            &[("server", "cloudflare")],
            &page
        ),
        PageClass::Landing
    );
    // Even on a bare page, the title alone does not make a 200 a challenge.
    assert_eq!(
        class(
            200,
            "https://pub.example.org/x",
            &[("server", "cloudflare")],
            b"<title>Just a moment...</title>"
        ),
        PageClass::Html
    );
}

#[test]
fn rule_order_pdf_beats_everything() {
    // A PDF served from an IdP-looking host or with a 403 status is still bytes.
    assert_eq!(
        class(
            200,
            "https://login.microsoftonline.com/x",
            &[("content-type", "application/pdf")],
            b"%PDF-1.4"
        ),
        PageClass::Pdf { junk_prefix: 0 }
    );
    assert_eq!(
        class(
            403,
            "https://e.org/a",
            &[("server", "cloudflare")],
            b"%PDF-1.4"
        ),
        PageClass::Pdf { junk_prefix: 0 }
    );
}

#[test]
fn rule_order_ezproxy_beats_generic_saml() {
    let page = data("ezproxy_login_page.html");
    assert_eq!(
        class(200, &format!("{LOGIN}/login?qurl=x"), &[], &page),
        PageClass::EzproxyNeedLogin
    );
    assert_eq!(
        class(200, "https://sso.example.org/login", &[], &page),
        PageClass::SamlAutoPost
    );
}

#[test]
fn rule_order_sso_beats_bot_walls_and_status_classes() {
    // Entra errors come with 4xx/5xx statuses; they are still the IdP page.
    assert_eq!(
        class(
            500,
            "https://login.microsoftonline.com/x",
            &[],
            b"AADSTS50105"
        ),
        PageClass::IdpLogin {
            idp: Idp::Entra,
            ca_error: Some("AADSTS50105".into())
        }
    );
    // An OpenAthens 404 is an unknown resource, not a generic NotFound.
    assert_eq!(
        class(
            404,
            "https://go.openathens.net/redirector/kcl.ac.uk?url=x",
            &[],
            b"x"
        ),
        PageClass::OpenathensUnknownResource
    );
}

#[test]
fn rule_order_redirect_beats_reader_shell_and_landing() {
    // A reader URL that redirects is a redirect first; a landing page with an interstitial refresh redirects too.
    assert_eq!(
        class(
            302,
            "https://x.org/doi/epdf/10.1/y",
            &[("location", "/doi/pdf/10.1/y")],
            b""
        ),
        PageClass::Redirect {
            next: "https://x.org/doi/pdf/10.1/y".into()
        }
    );
    let landing = html(
        "<meta name=citation_title content=T><meta http-equiv=refresh content=\"1;url=/real\">",
    );
    assert_eq!(
        class(200, "https://x.org/a", &[], &landing),
        PageClass::Redirect {
            next: "https://x.org/real".into()
        }
    );
}

#[test]
fn totality_on_hostile_inputs() {
    let nul = vec![0u8; 4096];
    for (status, url) in [
        (0, ""),
        (200, "not a url"),
        (302, "https://e.org/"),
        (999, "https://login.kcl.idm.oclc.org/login"),
        (200, "https://go.openathens.net/"),
    ] {
        for body in [&b""[..], &nul[..], b"\xff\xfe\xfd", b"<", b"<a href=\""] {
            let _ = class(
                status,
                url,
                &[
                    ("location", "http://["),
                    ("content-type", "\u{0}"),
                    ("retry-after", "-1"),
                ],
                body,
            );
            let _ = class(
                status,
                url,
                &[("Server", "EZproxy"), ("LOCATION", "/x")],
                body,
            );
        }
    }
    // 2 MiB of unclosed markup is classified in one pass.
    let big = "<div>".repeat(2 * 1024 * 1024 / 5);
    assert_eq!(
        class(200, "https://e.org/a", &[], big.as_bytes()),
        PageClass::Html
    );
}
