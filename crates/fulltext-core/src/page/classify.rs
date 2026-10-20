//! Response classifier: status + final URL + headers + body -> [`PageClass`]. Owned by core-page
//! (spec: `docs/wp/core-page.md`; rule table: research/SUMMARY.md section (c) and research/04 section 6.5,
//! research/06 section 5.2).
//!
//! Rules are checked IN ORDER and the first match wins. XHR follows redirects, so the rules classify the FINAL URL;
//! 3xx rules (`Redirect` via `Location`) only matter for no-follow probes.
//!
//! 1. `Pdf { junk_prefix }`: content type `application/pdf`, or `%PDF-` within the first KiB.
//! 2. `EzproxyNeedLogin`, `EzproxyHostNotConfigured`, `EzproxyError { suspend }`.
//! 3. `IdpLogin { idp, ca_error }` (Entra `login.microsoftonline.com`, `login.live.com`, `login.openathens.net/auth`,
//!    `kclidp.kcl.ac.uk`; title "Sign in to your account"; `AADSTS5xxxx` text becomes `ca_error`), `SamlAutoPost`
//!    (POST form with `SAMLRequest` / `SAMLResponse` / `RelayState` and an onload submit), `OpenathensWait`,
//!    `OpenathensUnknownResource`, `OpenathensBadOrg`, `PublisherSsoHop` (`/action/ssostart`, `/Shibboleth.sso/Login`,
//!    `auth.elsevier.com/ShibAuth`, `fsso.springer.com`, `sp.nature.com/saml`, `oup-sp.sams-sigma.com`).
//! 4. `Challenge { vendor, interactive }` (Cloudflare `cf-mitigated: challenge` / `_cf_chl_opt`; PMC proof of work;
//!    Thieme "Please wait"; ScienceDirect "Preparing your download"; Akamai `bm-verify`) and
//!    `Blocked { vendor }` (Cloudflare "Sorry, you have been blocked"; Akamai "Access Denied" / `errors.edgesuite.net`).
//! 5. `RateLimited { retry_after_s }` (429, "unusual traffic"), `LicenceStop` (publisher "usage policy / excessive
//!    downloads" pages), `NotFound` (404/410), `AccessDenied` (401/403 that is not a bot wall).
//! 6. `Redirect { next }`: linkinghub `input#redirectURL` (decoded, http upgraded) or a meta refresh of <= 15 s with a
//!    URL that is NOT inside `<noscript>`; a refresh without a URL is ignored. For a 3xx, the resolved `Location`.
//! 7. `ReaderShell { rewrite }`: `/doi/epdf/`, `/doi/reader/`, `.pdf+html` with the rewritten PDF URL.
//! 8. `Landing` when the page carries article signals (`citation_title`, `dc.identifier`, ...), else `Html`.
//!
//! # Deliberate refinements of the written rules
//!
//! * **Missing `server` header.** The OA-side fixtures (and any caller that passes only `content-type`) arrive without
//!   a `server` header. A Cloudflare challenge or block is therefore also recognised when the `server` header is
//!   ABSENT and the body carries Cloudflare-only markers (`_cf_chl_opt`, `/cdn-cgi/challenge-platform/`,
//!   `cf-error-details`, title `Attention Required! | Cloudflare`). A different `server` value (for example
//!   `AkamaiGHost`) still rules Cloudflare out.
//! * **Challenge documents without a status.** The login window poll (`inspect::login_state`) only holds the
//!   document, so a page titled "Just a moment..." that carries `_cf_chl_opt` is a Cloudflare challenge at ANY status.
//!   Either marker alone, or `/cdn-cgi/challenge-platform/` (also loaded by ordinary pages), still needs a wall status.
//! * **URL-only probes.** `InspectSpec::Probe` is called with an EMPTY body for browser settle detection, so IdP,
//!   EZproxy login and publisher-SSO hops classify from the URL (plus status) alone. A 200 from the EZproxy login
//!   path with no body is `EzproxyNeedLogin`; `EzproxyError` needs a body that lacks the login form.
//! * **Silverchair "unusual traffic".** That page may come back with status 200; the phrase `experiencing unusual
//!   traffic` is `RateLimited` whatever the status.
//! * **Licence-stop text is only trusted on pages that do not look like articles** (an article about "systematic
//!   download" would otherwise stop a run).
use super::html::Doc;
use crate::model::{Idp, PageClass, Vendor};
use crate::text::head;
use crate::urls::{canonical_key, percent_decode, resolve};
use std::cell::OnceCell;
use std::collections::BTreeMap;
use url::Url;

