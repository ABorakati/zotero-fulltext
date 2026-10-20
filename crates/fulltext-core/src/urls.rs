//! URL helpers: host keys, institutional URL builders, EZproxy proxify / unproxify, URL classification.
//! Owned by core-pdf-urls-inspect (spec: `docs/wp/core-pdf-urls-inspect.md`). The small shared primitives at the top
//! are final: their signatures are relied on by other packages. Everything is pure and total.
use crate::model::{Handoff, Ids, UrlClassKind};
use std::borrow::Cow;
use std::collections::BTreeMap;
use url::{Host, Url};

// ------------------------------------------------------------------ small shared primitives

/// Parse `s` as an absolute http(s) URL.
pub fn parse_http(s: &str) -> Option<Url> {
    let view = parser_view(s);
    if host_is_unusable(&view) {
        return None;
    }
    let u = Url::parse(&view).ok()?;
    matches!(u.scheme(), "http" | "https").then_some(u)
}

/// Lower-case host of an http(s) URL.
pub fn host_of(url: &str) -> Option<String> {
    parse_http(url)?.host_str().map(str::to_ascii_lowercase)
}

/// Publisher-rule key for a host: lower-case with dots replaced by hyphens (`www.nejm.org` -> `www-nejm-org`).
/// Rules match on this so the same table works for direct and EZproxy-proxified hosts (`www-nejm-org.kcl.idm.oclc.org`).
pub fn host_key(host: &str) -> String {
    host.to_ascii_lowercase().replace('.', "-")
}

/// Resolve `href` against `base`; only http(s) results survive; the fragment is dropped.
pub fn resolve(base: &Url, href: &str) -> Option<Url> {
    let view = parser_view(href);
    if host_is_unusable(&view) {
        return None;
    }
    let mut u = base.join(&view).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    u.set_fragment(None);
    Some(u)
}

/// The text the URL parser will actually work on: without surrounding whitespace and C0 controls, and without the
/// tabs and newlines it deletes anywhere in the input.
fn parser_view(s: &str) -> Cow<'_, str> {
    let s = s.trim_matches(|c: char| c <= ' ' || c.is_whitespace());
    if s.contains(['\t', '\n', '\r']) {
        Cow::Owned(
            s.chars()
                .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
                .collect(),
        )
    } else {
        Cow::Borrowed(s)
    }
}

/// Longest host text worth parsing: a DNS name is at most 253 octets, plus `:65535`. The host check below refuses anything
/// longer before the URL parser sees it: a 100 KB host (50 000 labels) otherwise costs seconds in a debug build.
const MAX_HOST_BYTES: usize = 260;

