//! Page analysis: tokenizer wrapper, response classifier, PDF-link extractor, publisher rules.
//! Owned by core-page (spec: `docs/wp/core-page.md`).
//!
//! Two entry points are used by [`crate::inspect`]; their signatures are frozen:
//!
//! * [`inspect_page`]: `InspectSpec::Page` -> [`PageInspection`] (class + candidates + ids + access hint).
//! * [`classify_response`]: `InspectSpec::Probe` and `login_state` -> [`PageClass`] only (cheap; body may be empty).
//!
//! Module map: [`html`] (tokenizer wrapper, `Doc`), [`classify`] (ordered rule table), [`extract`] (generic PDF-link
//! signals, screening, access hints), [`rules`] (per-publisher table), `util` (private shared helpers).
pub mod classify;
pub mod extract;
pub mod html;
pub mod rules;
mod util;

use crate::model::{AccessHint, Ids, PageClass, PageIds, PageInspection, RespMeta, Route};
use crate::text::{MAX_TEXT_BYTES, decode};
use crate::urls::parse_http;
use classify::{Facts, classify};
use extract::{PageCtx, extract};
use std::borrow::Cow;
use std::collections::BTreeMap;
use url::Url;

/// The URL used when a response carries no usable `final_url`.
const UNKNOWN_URL: &str = "https://unknown.invalid/";

/// Classify a response and, for landing pages, extract PDF candidates.
///
/// * Decodes the body lossily (at most [`MAX_TEXT_BYTES`]), parses it with [`html::parse`], builds
///   [`classify::Facts`] from `resp`. An unparseable `final_url` becomes `https://unknown.invalid/`.
/// * Only classes `Landing`, `Html` and `ReaderShell` run [`extract::extract`]; challenge / login / block classes
///   return no candidates (nothing on those pages is a link we want). For `ReaderShell { rewrite: Some(u) }` the
///   rewritten URL is added as the top candidate with signal `reader_rewrite`.
/// * `Redirect` pages carry their target in `PageClass::Redirect { next }`; candidates stay empty.
pub fn inspect_page(resp: &RespMeta, requested: &Ids, route: Route, body: &[u8]) -> PageInspection {
    let Some(a) = Analysis::new(resp, body) else {
        return plain(PageClass::Html);
    };
    let class = a.classify();
    match &class {
        PageClass::Landing | PageClass::Html | PageClass::ReaderShell { .. } => {
            let mut e = extract(&PageCtx {
                url: &a.url,
                requested,
                route,
                doc: &a.doc,
            });
            if let PageClass::ReaderShell {
                rewrite: Some(next),
            } = &class
                && let Ok(u) = Url::parse(next)
            {
                let mut c = util::candidate(
                    &u,
                    &a.url,
                    e.page_ids.doi.as_deref(),
                    "reader_rewrite".to_owned(),
                    100,
                );
                c.needs_browser = e.candidates.iter().any(|x| x.needs_browser);
                e.candidates.insert(0, c);
            }
            PageInspection {
                class,
                page_ids: e.page_ids,
                doi_match: e.doi_match,
                access: e.access,
                candidates: e.candidates,
                rejected: e.rejected,
                rule: e.rule,
            }
        }
        _ => plain(class),
    }
}

/// A [`PageInspection`] that only carries a class (no ids, candidates or hints).
fn plain(class: PageClass) -> PageInspection {
    PageInspection {
        class,
        page_ids: PageIds::default(),
        doi_match: None,
        access: AccessHint::Unknown,
        candidates: Vec::new(),
        rejected: Vec::new(),
        rule: None,
    }
}

/// Class of a response without extraction. Uses status, `final_url`, headers (including `location` for no-follow
/// probes, resolved against `final_url`) and whatever body bytes are present (possibly none).
pub fn classify_response(resp: &RespMeta, body: &[u8]) -> PageClass {
    Analysis::new(resp, body).map_or(PageClass::Html, |a| a.classify())
}

/// A response decoded and parsed once, shared by both entry points.
struct Analysis<'a> {
    status: u16,
    url: Url,
    headers: Cow<'a, BTreeMap<String, String>>,
    body: &'a [u8],
    text: Cow<'a, str>,
    doc: html::Doc,
}

impl<'a> Analysis<'a> {
    /// `None` only if even the constant fallback URL fails to parse, which cannot happen.
    fn new(resp: &'a RespMeta, body: &'a [u8]) -> Option<Self> {
        let url = parse_http(&resp.final_url).or_else(|| Url::parse(UNKNOWN_URL).ok())?;
        // The contract says header names are lower-case; tolerate a host that forgot.
        let headers = if resp
            .headers
            .keys()
            .all(|k| !k.bytes().any(|b| b.is_ascii_uppercase()))
        {
            Cow::Borrowed(&resp.headers)
        } else {
            Cow::Owned(
                resp.headers
                    .iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                    .collect(),
            )
        };
        let text = decode(body.get(..MAX_TEXT_BYTES).unwrap_or(body));
        let doc = html::parse(&text);
        Some(Analysis {
            status: resp.status,
            url,
            headers,
            body,
            text,
            doc,
        })
    }

