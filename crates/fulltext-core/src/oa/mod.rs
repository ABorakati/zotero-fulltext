//! One parser per open-access API. Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! Every parser has the same shape and the same discipline:
//!
//! ```text
//! pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection
//! ```
//!
//! * Total: any status, any bytes (HTML error pages served for JSON endpoints, truncated JSON, empty body).
//! * Status first: call [`gate`] before touching the body; error bodies are often HTML, XML or plain text.
//! * "Not found" is often a 200 with an empty result: map it to `ApiStatus::Empty` (the API answered, nothing there)
//!   and reserve `NotFound` for 404-style answers.
//! * `Found` means the response yielded something the engine can act on: at least one candidate (the NCBI ID converter
//!   yields ids only, so a learned id is enough there; a bioRxiv `pubs` answer yields the published DOI in `note`).
//!   A record that exists but offers nothing (closed work, subscription-only links) is `Empty`, with `learned` and
//!   `oa_status` still filled because they are true whatever the status.
//! * Compare DOIs case-insensitively (`crate::ids::same_doi`); set `Candidate.record_doi` from the record, so ranking
//!   can detect OpenAlex-style merged records.
//! * Candidate URLs are absolute https URLs where the source allows (upgrade `http://`), tagged with
//!   `source`, `version`, `license` (lower-case, e.g. `cc-by`), `host_kind` and `kind` (`Pdf` only for a direct file
//!   URL, `Landing` for an HTML page). `score` and `tier` stay 0; `crate::rank::rank` sets them.
//! * Never invent identifiers: `learned` only carries ids the response states.
//! * Notes are short fixed codes (`no_pdf`, `bad_email`, `published_doi:<doi>`); a body is never echoed.
use crate::model::{ApiInspection, ApiStatus, HostKind, Ids, RespMeta, Source, Version};
use crate::urls;

pub mod alma;
pub mod arxiv;
pub mod biorxiv;
pub mod core_api;
pub mod crossref;
pub mod europepmc;
pub mod openalex;
pub mod pmc_s3;
pub mod semantic_scholar;
pub mod unpaywall;
pub mod zotero_oa;

mod util;
mod xml;

/// Route a recorded response to its parser. `Source::Doi` means "DOI registry metadata" and is handled by the
/// Crossref parser; sources that are not HTTP APIs (`ItemUrl`, `Translator`, `Extractor`, `Redirector`, `Ezproxy`)
/// return `Unparseable` with an explanatory note.
pub fn parse(source: Source, resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    match source {
        Source::Openalex => openalex::parse(resp, body, requested),
        Source::Europepmc => europepmc::parse(resp, body, requested),
        Source::Biorxiv => biorxiv::parse(resp, body, requested),
        Source::PmcS3 => pmc_s3::parse(resp, body, requested),
        Source::Arxiv => arxiv::parse(resp, body, requested),
        Source::Unpaywall => unpaywall::parse(resp, body, requested),
        Source::SemanticScholar => semantic_scholar::parse(resp, body, requested),
        Source::Core => core_api::parse(resp, body, requested),
        Source::Doi => crossref::parse(resp, body, requested),
        Source::Alma => alma::parse(resp, body, requested),
        Source::ZoteroOa => zotero_oa::parse(resp, body, requested),
        Source::ItemUrl
        | Source::Translator
        | Source::Extractor
        | Source::Redirector
        | Source::Ezproxy => unparsed(source, "source is not an HTTP API"),
    }
}

/// An `ApiInspection` with the given status and everything else empty.
pub fn empty(source: Source, status: ApiStatus) -> ApiInspection {
    ApiInspection {
        source,
        status,
        learned: Default::default(),
        candidates: Vec::new(),
        oa_status: None,
        note: None,
        retry_after_s: None,
    }
}

/// `Unparseable` with a note: the body could not be read or had an unexpected shape.
pub fn unparsed(source: Source, note: &str) -> ApiInspection {
    ApiInspection {
        note: Some(note.to_owned()),
        ..empty(source, ApiStatus::Unparseable)
    }
}