/// Whether `view` is a scheme-qualified or network-path URL whose host cannot be a usable one: it has non-ASCII
/// characters (also after percent-decoding, which the parser does before the IDNA step) or is longer than any DNS name
/// ([`MAX_HOST_BYTES`]).
///
/// The workspace pins `idna_adapter = 1.0.0` (ASCII hosts only): it rejects every non-ASCII host, so refusing them here
/// changes no result. It matters for debug builds only, where that adapter `debug_assert!`s on an ASCII upper-case
/// letter that precedes a non-ASCII one (`https://Xé.com/`) and would turn hostile input into a panic.
fn host_is_unusable(view: &str) -> bool {
    let (rest, qualified) = match view.split_once(':') {
        Some((scheme, rest)) if is_scheme(scheme) => (rest, true),
        _ => (view, false),
    };
    let after_slashes = rest.trim_start_matches(['/', '\\']);
    if !qualified && rest.len() - after_slashes.len() < 2 {
        return false; // a path-relative reference has no host
    }
    let authority = after_slashes
        .split(['/', '\\', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    if host.len() > 3 * MAX_HOST_BYTES {
        return true; // too long even if every byte were a percent-escape
    }
    if host.contains('%') {
        let decoded = percent_decode(host);
        !decoded.is_ascii() || decoded.len() > MAX_HOST_BYTES
    } else {
        !host.is_ascii() || host.len() > MAX_HOST_BYTES
    }
}

fn is_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Dedupe key: `https`, lower-case host, default port dropped, no fragment, path and QUERY kept verbatim
/// (queries distinguish PDF variants, for example PLOS `file?id=..&type=printable` vs `type=manuscript`).
/// Unparseable input is returned trimmed so callers can still compare it.
pub fn canonical_key(url: &str) -> String {
    match parse_http(url) {
        Some(mut u) => {
            u.set_fragment(None);
            let s = u.as_str();
            let rest = s
                .strip_prefix("https")
                .or_else(|| s.strip_prefix("http"))
                .unwrap_or(s);
            format!("https{rest}")
        }
        None => url.trim().to_owned(),
    }
}

/// `encodeURIComponent` semantics: keep `A-Z a-z 0-9 - _ . ! ~ * ' ( )`, percent-encode every other byte of the UTF-8.
pub fn percent_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 4);
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Lenient percent-decoding: invalid escapes are kept literally, invalid UTF-8 becomes U+FFFD.
pub fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        let decoded = (c == b'%')
            .then(|| {
                let hi = b.get(i + 1).copied().and_then(hex)?;
                let lo = b.get(i + 2).copied().and_then(hex)?;
                Some(hi * 16 + lo)
            })
            .flatten();
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `https://doi.org/<doi>` with each path segment percent-encoded (`/` kept).
pub fn doi_url(doi: &str) -> String {
    let path: Vec<String> = doi.split('/').map(percent_encode_component).collect();
    format!("https://doi.org/{}", path.join("/"))
}

/// OpenAthens redirector deep link: `https://go.openathens.net/redirector/<domain>?url=<encoded target>`.
pub fn redirector_url(openathens_domain: &str, target: &str) -> String {
    format!(
        "https://go.openathens.net/redirector/{openathens_domain}?url={}",
        percent_encode_component(target)
    )
}

/// EZproxy login URL: `https://<login_host>/login?qurl=<encoded target>`. Always `qurl`, never `url`
/// (`url=` breaks when the target has a query string; research/04 section 6.3).
pub fn ezproxy_login_url(login_host: &str, target: &str) -> String {
    format!(
        "https://{login_host}/login?qurl={}",
        percent_encode_component(target)
    )
}

/// LibKey deep link for a DOI at a library (`https://libkey.io/libraries/<id>/<doi>`). Informational hand-off only.
pub fn libkey_url(library_id: u32, doi: &str) -> String {
    format!("https://libkey.io/libraries/{library_id}/{doi}")
}

// ------------------------------------------------------------------ EZproxy

/// The two EZproxy facts the pure functions need (from `Institution.ezproxy`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProxyConf<'a> {
    /// `login.kcl.idm.oclc.org`
    pub login_host: &'a str,
    /// `kcl.idm.oclc.org`
    pub suffix: &'a str,
}

/// How [`unproxify`] arrived at its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnproxyKind {
    /// The URL was not proxied; returned unchanged.
    NotProxied,
    /// Decoded from `login?qurl=` (decoded once).
    Qurl,
    /// Taken from `login?url=` (rest of the query verbatim).
    UrlParam,
    /// Unwrapped from the OpenAthens redirector `?url=`.
    Redirector,
    /// Proxied host label mapped through the learned map or the static hyphen table.
    Label,
    /// Proxied host label with ambiguous hyphens; naive `-` -> `.` replacement.
    Ambiguous,
}

/// Result of [`unproxify`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unproxied {
    pub url: String,
    pub kind: UnproxyKind,
}

/// Host the OpenAthens redirector lives on.
pub const OPENATHENS_HOST: &str = "go.openathens.net";
/// EZproxy login host assumed by [`url_class`] when the caller has no [`ProxyConf`] (King's College London).
pub const DEFAULT_EZPROXY_LOGIN_HOST: &str = "login.kcl.idm.oclc.org";

