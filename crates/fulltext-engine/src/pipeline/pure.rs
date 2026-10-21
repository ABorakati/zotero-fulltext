//! Pure helpers the pipeline needs, as one adapter over `fulltext_core` so that no other pipeline file imports
//! core's URL / id modules directly (a rename in core touches only this file). Everything here is a thin
//! forward, except `storable_url` (engine policy built from core's primitives).
use crate::proto::Institution;
use fulltext_core::urls::{self, ProxyConf};
use fulltext_core::{ids, model::*, oa, wayfless};
use std::collections::BTreeMap;

/// Identifiers of an item: DOI / PMID / PMCID / arXiv from fields and Extra, `rxiv`, the item URL.
pub(crate) fn normalise_ids(snap: &ItemSnapshot) -> Ids {
    ids::normalise(&snap.fields)
}

/// Fold identifiers a source taught us into `ids` (never overwrites a known id). True when something was new.
pub(crate) fn apply_learned(known: &mut Ids, learned: &LearnedIds, source: &str) -> bool {
    ids::apply_learned(known, learned, source)
}

/// Case-insensitive DOI comparison (DOIs come back lower-cased from several APIs).
pub(crate) fn same_doi(a: &str, b: &str) -> bool {
    ids::same_doi(a, b)
}

/// Bare, lower-case DOI from any written form (`https://doi.org/10.1/X` -> `10.1/x`).
pub(crate) fn clean_doi(raw: &str) -> Option<String> {
    ids::clean_doi(raw)
}

/// `https://doi.org/<doi>` (path segments percent-encoded).
pub(crate) fn doi_url(doi: &str) -> String {
    urls::doi_url(doi)
}

/// Lower-case host of an http(s) URL.
pub(crate) fn host_of(url: &str) -> Option<String> {
    urls::host_of(url)
}

/// Dedupe key of a URL (https, lower-case host, no fragment, query kept).
pub(crate) fn canonical_key(url: &str) -> String {
    urls::canonical_key(url)
}

/// `encodeURIComponent`.
pub(crate) fn pct_encode(s: &str) -> String {
    urls::percent_encode_component(s)
}

/// A DOI as a URL path: every `/`-separated segment percent-encoded, the slashes kept
/// (`10.1002/(sici)1097` -> `10.1002/(sici)1097`; `10.1/a b` -> `10.1/a%20b`).
pub(crate) fn doi_path(doi: &str) -> String {
    doi.split('/').map(pct_encode).collect::<Vec<_>>().join("/")
}

/// What kind of host serves `url` (publisher / repository / preprint / PMC / aggregator / other).
pub(crate) fn host_kind(url: &str) -> HostKind {
    oa::host_kind(url)
}

/// The candidates one Zotero OA hit stands for (`url` as a PDF, `page_url` as a landing page).
pub(crate) fn zotero_hit_candidates(
    url: &str,
    page_url: Option<&str>,
    version: Option<&str>,
) -> Vec<Candidate> {
    oa::zotero_oa::candidates_from_hit(url, page_url, version)
}

/// OpenAthens redirector deep link for `target` (must be a DOI URL; `thelancet.com` URLs 404). `None` when the
/// institution has no OpenAthens domain: an empty-domain redirector URL is never built.
pub(crate) fn redirector_url(inst: &Institution, target: &str) -> Option<String> {
    let domain = inst.openathens_domain.trim();
    (!domain.is_empty()).then(|| urls::redirector_url(domain, target))
}

/// The institution's SAML IdP `entityID`, when set and well formed.
pub(crate) fn saml_entity(inst: &Institution) -> Option<&str> {
    inst.saml_entity_id
        .as_deref()
        .map(str::trim)
        .filter(|e| wayfless::valid_entity_id(e))
}

/// The institution's EZproxy settings as core's [`ProxyConf`]; `None` when it has no (usable) EZproxy.
fn proxy_conf(inst: &Institution) -> Option<ProxyConf<'_>> {
    inst.ezproxy
        .as_ref()
        .filter(|e| !e.login_host.trim().is_empty())
        .map(|e| ProxyConf {
            login_host: &e.login_host,
            suffix: &e.suffix,
        })
}

/// Publisher WAYFless sign-in link for the first of `urls` that is on a supported platform, else for the article the
/// DOI stands for. `None` without an `entityID` or for an unsupported publisher.
pub(crate) fn wayfless_url(inst: &Institution, doi: Option<&str>, urls: &[&str]) -> Option<String> {
    wayfless::wayfless_for(proxy_conf(inst).as_ref(), saml_entity(inst)?, doi, urls)
}

/// True when the institution can use the OpenAthens redirector (a non-blank domain).
pub(crate) fn has_openathens(inst: &Institution) -> bool {
    !inst.openathens_domain.trim().is_empty()
}