/// KCL EZproxy constants. Other `*.idm.oclc.org` hosts are recognised generically.
mod kcl {
    /// `Institution.ezproxy.login_host`
    pub const LOGIN_HOST: &str = "login.kcl.idm.oclc.org";
    /// `Institution.ezproxy.suffix`
    pub const SUFFIX: &str = "kcl.idm.oclc.org";
}

/// Entra / Microsoft account sign-in hosts.
const ENTRA_HOSTS: &[&str] = &[
    "login.microsoftonline.com",
    "login.live.com",
    "login.microsoft.com",
    "device.login.microsoftonline.com",
];

/// Substrings (lower-case) of URLs that are publisher-side SP-initiated SSO hops.
const SSO_HOP_MARKERS: &[&str] = &[
    "/shibboleth.sso/login",
    "/shibboleth.sso/discovery",
    "/action/ssostart",
    "auth.elsevier.com/shibauth",
    "fsso.springer.com",
    "sp.nature.com/saml",
    "secure.nature.com/saml2",
    "oup-sp.sams-sigma.com",
    "shibboleth.highwire.org/session/init",
    "profile.thieme.de/html/sso",
    "karger.com/externalauthentication",
    "shibboleth.cambridge.org",
];

/// Phrases (lower-case) of publisher "your access has been stopped" pages.
const LICENCE_STOP: &[&str] = &[
    "excessive download",
    "systematic download",
    "your access has been suspended",
    "access has been temporarily suspended",
    "usage policy violation",
    "we have detected unusual activity",
    "unusual activity from your",
];

/// How far into a body a `%PDF-` header is looked for when the content type already says PDF.
const PDF_MAGIC_WINDOW_TYPED: usize = 4096;
/// How far into a body a `%PDF-` header is looked for when nothing else says PDF.
const PDF_MAGIC_WINDOW: usize = 1024;

/// Everything the classifier looks at, borrowed from the caller.
#[derive(Clone, Copy, Debug)]
pub struct Facts<'a> {
    pub status: u16,
    pub url: &'a Url,
    /// Lower-case header names.
    pub headers: &'a BTreeMap<String, String>,
    /// The (possibly truncated) body as bytes; used for the `%PDF-` check.
    pub body: &'a [u8],
    /// Lossy-decoded body text (for substring rules).
    pub text: &'a str,
    pub doc: &'a Doc,
}

/// Classify one response. Total; the order in the module docs is normative.
pub fn classify(f: &Facts<'_>) -> PageClass {
    let cx = Cx::new(f);
    pdf(&cx)
        .or_else(|| ezproxy(&cx))
        .or_else(|| sso(&cx))
        .or_else(|| bot_wall(&cx))
        .or_else(|| status_class(&cx))
        .or_else(|| redirect(&cx))
        .or_else(|| reader_shell(&cx))
        .unwrap_or_else(|| {
            if has_article_signals(f.doc) {
                PageClass::Landing
            } else {
                PageClass::Html
            }
        })
}

/// True when the document carries article metadata: `citation_title`, `citation_doi`, `citation_pdf_url`,
/// `dc.identifier`, `prism.doi`, `wkhealth_pdf_url`, or `og:type` = `article`.
pub fn has_article_signals(doc: &Doc) -> bool {
    [
        "citation_title",
        "citation_doi",
        "citation_pdf_url",
        "dc.identifier",
        "prism.doi",
        "wkhealth_pdf_url",
    ]
    .iter()
    .any(|n| doc.meta(n).is_some())
        || doc
            .meta("og:type")
            .is_some_and(|v| v.eq_ignore_ascii_case("article"))
}