    fn classify(&self) -> PageClass {
        classify(&Facts {
            status: self.status,
            url: &self.url,
            headers: &self.headers,
            body: self.body,
            text: &self.text,
            doc: &self.doc,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Vendor;

    fn resp(status: u16, url: &str, headers: &[(&str, &str)]) -> RespMeta {
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

    #[test]
    fn landing_page_yields_candidates_and_ids() {
        let html = br#"<html><head><meta name="citation_title" content="T"><meta name="citation_doi" content="10.1234/abc"><meta name="citation_pdf_url" content="/a.pdf"></head></html>"#;
        let pi = inspect_page(
            &resp(200, "https://pub.example.org/x", &[]),
            &Ids {
                doi: Some("10.1234/ABC".into()),
                ..Default::default()
            },
            Route::Oa,
            html,
        );
        assert_eq!(pi.class, PageClass::Landing);
        assert_eq!(pi.candidates[0].url, "https://pub.example.org/a.pdf");
        assert_eq!(pi.page_ids.doi.as_deref(), Some("10.1234/abc"));
        assert_eq!(pi.doi_match, Some(true));
    }

    #[test]
    fn challenge_and_redirect_pages_return_no_candidates() {
        let cf = br#"<title>Just a moment...</title><meta name="citation_pdf_url" content="/a.pdf"><script>window._cf_chl_opt={}</script>"#;
        let pi = inspect_page(
            &resp(
                403,
                "https://pub.example.org/x",
                &[("server", "cloudflare")],
            ),
            &Ids::default(),
            Route::Oa,
            cf,
        );
        assert_eq!(
            pi.class,
            PageClass::Challenge {
                vendor: Vendor::Cloudflare,
                interactive: false
            }
        );
        assert!(pi.candidates.is_empty() && pi.rule.is_none());
        let redirect = br#"<meta http-equiv="refresh" content="1;url=/next">"#;
        let pi = inspect_page(
            &resp(200, "https://pub.example.org/x", &[]),
            &Ids::default(),
            Route::Oa,
            redirect,
        );
        assert_eq!(
            pi.class,
            PageClass::Redirect {
                next: "https://pub.example.org/next".into()
            }
        );
        assert!(pi.candidates.is_empty());
    }

    #[test]
    fn reader_shell_adds_the_rewrite_on_top() {
        let html = br#"<meta name="citation_title" content="T"><meta name="dc.Identifier" scheme="doi" content="10.1080/07853890.2016.1186828">"#;
        let pi = inspect_page(
            &resp(
                200,
                "https://www.tandfonline.com/doi/epdf/10.1080/07853890.2016.1186828?needAccess=true",
                &[],
            ),
            &Ids::default(),
            Route::Oa,
            html,
        );
        assert_eq!(
            pi.class,
            PageClass::ReaderShell {
                rewrite: Some(
                    "https://www.tandfonline.com/doi/pdf/10.1080/07853890.2016.1186828".into()
                )
            }
        );
        assert_eq!(
            pi.candidates[0].url,
            "https://www.tandfonline.com/doi/pdf/10.1080/07853890.2016.1186828"
        );
        assert_eq!(pi.candidates[0].signal.as_deref(), Some("reader_rewrite"));
    }

    #[test]
    fn probes_classify_from_headers_and_url_alone() {
        let r = resp(
            302,
            "https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71",
            &[
                (
                    "location",
                    "https://login.kcl.idm.oclc.org/login?qurl=https%3a%2f%2fwww.bmj.com%2fcontent%2f372%2fbmj.n71",
                ),
                ("server", "EZproxy"),
            ],
        );
        assert_eq!(classify_response(&r, b""), PageClass::EzproxyNeedLogin);
        let r = resp(
            200,
            "https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/saml2",
            &[],
        );
        assert_eq!(
            classify_response(&r, b""),
            PageClass::IdpLogin {
                idp: crate::model::Idp::Entra,
                ca_error: None
            }
        );
        let r = resp(
            200,
            "https://onlinelibrary.wiley.com/action/ssostart?idp=x&redirectUri=y",
            &[],
        );
        assert_eq!(classify_response(&r, b""), PageClass::PublisherSsoHop);
    }

    #[test]
    fn unparseable_urls_and_upper_case_headers_are_tolerated() {
        let r = resp(
            403,
            "not a url",
            &[("Server", "cloudflare"), ("CF-Mitigated", "challenge")],
        );
        assert_eq!(
            classify_response(&r, b""),
            PageClass::Challenge {
                vendor: Vendor::Cloudflare,
                interactive: false
            }
        );
        let r = resp(200, "", &[]);
        assert_eq!(classify_response(&r, b"<html></html>"), PageClass::Html);
        let _ = inspect_page(&r, &Ids::default(), Route::Inst, b"\x00\xff\xfe<a href=");
    }
}
