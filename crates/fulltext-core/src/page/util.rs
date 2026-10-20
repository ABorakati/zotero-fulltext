//! Small helpers shared by [`super::extract`] and [`super::rules`]: identifier scanning in URLs, "same site" tests,
//! the ScienceDirect embedded-JSON PDF locator, and candidate construction. Owned by core-page; private to `page`.
//!
//! Every function is total (no panics, no indexing, no slicing with `[]`).
use super::html::Doc;
use crate::ids::clean_doi;
use crate::model::{CandKind, Candidate, HostKind, Source};
use crate::rank::new_candidate;
use crate::urls::{host_key, percent_decode, percent_encode_component, resolve};
use url::Url;

/// How far into a script the brace-balanced slicer looks before giving up.
const MAX_JSON_SCAN: usize = 256 * 1024;
/// At most this many ids are returned per scan.
const MAX_IDS: usize = 16;
/// Only this many bytes of a URL are scanned for identifiers.
const MAX_SCAN_URL: usize = 4096;
/// At most this many `10.` positions are examined per scan.
const MAX_PROBES: usize = 64;
/// A DOI longer than this is cut (real DOIs are far shorter).
const MAX_DOI_LEN: usize = 256;

/// The longest prefix of `s` of at most `max` bytes that ends on a char boundary.
fn prefix_at_boundary(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("")
}

// ------------------------------------------------------------------ identifiers inside URLs and strings

/// Every DOI that appears in `decoded` (a URL that has already been percent-decoded), lower-cased and cleaned.
/// A DOI ends at whitespace, `& ? # " ' < > | ,`.
pub(super) fn find_dois(decoded: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Bounded work for hostile input: only the first KiBs of a URL are scanned, at most `MAX_PROBES` candidate
    // positions are examined, and a DOI is cut at `MAX_DOI_LEN` bytes.
    let decoded = prefix_at_boundary(decoded, MAX_SCAN_URL);
    for (probes, (i, _)) in decoded.match_indices("10.").enumerate() {
        if out.len() >= MAX_IDS || probes >= MAX_PROBES {
            break;
        }
        let boundary = decoded
            .get(..i)
            .and_then(|p| p.chars().next_back())
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        if !boundary {
            continue;
        }
        let Some(tail) = decoded.get(i..) else {
            continue;
        };
        let digits = tail
            .get(3..)
            .map_or(0, |r| r.bytes().take_while(u8::is_ascii_digit).count());
        if !(4..=9).contains(&digits) || tail.as_bytes().get(3 + digits) != Some(&b'/') {
            continue;
        }
        let end = tail
            .char_indices()
            .take(MAX_DOI_LEN)
            .find(|&(_, c)| {
                c.is_whitespace()
                    || matches!(c, '&' | '?' | '#' | '"' | '\'' | '<' | '>' | '|' | ',')
            })
            .map_or_else(|| prefix_at_boundary(tail, MAX_DOI_LEN).len(), |(i, _)| i);
        if let Some(doi) = tail.get(..end).and_then(clean_doi)
            && !out.contains(&doi)
        {
            out.push(doi);
        }
    }
    out
}

/// True when `found` (a lower-case DOI scanned from a URL) is `own` or `own` followed by a file extension or a path
/// (`10.1007/s1.pdf`, `10.1371/journal.pmed.1/file`).
pub(super) fn is_own_doi(found: &str, own: &str) -> bool {
    found == own
        || found
            .strip_prefix(own)
            .is_some_and(|rest| rest.starts_with(['.', '/', '_']))
}