/// Real hosts whose own name contains a hyphen. The proxied label cannot be mapped back by replacing `-` with `.`,
/// so [`unproxify`] looks these up by [`host_key`] before guessing (research/04 section 6.4).
const HYPHENATED_HOSTS: [&str; 9] = [
    "www.thieme-connect.de",
    "www.thieme-connect.com",
    "www.oecd-ilibrary.org",
    "www.pep-web.org",
    "www.wiley-vch.de",
    "www.e-elgar.com",
    "www.jbe-platform.com",
    "www.un-ilibrary.org",
    "wrds-www.wharton.upenn.edu",
];

/// Lower-case domain name of a URL, without a trailing root dot. `None` for IP literals and empty hosts.
fn domain_of(url: &Url) -> Option<String> {
    match url.host()? {
        Host::Domain(d) => {
            Some(d.trim_end_matches('.').to_ascii_lowercase()).filter(|d| !d.is_empty())
        }
        Host::Ipv4(_) | Host::Ipv6(_) => None,
    }
}

/// `<path>[?<query>][#<fragment>]` of a parsed URL (the part that survives proxying unchanged).
fn path_query_fragment(url: &Url) -> String {
    let mut out = String::from(url.path());
    if let Some(q) = url.query() {
        out.push('?');
        out.push_str(q);
    }
    if let Some(f) = url.fragment() {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// ASCII case-insensitive `starts_with` that never slices inside a UTF-8 sequence.
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

/// Value of the first `key=value` pair of a raw query (value ends at the next `&`; key compared case-insensitively).
fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        k.eq_ignore_ascii_case(key).then_some(v)
    })
}

/// Everything after the first `url=` that starts the query or follows an `&`, WITHOUT splitting on further `&`
/// (EZproxy's own rule for `login?url=`: the target is not encoded, so its query pairs belong to the target).
fn url_param_rest(query: &str) -> Option<&str> {
    let mut start = 0;
    for part in query.split('&') {
        if starts_with_ci(part, "url=") {
            return query.get(start + "url=".len()..);
        }
        start += part.len() + 1;
    }
    None
}

/// A candidate target is accepted when it parses as http(s); whitespace or control bytes force the normalised form.
fn accept_target(candidate: &str) -> Option<String> {
    let parsed = parse_http(candidate)?;
    if candidate.bytes().any(|b| b <= b' ' || b == 0x7f) {
        Some(parsed.to_string())
    } else {
        Some(candidate.to_owned())
    }
}

/// HttpsHyphens proxify (research/04 section 6.3). Steps in order:
///
/// 1. the host is the login host or already ends with `.<suffix>` -> the target unchanged;
/// 2. https: a port other than 443 or an IP literal -> `None` (use [`ezproxy_login_url`] instead); otherwise
///    `https://<host with dots -> hyphens>.<suffix><path><query><fragment>` (existing hyphens are kept, not doubled;
///    IDN hosts use their punycode form);
/// 3. http: a port other than 80 -> `None`, else the dotted form `http://<host>.<suffix>...`;
/// 4. any other scheme, an unparseable target or an empty suffix -> `None`.
pub fn proxify(conf: &ProxyConf<'_>, target: &str) -> Option<String> {
    let url = parse_http(target)?;
    let host = domain_of(&url)?;
    let suffix = conf.suffix.trim().trim_matches('.').to_ascii_lowercase();
    if suffix.is_empty() {
        return None;
    }
    if host == conf.login_host.trim().to_ascii_lowercase() || host.ends_with(&format!(".{suffix}"))
    {
        return Some(target.to_owned());
    }
    // `Url::port` is already `None` for the scheme's default port, so any `Some` is a non-default one.
    if url.port().is_some() {
        return None;
    }
    let rest = path_query_fragment(&url);
    match url.scheme() {
        "https" => Some(format!("https://{}.{suffix}{rest}", host.replace('.', "-"))),
        "http" => Some(format!("http://{host}.{suffix}{rest}")),
        _ => None,
    }
}