/// The PDF URL a reader-shell URL stands for: Wiley `/doi/epdf/X` -> `/doi/pdfdirect/X`, other Atypon `/doi/epdf/X` and
/// `/doi/reader/X` -> `/doi/pdf/X`, HighWire `.pdf+html` -> `.pdf`; `needAccess` query pairs are dropped. `None` when
/// the URL is not a rewritable reader URL.
pub fn reader_rewrite(url: &Url) -> Option<String> {
    let path = url.path();
    let wiley = url
        .host_str()
        .is_some_and(|h| h.to_ascii_lowercase().contains("wiley.com"));
    let new_path = if path.contains("/doi/epdf/") {
        path.replacen(
            "/doi/epdf/",
            if wiley {
                "/doi/pdfdirect/"
            } else {
                "/doi/pdf/"
            },
            1,
        )
    } else if path.contains("/doi/reader/") {
        path.replacen("/doi/reader/", "/doi/pdf/", 1)
    } else if let Some(stem) = path.strip_suffix("+html") {
        stem.to_owned()
    } else {
        return None;
    };
    let mut out = url.clone();
    out.set_fragment(None);
    out.set_path(&new_path);
    let kept: Vec<&str> = url
        .query()
        .map(|q| {
            q.split('&')
                .filter(|kv| !kv.is_empty() && !kv.to_ascii_lowercase().starts_with("needaccess"))
                .collect()
        })
        .unwrap_or_default();
    let query = kept.join("&");
    out.set_query(if query.is_empty() {
        None
    } else {
        Some(query.as_str())
    });
    Some(out.to_string())
}

// ------------------------------------------------------------------ context

/// Derived, lazily computed views of the facts shared by all rules.
struct Cx<'a> {
    f: &'a Facts<'a>,
    /// Lower-case host of the final URL.
    host: String,
    path: &'a str,
    content_type: String,
    server: String,
    /// Lower-cased, trimmed `<title>`.
    title: String,
    /// The `Location` header resolved against the URL, for 3xx responses.
    location: Option<Url>,
    lower: OnceCell<String>,
}

impl<'a> Cx<'a> {
    fn new(f: &'a Facts<'a>) -> Self {
        let header = |n: &str| {
            f.headers
                .get(n)
                .map(|v| v.trim().to_ascii_lowercase())
                .unwrap_or_default()
        };
        let location = (300..400)
            .contains(&f.status)
            .then(|| f.headers.get("location"))
            .flatten()
            .and_then(|l| resolve(f.url, l));
        Cx {
            f,
            host: f.url.host_str().unwrap_or("").to_ascii_lowercase(),
            path: f.url.path(),
            content_type: header("content-type"),
            server: header("server"),
            title: f.doc.title.trim().to_ascii_lowercase(),
            location,
            lower: OnceCell::new(),
        }
    }

    fn status(&self) -> u16 {
        self.f.status
    }

    fn is_redirect(&self) -> bool {
        (300..400).contains(&self.f.status)
    }

    /// Lower-cased body text (computed once).
    fn text(&self) -> &str {
        self.lower.get_or_init(|| self.f.text.to_ascii_lowercase())
    }

    /// True when the lower-cased body text contains `needle` (which must be lower-case).
    fn has(&self, needle: &str) -> bool {
        self.text().contains(needle)
    }

    fn header(&self, name: &str) -> &str {
        self.f.headers.get(name).map_or("", |v| v.trim())
    }

    /// `server: cloudflare`, or no `server` header at all (callers that pass only the content type).
    fn maybe_cloudflare(&self) -> bool {
        self.server.contains("cloudflare") || self.server.is_empty()
    }

    fn url_string_lower(&self) -> String {
        self.f.url.as_str().to_ascii_lowercase()
    }
}

// ------------------------------------------------------------------ 1. PDF

fn pdf(cx: &Cx<'_>) -> Option<PageClass> {
    let typed = cx.content_type.contains("application/pdf") || cx.content_type.contains("x-pdf");
    let magic = |window: usize| find_magic(head(cx.f.body, window));
    let junk = if typed {
        if cx.f.body.is_empty() {
            Some(0)
        } else {
            magic(PDF_MAGIC_WINDOW_TYPED)
        }
    } else {
        magic(PDF_MAGIC_WINDOW)
    };
    junk.map(|offset| PageClass::Pdf {
        junk_prefix: u32::try_from(offset).unwrap_or(u32::MAX),
    })
}