/// Every Elsevier-style PII in `decoded`, normalised to `S` plus 16 digits (punctuation `-()` removed, `X` upper-case).
/// Accepts `S0140-6736(20)30183-5`, `PIIS0140-6736(20)30183-5` and `S1743919121000406`; the id must not be followed
/// by another digit and must not be part of a longer alphanumeric word.
pub(super) fn find_piis(decoded: &str) -> Vec<String> {
    let bytes = decoded.as_bytes();
    let mut out: Vec<String> = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        if out.len() >= MAX_IDS {
            break;
        }
        if b != b'S' && b != b's' {
            continue;
        }
        let before = i.checked_sub(1).and_then(|j| bytes.get(j)).copied();
        let after_pii = i
            .checked_sub(3)
            .and_then(|j| bytes.get(j..i))
            .is_some_and(|w| w.eq_ignore_ascii_case(b"pii"));
        if before.is_some_and(|c| c.is_ascii_alphanumeric()) && !after_pii {
            continue;
        }
        if !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            continue;
        }
        let mut digits = String::with_capacity(16);
        let mut j = i + 1;
        let mut ok = false;
        while let Some(&c) = bytes.get(j) {
            match c {
                b'0'..=b'9' | b'X' | b'x' => {
                    digits.push(char::from(c.to_ascii_uppercase()));
                    if digits.len() == 16 {
                        ok = !bytes.get(j + 1).is_some_and(u8::is_ascii_digit);
                        break;
                    }
                }
                b'-' | b'(' | b')' => {}
                _ => break,
            }
            j += 1;
        }
        if ok {
            let pii = format!("S{digits}");
            if !out.contains(&pii) {
                out.push(pii);
            }
        }
    }
    out
}

/// Re-punctuate a normalised 17-character PII (`S0140673620301835` -> `S0140-6736(20)30183-5`).
pub(super) fn pii_punctuated(norm: &str) -> Option<String> {
    if norm.len() != 17 || !norm.is_ascii() {
        return None;
    }
    Some(format!(
        "{}-{}({}){}-{}",
        norm.get(..5)?,
        norm.get(5..9)?,
        norm.get(9..11)?,
        norm.get(11..16)?,
        norm.get(16..17)?
    ))
}

/// The DOI inside `s` with its original letter case (`10.1056/NEJMoa2002032`), for building URLs on sites whose
/// paths are case-sensitive. Falls back to the lower-case form when the case cannot be recovered.
pub(super) fn raw_doi(s: &str) -> Option<String> {
    let lower = clean_doi(s)?;
    let hay = s.to_ascii_lowercase();
    let idx = hay.find(&lower)?;
    Some(
        s.get(idx..idx + lower.len())
            .map_or(lower.clone(), str::to_owned),
    )
}

/// Percent-decode and lower-case a URL for identifier and token scanning.
pub(super) fn decoded_lower(url: &str) -> String {
    percent_decode(url).to_ascii_lowercase()
}

/// `lower` (an ASCII-lower-case needle) located in `hay` case-insensitively, returned with the letter case `hay` has.
fn original_case(hay: &str, lower: &str) -> Option<String> {
    let idx = hay.to_ascii_lowercase().find(lower)?;
    hay.get(idx..idx + lower.len()).map(str::to_owned)
}

/// The DOI a page claims for itself, with its ORIGINAL letter case (site paths such as NEJM's are case-sensitive).
/// Sources, best first: `citation_doi`, every `dc.identifier` (T&F also has a `publisher-id` one), `prism.doi`,
/// `wkhealth_doi`, `rft_id`, JSON-LD `"doi"`, then the first DOI in `og:url`, the canonical link and the page URL.
pub(super) fn find_page_doi_raw(doc: &Doc, url: &Url) -> Option<String> {
    let meta_names = [
        "citation_doi",
        "dc.identifier",
        "prism.doi",
        "wkhealth_doi",
        "rft_id",
    ];
    for name in meta_names {
        if let Some(d) = doc.meta_all(name).find_map(raw_doi) {
            return Some(d);
        }
    }
    if let Some(d) = doc
        .scripts
        .iter()
        .filter(|s| {
            s.kind
                .as_deref()
                .is_some_and(|k| k.to_ascii_lowercase().contains("ld+json"))
        })
        .find_map(|s| json_ld_doi(&s.text))
    {
        return Some(d);
    }
    let canonical = doc
        .links
        .iter()
        .find(|l| l.has_token("rel", "canonical"))
        .and_then(|l| l.attr("href"));
    let from_urls = [doc.meta("og:url"), canonical, Some(url.as_str())];
    from_urls.into_iter().flatten().find_map(|u| {
        let decoded = percent_decode(u);
        let first = find_dois(&decoded.to_ascii_lowercase())
            .into_iter()
            .next()?;
        Some(original_case(&decoded, &first).unwrap_or(first))
    })
}