/// Invert proxify / login URLs / the OpenAthens redirector (research/04 section 6.4). `learned` maps a proxied host
/// label (`doi-org`) to the real host (`doi.org`), filled from observed `login?qurl=` redirects. Anything else is
/// returned unchanged as [`UnproxyKind::NotProxied`].
///
/// * login host, path `/login` or `/connect`: `qurl=` (percent-decoded once), else everything after `url=`;
/// * `<label>.<suffix>`: dotted label or http scheme -> the label is the host; hyphenated https label -> `learned`,
///   then the built-in table of hyphenated publisher hosts, then `-` -> `.` ([`UnproxyKind::Ambiguous`]);
/// * `go.openathens.net/redirector/<domain>?url=`: the `url` value, percent-decoded once.
///
/// Path, query and fragment are kept and the host is lower-cased; the port is dropped.
pub fn unproxify(conf: &ProxyConf<'_>, url: &str, learned: &BTreeMap<String, String>) -> Unproxied {
    let unchanged = || Unproxied {
        url: url.to_owned(),
        kind: UnproxyKind::NotProxied,
    };
    let Some(parsed) = parse_http(url) else {
        return unchanged();
    };
    let Some(host) = domain_of(&parsed) else {
        return unchanged();
    };
    let suffix = conf.suffix.trim().trim_matches('.').to_ascii_lowercase();
    let result = if host == conf.login_host.trim().to_ascii_lowercase() {
        unwrap_login(&parsed)
    } else if let Some(label) = host.strip_suffix(&format!(".{suffix}")) {
        unwrap_label(&parsed, label, learned)
    } else if host == OPENATHENS_HOST {
        unwrap_redirector(&parsed)
    } else {
        None
    };
    result.unwrap_or_else(unchanged)
}

/// `login?qurl=<encoded>` or `login?url=<raw>` on the EZproxy login host.
fn unwrap_login(parsed: &Url) -> Option<Unproxied> {
    let path = parsed.path();
    if !(path.eq_ignore_ascii_case("/login") || path.eq_ignore_ascii_case("/connect")) {
        return None;
    }
    let query = parsed.query()?;
    if let Some(target) = query_value(query, "qurl").and_then(|v| accept_target(&percent_decode(v)))
    {
        return Some(Unproxied {
            url: target,
            kind: UnproxyKind::Qurl,
        });
    }
    let rest = url_param_rest(query)?;
    let target = accept_target(rest).or_else(|| accept_target(&percent_decode(rest)))?;
    Some(Unproxied {
        url: target,
        kind: UnproxyKind::UrlParam,
    })
}

/// `<label>.<suffix>`: recover the real host from the proxied label.
fn unwrap_label(
    parsed: &Url,
    label: &str,
    learned: &BTreeMap<String, String>,
) -> Option<Unproxied> {
    if label == "login" || label.is_empty() {
        return None;
    }
    let (real_host, kind) = if parsed.scheme() == "http" || label.contains('.') {
        (label.to_owned(), UnproxyKind::Label)
    } else if let Some(real) = learned
        .get(label)
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
    {
        (real, UnproxyKind::Label)
    } else if let Some(known) = HYPHENATED_HOSTS.iter().find(|h| host_key(h) == label) {
        ((*known).to_owned(), UnproxyKind::Label)
    } else {
        (label.replace('-', "."), UnproxyKind::Ambiguous)
    };
    let rebuilt = format!(
        "{}://{real_host}{}",
        parsed.scheme(),
        path_query_fragment(parsed)
    );
    // A learned or guessed host can still be junk (`www.thieme..connect.de`): then the input is not recoverable.
    parse_http(&rebuilt)?;
    Some(Unproxied { url: rebuilt, kind })
}

/// `go.openathens.net/redirector/<domain>?url=<encoded target>`.
fn unwrap_redirector(parsed: &Url) -> Option<Unproxied> {
    let path = parsed.path();
    if !starts_with_ci(path, "/redirector/") || path.len() <= "/redirector/".len() {
        return None;
    }
    let value = query_value(parsed.query()?, "url")?;
    let target = accept_target(&percent_decode(value))?;
    Some(Unproxied {
        url: target,
        kind: UnproxyKind::Redirector,
    })
}