fn find_magic(bytes: &[u8]) -> Option<usize> {
    bytes.windows(5).position(|w| w == b"%PDF-")
}

// ------------------------------------------------------------------ 2. EZproxy

/// `login.kcl.idm.oclc.org`, or any `login.<institution>.idm.oclc.org`.
fn is_login_host(h: &str) -> bool {
    h == kcl::LOGIN_HOST || (h.starts_with("login.") && h.ends_with(".idm.oclc.org"))
}

/// A proxied-resource host: `*.kcl.idm.oclc.org` or any other `*.idm.oclc.org`, except a login host.
fn is_proxied_host(h: &str) -> bool {
    (h == kcl::SUFFIX || h.ends_with(".idm.oclc.org")) && !is_login_host(h)
}

/// `.../login?url=...` or `.../login?qurl=...` on a login host.
fn is_login_url(u: &Url, ezproxy_server: bool) -> bool {
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    (is_login_host(&host) || (ezproxy_server && host.starts_with("login.")))
        && u.path() == "/login"
        && u.query()
            .is_some_and(|q| q.starts_with("url=") || q.starts_with("qurl="))
}

fn ezproxy(cx: &Cx<'_>) -> Option<PageClass> {
    let ezproxy_server = cx.server.contains("ezproxy");
    let login_page = is_login_host(&cx.host) || (ezproxy_server && cx.path == "/login");
    let proxied = is_proxied_host(&cx.host) || (ezproxy_server && !login_page);
    if !login_page && !proxied {
        return None;
    }

    if let Some(loc) = &cx.location {
        // (a) proxied host -> login?(q)url=   (c) login host /<x> -> login?url=...<x>
        let from_login_other_path = login_page && cx.path != "/login";
        if (proxied || from_login_other_path) && is_login_url(loc, ezproxy_server) {
            return Some(PageClass::EzproxyNeedLogin);
        }
        // Host not configured: the proxy bounces the request to the login host with the same path and no /login.
        let loc_host = loc.host_str().unwrap_or("").to_ascii_lowercase();
        let same_target = loc.path() == cx.path && loc.query() == cx.f.url.query();
        if proxied && is_login_host(&loc_host) && loc.path() != "/login" && same_target {
            return Some(PageClass::EzproxyHostNotConfigured);
        }
        return None;
    }

    if !login_page {
        return None;
    }
    if cx.status() == 404 && cx.has("<h1>404 not found</h1>") {
        return Some(PageClass::EzproxyHostNotConfigured);
    }
    if cx.status() == 200 && cx.path.starts_with("/login") {
        let has_form = cx.has("name='ezproxyform'")
            || cx.has("name=\"ezproxyform\"")
            || cx.has("name=ezproxyform");
        let has_relay =
            cx.has("name='relaystate' value='ezp.") || cx.has("name=\"relaystate\" value=\"ezp.");
        if has_form || has_relay || cx.title.contains("shibboleth authentication request") {
            return Some(PageClass::EzproxyNeedLogin);
        }
        let html = cx.content_type.is_empty() || cx.content_type.contains("text/html");
        if cx.f.body.is_empty() {
            // URL-only probe of the login page: it is the sign-in page.
            return Some(PageClass::EzproxyNeedLogin);
        }
        if html {
            let suspend = cx.has("suspend") || cx.has("usage limit") || cx.has("excessive");
            return Some(PageClass::EzproxyError { suspend });
        }
    }
    None
}

// ------------------------------------------------------------------ 3. SSO