fn json_ld_doi(text: &str) -> Option<String> {
    fn walk(v: &serde_json::Value, depth: usize) -> Option<String> {
        if depth > 8 {
            return None;
        }
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    if k.eq_ignore_ascii_case("doi")
                        && let Some(d) = val.as_str().and_then(raw_doi)
                    {
                        return Some(d);
                    }
                }
                map.values().find_map(|val| walk(val, depth + 1))
            }
            serde_json::Value::Array(items) => items.iter().find_map(|val| walk(val, depth + 1)),
            _ => None,
        }
    }
    walk(&serde_json::from_str::<serde_json::Value>(text).ok()?, 0)
}

/// The page's own PII (normalised, see [`find_piis`]): `citation_pii`, the page URL, the canonical link, `og:url`.
pub(super) fn find_page_pii(doc: &Doc, url: &Url) -> Option<String> {
    let canonical = doc
        .links
        .iter()
        .find(|l| l.has_token("rel", "canonical"))
        .and_then(|l| l.attr("href"));
    [
        doc.meta("citation_pii"),
        Some(url.as_str()),
        canonical,
        doc.meta("og:url"),
    ]
    .into_iter()
    .flatten()
    .find_map(|u| find_piis(&percent_decode(u)).into_iter().next())
}

// ------------------------------------------------------------------ hosts

/// Approximate registrable domain: the last two labels, or three when the TLD is a two-letter ccTLD under a
/// generic second level (`ac.uk`, `co.uk`, `nhs.uk`, `com.au` ...).
pub(super) fn site_of(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    let n = labels.len();
    if n <= 2 {
        return host.to_ascii_lowercase();
    }
    let tld = labels.get(n - 1).copied().unwrap_or("");
    let sld = labels.get(n - 2).copied().unwrap_or("");
    let take = if tld.len() == 2
        && matches!(
            sld,
            "co" | "com" | "ac" | "org" | "gov" | "net" | "edu" | "nhs" | "sch" | "ltd"
        ) {
        3
    } else {
        2
    };
    labels
        .get(n.saturating_sub(take)..)
        .map(|l| l.join(".").to_ascii_lowercase())
        .unwrap_or_default()
}

/// The host key of `host` with an EZproxy suffix removed (`www-nejm-org.kcl.idm.oclc.org` -> `www-nejm-org`,
/// `doi.org.kcl.idm.oclc.org` -> `doi-org`). Any `*.idm.oclc.org` host is treated as a proxy: the label directly
/// under `idm.oclc.org` is the institution.
pub(super) fn unproxied_host_key(host: &str) -> String {
    let h = host.to_ascii_lowercase();
    match h.strip_suffix(".idm.oclc.org") {
        Some(rest) => host_key(
            rest.rsplit_once('.')
                .map_or(rest, |(label, _institution)| label),
        ),
        None => host_key(&h),
    }
}

/// True when `a` and `b` are the same site: same registrable domain, or `a` is the unproxied form of the proxied
/// host `b` (or the other way round).
pub(super) fn same_site(a: &str, b: &str) -> bool {
    if a.eq_ignore_ascii_case(b) || site_of(a) == site_of(b) {
        return true;
    }
    let (ka, kb) = (unproxied_host_key(a), unproxied_host_key(b));
    ka == kb || ka == host_key(b) || kb == host_key(a)
}