/// True when the institution has a usable EZproxy (a login host and a suffix).
pub(crate) fn has_ezproxy(inst: &Institution) -> bool {
    inst.ezproxy
        .as_ref()
        .is_some_and(|e| !e.login_host.trim().is_empty() && !e.suffix.trim().is_empty())
}

/// EZproxy `login?qurl=` entry for `target`. `None` when the institution has no EZproxy.
pub(crate) fn ezproxy_login_url(inst: &Institution, target: &str) -> Option<String> {
    inst.ezproxy
        .as_ref()
        .filter(|e| !e.login_host.trim().is_empty())
        .map(|e| urls::ezproxy_login_url(e.login_host.trim(), target))
}

/// Direct proxified URL (`https://<host-with-hyphens>.<suffix>/...`) when the host can be proxified.
pub(crate) fn proxify(inst: &Institution, target: &str) -> Option<String> {
    let e = inst.ezproxy.as_ref()?;
    urls::proxify(
        &ProxyConf {
            login_host: &e.login_host,
            suffix: &e.suffix,
        },
        target,
    )
}

/// Undo proxification / login URLs / the redirector. `learned` maps proxied host labels to real hosts. An
/// institution without EZproxy still gets the OpenAthens redirector unwrapped (empty proxy settings match no host).
pub(crate) fn unproxify(
    inst: &Institution,
    url: &str,
    learned: &BTreeMap<String, String>,
) -> String {
    let (login_host, suffix) = match inst.ezproxy.as_ref() {
        Some(e) => (e.login_host.as_str(), e.suffix.as_str()),
        None => ("", ""),
    };
    urls::unproxify(&ProxyConf { login_host, suffix }, url, learned).url
}

/// Classify a URL for `BrowserReq.hold_while` and for deciding whether a landing is a login page.
pub(crate) fn url_class(inst: &Institution, url: &str) -> UrlClassKind {
    let conf = inst.ezproxy.as_ref().map(|e| ProxyConf {
        login_host: &e.login_host,
        suffix: &e.suffix,
    });
    let class = urls::url_class(conf.as_ref(), url);
    if class == UrlClassKind::Other && host_of(url).is_some_and(|h| is_idp_host(inst, &h)) {
        return UrlClassKind::IdpLogin;
    }
    class
}

/// A host the institution lists as its sign-in host (`Institution.idp_hosts`, exact match).
fn is_idp_host(inst: &Institution, host: &str) -> bool {
    inst.idp_hosts
        .iter()
        .any(|h| !h.trim().is_empty() && h.trim().eq_ignore_ascii_case(host))
}

/// True when `url` is on the institution's EZproxy (the login host or a proxied `<label>.<suffix>` host).
pub(crate) fn is_ezproxy_url(inst: &Institution, url: &str) -> bool {
    let (Some(e), Some(host)) = (inst.ezproxy.as_ref(), host_of(url)) else {
        return false;
    };
    let suffix = e.suffix.trim().trim_matches('.').to_ascii_lowercase();
    host == e.login_host.trim().to_ascii_lowercase()
        || (!suffix.is_empty() && host.ends_with(&format!(".{suffix}")))
}

// ------------------------------------------------------------------------------------------ storable_url

/// Hosts that serve short-lived signed copies; their URLs are never stored.
const SIGNED_ASSET_HOSTS: &[&str] = &["sciencedirectassets.com"];

/// Query parameters (lower-case) that carry a signature or credential: a URL with one is never stored as is.
const TOKEN_PARAMS: &[&str] = &[
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "x-amz-date",
    "x-amz-expires",
    "x-amz-algorithm",
    "x-amz-signedheaders",
    "awsaccesskeyid",
    "signature",
    "sig",
    "se",
    "sp",
    "sv",
    "token",
    "access_token",
    "api_key",
    "apikey",
    "hdnea",
    "expires",
    "x-goog-signature",
];

fn host_has_suffix(host: &str, suffix: &str) -> bool {
    host == suffix
        || host
            .strip_suffix(suffix)
            .is_some_and(|head| head.ends_with('.'))
}

/// `(base, query, fragment)` of a URL, without allocating.
fn split_url(url: &str) -> (&str, Option<&str>, Option<&str>) {
    let (before, fragment) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    match before.split_once('?') {
        Some((base, query)) => (base, Some(query), fragment),
        None => (before, None, fragment),
    }
}

fn is_token_pair(pair: &str) -> bool {
    let name = pair
        .split('=')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    TOKEN_PARAMS.contains(&name.as_str())
}

fn has_token_params(url: &str) -> bool {
    split_url(url)
        .1
        .is_some_and(|q| q.split('&').any(is_token_pair))
}

