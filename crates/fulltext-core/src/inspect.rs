//! The single pure entry point the host reaches through `fx_inspect`. Owned by core-pdf-urls-inspect
//! (spec: `docs/wp/core-pdf-urls-inspect.md`).
//!
//! # Response-shape rule
//! The returned [`Inspection`] always describes what the bytes actually are, whatever the spec expected:
//!
//! | spec | body is a PDF | body is HTML | anything else |
//! |---|---|---|---|
//! | `Api` | `Failed` (not an API body) | `Api` with `Unparseable` / `NotFound` decided by status first | `Api` (parser decides) |
//! | `Page` | `Pdf(verdict)` | `Page(..)` | `Page(..)` (class `Html`, no candidates) |
//! | `Pdf` | `Pdf(verdict)` | `Page(..)` (so a challenge, login or landing page is classified, not just "not a PDF") | `Pdf(verdict)` with `is_pdf = false` |
//! | `Probe` | `Probe { class: Pdf }` | `Probe { class }` | `Probe { class }` |
//! | `None` | `None` | `None` | `None` |
//!
//! Two refinements keep the table total: a `Pdf` spec whose response is an error (HTTP status >= 400) with a
//! non-empty text body is classified as a page too (an `Access denied` or rate-limit message is not HTML but it is
//! not a PDF either); and any body over [`MAX_BODY_BYTES`] is `Failed { reason: "body_too_large" }` before anything
//! looks at it.
//!
//! The engine therefore must accept both `Inspection::Pdf` and `Inspection::Page` for `Page` and `Pdf` specs.
use crate::model::{
    InspectMeta, InspectSpec, Inspection, LoginState, PageClass, RespMeta, UrlClassKind,
};
use crate::{oa, page, pdf, text, urls};

/// Largest body the dispatcher looks at (the host stages far less: 2 MiB of text or head plus tail of a binary).
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Bytes inspected when deciding whether an error body is text.
const TEXT_PROBE_BYTES: usize = 512;

/// Total dispatcher: `meta.spec` picks the analysis, `meta.resp` carries the response facts, `body` the staged bytes
/// (text up to 2 MiB, or `head ++ tail` of a binary body when `resp.head_len` is set).
/// Never panics; internal failures become [`Inspection::Failed`].
pub fn inspect(meta: &InspectMeta, body: &[u8]) -> Inspection {
    if body.len() > MAX_BODY_BYTES {
        return failed("body_too_large");
    }
    let resp = &meta.resp;
    match &meta.spec {
        InspectSpec::Api { source, requested } => {
            if body.starts_with(pdf::MAGIC) {
                failed("api_body_is_pdf")
            } else {
                Inspection::Api(oa::parse(*source, resp, body, requested))
            }
        }
        InspectSpec::Page { requested, route } => {
            let verdict = pdf::verdict(resp, body);
            if verdict.is_pdf {
                Inspection::Pdf(verdict)
            } else {
                Inspection::Page(page::inspect_page(resp, requested, *route, body))
            }
        }
        InspectSpec::Pdf { requested, route } => {
            let verdict = pdf::verdict(resp, body);
            if !verdict.is_pdf && (verdict.looks_html || (resp.status >= 400 && is_text(body))) {
                Inspection::Page(page::inspect_page(resp, requested, *route, body))
            } else {
                Inspection::Pdf(verdict)
            }
        }
        InspectSpec::Probe { .. } => {
            let verdict = pdf::verdict(resp, body);
            let class = if verdict.is_pdf {
                PageClass::Pdf {
                    junk_prefix: verdict.junk_prefix,
                }
            } else {
                page::classify_response(resp, body)
            };
            Inspection::Probe { class }
        }
        InspectSpec::None => Inspection::None,
    }
}

fn failed(reason: &str) -> Inspection {
    Inspection::Failed {
        reason: reason.to_owned(),
    }
}

/// A non-empty body whose first bytes contain no control characters other than tab, newline, form feed and CR.
fn is_text(body: &[u8]) -> bool {
    let sample = text::head(body, TEXT_PROBE_BYTES);
    !sample.is_empty()
        && sample
            .iter()
            .all(|&b| b >= b' ' || matches!(b, b'\t' | b'\n' | b'\r' | 0x0c))
}