fn sso(cx: &Cx<'_>) -> Option<PageClass> {
    let host = cx.host.as_str();
    let idp = if ENTRA_HOSTS.contains(&host) {
        Some(Idp::Entra)
    } else if host == "login.openathens.net" && cx.path.starts_with("/auth/") {
        Some(Idp::Openathens)
    } else if host == "kclidp.kcl.ac.uk" {
        Some(Idp::Kcl)
    } else {
        None
    };
    if let Some(idp) = idp {
        return Some(PageClass::IdpLogin {
            idp,
            ca_error: aadsts_code(cx.f.text),
        });
    }

    if cx.f.doc.forms.iter().any(|form| {
        form.method == "post"
            && form.inputs.iter().any(|i| {
                i.attr("name").is_some_and(|n| {
                    n.eq_ignore_ascii_case("SAMLRequest") || n.eq_ignore_ascii_case("SAMLResponse")
                })
            })
    }) {
        return Some(PageClass::SamlAutoPost);
    }

    if host == "go.openathens.net" {
        match cx.status() {
            200 if cx.title.contains("please wait") => return Some(PageClass::OpenathensWait),
            404 => return Some(PageClass::OpenathensUnknownResource),
            403 => return Some(PageClass::OpenathensBadOrg),
            _ => {}
        }
    }

    let is_hop = |s: &str| SSO_HOP_MARKERS.iter().any(|m| s.contains(m));
    let location_hop = cx.is_redirect()
        && (cx
            .f
            .headers
            .get("location")
            .is_some_and(|l| is_hop(&l.to_ascii_lowercase()))
            || cx
                .location
                .as_ref()
                .is_some_and(|l| is_hop(&l.as_str().to_ascii_lowercase())));
    if is_hop(&cx.url_string_lower()) || location_hop {
        return Some(PageClass::PublisherSsoHop);
    }
    None
}

/// `AADSTS` followed by digits, taken from the page text (`AADSTS53000`, `AADSTS50105`, ...).
fn aadsts_code(text: &str) -> Option<String> {
    text.match_indices("AADSTS").find_map(|(i, m)| {
        let digits: String = text
            .get(i + m.len()..)?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        (!digits.is_empty()).then(|| format!("AADSTS{digits}"))
    })
}

// ------------------------------------------------------------------ 4. bot walls

fn bot_wall(cx: &Cx<'_>) -> Option<PageClass> {
    let wall_status = matches!(cx.status(), 403 | 503);

    // Cloudflare managed / JS challenge.
    let strong_cf = cx.has("_cf_chl_opt") || cx.has("/cdn-cgi/challenge-platform/");
    let just_a_moment = cx.title.starts_with("just a moment");
    // The interstitial itself, at any status: callers that only hold the document (the login window poll) cannot
    // pass a status. Its title plus the challenge options object is unmistakable; the `/cdn-cgi/challenge-platform/`
    // path alone is not, because ordinary pages behind Cloudflare load `scripts/jsd/main.js` from it.
    let interstitial = just_a_moment && cx.has("_cf_chl_opt");
    if cx
        .header("cf-mitigated")
        .to_ascii_lowercase()
        .contains("challenge")
        || (cx.maybe_cloudflare()
            && (interstitial || (wall_status && (strong_cf || just_a_moment))))
    {
        let interactive = ["cf-turnstile", "turnstile", "g-recaptcha", "h-captcha"]
            .iter()
            .any(|m| cx.has(m));
        return Some(PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive,
        });
    }

    // PMC proof of work.
    if cx.title.starts_with("preparing to download")
        && (cx.f.text.contains("POW_CHALLENGE") || cx.f.doc.has_script_src("pow-"))
    {
        return Some(PageClass::Challenge {
            vendor: Vendor::Pmc,
            interactive: false,
        });
    }
    // Thieme "Please wait" script challenge.
    if cx.title == "please wait" && cx.f.doc.has_script_src("/products/assets/challenge-") {
        return Some(PageClass::Challenge {
            vendor: Vendor::Thieme,
            interactive: false,
        });
    }
    // ScienceDirect download interstitial. The interstitial carries no article metadata, so an article that merely
    // quotes these phrases (or links to `ref=cra_js_challenge`) is not a challenge.
    if (cx.has("preparing your download")
        || cx.has("/craft/challenge/pdf")
        || cx.has("ref=cra_js_challenge"))
        && !has_article_signals(cx.f.doc)
    {
        return Some(PageClass::Challenge {
            vendor: Vendor::Sciencedirect,
            interactive: false,
        });
    }
    // Akamai Bot Manager meta refresh.
    if cx
        .f
        .doc
        .refreshes
        .iter()
        .any(|r| r.parse().1.is_some_and(|t| t.contains("bm-verify=")))
    {
        return Some(PageClass::Challenge {
            vendor: Vendor::Akamai,
            interactive: false,
        });
    }

    // Cloudflare WAF block page.
    if cx.status() == 403 {
        let cf_markers = cx.has("cf-error-details")
            || cx.has("sorry, you have been blocked")
            || cx.title == "attention required! | cloudflare";
        if cf_markers
            && (cx.server.contains("cloudflare") || (cx.server.is_empty() && cx.has("cloudflare")))
        {
            return Some(PageClass::Blocked {
                vendor: Vendor::Cloudflare,
            });
        }
        // Akamai edge block.
        if !cx.header("akamai-grn").is_empty() || cx.has("edgesuite") || cx.title == "access denied"
        {
            return Some(PageClass::Blocked {
                vendor: Vendor::Akamai,
            });
        }
    }
    None
}