/// Candidate host class from its URL host. The page being analysed is a publisher page unless the host is a known
/// repository, preprint server, PMC mirror or aggregator.
pub(super) fn host_kind_for(host: &str) -> HostKind {
    let h = host.to_ascii_lowercase();
    let is = |d: &str| h == d || h.ends_with(&format!(".{d}"));
    if is("europepmc.org")
        || h == "pmc.ncbi.nlm.nih.gov"
        || h == "www.ncbi.nlm.nih.gov"
        || h == "ncbi.nlm.nih.gov"
    {
        HostKind::Pmc
    } else if [
        "arxiv.org",
        "biorxiv.org",
        "medrxiv.org",
        "chemrxiv.org",
        "preprints.org",
        "ssrn.com",
        "researchsquare.com",
        "osf.io",
    ]
    .iter()
    .any(|d| is(d))
    {
        HostKind::Preprint
    } else if [
        "core.ac.uk",
        "semanticscholar.org",
        "researchgate.net",
        "scholar.archive.org",
    ]
    .iter()
    .any(|d| is(d))
    {
        HostKind::Aggregator
    } else if h.contains("eprints")
        || h.contains("repository")
        || h.contains("dspace")
        || is("hdl.handle.net")
        || is("zenodo.org")
        || is("figshare.com")
    {
        HostKind::Repository
    } else {
        HostKind::Publisher
    }
}

// ------------------------------------------------------------------ URLs

/// `http` becomes `https`; everything else is unchanged.
pub(super) fn upgrade_https(mut u: Url) -> Url {
    if u.scheme() == "http" {
        // `set_scheme` between two special schemes cannot fail.
        let _ = u.set_scheme("https");
    }
    u
}

/// Resolve `href` against `base` (http(s) only, fragment dropped) and upgrade `http` to `https`.
pub(super) fn abs_url(base: &Url, href: &str) -> Option<Url> {
    resolve(base, href).map(upgrade_https)
}

/// The base URL relative references resolve against: `<base href>` (resolved against the page URL) or the page URL.
pub(super) fn base_url(page: &Url, doc: &Doc) -> Url {
    doc.base_href
        .as_deref()
        .and_then(|b| resolve(page, b))
        .unwrap_or_else(|| page.clone())
}

/// `<origin>` + `path_and_query` on the page's own origin (a proxied page stays proxied).
pub(super) fn on_origin(page: &Url, path_and_query: &str) -> Option<Url> {
    let mut origin = page.clone();
    origin.set_fragment(None);
    origin.set_query(None);
    origin.set_path("/");
    origin.join(path_and_query).ok()
}

// ------------------------------------------------------------------ embedded JSON state

/// The first `{...}` object at the start of `s`, string- and escape-aware, scanning at most [`MAX_JSON_SCAN`] bytes.
pub(super) fn balanced_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'{') {
        return None;
    }
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    for (i, &b) in bytes.iter().enumerate().take(MAX_JSON_SCAN) {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return s.get(..=i);
                }
            }
            _ => {}
        }
    }
    None
}

/// ScienceDirect-style embedded state, `"pdfDownload":{"urlMetadata":{"queryParams":{"md5":..,"pid":..},"pii":..,
/// "pdfExtension":"/pdfft","path":"science/article/pii"}}`, in a PLAIN inline `<script>`, turned into
/// `<page origin>/<path>/<pii><pdfExtension>?md5=..&pid=..`.
pub(super) fn json_state_pdf(doc: &Doc, page: &Url) -> Option<Url> {
    doc.scripts
        .iter()
        .filter(|s| s.src.is_none())
        .find_map(|s| pdf_from_script(&s.text, page))
}