/// State of a login / challenge window from its current URL and (optionally) its current document HTML.
///
/// * `Pending`: still on an IdP, SAML auto-post, OpenAthens "please wait", EZproxy login or publisher SSO hop.
/// * `ChallengeShown`: a Cloudflare / Akamai / proof-of-work challenge is on screen (the user may need to act).
/// * `Landed`: on a non-SSO page of a target host (publisher, proxied publisher, PDF).
/// * `Refused { code }`: the IdP or proxy refused the sign-in (Entra `AADSTS5xxxx` conditional-access / device-policy
///   errors, OpenAthens bad organisation, EZproxy suspend); `code` is the `AADSTS` code when present.
///
/// Without a document the URL alone decides (an SSO-looking URL is `Pending`). With a document the page classifier
/// decides, except that a document the classifier cannot tell from a landing page, shown at an SSO-looking URL, is
/// still `Pending`. Also exposed to JS as the additive export `fx_login_state`.
pub fn login_state(url: &str, html: Option<&str>) -> LoginState {
    let html = html.filter(|h| !h.is_empty());
    let on_sso_url = !matches!(urls::url_class(None, url), UrlClassKind::Other);
    let Some(html) = html else {
        return if on_sso_url {
            LoginState::Pending
        } else {
            classified(url, "")
        };
    };
    match classified(url, html) {
        LoginState::Landed if on_sso_url => LoginState::Pending,
        state => state,
    }
}

/// Classify `html` (cut at [`text::MAX_TEXT_BYTES`]) as a 200 response from `url` and map the class.
fn classified(url: &str, html: &str) -> LoginState {
    let bytes = text::head(html.as_bytes(), text::MAX_TEXT_BYTES);
    let mut resp = RespMeta {
        status: 200,
        final_url: url.to_owned(),
        body_len: u64::try_from(html.len()).unwrap_or(u64::MAX),
        truncated: bytes.len() < html.len(),
        ..RespMeta::default()
    };
    if !html.is_empty() {
        resp.headers.insert(
            "content-type".to_owned(),
            "text/html; charset=utf-8".to_owned(),
        );
    }
    state_of(&page::classify_response(&resp, bytes))
}

/// Map a page class to the state of the login window that is showing it.
pub fn state_of(class: &PageClass) -> LoginState {
    let refused = |code: &str| LoginState::Refused {
        code: Some(code.to_owned()),
    };
    match class {
        PageClass::IdpLogin {
            ca_error: Some(code),
            ..
        } => LoginState::Refused {
            code: Some(code.clone()),
        },
        PageClass::IdpLogin { ca_error: None, .. }
        | PageClass::EzproxyNeedLogin
        | PageClass::SamlAutoPost
        | PageClass::OpenathensWait
        | PageClass::PublisherSsoHop
        | PageClass::Redirect { .. } => LoginState::Pending,
        PageClass::Challenge { .. } => LoginState::ChallengeShown,
        PageClass::OpenathensBadOrg => refused("openathens_bad_org"),
        PageClass::OpenathensUnknownResource => refused("openathens_unknown_resource"),
        PageClass::EzproxyHostNotConfigured => refused("ezproxy_host_not_configured"),
        PageClass::EzproxyError { suspend: true } => refused("ezproxy_suspended"),
        PageClass::EzproxyError { suspend: false } => refused("ezproxy_error"),
        PageClass::Blocked { .. } => refused("blocked"),
        PageClass::RateLimited { .. } => refused("rate_limited"),
        PageClass::LicenceStop => refused("licence_stop"),
        PageClass::Pdf { .. }
        | PageClass::Landing
        | PageClass::Html
        | PageClass::ReaderShell { .. }
        | PageClass::NotFound
        | PageClass::AccessDenied => LoginState::Landed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Ids, Route, Source};

    fn meta(spec: InspectSpec) -> InspectMeta {
        InspectMeta {
            spec,
            resp: RespMeta {
                status: 200,
                final_url: "https://example.org/".into(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn dispatch_is_total_on_empty_bodies() {
        for spec in [
            InspectSpec::Api {
                source: Source::Openalex,
                requested: Ids::default(),
            },
            InspectSpec::Page {
                requested: Ids::default(),
                route: Route::Oa,
            },
            InspectSpec::Pdf {
                requested: Ids::default(),
                route: Route::Inst,
            },
            InspectSpec::Probe { route: Route::Oa },
            InspectSpec::None,
        ] {
            let _ = inspect(&meta(spec), b"");
        }
        assert_eq!(inspect(&meta(InspectSpec::None), b"x"), Inspection::None);
    }

    #[test]
    fn text_probe() {
        assert!(is_text(b"Access denied\r\n"));
        assert!(!is_text(b""));
        assert!(!is_text(b"ab\x00cd"));
    }
}