/// Status-first gate shared by all parsers. Returns `Some(result)` when the HTTP status alone decides:
/// 404 / 410 -> `NotFound`; 429 (and 503 with `Retry-After`) -> `Throttled` with `retry_after_s` from the header
/// (seconds form only); any other non-2xx -> `Unparseable` with note `http_<status>`. Returns `None` for 2xx.
pub fn gate(source: Source, resp: &RespMeta) -> Option<ApiInspection> {
    let retry = resp
        .headers
        .get("retry-after")
        .and_then(|v| v.trim().parse::<u32>().ok());
    let status = match resp.status {
        200..=299 => return None,
        404 | 410 => ApiStatus::NotFound,
        429 => ApiStatus::Throttled,
        503 if retry.is_some() => ApiStatus::Throttled,
        _ => ApiStatus::Unparseable,
    };
    Some(ApiInspection {
        note: Some(format!("http_{}", resp.status)),
        retry_after_s: if status == ApiStatus::Throttled {
            retry
        } else {
            None
        },
        ..empty(source, status)
    })
}

/// Map a source's version vocabulary to [`Version`]: `publishedVersion` / `published` / `vor` -> Published;
/// `acceptedVersion` / `accepted` / `authorManuscript` / `am` -> Accepted; `submittedVersion` / `submitted` /
/// `preprint` -> Submitted; everything else -> Unknown. Case-insensitive; spaces, dashes and underscores are ignored
/// (`"Version of Record"`, `"author_manuscript"`).
pub fn version_from_str(s: &str) -> Version {
    let key: String = s
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match key.as_str() {
        "publishedversion" | "published" | "vor" | "versionofrecord" => Version::Published,
        "acceptedversion" | "accepted" | "authormanuscript" | "acceptedmanuscript" | "am"
        | "aam" | "postprint" => Version::Accepted,
        "submittedversion" | "submitted" | "preprint" | "prepublication" => Version::Submitted,
        _ => Version::Unknown,
    }
}

/// Host suffixes of the publishers whose PDFs the plugin meets most often. Matched on a label boundary, so
/// `link.springer.com` is `springer.com` but `notspringer.com` is not.
const PUBLISHER_HOSTS: &[&str] = &[
    "nature.com",
    "springer.com",
    "springernature.com",
    "springeropen.com",
    "biomedcentral.com",
    "sciencedirect.com",
    "elsevier.com",
    "cell.com",
    "thelancet.com",
    "wiley.com",
    "tandfonline.com",
    "sagepub.com",
    "oup.com",
    "cambridge.org",
    "jamanetwork.com",
    "nejm.org",
    "bmj.com",
    "plos.org",
    "frontiersin.org",
    "mdpi.com",
    "hindawi.com",
    "acs.org",
    "rsc.org",
    "iop.org",
    "aps.org",
    "science.org",
    "sciencemag.org",
    "pnas.org",
    "annualreviews.org",
    "thieme-connect.com",
    "thieme-connect.de",
    "thieme.de",
    "karger.com",
    "lww.com",
    "wolterskluwer.com",
    "degruyter.com",
    "degruyterbrill.com",
    "ieee.org",
    "acm.org",
    "aaai.org",
    "royalsocietypublishing.org",
    "elifesciences.org",
    "peerj.com",
    "f1000research.com",
    "cureus.com",
    "ahajournals.org",
    "aacrjournals.org",
    "asm.org",
    "jbc.org",
    "rupress.org",
    "embopress.org",
    "physiology.org",
    "ersjournals.com",
    "atsjournals.org",
    "jci.org",
    "emerald.com",
    "brill.com",
    "ingentaconnect.com",
    "bjs.co.uk",
    "bjsopen.com",
];

/// Hosts that re-host or index other people's papers.
const AGGREGATOR_HOSTS: &[&str] = &[
    "core.ac.uk",
    "semanticscholar.org",
    "researchgate.net",
    "scholar.archive.org",
    "archive.org",
    "academia.edu",
    "langtaosha.org.cn",
    "citeseerx.ist.psu.edu",
    "paperity.org",
    "base-search.net",
    "scite.ai",
    "jstor.org",
];

/// Hosts of repositories that are not recognisable by an `eprints` / `repository` / academic-domain pattern.
const REPOSITORY_HOSTS: &[&str] = &[
    "hdl.handle.net",
    "handle.net",
    "zenodo.org",
    "figshare.com",
    "osf.io",
    "hal.science",
    "hal.archives-ouvertes.fr",
    "dash.harvard.edu",
    "nrs.harvard.edu",
    "doaj.org",
    "escholarship.org",
    "dataverse.harvard.edu",
    "scielo.org",
    "redalyc.org",
];