// ------------------------------------------------------------------ 5. status classes

fn status_class(cx: &Cx<'_>) -> Option<PageClass> {
    let throttled_text = cx.has("unusual traffic") || cx.has("too many requests");
    if cx.status() == 429
        || (matches!(cx.status(), 403 | 503) && throttled_text)
        || cx.has("experiencing unusual traffic")
    {
        let retry_after_s = cx.header("retry-after").parse::<u32>().ok();
        return Some(PageClass::RateLimited { retry_after_s });
    }
    if !has_article_signals(cx.f.doc) && LICENCE_STOP.iter().any(|p| cx.has(p)) {
        return Some(PageClass::LicenceStop);
    }
    match cx.status() {
        404 | 410 => Some(PageClass::NotFound),
        401 | 403 => Some(PageClass::AccessDenied),
        _ => None,
    }
}

// ------------------------------------------------------------------ 6. redirect

fn redirect(cx: &Cx<'_>) -> Option<PageClass> {
    if let Some(loc) = &cx.location {
        return Some(PageClass::Redirect {
            next: loc.to_string(),
        });
    }
    let doc = cx.f.doc;
    // Elsevier linkinghub: <input id="redirectURL" value="http%3A%2F%2F...">
    if let Some(value) = doc.input_by_id("redirectURL").and_then(|i| i.attr("value")) {
        let decoded = percent_decode(value.trim());
        if let Some(mut next) = resolve(cx.f.url, &decoded) {
            if next.scheme() == "http" {
                let _ = next.set_scheme("https"); // http -> https cannot fail
            }
            return Some(PageClass::Redirect {
                next: next.to_string(),
            });
        }
    }
    // Meta refresh of at most 15 s with a target. <noscript> refreshes never reach `Doc`.
    let here = canonical_key(cx.f.url.as_str());
    doc.refreshes.iter().find_map(|r| {
        let (delay, target) = r.parse();
        let next = resolve(cx.f.url, &target?)?;
        (delay.unwrap_or(0.0) <= 15.0 && canonical_key(next.as_str()) != here).then(|| {
            PageClass::Redirect {
                next: next.to_string(),
            }
        })
    })
}

// ------------------------------------------------------------------ 7. reader shells

