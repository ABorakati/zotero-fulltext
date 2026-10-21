//! Publisher "WAYFless" sign-in URLs for SAML / Shibboleth (UK Access Management Federation).
//!
//! A WAYFless URL is a publisher deep link that already names the institution's identity provider (its SAML
//! `entityID`), so the reader skips the publisher's "find your institution" page and lands on the institution's own
//! sign-in. Given `(an article URL or a DOI, entityID)` this module builds that link for the platforms below. Pure and
//! total: an unsupported publisher, a malformed `entityID` or an unusable URL is `None`, never a panic.
//!
//! # Platforms and how each pattern was verified
//! Verification used unauthenticated `GET`s only (redirects followed manually up to the first identity-provider host,
//! nothing submitted, no credentials) with KCL's IdP (`https://kclidp.kcl.ac.uk/idp/shibboleth`, found with the
//! SeamlessAccess MDQ search) and the OpenAthens redirector (`go.openathens.net/redirector/kcl.ac.uk?url=`), which
//! publishes the WAYFless link it knows for a target. "chain" = our URL was followed to the identity provider's SSO
//! endpoint; "redirector" = the redirector emitted exactly this URL shape (the publisher answers non-browser clients
//! with a Cloudflare challenge, so the chain stops there). Full evidence: `docs/design/core.md` section 3.4.
//!
//! | [`Platform`] | publishers | URL | verified |
//! |---|---|---|---|
//! | `Atypon` | Wiley, Taylor & Francis, SAGE, NEJM, Science, PNAS, ASCO, EMBO Press, AHA | `https://<host>/action/ssostart?idp=E&redirectUri=T` | redirector |
//! | `Elsevier` | ScienceDirect, The Lancet, Cell Press (via the PII) | `https://auth.elsevier.com/ShibAuth/institutionLogin?entityID=E&appReturnURL=T` | chain (up to the SHIRE auto-POST form) |
//! | `Springer` | link.springer.com, springer.com | `https://fsso.springer.com/federation/init?entityId=E&returnUrl=T` | chain to the IdP |
//! | `Nature` | nature.com | `https://sp.nature.com/saml/login?idp=E&targetUrl=T` | chain to the IdP |
//! | `Oup` | academic.oup.com (incl. BJS) | `https://oup-sp.sams-sigma.com/Shibboleth.sso/Login?entityID=E&target=<sams dest chain>` | chain to the IdP |
//! | `ShibbolethSso` | JAMA Network, ACS, Emerald | `https://<host>/Shibboleth.sso/Login?entityID=E&target=T` | redirector |
//! | `Cambridge` | Cambridge Core | `https://shibboleth.cambridge.org/Shibboleth.sso/discovery?SAMLDS=1&target=<CJOShibb2>&entityID=E` | chain to the IdP |
//! | `HighWire` | BMJ journals (`*.bmj.com`) | `https://shibboleth.highwire.org/session/init?entityID=E&hw-shib-return-uri=T&subcode=bmjjournals&env=prod` | chain to the IdP |
//! | `Karger` | karger.com | `https://karger.com/ExternalAuthentication/KeystoneDeeplink?entityId=E&destination=T` | redirector |
//! | `Thieme` | thieme-connect | `https://profile.thieme.de/HTML/sso/ejournals/shibboleth.htm?federationid=http://ukfederation.org.uk&entityid=E&hook_url=T` | chain to the IdP |
//! | `AnnualReviews` | annualreviews.org | `https://www.annualreviews.org/session/ext/athens?url=T&entityId=E` | redirector |
//!
//! Not supported (no WAYFless form exists or none was observed): Wolters Kluwer / LWW (`journals.lww.com`: the
//! redirector passes the target through unchanged), Oxford-hosted books, platforms that only offer a discovery page.
//!
//! `E` is the percent-encoded `entityID`, `T` the percent-encoded, un-proxied, tracking-free, https article URL.
//! Publisher detection uses [`urls::host_key`] and [`urls::unproxify`], so an EZproxy-proxified or
//! redirector-wrapped input URL gives the same link as the plain one.
use crate::urls::{
    ProxyConf, host_key, host_of, parse_http, percent_encode_component, strip_tracking, unproxify,
};
use std::collections::BTreeMap;

/// Longest `entityID` accepted (SAML metadata entity IDs are at most 1024 bytes; real ones are far shorter).
const MAX_ENTITY_ID: usize = 512;