/// True when `host` is `suffix` or a subdomain of it.
fn host_is(host: &str, suffix: &str) -> bool {
    host == suffix
        || host
            .strip_suffix(suffix)
            .is_some_and(|head| head.ends_with('.'))
}

fn host_in(host: &str, list: &[&str]) -> bool {
    list.iter().any(|suffix| host_is(host, suffix))
}

/// Classify a URL's host: PMC / Europe PMC full-text hosts -> `Pmc`; arxiv / biorxiv / medrxiv / preprints -> `Preprint`;
/// `core.ac.uk`, `semanticscholar`, `researchgate`, `scholar.archive.org`, `langtaosha` mirrors -> `Aggregator`;
/// recognised publisher hosts -> `Publisher`; university / institutional repository hosts (`eprints`, `repository`,
/// `dspace`, `hdl.handle.net`, academic domains such as `.ac.uk` or `.edu`) -> `Repository`; else `Other`
/// (`doi.org` is a resolver, not a host, and stays `Other`; callers that know the record's source type refine it).
pub fn host_kind(url: &str) -> HostKind {
    let Some(parsed) = urls::parse_http(url) else {
        return HostKind::Other;
    };
    let Some(host) = parsed.host_str().map(str::to_ascii_lowercase) else {
        return HostKind::Other;
    };
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = parsed.path().to_ascii_lowercase();

    let pmc = host == "pmc.ncbi.nlm.nih.gov"
        || host == "pmc-oa-opendata.s3.amazonaws.com"
        || host_is(host, "europepmc.org")
        || (host == "ncbi.nlm.nih.gov" && path.starts_with("/pmc/"))
        || (host == "ebi.ac.uk" && path.starts_with("/europepmc"));
    if pmc {
        return HostKind::Pmc;
    }
    // Any host with a `*rxiv` label is a preprint server (arxiv, biorxiv, medrxiv, chemrxiv, psyarxiv, ...).
    let rxiv = host
        .split('.')
        .any(|label| label.len() > 4 && label.ends_with("rxiv"));
    if rxiv
        || host_in(
            host,
            &[
                "preprints.org",
                "ssrn.com",
                "researchsquare.com",
                "authorea.com",
            ],
        )
    {
        return HostKind::Preprint;
    }
    if host_in(host, AGGREGATOR_HOSTS) {
        return HostKind::Aggregator;
    }
    if host_in(host, PUBLISHER_HOSTS) {
        return HostKind::Publisher;
    }
    let academic = host.ends_with(".edu")
        || host.contains(".edu.")
        || host.ends_with(".ac.uk")
        || host.contains(".ac.");
    let repo_label = [
        "eprints",
        "repositor",
        "dspace",
        "digitalcommons",
        "scholarworks",
    ]
    .iter()
    .any(|p| host.contains(p));
    if academic || repo_label || host_in(host, REPOSITORY_HOSTS) || path.contains("/id/eprint") {
        return HostKind::Repository;
    }
    HostKind::Other
}

/// `s3://pmc-oa-opendata/<key>?md5=...` -> `https://pmc-oa-opendata.s3.amazonaws.com/<key>` (drops the query).
/// Other inputs (other buckets, other schemes, an empty key) return `None`.
pub fn s3_to_https(s3_uri: &str) -> Option<String> {
    let rest = s3_uri.trim().strip_prefix("s3://pmc-oa-opendata/")?;
    let key = rest.split(['?', '#']).next()?;
    if key.is_empty() || key.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let encoded: Vec<String> = key.split('/').map(urls::percent_encode_component).collect();
    Some(format!(
        "https://pmc-oa-opendata.s3.amazonaws.com/{}",
        encoded.join("/")
    ))
}