fn reader_shell(cx: &Cx<'_>) -> Option<PageClass> {
    let path = cx.path;
    let shell = path.contains("/doi/epdf/")
        || path.contains("/doi/reader/")
        || path.contains("/epdf/")
        || path.ends_with(".pdf+html");
    shell.then(|| PageClass::ReaderShell {
        rewrite: reader_rewrite(cx.f.url),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::html::parse;

    fn run(status: u16, url: &str, headers: &[(&str, &str)], body: &str) -> PageClass {
        let url = Url::parse(url).unwrap();
        let headers: BTreeMap<String, String> = headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let doc = parse(body);
        classify(&Facts {
            status,
            url: &url,
            headers: &headers,
            body: body.as_bytes(),
            text: body,
            doc: &doc,
        })
    }

    #[test]
    fn pdf_by_type_and_by_magic() {
        assert_eq!(
            run(
                200,
                "https://e.org/a",
                &[("content-type", "application/pdf")],
                ""
            ),
            PageClass::Pdf { junk_prefix: 0 }
        );
        assert_eq!(
            run(
                200,
                "https://e.org/a",
                &[("content-type", "application/pdf")],
                "%PDF-1.7 x"
            ),
            PageClass::Pdf { junk_prefix: 0 }
        );
        assert_eq!(
            run(
                200,
                "https://e.org/a",
                &[("content-type", "text/html")],
                "\r\n  %PDF-1.4"
            ),
            PageClass::Pdf { junk_prefix: 4 }
        );
        assert_eq!(
            run(
                200,
                "https://e.org/a",
                &[("content-type", "application/pdf")],
                "<html>not a pdf</html>"
            ),
            PageClass::Html
        );
        assert_eq!(
            run(
                200,
                "https://e.org/a",
                &[("content-type", "application/x-pdf")],
                ""
            ),
            PageClass::Pdf { junk_prefix: 0 }
        );
    }

    #[test]
    fn landing_vs_html() {
        assert_eq!(
            run(
                200,
                "https://e.org/a",
                &[],
                "<meta name=citation_title content=T>"
            ),
            PageClass::Landing
        );
        assert_eq!(
            run(
                200,
                "https://e.org/a",
                &[],
                "<meta property=og:type content=Article>"
            ),
            PageClass::Landing
        );
        assert_eq!(
            run(
                200,
                "https://e.org/a",
                &[],
                "<meta property=og:type content=website><p>Just a moment...</p>"
            ),
            PageClass::Html
        );
        assert_eq!(run(200, "https://e.org/a", &[], ""), PageClass::Html);
    }

    #[test]
    fn rate_limit_and_status_classes() {
        assert_eq!(
            run(429, "https://e.org/a", &[("retry-after", "120")], ""),
            PageClass::RateLimited {
                retry_after_s: Some(120)
            }
        );
        assert_eq!(
            run(
                429,
                "https://e.org/a",
                &[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")],
                ""
            ),
            PageClass::RateLimited {
                retry_after_s: None
            }
        );
        assert_eq!(
            run(
                503,
                "https://e.org/a",
                &[],
                "<p>We detected unusual traffic from your network</p>"
            ),
            PageClass::RateLimited {
                retry_after_s: None
            }
        );
        assert_eq!(
            run(404, "https://e.org/a", &[], "<p>gone</p>"),
            PageClass::NotFound
        );
        assert_eq!(run(410, "https://e.org/a", &[], ""), PageClass::NotFound);
        assert_eq!(
            run(401, "https://e.org/a", &[], ""),
            PageClass::AccessDenied
        );
        assert_eq!(
            run(403, "https://e.org/a", &[], "<p>forbidden</p>"),
            PageClass::AccessDenied
        );
        assert_eq!(
            run(500, "https://e.org/a", &[], "<p>oops</p>"),
            PageClass::Html
        );
    }

    #[test]
    fn reader_rewrites() {
        let rw = |u: &str| reader_rewrite(&Url::parse(u).unwrap());
        assert_eq!(
            rw("https://onlinelibrary.wiley.com/doi/epdf/10.1002/wjs.12026").as_deref(),
            Some("https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/wjs.12026")
        );
        assert_eq!(
            rw("https://www.tandfonline.com/doi/epdf/10.1080/x.1?needAccess=true").as_deref(),
            Some("https://www.tandfonline.com/doi/pdf/10.1080/x.1")
        );
        assert_eq!(
            rw("https://journals.sagepub.com/doi/reader/10.1177/2050312118807624").as_deref(),
            Some("https://journals.sagepub.com/doi/pdf/10.1177/2050312118807624")
        );
        assert_eq!(
            rw("https://x.org/doi/epdf/10.1/y?a=1&needAccess=true&b=2").as_deref(),
            Some("https://x.org/doi/pdf/10.1/y?a=1&b=2")
        );
        assert_eq!(
            rw("https://x.highwire.org/content/1/2/3.full.pdf+html").as_deref(),
            Some("https://x.highwire.org/content/1/2/3.full.pdf")
        );
        assert_eq!(rw("https://x.org/epdf/abc"), None);
        assert_eq!(rw("https://x.org/article"), None);
    }

    #[test]
    fn total_on_odd_input() {
        let _ = run(0, "https://e.org/", &[], "");
        let _ = run(999, "https://e.org/", &[("location", "::::")], "\u{0}\u{0}");
        let _ = run(302, "https://e.org/", &[("location", "")], "");
        let _ = run(
            302,
            "https://login.kcl.idm.oclc.org/x",
            &[("location", "http://[::1")],
            "",
        );
    }
}