fn pdf_from_script(text: &str, page: &Url) -> Option<Url> {
    const KEY: &str = "\"pdfDownload\"";
    let mut from = 0usize;
    for _ in 0..4 {
        let Some(rel) = text.get(from..).and_then(|t| t.find(KEY)) else {
            break;
        };
        let idx = from + rel + KEY.len();
        from = idx;
        let Some(after) = text
            .get(idx..)
            .map(str::trim_start)
            .and_then(|t| t.strip_prefix(':'))
            .map(str::trim_start)
        else {
            continue;
        };
        let Some(obj) = balanced_object(after) else {
            continue;
        };
        if let Some(u) = pdf_from_object(obj, page) {
            return Some(u);
        }
    }
    None
}

fn pdf_from_object(obj: &str, page: &Url) -> Option<Url> {
    let v: serde_json::Value = serde_json::from_str(obj).ok()?;
    let meta = v.get("urlMetadata")?;
    let pii = meta.get("pii")?.as_str()?;
    let ext = meta
        .get("pdfExtension")
        .and_then(|e| e.as_str())
        .unwrap_or("/pdfft");
    let path = meta
        .get("path")
        .and_then(|p| p.as_str())
        .unwrap_or("science/article/pii");
    // Values come from page script, so they are only used when they are plain path words (no dots, no `..`).
    let word = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    };
    let words = |s: &str| s.trim_matches('/').split('/').all(word);
    if !word(pii) || !words(path) || !ext.starts_with('/') || !word(ext.trim_start_matches('/')) {
        return None;
    }
    let mut url = on_origin(page, &format!("/{}/{}{}", path.trim_matches('/'), pii, ext))?;
    if let Some(qp) = meta.get("queryParams").and_then(|q| q.as_object()) {
        // md5 and pid first (the order the site emits), then anything else alphabetically.
        let mut keys: Vec<&String> = qp.keys().collect();
        keys.sort_by_key(|k| {
            (
                usize::from(k.as_str() != "md5")
                    + usize::from(!matches!(k.as_str(), "md5" | "pid")),
                k.as_str(),
            )
        });
        let query: Vec<String> = keys
            .iter()
            .filter_map(|k| {
                qp.get(*k).and_then(|v| v.as_str()).map(|v| {
                    format!(
                        "{}={}",
                        percent_encode_component(k),
                        percent_encode_component(v)
                    )
                })
            })
            .collect();
        if !query.is_empty() {
            url.set_query(Some(&query.join("&")));
        }
    }
    Some(url)
}

// ------------------------------------------------------------------ candidates