/// A publisher platform with a known WAYFless URL form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Atypon,
    Elsevier,
    Springer,
    Nature,
    Oup,
    ShibbolethSso,
    Cambridge,
    HighWire,
    Karger,
    Thieme,
    AnnualReviews,
}

/// Atypon sites with a `/action/ssostart` WAYFless form (host keys without the leading `www-`).
const ATYPON_KEYS: [&str; 9] = [
    "onlinelibrary-wiley-com",
    "tandfonline-com",
    "journals-sagepub-com",
    "nejm-org",
    "science-org",
    "pnas-org",
    "ascopubs-org",
    "embopress-org",
    "ahajournals-org",
];

/// Elsevier hosts (host keys without `www-`). Non-ScienceDirect hosts are re-aimed at `linkinghub.elsevier.com`.
const ELSEVIER_KEYS: [&str; 4] = [
    "sciencedirect-com",
    "linkinghub-elsevier-com",
    "thelancet-com",
    "cell-com",
];

/// `<host>/Shibboleth.sso/Login` (a Shibboleth SP running on the publisher's own host).
const SHIB_SSO_KEYS: [&str; 3] = ["jamanetwork-com", "pubs-acs-org", "emerald-com"];

/// `host_key` without a leading `www-`.
fn bare_key(host: &str) -> String {
    let key = host_key(host);
    key.strip_prefix("www-").map(str::to_owned).unwrap_or(key)
}

/// The platform that serves `host` (lower-case host, not a URL). `None` when no WAYFless form is known.
pub fn platform_of(host: &str) -> Option<Platform> {
    let key = bare_key(host);
    let key = key.as_str();
    let lower = host.to_ascii_lowercase();
    if ATYPON_KEYS.contains(&key) {
        Some(Platform::Atypon)
    } else if ELSEVIER_KEYS.contains(&key) {
        Some(Platform::Elsevier)
    } else if matches!(key, "link-springer-com" | "springer-com") {
        Some(Platform::Springer)
    } else if key == "nature-com" {
        Some(Platform::Nature)
    } else if key == "academic-oup-com" {
        Some(Platform::Oup)
    } else if SHIB_SSO_KEYS.contains(&key) {
        Some(Platform::ShibbolethSso)
    } else if key == "cambridge-org" {
        Some(Platform::Cambridge)
    } else if lower == "bmj.com" || lower.ends_with(".bmj.com") {
        Some(Platform::HighWire)
    } else if key == "karger-com" {
        Some(Platform::Karger)
    } else if matches!(key, "thieme-connect-com" | "thieme-connect-de") {
        Some(Platform::Thieme)
    } else if key == "annualreviews-org" {
        Some(Platform::AnnualReviews)
    } else {
        None
    }
}

/// A SAML `entityID` we are willing to put in a URL: an `http(s)` URL or a `urn:` name, no whitespace or control
/// characters, at most [`MAX_ENTITY_ID`] bytes.
pub fn valid_entity_id(entity_id: &str) -> bool {
    let e = entity_id.trim();
    !e.is_empty()
        && e.len() <= MAX_ENTITY_ID
        && e.len() == entity_id.len()
        && e.chars().all(|c| !c.is_whitespace() && !c.is_control())
        && (parse_http(e).is_some() || e.to_ascii_lowercase().starts_with("urn:"))
}