/// Hosts of the Microsoft / KCL identity providers.
const IDP_HOSTS: [&str; 5] = [
    "login.microsoftonline.com",
    "login.live.com",
    "login.microsoft.com",
    "device.login.microsoftonline.com",
    "kclidp.kcl.ac.uk",
];

/// Substrings (lower-case) of the SP-initiated SSO endpoints publishers redirect to (research/04 section 6.5).
const SSO_HOP_MARKERS: [&str; 12] = [
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

/// Classify a URL for `BrowserReq.hold_while` and the login-window poll: OpenAthens redirector, an IdP login page
/// (Entra, `login.live.com`, `login.openathens.net/auth/`, `kclidp.kcl.ac.uk`), the EZproxy login host
/// (`conf.login_host`, default [`DEFAULT_EZPROXY_LOGIN_HOST`]) at `/login` or `/connect`, a publisher SSO hop, or
/// `Other`. Checked in that order. Never fails: an unparseable URL can only be an SSO hop by substring.
pub fn url_class(conf: Option<&ProxyConf<'_>>, url: &str) -> UrlClassKind {
    let parsed = parse_http(url);
    let host = parsed.as_ref().and_then(domain_of).unwrap_or_default();
    let path = parsed.as_ref().map_or("", Url::path);
    if host == OPENATHENS_HOST && starts_with_ci(path, "/redirector/") {
        return UrlClassKind::OpenathensRedirector;
    }
    if IDP_HOSTS.contains(&host.as_str())
        || (host == "login.openathens.net" && starts_with_ci(path, "/auth/"))
    {
        return UrlClassKind::IdpLogin;
    }
    let login_host = conf
        .map_or(DEFAULT_EZPROXY_LOGIN_HOST, |c| c.login_host)
        .trim()
        .to_ascii_lowercase();
    if host == login_host
        && (path.eq_ignore_ascii_case("/login") || path.eq_ignore_ascii_case("/connect"))
    {
        return UrlClassKind::EzproxyLogin;
    }
    let lower = url.to_ascii_lowercase();
    if SSO_HOP_MARKERS.iter().any(|m| lower.contains(m)) {
        return UrlClassKind::PublisherSsoHop;
    }
    UrlClassKind::Other
}

/// Whether a query parameter name is a tracking parameter (ASCII case-insensitive).
fn is_tracking_param(name: &str) -> bool {
    const EXACT: [&str; 12] = [
        "fbclid", "gclid", "dclid", "msclkid", "mc_cid", "mc_eid", "ref_src", "_ga", "igshid",
        "spm", "cmpid", "wt.mc_id",
    ];
    starts_with_ci(name, "utm_") || EXACT.iter().any(|e| name.eq_ignore_ascii_case(e))
}

/// Remove tracking parameters (`utm_*`, `fbclid`, `gclid`, `dclid`, `msclkid`, `mc_cid`, `mc_eid`, `ref_src`, `_ga`,
/// `igshid`, `spm`, `cmpid`, `WT.mc_id`) while keeping every other query pair byte-for-byte and in order (signed
/// publisher URLs depend on that). The `?` is dropped when nothing remains and the fragment is kept. Input that is
/// not an http(s) URL, has no query, or has nothing to strip is returned unchanged.
pub fn strip_tracking(url: &str) -> String {
    if parse_http(url).is_none() {
        return url.to_owned();
    }
    let (before_fragment, fragment) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    let Some((base, query)) = before_fragment.split_once('?') else {
        return url.to_owned();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !is_tracking_param(pair.split('=').next().unwrap_or_default()))
        .collect();
    if kept.len() == query.split('&').count() {
        return url.to_owned();
    }
    let mut out = String::from(base);
    if kept.iter().any(|pair| !pair.is_empty()) {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

// ------------------------------------------------------------------ hand-off links

/// What [`handoff`] needs from the institution configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffConf<'a> {
    pub openathens_domain: &'a str,
    pub ezproxy: Option<ProxyConf<'a>>,
    pub libkey_library_id: Option<u32>,
}

/// Build the "open it in your browser" links shown on `NeedsUser` / `NotEntitled` / `Blocked` rows.
///
/// The target is `https://doi.org/<doi>` when a DOI is known, else `landing`, else `ids.url` (http(s) only); with no
/// target the result is empty. Then:
/// * `redirector_url`: the OpenAthens redirector deep link, when `openathens_domain` is non-empty;
/// * `ezproxy_url`: the EZproxy `login?qurl=` link (always `qurl`, so targets with queries survive), when configured;
/// * `libkey_url`: the LibKey deep link, only with a DOI and a library id (the DOI is percent-encoded per segment);
/// * `doi_url`: the plain DOI link, when a DOI is known.
pub fn handoff(conf: &HandoffConf<'_>, ids: &Ids, landing: Option<&str>) -> Handoff {
    let doi = ids.doi.as_deref().map(str::trim).filter(|d| !d.is_empty());
    let page = [landing, ids.url.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|u| parse_http(u).is_some());
    let doi_link = doi.map(doi_url);
    let Some(target) = doi_link.clone().or_else(|| page.map(str::to_owned)) else {
        return Handoff::default();
    };
    let domain = conf.openathens_domain.trim();
    Handoff {
        redirector_url: (!domain.is_empty()).then(|| redirector_url(domain, &target)),
        ezproxy_url: conf
            .ezproxy
            .map(|p| ezproxy_login_url(p.login_host, &target)),
        libkey_url: conf
            .libkey_library_id
            .zip(doi)
            .map(|(id, d)| libkey_url(id, &encode_doi_path(d))),
        doi_url: doi_link,
    }
}

/// A DOI as a URL path: each `/`-separated segment percent-encoded (`encodeURIComponent`), the slashes kept.
fn encode_doi_path(doi: &str) -> String {
    doi.split('/')
        .map(percent_encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_keys_and_resolve() {
        assert_eq!(host_key("Www.NEJM.org"), "www-nejm-org");
        let base = parse_http("https://example.org/a/b?x=1").unwrap();
        assert_eq!(
            resolve(&base, "../c.pdf#top").unwrap().as_str(),
            "https://example.org/c.pdf"
        );
        assert!(resolve(&base, "mailto:a@b.c").is_none());
        assert!(resolve(&base, "javascript:;").is_none());
    }

    #[test]
    fn canonical_key_keeps_query_and_upgrades_scheme() {
        assert_eq!(
            canonical_key("http://Example.org:80/p?a=1#f"),
            "https://example.org/p?a=1"
        );
        assert_ne!(
            canonical_key("https://e.org/f?id=1&type=printable"),
            canonical_key("https://e.org/f?id=1&type=manuscript")
        );
    }

    #[test]
    fn encode_decode_round_trip() {
        let t = "https://doi.org/10.1097/SLA.0000000000005603?x=a b";
        let e = percent_encode_component(t);
        assert_eq!(
            e,
            "https%3A%2F%2Fdoi.org%2F10.1097%2FSLA.0000000000005603%3Fx%3Da%20b"
        );
        assert_eq!(percent_decode(&e), t);
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn builders() {
        assert_eq!(
            redirector_url("kcl.ac.uk", "https://doi.org/10.1002/bjs.11646"),
            "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646"
        );
        assert_eq!(
            ezproxy_login_url(
                "login.kcl.idm.oclc.org",
                "https://www.bmj.com/content/372/bmj.n71"
            ),
            "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fwww.bmj.com%2Fcontent%2F372%2Fbmj.n71"
        );
        assert_eq!(
            doi_url("10.1002/bjs.11646"),
            "https://doi.org/10.1002/bjs.11646"
        );
        assert_eq!(
            libkey_url(286, "10.1002/bjs.11646"),
            "https://libkey.io/libraries/286/10.1002/bjs.11646"
        );
    }
}