/// An extractor candidate: `kind Pdf`, `source Extractor`, with the page URL as `landing_url`, the page DOI as
/// `record_doi` and the host class of the candidate URL. `needs_browser` starts false; [`super::extract`] raises it.
pub(super) fn candidate(
    url: &Url,
    landing: &Url,
    record_doi: Option<&str>,
    signal: String,
    score: i32,
) -> Candidate {
    let mut c = new_candidate(url.as_str(), CandKind::Pdf, Source::Extractor);
    c.host_kind = url.host_str().map(host_kind_for).unwrap_or_default();
    c.record_doi = record_doi.map(str::to_owned);
    c.landing_url = Some(landing.as_str().to_owned());
    c.signal = Some(signal);
    c.score = score;
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::html::parse;

    #[test]
    fn dois_in_urls() {
        let d = decoded_lower(
            "https://www.thelancet.com/servlet/linkout?doi=10.1016%2FS0140-6736%2820%2930183-5&key=10.1016%2FS0140-6736%2803%2913967-0&cf=pdf",
        );
        assert_eq!(
            find_dois(&d),
            vec![
                "10.1016/s0140-6736(20)30183-5",
                "10.1016/s0140-6736(03)13967-0"
            ]
        );
        assert_eq!(
            find_dois("https://x.org/content/pdf/10.1186/s12893-023-02300-3.pdf"),
            vec!["10.1186/s12893-023-02300-3.pdf"]
        );
        assert!(
            find_dois("https://x.org/a?id=notadoi10.1234/x").is_empty(),
            "10. must start at a boundary"
        );
        assert!(find_dois("https://x.org/10.12/short").is_empty());
    }

    #[test]
    fn identifier_scans_are_bounded_on_hostile_input() {
        // Overlapping, never-terminated "DOIs" would make a naive scan quadratic.
        let hostile = "10.1234/".repeat(200_000);
        let dois = find_dois(&hostile);
        assert!(dois.len() <= 16);
        let piis = find_piis(&"S0140-6736(20)30183-5S".repeat(50_000));
        assert!(piis.len() <= 16);
        assert!(
            find_dois("é".repeat(5000).as_str()).is_empty(),
            "multi-byte input is cut on a char boundary"
        );
    }

    #[test]
    fn own_doi_boundaries() {
        assert!(is_own_doi(
            "10.1186/s12893-023-02300-3",
            "10.1186/s12893-023-02300-3"
        ));
        assert!(is_own_doi(
            "10.1186/s12893-023-02300-3.pdf",
            "10.1186/s12893-023-02300-3"
        ));
        assert!(is_own_doi(
            "10.1371/journal.pmed.1004377/file",
            "10.1371/journal.pmed.1004377"
        ));
        assert!(!is_own_doi(
            "10.1371/journal.pmed.10043770",
            "10.1371/journal.pmed.1004377"
        ));
        assert!(!is_own_doi(
            "10.1371/journal.pmed.1004449",
            "10.1371/journal.pmed.1004377"
        ));
    }

    #[test]
    fn piis_in_urls() {
        assert_eq!(
            find_piis(
                "https://www.thelancet.com/pdfs/journals/lancet/PIIS0140-6736(20)30183-5.pdf"
            ),
            vec!["S0140673620301835"]
        );
        assert_eq!(
            find_piis("/science/article/pii/S1743919121000406/pdfft"),
            vec!["S1743919121000406"]
        );
        assert_eq!(
            find_piis("/cell/fulltext/S0092-8674(20)30229-4"),
            vec!["S0092867420302294"]
        );
        assert!(
            find_piis("/articles/s41598-024-73565-8.pdf").is_empty(),
            "Nature ids are too short"
        );
        assert!(
            find_piis("/x/S12345678901234567").is_empty(),
            "17 digits is not a PII"
        );
        assert!(
            find_piis("/x/BS0140673620301835").is_empty(),
            "must not sit inside a longer word"
        );
    }

    #[test]
    fn pii_round_trip() {
        assert_eq!(
            pii_punctuated("S0140673620301835").as_deref(),
            Some("S0140-6736(20)30183-5")
        );
        assert_eq!(pii_punctuated("S01"), None);
    }

    #[test]
    fn raw_doi_keeps_case() {
        assert_eq!(
            raw_doi("10.1056/NEJMoa2002032").as_deref(),
            Some("10.1056/NEJMoa2002032")
        );
        assert_eq!(
            raw_doi("doi:10.1056/NEJMoa2002032").as_deref(),
            Some("10.1056/NEJMoa2002032")
        );
        assert_eq!(raw_doi("no doi here"), None);
    }

    #[test]
    fn sites_and_proxies() {
        assert_eq!(site_of("www.nejm.org"), "nejm.org");
        assert_eq!(
            site_of("public-pages-files-2025.frontiersin.org"),
            "frontiersin.org"
        );
        assert_eq!(site_of("www.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(site_of("localhost"), "localhost");
        assert!(!same_site("media.springernature.com", "link.springer.com"));
        assert!(same_site("www.nejm.org", "nejm.org"));
        assert!(same_site("www.nejm.org", "www-nejm-org.kcl.idm.oclc.org"));
        assert_eq!(
            unproxied_host_key("www-nejm-org.kcl.idm.oclc.org"),
            "www-nejm-org"
        );
        assert_eq!(unproxied_host_key("doi.org.kcl.idm.oclc.org"), "doi-org");
        assert_eq!(unproxied_host_key("WWW.NEJM.ORG"), "www-nejm-org");
    }

    #[test]
    fn host_kinds() {
        assert_eq!(host_kind_for("pmc.ncbi.nlm.nih.gov"), HostKind::Pmc);
        assert_eq!(host_kind_for("europepmc.org"), HostKind::Pmc);
        assert_eq!(host_kind_for("www.medrxiv.org"), HostKind::Preprint);
        assert_eq!(host_kind_for("eprints.soton.ac.uk"), HostKind::Repository);
        assert_eq!(host_kind_for("core.ac.uk"), HostKind::Aggregator);
        assert_eq!(host_kind_for("www.nejm.org"), HostKind::Publisher);
    }

    #[test]
    fn balanced_slices_respect_strings() {
        assert_eq!(
            balanced_object(r#"{"a":"}{","b":{"c":1}} tail"#),
            Some(r#"{"a":"}{","b":{"c":1}}"#)
        );
        assert_eq!(balanced_object(r#"{"a":"\"}"} x"#), Some(r#"{"a":"\"}"}"#));
        assert_eq!(balanced_object("{unterminated"), None);
        assert_eq!(balanced_object("}"), None);
        assert_eq!(balanced_object("no brace"), None);
    }

    #[test]
    fn json_state_builds_sciencedirect_url() {
        let page =
            Url::parse("https://www.sciencedirect.com/science/article/pii/S1743919121000406")
                .unwrap();
        let doc = parse(
            r#"<script>window.__PRELOADED_STATE__ = {"article":{"pdfDownload":{"isPdfFullText":false,"urlMetadata":{"queryParams":{"pid":"1-s2.0-S1743919121000406-main.pdf","md5":"b6d8b093900d49eecaf3e85a8799af17"},"pii":"S1743919121000406","pdfExtension":"/pdfft","path":"science/article/pii"}}}};</script>"#,
        );
        assert_eq!(
            json_state_pdf(&doc, &page)
                .map(|u| u.to_string())
                .as_deref(),
            Some(
                "https://www.sciencedirect.com/science/article/pii/S1743919121000406/pdfft?md5=b6d8b093900d49eecaf3e85a8799af17&pid=1-s2.0-S1743919121000406-main.pdf"
            )
        );
        // Hostile values are refused rather than turned into a URL.
        let bad = parse(
            r#"<script>x = {"pdfDownload":{"urlMetadata":{"pii":"../../evil","queryParams":{}}}}</script>"#,
        );
        assert!(json_state_pdf(&bad, &page).is_none());
        // A script with a src is never read, and garbage is harmless.
        assert!(
            json_state_pdf(
                &parse(r#"<script src="/a.js"></script><script>"pdfDownload": {"#),
                &page
            )
            .is_none()
        );
    }

    #[test]
    fn base_and_origin_helpers() {
        let page =
            Url::parse("https://www-nejm-org.kcl.idm.oclc.org/doi/full/10.1056/X?a=1").unwrap();
        assert_eq!(
            on_origin(&page, "/doi/pdf/10.1056/X")
                .map(|u| u.to_string())
                .as_deref(),
            Some("https://www-nejm-org.kcl.idm.oclc.org/doi/pdf/10.1056/X")
        );
        let doc = parse(r#"<base href="https://cdn.example.org/a/">"#);
        assert_eq!(base_url(&page, &doc).as_str(), "https://cdn.example.org/a/");
        assert_eq!(base_url(&page, &parse("")).as_str(), page.as_str());
        assert_eq!(
            abs_url(&page, "http://x.org/a#f")
                .map(|u| u.to_string())
                .as_deref(),
            Some("https://x.org/a")
        );
        assert!(abs_url(&page, "mailto:a@b.c").is_none());
    }
}