/// The article URL a WAYFless link will send the reader back to: un-proxied, https, tracking and fragment dropped.
/// `None` for an unparseable URL, a non-default port or an IP literal.
fn clean_target(proxy: Option<&ProxyConf<'_>>, url: &str) -> Option<(String, String)> {
    let conf = proxy.copied().unwrap_or(ProxyConf {
        login_host: "",
        suffix: "",
    });
    let plain = unproxify(&conf, url, &BTreeMap::new()).url;
    let mut parsed = parse_http(&plain)?;
    if parsed.port().is_some() {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    parsed.set_fragment(None);
    let https = parsed.as_str().replacen("http://", "https://", 1);
    Some((strip_tracking(&https), host))
}

/// Elsevier PII ("S0140673620301835") from a DOI suffix, a `PIIS...` path segment or a bare PII. Accepts the
/// punctuated forms (`S0140-6736(20)30183-5`) and any letter case.
fn pii_of(text: &str) -> Option<String> {
    let upper = text.trim().to_ascii_uppercase();
    let body = upper.strip_prefix("PII").unwrap_or(&upper);
    let compact: String = body
        .chars()
        .filter(|c| !matches!(c, '-' | '(' | ')'))
        .collect();
    let chars: Vec<char> = compact.chars().collect();
    let shape_ok = chars.len() == 17
        && chars.first() == Some(&'S')
        && chars
            .iter()
            .enumerate()
            .skip(1)
            .all(|(i, c)| c.is_ascii_digit() || (*c == 'X' && (i == 8 || i == 16)));
    shape_ok.then_some(compact)
}

/// A PII found in a URL's path segments (`.../PIIS0140-6736(20)30183-5/fulltext`, `.../pii/S0140673620301835`).
fn pii_in_url(url: &str) -> Option<String> {
    let parsed = parse_http(url)?;
    let decoded = crate::urls::percent_decode(parsed.path());
    decoded.split('/').find_map(pii_of)
}

/// `https://linkinghub.elsevier.com/retrieve/pii/<PII>`.
fn linkinghub(pii: &str) -> String {
    format!("https://linkinghub.elsevier.com/retrieve/pii/{pii}")
}

/// Build the WAYFless URL for `url` and the institution's `entity_id`.
///
/// `proxy` (the institution's EZproxy) lets a proxified input be recognised; the link always targets the real,
/// un-proxied https article URL. Returns `None` when `entity_id` is invalid, `url` is not a usable http(s) URL, or
/// the publisher has no known WAYFless form (see the module table).
pub fn wayfless_url(proxy: Option<&ProxyConf<'_>>, entity_id: &str, url: &str) -> Option<String> {
    if !valid_entity_id(entity_id) {
        return None;
    }
    let (target, host) = clean_target(proxy, url)?;
    let platform = platform_of(&host)?;
    let e = percent_encode_component(entity_id.trim());
    let t = percent_encode_component(&target);
    Some(match platform {
        Platform::Atypon => {
            format!("https://{host}/action/ssostart?idp={e}&redirectUri={t}")
        }
        Platform::Elsevier => {
            let return_to = if bare_key(&host) == "sciencedirect-com"
                || bare_key(&host) == "linkinghub-elsevier-com"
            {
                target
            } else {
                linkinghub(&pii_in_url(&target)?)
            };
            let r = percent_encode_component(&return_to);
            format!(
                "https://auth.elsevier.com/ShibAuth/institutionLogin?entityID={e}&appReturnURL={r}"
            )
        }
        Platform::Springer => {
            format!("https://fsso.springer.com/federation/init?entityId={e}&returnUrl={t}")
        }
        Platform::Nature => format!("https://sp.nature.com/saml/login?idp={e}&targetUrl={t}"),
        Platform::Oup => {
            let dest = format!(
                "https://oup-sp.sams-sigma.com/shib?dest=https://www.academic.oup.com/SHIBBOLETH?dest={target}"
            );
            format!(
                "https://oup-sp.sams-sigma.com/Shibboleth.sso/Login?entityID={e}&target={}",
                percent_encode_component(&dest)
            )
        }
        Platform::ShibbolethSso => {
            format!("https://{host}/Shibboleth.sso/Login?entityID={e}&target={t}")
        }
        Platform::Cambridge => {
            let app = format!(
                "https://shibboleth.cambridge.org/CJOShibb2/index?app=https://www.cambridge.org/core/shibboleth?ref={target}"
            );
            format!(
                "https://shibboleth.cambridge.org/Shibboleth.sso/discovery?SAMLDS=1&target={}&entityID={e}",
                percent_encode_component(&app)
            )
        }
        Platform::HighWire => format!(
            "https://shibboleth.highwire.org/session/init?entityID={e}&hw-shib-return-uri={t}&subcode=bmjjournals&env=prod"
        ),
        Platform::Karger => format!(
            "https://karger.com/ExternalAuthentication/KeystoneDeeplink?entityId={e}&destination={t}"
        ),
        Platform::Thieme => format!(
            "https://profile.thieme.de/HTML/sso/ejournals/shibboleth.htm?federationid=http%3A%2F%2Fukfederation.org.uk&entityid={e}&hook_url={t}"
        ),
        Platform::AnnualReviews => {
            format!("https://www.annualreviews.org/session/ext/athens?url={t}&entityId={e}")
        }
    })
}

/// A DOI as URL path segments (`encodeURIComponent` per segment, slashes kept).
fn doi_path(doi: &str) -> String {
    doi.split('/')
        .map(percent_encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// The publisher article URL a bare DOI stands for, for registrants whose article URL is a function of the DOI
/// alone (Atypon `/doi/<doi>` sites, Springer, Nature, Elsevier `S...` DOIs via the PII, BMJ, Thieme, Annual
/// Reviews, ACS). `None` for every other DOI (OUP, JAMA, Cambridge, Karger, LWW need more than the DOI).
pub fn target_for_doi(doi: &str) -> Option<String> {
    let doi = doi.trim().trim_start_matches("doi:").to_ascii_lowercase();
    let (prefix, suffix) = doi.split_once('/')?;
    if suffix.is_empty() {
        return None;
    }
    let path = doi_path(&doi);
    let url = match prefix {
        "10.1002" | "10.1111" => format!("https://onlinelibrary.wiley.com/doi/{path}"),
        "10.1080" => format!("https://www.tandfonline.com/doi/full/{path}"),
        "10.1177" => format!("https://journals.sagepub.com/doi/{path}"),
        "10.1056" => format!("https://www.nejm.org/doi/full/{path}"),
        "10.1126" => format!("https://www.science.org/doi/{path}"),
        "10.1073" => format!("https://www.pnas.org/doi/{path}"),
        "10.1200" => format!("https://ascopubs.org/doi/{path}"),
        "10.15252" => format!("https://www.embopress.org/doi/{path}"),
        "10.1161" => format!("https://www.ahajournals.org/doi/{path}"),
        "10.1021" => format!("https://pubs.acs.org/doi/{path}"),
        "10.1146" => format!("https://www.annualreviews.org/doi/{path}"),
        "10.1055" => format!("https://www.thieme-connect.com/products/ejournals/abstract/{path}"),
        "10.1007" if suffix.starts_with("978-") => {
            format!("https://link.springer.com/chapter/{path}")
        }
        "10.1007" => format!("https://link.springer.com/article/{path}"),
        "10.1038" => format!("https://www.nature.com/articles/{}", doi_path(suffix)),
        "10.1016" => linkinghub(&pii_of(suffix)?),
        "10.1136" if suffix.starts_with("bmj.") => format!("https://www.bmj.com/lookup/doi/{path}"),
        _ => return None,
    };
    Some(url)
}

/// The WAYFless link for an item: the first of `urls` (a reached publisher page, the item's own URL) that belongs to
/// a supported platform, else the link for the article the DOI stands for ([`target_for_doi`]).
pub fn wayfless_for(
    proxy: Option<&ProxyConf<'_>>,
    entity_id: &str,
    doi: Option<&str>,
    urls: &[&str],
) -> Option<String> {
    urls.iter()
        .find_map(|u| wayfless_url(proxy, entity_id, u))
        .or_else(|| {
            let target = target_for_doi(doi?)?;
            wayfless_url(proxy, entity_id, &target)
        })
}

/// True when `url` is on a platform [`wayfless_url`] supports (after un-proxying).
pub fn is_supported(proxy: Option<&ProxyConf<'_>>, url: &str) -> bool {
    let conf = proxy.copied().unwrap_or(ProxyConf {
        login_host: "",
        suffix: "",
    });
    let plain = unproxify(&conf, url, &BTreeMap::new()).url;
    host_of(&plain).is_some_and(|h| platform_of(&h).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KCL: &str = "https://kclidp.kcl.ac.uk/idp/shibboleth";
    const KCL_E: &str = "https%3A%2F%2Fkclidp.kcl.ac.uk%2Fidp%2Fshibboleth";

    fn url(u: &str) -> String {
        wayfless_url(None, KCL, u).unwrap_or_default()
    }

    fn kcl_proxy() -> ProxyConf<'static> {
        ProxyConf {
            login_host: "login.kcl.idm.oclc.org",
            suffix: "kcl.idm.oclc.org",
        }
    }

    // Each expectation below is the URL the OpenAthens redirector emitted for KCL on 2026-10-21 (see the module docs).
    #[test]
    fn atypon_matches_the_redirector_output() {
        assert_eq!(
            url("https://onlinelibrary.wiley.com/doi/10.1002/bjs.11646"),
            format!(
                "https://onlinelibrary.wiley.com/action/ssostart?idp={KCL_E}&redirectUri=https%3A%2F%2Fonlinelibrary.wiley.com%2Fdoi%2F10.1002%2Fbjs.11646"
            )
        );
        assert_eq!(
            url("https://www.nejm.org/doi/full/10.1056/NEJMoa2034577"),
            format!(
                "https://www.nejm.org/action/ssostart?idp={KCL_E}&redirectUri=https%3A%2F%2Fwww.nejm.org%2Fdoi%2Ffull%2F10.1056%2FNEJMoa2034577"
            )
        );
        assert_eq!(
            url("https://journals.sagepub.com/doi/10.1177/0956797613504966"),
            format!(
                "https://journals.sagepub.com/action/ssostart?idp={KCL_E}&redirectUri=https%3A%2F%2Fjournals.sagepub.com%2Fdoi%2F10.1177%2F0956797613504966"
            )
        );
        assert_eq!(
            url("https://www.tandfonline.com/doi/abs/10.1080/00031305.2016.1154108"),
            format!(
                "https://www.tandfonline.com/action/ssostart?idp={KCL_E}&redirectUri=https%3A%2F%2Fwww.tandfonline.com%2Fdoi%2Fabs%2F10.1080%2F00031305.2016.1154108"
            )
        );
        for host in [
            "www.science.org",
            "www.pnas.org",
            "ascopubs.org",
            "www.embopress.org",
            "www.ahajournals.org",
        ] {
            let u = url(&format!("https://{host}/doi/10.1/x"));
            assert!(
                u.starts_with(&format!("https://{host}/action/ssostart?idp=")),
                "{u}"
            );
        }
    }

    #[test]
    fn elsevier_uses_the_page_or_the_linkinghub_pii() {
        assert_eq!(
            url("https://www.sciencedirect.com/science/article/pii/S0140673620301835"),
            format!(
                "https://auth.elsevier.com/ShibAuth/institutionLogin?entityID={KCL_E}&appReturnURL=https%3A%2F%2Fwww.sciencedirect.com%2Fscience%2Farticle%2Fpii%2FS0140673620301835"
            )
        );
        // thelancet.com / cell.com pages carry the PII in the path; the link goes through linkinghub.
        let expected = format!(
            "https://auth.elsevier.com/ShibAuth/institutionLogin?entityID={KCL_E}&appReturnURL=https%3A%2F%2Flinkinghub.elsevier.com%2Fretrieve%2Fpii%2FS0140673620301835"
        );
        assert_eq!(
            url(
                "https://www.thelancet.com/journals/lancet/article/PIIS0140-6736(20)30183-5/fulltext"
            ),
            expected
        );
        assert_eq!(
            url("https://linkinghub.elsevier.com/retrieve/pii/S0140673620301835"),
            format!(
                "https://auth.elsevier.com/ShibAuth/institutionLogin?entityID={KCL_E}&appReturnURL=https%3A%2F%2Flinkinghub.elsevier.com%2Fretrieve%2Fpii%2FS0140673620301835"
            )
        );
        // No PII anywhere: nothing safe to return to.
        assert_eq!(
            wayfless_url(None, KCL, "https://www.cell.com/cell/home"),
            None
        );
    }

    #[test]
    fn springer_and_nature() {
        assert_eq!(
            url("https://link.springer.com/article/10.1007/s00134-020-06022-5"),
            format!(
                "https://fsso.springer.com/federation/init?entityId={KCL_E}&returnUrl=https%3A%2F%2Flink.springer.com%2Farticle%2F10.1007%2Fs00134-020-06022-5"
            )
        );
        assert_eq!(
            url("https://www.nature.com/articles/nature12373"),
            format!(
                "https://sp.nature.com/saml/login?idp={KCL_E}&targetUrl=https%3A%2F%2Fwww.nature.com%2Farticles%2Fnature12373"
            )
        );
    }

    #[test]
    fn oup_nests_the_target_in_the_sams_sigma_chain() {
        assert_eq!(
            url("https://academic.oup.com/bjs/article/107/9/1097/6120694"),
            format!(
                "https://oup-sp.sams-sigma.com/Shibboleth.sso/Login?entityID={KCL_E}&target=https%3A%2F%2Foup-sp.sams-sigma.com%2Fshib%3Fdest%3Dhttps%3A%2F%2Fwww.academic.oup.com%2FSHIBBOLETH%3Fdest%3Dhttps%3A%2F%2Facademic.oup.com%2Fbjs%2Farticle%2F107%2F9%2F1097%2F6120694"
            )
        );
    }

    #[test]
    fn shibboleth_sso_hosts_cambridge_and_highwire() {
        assert_eq!(
            url("https://jamanetwork.com/journals/jama/fullarticle/2762130"),
            format!(
                "https://jamanetwork.com/Shibboleth.sso/Login?entityID={KCL_E}&target=https%3A%2F%2Fjamanetwork.com%2Fjournals%2Fjama%2Ffullarticle%2F2762130"
            )
        );
        assert_eq!(
            url("https://pubs.acs.org/doi/10.1021/jacs.9b00001"),
            format!(
                "https://pubs.acs.org/Shibboleth.sso/Login?entityID={KCL_E}&target=https%3A%2F%2Fpubs.acs.org%2Fdoi%2F10.1021%2Fjacs.9b00001"
            )
        );
        assert_eq!(
            url("https://www.cambridge.org/core/journals/psychological-medicine/article/x/AAAA"),
            format!(
                "https://shibboleth.cambridge.org/Shibboleth.sso/discovery?SAMLDS=1&target=https%3A%2F%2Fshibboleth.cambridge.org%2FCJOShibb2%2Findex%3Fapp%3Dhttps%3A%2F%2Fwww.cambridge.org%2Fcore%2Fshibboleth%3Fref%3Dhttps%3A%2F%2Fwww.cambridge.org%2Fcore%2Fjournals%2Fpsychological-medicine%2Farticle%2Fx%2FAAAA&entityID={KCL_E}"
            )
        );
        for host in ["www.bmj.com", "bmjopen.bmj.com", "gut.bmj.com"] {
            let u = url(&format!("https://{host}/content/10/1/e033324"));
            assert_eq!(
                u,
                format!(
                    "https://shibboleth.highwire.org/session/init?entityID={KCL_E}&hw-shib-return-uri=https%3A%2F%2F{host}%2Fcontent%2F10%2F1%2Fe033324&subcode=bmjjournals&env=prod"
                )
            );
        }
    }

    #[test]
    fn karger_thieme_and_annual_reviews() {
        assert_eq!(
            url("https://karger.com/gas/article/doi/10.1159/000501234"),
            format!(
                "https://karger.com/ExternalAuthentication/KeystoneDeeplink?entityId={KCL_E}&destination=https%3A%2F%2Fkarger.com%2Fgas%2Farticle%2Fdoi%2F10.1159%2F000501234"
            )
        );
        assert_eq!(
            url(
                "https://www.thieme-connect.com/products/ejournals/abstract/10.1055/s-0039-1696687"
            ),
            format!(
                "https://profile.thieme.de/HTML/sso/ejournals/shibboleth.htm?federationid=http%3A%2F%2Fukfederation.org.uk&entityid={KCL_E}&hook_url=https%3A%2F%2Fwww.thieme-connect.com%2Fproducts%2Fejournals%2Fabstract%2F10.1055%2Fs-0039-1696687"
            )
        );
        assert_eq!(
            url("https://www.annualreviews.org/doi/10.1146/annurev-med-050619-024003"),
            format!(
                "https://www.annualreviews.org/session/ext/athens?url=https%3A%2F%2Fwww.annualreviews.org%2Fdoi%2F10.1146%2Fannurev-med-050619-024003&entityId={KCL_E}"
            )
        );
    }

    #[test]
    fn unsupported_publishers_have_no_link() {
        // LWW: the redirector passes the target through unchanged, so no WAYFless form exists.
        assert_eq!(
            wayfless_url(
                None,
                KCL,
                "https://journals.lww.com/annalsofsurgery/fulltext/2022/11000/x.aspx"
            ),
            None
        );
        assert_eq!(wayfless_url(None, KCL, "https://example.org/a"), None);
        assert_eq!(
            wayfless_url(None, KCL, "https://doi.org/10.1136/bmj.n71"),
            None
        );
        assert_eq!(wayfless_url(None, KCL, "not a url"), None);
        assert_eq!(
            wayfless_url(None, KCL, "ftp://www.nature.com/articles/x"),
            None
        );
        // IP literals and non-default ports never become a return target.
        assert_eq!(
            wayfless_url(None, KCL, "https://127.0.0.1/doi/10.1/x"),
            None
        );
        assert_eq!(
            wayfless_url(None, KCL, "https://www.nature.com:8443/articles/x"),
            None
        );
        // Look-alike hosts are not the publisher.
        assert_eq!(wayfless_url(None, KCL, "https://evil-bmj.com/x"), None);
        assert_eq!(
            wayfless_url(None, KCL, "https://nature.com.evil.org/x"),
            None
        );
        assert_eq!(platform_of("fake-nejm.org"), None);
    }

    #[test]
    fn entity_ids_are_validated() {
        let long = format!("https://{}.example.org/idp", "a".repeat(600));
        for bad in [
            "",
            "   ",
            " https://idp.example.org/idp ",
            "idp.example.org",
            "https://idp.example.org/ idp",
            "ftp://idp.example.org/idp",
            "javascript:alert(1)",
            long.as_str(),
        ] {
            assert!(!valid_entity_id(bad), "{bad:?}");
            assert_eq!(
                wayfless_url(None, bad, "https://www.nature.com/articles/x"),
                None
            );
        }
        for good in [
            "https://kclidp.kcl.ac.uk/idp/shibboleth",
            "https://registry.shibboleth.ox.ac.uk/idp",
            "https://shibboleth.imperial.ac.uk/shibboleth",
            "https://idp.mcsoxford.org/entity",
            "urn:mace:incommon:example.edu",
        ] {
            assert!(valid_entity_id(good), "{good}");
        }
        // The entityID is percent-encoded like any parameter value.
        let u = wayfless_url(
            None,
            "https://idp.example.org/idp?a=b&c=d",
            "https://www.nature.com/articles/x",
        )
        .unwrap_or_default();
        assert!(
            u.contains("idp=https%3A%2F%2Fidp.example.org%2Fidp%3Fa%3Db%26c%3Dd&"),
            "{u}"
        );
    }

    #[test]
    fn proxied_and_wrapped_inputs_give_the_plain_link() {
        let plain = url("https://www.nature.com/articles/nature12373");
        let p = kcl_proxy();
        for input in [
            // HttpsHyphens proxified host
            "https://www-nature-com.kcl.idm.oclc.org/articles/nature12373",
            // EZproxy login?qurl=
            "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fwww.nature.com%2Farticles%2Fnature12373",
            // EZproxy login?url= (raw)
            "https://login.kcl.idm.oclc.org/login?url=https://www.nature.com/articles/nature12373",
            // OpenAthens redirector
            "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fwww.nature.com%2Farticles%2Fnature12373",
            // http and a fragment / tracking parameters
            "http://www.nature.com/articles/nature12373?utm_source=x#section",
        ] {
            assert_eq!(
                wayfless_url(Some(&p), KCL, input).as_deref(),
                Some(plain.as_str()),
                "{input}"
            );
        }
        // Without the EZproxy settings a proxified host is just an unknown host.
        assert_eq!(
            wayfless_url(
                None,
                KCL,
                "https://www-nature-com.kcl.idm.oclc.org/articles/nature12373"
            ),
            None
        );
        assert!(is_supported(
            Some(&p),
            "https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71"
        ));
        assert!(!is_supported(
            Some(&p),
            "https://journals-lww-com.kcl.idm.oclc.org/x"
        ));
    }

    #[test]
    fn query_strings_survive_and_tracking_is_dropped() {
        let u = url(
            "https://www.nejm.org/doi/full/10.1056/NEJMoa2034577?query=featured_home&utm_medium=email",
        );
        assert!(
            u.ends_with("redirectUri=https%3A%2F%2Fwww.nejm.org%2Fdoi%2Ffull%2F10.1056%2FNEJMoa2034577%3Fquery%3Dfeatured_home"),
            "{u}"
        );
    }

    #[test]
    fn dois_stand_in_for_urls_on_known_registrants() {
        let cases = [
            (
                "10.1002/bjs.11646",
                "https://onlinelibrary.wiley.com/action/ssostart?idp=",
                "redirectUri=https%3A%2F%2Fonlinelibrary.wiley.com%2Fdoi%2F10.1002%2Fbjs.11646",
            ),
            (
                "10.1080/00031305.2016.1154108",
                "https://www.tandfonline.com/action/ssostart?idp=",
                "redirectUri=https%3A%2F%2Fwww.tandfonline.com%2Fdoi%2Ffull%2F10.1080%2F00031305.2016.1154108",
            ),
            (
                "10.1007/s00134-020-06022-5",
                "https://fsso.springer.com/federation/init?entityId=",
                "returnUrl=https%3A%2F%2Flink.springer.com%2Farticle%2F10.1007%2Fs00134-020-06022-5",
            ),
            (
                "10.1038/nature12373",
                "https://sp.nature.com/saml/login?idp=",
                "targetUrl=https%3A%2F%2Fwww.nature.com%2Farticles%2Fnature12373",
            ),
            (
                "10.1016/S0140-6736(20)30183-5",
                "https://auth.elsevier.com/ShibAuth/institutionLogin?entityID=",
                "appReturnURL=https%3A%2F%2Flinkinghub.elsevier.com%2Fretrieve%2Fpii%2FS0140673620301835",
            ),
            (
                "10.1136/bmj.n71",
                "https://shibboleth.highwire.org/session/init?entityID=",
                "hw-shib-return-uri=https%3A%2F%2Fwww.bmj.com%2Flookup%2Fdoi%2F10.1136%2Fbmj.n71&subcode=bmjjournals&env=prod",
            ),
            (
                "10.1021/jacs.9b00001",
                "https://pubs.acs.org/Shibboleth.sso/Login?entityID=",
                "target=https%3A%2F%2Fpubs.acs.org%2Fdoi%2F10.1021%2Fjacs.9b00001",
            ),
        ];
        for (doi, start, end) in cases {
            let u = wayfless_for(None, KCL, Some(doi), &[]).unwrap_or_default();
            assert!(u.starts_with(start) && u.ends_with(end), "{doi}: {u}");
        }
        // DOIs that are not a function of the registrant alone.
        for doi in [
            "10.1093/bjs/znab001",
            "10.1001/jama.2020.1585",
            "10.1017/s0033291719003192",
            "10.1097/sla.0000000000005603",
            "10.1016/j.cell.2020.01.001",
            "10.1136/gutjnl-2019-319000",
            "10.9999/x",
            "10.1002",
            "",
        ] {
            assert_eq!(wayfless_for(None, KCL, Some(doi), &[]), None, "{doi}");
        }
        // Books on Springer.
        assert_eq!(
            target_for_doi("10.1007/978-3-030-00001-1_1").as_deref(),
            Some("https://link.springer.com/chapter/10.1007/978-3-030-00001-1_1")
        );
        // Upper-case and prefixed DOIs.
        assert_eq!(
            target_for_doi("doi:10.1038/NATURE12373").as_deref(),
            Some("https://www.nature.com/articles/nature12373")
        );
    }

    #[test]
    fn a_reached_page_beats_the_doi_and_unsupported_urls_fall_through() {
        let got = wayfless_for(
            None,
            KCL,
            Some("10.1038/nature12373"),
            &[
                "https://example.org/pubmed/1",
                "https://academic.oup.com/bjs/article/1/2/3/4",
            ],
        )
        .unwrap_or_default();
        assert!(got.starts_with("https://oup-sp.sams-sigma.com/"), "{got}");
        let via_doi = wayfless_for(
            None,
            KCL,
            Some("10.1038/nature12373"),
            &["https://example.org/p"],
        )
        .unwrap_or_default();
        assert!(
            via_doi.starts_with("https://sp.nature.com/saml/login"),
            "{via_doi}"
        );
    }

    #[test]
    fn pii_shapes() {
        assert_eq!(
            pii_of("S0140-6736(20)30183-5").as_deref(),
            Some("S0140673620301835")
        );
        assert_eq!(
            pii_of("s0140-6736(20)30183-5").as_deref(),
            Some("S0140673620301835")
        );
        assert_eq!(
            pii_of("PIIS0140-6736(20)30183-5").as_deref(),
            Some("S0140673620301835")
        );
        assert_eq!(
            pii_of("S0140673620301835").as_deref(),
            Some("S0140673620301835")
        );
        assert_eq!(pii_of("S0140673620301X35"), None);
        assert_eq!(
            pii_of("S1470204520300001").as_deref(),
            Some("S1470204520300001")
        );
        assert_eq!(
            pii_of("S014067362030183X").as_deref(),
            Some("S014067362030183X")
        );
        assert_eq!(pii_of("j.cell.2020.01.001"), None);
        assert_eq!(pii_of(""), None);
    }

    #[test]
    fn total_on_odd_input() {
        for s in [
            "",
            "https://",
            "https://www.nature.com",
            "https://www.nature.com/",
            "http://[::1]/x",
            "https://www.nature.com/%zz%ff",
            "\u{0}\u{1}",
        ] {
            let _ = wayfless_url(None, KCL, s);
            let _ = target_for_doi(s);
            let _ = wayfless_for(None, s, Some(s), &[s]);
        }
    }
}