/// URLs that only a real browser can fetch (JavaScript proof-of-work or a Cloudflare challenge): PMC website PDFs
/// (`pmc.ncbi.nlm.nih.gov/articles/PMC<n>/pdf/...`, also on the legacy `ncbi.nlm.nih.gov/pmc/articles/...` host) and
/// Europe PMC's rendered PDFs (`europepmc.org/...?pdf=render`, `ptpmcrender.fcgi`). `Candidate.needs_browser` is set
/// from this by the parsers and by `rank`.
pub fn is_browser_walled(url: &str) -> bool {
    let Some(parsed) = urls::parse_http(url) else {
        return false;
    };
    let Some(host) = parsed.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = parsed.path().to_ascii_lowercase();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match host {
        "pmc.ncbi.nlm.nih.gov" => matches!(segments.as_slice(), ["articles", _, "pdf", ..]),
        "ncbi.nlm.nih.gov" => matches!(segments.as_slice(), ["pmc", "articles", _, "pdf", ..]),
        "europepmc.org" => {
            path.starts_with("/backend/ptpmcrender")
                || parsed
                    .query_pairs()
                    .any(|(k, v)| k == "pdf" && v.eq_ignore_ascii_case("render"))
        }
        _ => false,
    }
}

/// Normalise a licence string to a lower-case slug: `"CC BY"` / `"cc by"` / `"cc_by"` / `"CCBY"` -> `cc-by`,
/// `"CC BY-NC-ND 4.0"` -> `cc-by-nc-nd`, `"cc0_ng"` -> `cc0`, `"TDM"` -> `tdm`, `"other-oa"` unchanged.
/// Empty strings and placeholders (`null`, `unknown`, `cc_no`) are `None`.
pub fn license_slug(raw: &str) -> Option<String> {
    let mut slug: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_whitespace() || c == '_' {
                '-'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    slug = slug.trim_matches('-').to_owned();
    if matches!(
        slug.as_str(),
        "" | "none" | "null" | "unknown" | "na" | "n/a" | "cc-no"
    ) {
        return None;
    }
    if slug.starts_with("cc0") || slug == "cc-zero" {
        return Some("cc0".to_owned());
    }
    // Drop a trailing version number (`cc-by-4.0`).
    if let Some((head, last)) = slug.rsplit_once('-')
        && !last.is_empty()
        && last.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    {
        slug = head.to_owned();
    }
    // `ccby`, `ccbyncnd` -> `cc-by`, `cc-by-nc-nd`.
    if let Some(rest) = slug.strip_prefix("cc")
        && !rest.is_empty()
        && !rest.contains('-')
    {
        let mut parts = Vec::new();
        let mut tail = rest;
        while !tail.is_empty() {
            let part = ["by", "nc", "nd", "sa"]
                .into_iter()
                .find(|p| tail.starts_with(p))?;
            parts.push(part);
            tail = tail.get(part.len()..).unwrap_or("");
        }
        return Some(format!("cc-{}", parts.join("-")));
    }
    Some(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_map_from_every_vocabulary() {
        for (raw, want) in [
            ("publishedVersion", Version::Published),
            ("VoR", Version::Published),
            ("Version of Record", Version::Published),
            ("acceptedVersion", Version::Accepted),
            ("author_manuscript", Version::Accepted),
            ("AM", Version::Accepted),
            ("submittedVersion", Version::Submitted),
            ("preprint", Version::Submitted),
            ("", Version::Unknown),
            ("deprecated", Version::Unknown),
        ] {
            assert_eq!(version_from_str(raw), want, "{raw}");
        }
    }

    #[test]
    fn host_kinds() {
        for (url, want) in [
            (
                "https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/",
                HostKind::Pmc,
            ),
            (
                "https://www.ncbi.nlm.nih.gov/pmc/articles/2707599",
                HostKind::Pmc,
            ),
            (
                "https://europepmc.org/api/getPdf?pmcid=PMC2707599",
                HostKind::Pmc,
            ),
            (
                "https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf",
                HostKind::Pmc,
            ),
            ("https://arxiv.org/pdf/1706.03762", HostKind::Preprint),
            (
                "https://www.medrxiv.org/content/medrxiv/early/2023/06/01/x.full.pdf",
                HostKind::Preprint,
            ),
            (
                "https://www.biorxiv.org/content/10.1101/x",
                HostKind::Preprint,
            ),
            ("https://core.ac.uk/download/1.pdf", HostKind::Aggregator),
            (
                "https://langtaosha.org.cn/index.php/lts/preprint/download/10/108",
                HostKind::Aggregator,
            ),
            (
                "https://www.nature.com/articles/nature12373.pdf",
                HostKind::Publisher,
            ),
            (
                "https://journals.plos.org/plosmedicine/article/file?id=x&type=printable",
                HostKind::Publisher,
            ),
            (
                "https://iris.unimore.it/bitstream/11380/1368022/2/document.pdf",
                HostKind::Other,
            ),
            ("https://hdl.handle.net/11380/1368022", HostKind::Repository),
            (
                "https://research.birmingham.ac.uk/en/publications/414e76f5",
                HostKind::Repository,
            ),
            (
                "https://eprints.example.org/1/paper.pdf",
                HostKind::Repository,
            ),
            (
                "https://doi.org/10.1371/journal.pmed.1000097",
                HostKind::Other,
            ),
            ("not a url", HostKind::Other),
            ("ftp://arxiv.org/x", HostKind::Other),
        ] {
            assert_eq!(host_kind(url), want, "{url}");
        }
        assert!(!host_is("notspringer.com", "springer.com"));
    }

    #[test]
    fn s3_uris_become_https() {
        assert_eq!(
            s3_to_https("s3://pmc-oa-opendata/PMC2707599.1/PMC2707599.1.pdf?md5=dc66").as_deref(),
            Some("https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf")
        );
        for bad in [
            "",
            "s3://other-bucket/a.pdf",
            "https://pmc-oa-opendata.s3.amazonaws.com/a.pdf",
            "s3://pmc-oa-opendata/",
            "s3://pmc-oa-opendata/a b",
        ] {
            assert_eq!(s3_to_https(bad), None, "{bad}");
        }
    }

    #[test]
    fn browser_walled_hosts() {
        assert!(is_browser_walled(
            "https://pmc.ncbi.nlm.nih.gov/articles/PMC11156280/pdf/pone.0298402.pdf"
        ));
        assert!(is_browser_walled(
            "https://www.ncbi.nlm.nih.gov/pmc/articles/PMC11156280/pdf/pone.0298402.pdf"
        ));
        assert!(is_browser_walled(
            "https://europepmc.org/articles/PMC4221854?pdf=render"
        ));
        assert!(!is_browser_walled(
            "https://pmc.ncbi.nlm.nih.gov/articles/PMC11156280/"
        ));
        assert!(!is_browser_walled(
            "https://europepmc.org/api/getPdf?pmcid=PMC2707599"
        ));
        assert!(!is_browser_walled("https://arxiv.org/pdf/1706.03762"));
    }

    #[test]
    fn license_slugs() {
        for (raw, want) in [
            ("CC BY", Some("cc-by")),
            ("cc by", Some("cc-by")),
            ("CCBY", Some("cc-by")),
            ("cc_by_nc_nd", Some("cc-by-nc-nd")),
            ("CC BY-NC-ND 4.0", Some("cc-by-nc-nd")),
            ("cc0_ng", Some("cc0")),
            ("TDM", Some("tdm")),
            ("other-oa", Some("other-oa")),
            ("cc_no", None),
            ("", None),
            ("  ", None),
        ] {
            assert_eq!(license_slug(raw).as_deref(), want, "{raw}");
        }
    }

    #[test]
    fn gate_maps_status_codes() {
        let resp = |status: u16, retry: Option<&str>| RespMeta {
            status,
            headers: retry
                .map(|r| ("retry-after".to_owned(), r.to_owned()))
                .into_iter()
                .collect(),
            ..Default::default()
        };
        assert!(gate(Source::Openalex, &resp(200, None)).is_none());
        assert_eq!(
            gate(Source::Openalex, &resp(404, None)).map(|a| a.status),
            Some(ApiStatus::NotFound)
        );
        let throttled = gate(Source::Openalex, &resp(429, Some("7")));
        assert_eq!(
            throttled.as_ref().map(|a| (a.status, a.retry_after_s)),
            Some((ApiStatus::Throttled, Some(7)))
        );
        assert_eq!(
            gate(Source::Openalex, &resp(503, None)).map(|a| a.status),
            Some(ApiStatus::Unparseable)
        );
        assert_eq!(
            gate(Source::Openalex, &resp(503, Some("3"))).map(|a| a.status),
            Some(ApiStatus::Throttled)
        );
    }
}