/// `url` without its signature / credential parameters (all other pairs byte-for-byte, in order).
fn strip_token_params(url: &str) -> String {
    let (base, query, fragment) = split_url(url);
    let mut out = String::from(base);
    if let Some(q) = query {
        let kept: Vec<&str> = q
            .split('&')
            .filter(|pair| !pair.is_empty() && !is_token_pair(pair))
            .collect();
        if !kept.is_empty() {
            out.push('?');
            out.push_str(&kept.join("&"));
        }
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// May this (already un-proxied, tracking-free) URL be stored on an attachment?
fn is_storable(inst: &Institution, url: &str) -> bool {
    let Some(host) = host_of(url) else {
        return false;
    };
    !SIGNED_ASSET_HOSTS.iter().any(|h| host_has_suffix(&host, h))
        && !has_token_params(url)
        && url_class(inst, url) == UrlClassKind::Other
        && !is_ezproxy_url(inst, url)
}

/// URL stored on the attachment: the un-proxied, tracking-free article URL; never `login?`, redirector or SAML
/// URLs. Signed-asset hosts (`pdf.sciencedirectassets.com`, `X-Amz-*` / token query params) fall back to the
/// landing URL, then to the DOI URL, then to the item's own URL; as a last resort the candidate URL is stored
/// with its token parameters removed.
pub(crate) fn storable_url(
    candidate_url: &str,
    landing_url: Option<&str>,
    ids: &Ids,
    inst: &Institution,
) -> String {
    let no_learned = BTreeMap::new();
    let clean = |u: &str| urls::strip_tracking(&unproxify(inst, u, &no_learned));
    let candidate = clean(candidate_url);
    if is_storable(inst, &candidate) {
        return candidate;
    }
    if let Some(landing) = landing_url.map(clean).filter(|l| is_storable(inst, l)) {
        return landing;
    }
    if let Some(doi) = ids.doi.as_deref() {
        return urls::doi_url(doi);
    }
    if let Some(own) = ids
        .url
        .as_deref()
        .map(clean)
        .filter(|u| is_storable(inst, u))
    {
        return own;
    }
    strip_token_params(&candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Ezproxy;

    fn kcl() -> Institution {
        Institution {
            name: "King's College London".into(),
            openathens_domain: "kcl.ac.uk".into(),
            ezproxy: Some(Ezproxy {
                login_host: "login.kcl.idm.oclc.org".into(),
                suffix: "kcl.idm.oclc.org".into(),
            }),
            ezproxy_hosts: vec![],
            ezproxy_all_hosts: false,
            saml_entity_id: None,
            alma: None,
            libkey_library_id: Some(286),
            idp_hosts: vec![],
            entra_tenant: None,
            passthrough_hosts: vec![],
            warmup_url: String::new(),
        }
    }

    fn ids_with(doi: Option<&str>, url: Option<&str>) -> Ids {
        Ids {
            doi: doi.map(str::to_owned),
            url: url.map(str::to_owned),
            ..Ids::default()
        }
    }

    #[test]
    fn doi_path_keeps_slashes_and_encodes_the_rest() {
        assert_eq!(
            doi_path("10.1371/journal.pmed.1000097"),
            "10.1371/journal.pmed.1000097"
        );
        assert_eq!(doi_path("10.1/a b"), "10.1/a%20b");
        assert_eq!(
            doi_path("10.1002/(sici)1097-0258;2-i"),
            "10.1002/(sici)1097-0258%3B2-i"
        );
    }

    #[test]
    fn proxied_candidate_urls_are_stored_un_proxied() {
        let inst = kcl();
        let none = ids_with(None, None);
        // research/04 section 6.4 vectors
        assert_eq!(
            storable_url(
                "https://doi-org.kcl.idm.oclc.org/10.1038/nature12373",
                None,
                &none,
                &inst
            ),
            "https://doi.org/10.1038/nature12373"
        );
        assert_eq!(
            storable_url(
                "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fwww.bmj.com%2Fcontent%2F372%2Fbmj.n71",
                None,
                &none,
                &inst
            ),
            "https://www.bmj.com/content/372/bmj.n71"
        );
        assert_eq!(
            storable_url(
                "https://login.kcl.idm.oclc.org/login?url=https://www.bmj.com/x?a=1&b=2",
                None,
                &none,
                &inst
            ),
            "https://www.bmj.com/x?a=1&b=2"
        );
        assert_eq!(
            storable_url(
                "https://WWW-BMJ-COM.kcl.idm.oclc.org/content/372/bmj.n71.full.pdf?utm_source=x&fbclid=y",
                None,
                &none,
                &inst
            ),
            "https://www.bmj.com/content/372/bmj.n71.full.pdf"
        );
        assert_eq!(
            storable_url(
                "https://www-thieme-connect-de.kcl.idm.oclc.org/products/ejournals/pdf/10.1055/s-0041-1.pdf",
                None,
                &none,
                &inst
            ),
            "https://www.thieme-connect.de/products/ejournals/pdf/10.1055/s-0041-1.pdf"
        );
    }

    #[test]
    fn redirector_links_are_unwrapped_even_without_ezproxy() {
        let mut inst = kcl();
        inst.ezproxy = None;
        let wrapped = "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646";
        assert_eq!(
            storable_url(wrapped, None, &ids_with(None, None), &inst),
            "https://doi.org/10.1002/bjs.11646"
        );
    }

    #[test]
    fn a_clean_open_access_url_is_kept_even_when_a_doi_is_known() {
        let inst = kcl();
        let ids = ids_with(Some("10.1371/journal.pmed.1000097"), None);
        let pmc = "https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf";
        assert_eq!(storable_url(pmc, None, &ids, &inst), pmc);
    }

    #[test]
    fn signed_assets_fall_back_to_landing_then_doi_then_item_url() {
        let inst = kcl();
        let signed = "https://pdf.sciencedirectassets.com/271095/1-s2.0-S0140673620X-main.pdf?X-Amz-Security-Token=abc&X-Amz-Signature=def";
        let landing = "https://www.sciencedirect.com/science/article/pii/S0140673620X";
        let ids = ids_with(
            Some("10.1016/s0140-6736(20)x"),
            Some("https://example.org/own"),
        );
        assert_eq!(storable_url(signed, Some(landing), &ids, &inst), landing);
        assert_eq!(
            storable_url(signed, None, &ids, &inst),
            "https://doi.org/10.1016/s0140-6736(20)x"
        );
        let no_doi = ids_with(None, Some("https://example.org/own"));
        assert_eq!(
            storable_url(signed, None, &no_doi, &inst),
            "https://example.org/own"
        );
        // a signed landing URL is not good enough either
        let signed_landing = "https://s3.amazonaws.com/b/k.html?X-Amz-Signature=zz";
        assert_eq!(
            storable_url(
                signed,
                Some(signed_landing),
                &ids_with(Some("10.1/x"), None),
                &inst
            ),
            "https://doi.org/10.1/x"
        );
    }

    #[test]
    fn token_parameters_are_the_last_resort_and_are_removed() {
        let inst = kcl();
        let signed = "https://cdn.example.org/a.pdf?id=7&X-Amz-Signature=zz&download=1";
        assert_eq!(
            storable_url(signed, None, &ids_with(None, None), &inst),
            "https://cdn.example.org/a.pdf?id=7&download=1"
        );
    }

    #[test]
    fn login_and_sso_urls_are_never_stored() {
        let inst = kcl();
        let ids = ids_with(Some("10.1/x"), None);
        for bad in [
            "https://login.microsoftonline.com/8370cf14/saml2?SAMLRequest=abc",
            "https://login.kcl.idm.oclc.org/menu",
            "https://onlinelibrary.wiley.com/action/ssostart?idp=x&redirectUri=y",
        ] {
            assert_eq!(
                storable_url(bad, None, &ids, &inst),
                "https://doi.org/10.1/x",
                "{bad}"
            );
        }
    }

    #[test]
    fn ezproxy_detection() {
        let inst = kcl();
        assert!(is_ezproxy_url(
            &inst,
            "https://www-bmj-com.kcl.idm.oclc.org/x"
        ));
        assert!(is_ezproxy_url(
            &inst,
            "https://login.kcl.idm.oclc.org/login"
        ));
        assert!(!is_ezproxy_url(&inst, "https://www.bmj.com/x"));
        let mut none = kcl();
        none.ezproxy = None;
        assert!(!is_ezproxy_url(
            &none,
            "https://login.kcl.idm.oclc.org/login"
        ));
    }

    #[test]
    fn forwards_agree_with_core() {
        assert_eq!(
            canonical_key("http://Example.org:80/p?a=1#f"),
            "https://example.org/p?a=1"
        );
        assert!(same_doi(
            "10.1371/JOURNAL.pmed.1000097",
            "10.1371/journal.pmed.1000097"
        ));
        assert_eq!(
            clean_doi("https://doi.org/10.1002/BJS.11646").as_deref(),
            Some("10.1002/bjs.11646")
        );
        assert_eq!(
            host_kind("https://www.nature.com/articles/n.pdf"),
            HostKind::Publisher
        );
        let hit = zotero_hit_candidates(
            "https://arxiv.org/pdf/1304.1068",
            Some("http://arxiv.org/abs/1304.1068"),
            Some("submittedVersion"),
        );
        assert_eq!(
            hit.first().map(|c| (c.kind, c.version)),
            Some((CandKind::Pdf, Version::Submitted))
        );
    }
}
