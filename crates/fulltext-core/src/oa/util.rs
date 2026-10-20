//! Small helpers shared by the OA parsers: tolerant JSON reads, https URLs, candidate construction and per-response
//! dedupe. Private to `oa`.
use super::{host_kind, is_browser_walled};
use crate::ids;
use crate::model::{ApiInspection, ApiStatus, CandKind, Candidate, LearnedIds, Source, Version};
use crate::urls;
use serde_json::Value;

/// Parse a JSON body (a UTF-8 byte-order mark is ignored). `None` for anything that is not JSON.
pub(super) fn json(body: &[u8]) -> Option<Value> {
    serde_json::from_slice(body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body)).ok()
}

/// A non-empty, trimmed string field.
pub(super) fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// An identifier that APIs spell sometimes as a string and sometimes as a number (`pmid`).
pub(super) fn id_of(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// An array field; missing or mistyped fields read as empty.
pub(super) fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// A DOI-valued string field, cleaned.
pub(super) fn doi_of(v: &Value, key: &str) -> Option<String> {
    str_of(v, key).and_then(ids::clean_doi)
}

/// An absolute http(s) URL as an https URL without fragment. `http://` is upgraded unless it names a port; other
/// schemes, relative URLs and empty strings are dropped.
pub(super) fn https(raw: &str) -> Option<String> {
    let mut url = urls::parse_http(raw)?;
    if url.scheme() == "http" && url.port().is_none() {
        url.set_scheme("https").ok()?;
    }
    url.set_fragment(None);
    Some(url.into())
}

/// Heuristic for "this URL is a file, not a page": a `.pdf` path or a `/pdf/` segment (`arxiv.org/pdf/1706.03762`).
pub(super) fn looks_like_pdf(url: &str) -> bool {
    urls::parse_http(url).is_some_and(|u| {
        let path = u.path().to_ascii_lowercase();
        path.ends_with(".pdf") || path.split('/').any(|segment| segment == "pdf")
    })
}

/// A candidate with the parser's source, host kind from the URL and `needs_browser` from the known walled hosts.
/// `None` when `raw_url` is not an http(s) URL.
pub(super) fn candidate(source: Source, kind: CandKind, raw_url: &str) -> Option<Candidate> {
    let url = https(raw_url)?;
    Some(Candidate {
        kind,
        source,
        version: Version::Unknown,
        license: None,
        host_kind: host_kind(&url),
        record_doi: None,
        landing_url: None,
        signal: None,
        score: 0,
        tier: 0,
        needs_browser: is_browser_walled(&url),
        url,
    })
}

/// Candidates of one response, deduplicated by [`urls::canonical_key`] (query included).
#[derive(Default)]
pub(super) struct CandidateSet {
    items: Vec<Candidate>,
    keys: Vec<String>,
}

impl CandidateSet {
    /// Add `c`; a duplicate URL keeps the first candidate, upgrading it to `Pdf` and filling gaps from the newcomer.
    pub fn push(&mut self, c: Option<Candidate>) {
        let Some(c) = c else { return };
        let key = urls::canonical_key(&c.url);
        let Some(at) = self.keys.iter().position(|k| *k == key) else {
            self.keys.push(key);
            self.items.push(c);
            return;
        };
        if let Some(existing) = self.items.get_mut(at) {
            if c.kind == CandKind::Pdf {
                existing.kind = CandKind::Pdf;
            }
            if existing.version == Version::Unknown {
                existing.version = c.version;
            }
            existing.license = existing.license.take().or(c.license);
            existing.landing_url = existing.landing_url.take().or(c.landing_url);
            existing.record_doi = existing.record_doi.take().or(c.record_doi);
            // A URL that is also offered for text mining stays flagged as such (Crossref lists one URL per purpose).
            if c.signal.as_deref().is_some_and(|s| s.ends_with("_tdm")) {
                existing.signal = c.signal;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn has_pdf(&self) -> bool {
        self.items.iter().any(|c| c.kind == CandKind::Pdf)
    }

    pub fn into_vec(self) -> Vec<Candidate> {
        self.items
    }
}

/// The finished answer: `Found` when there is at least one candidate, else `Empty`.
pub(super) fn finish(
    source: Source,
    learned: LearnedIds,
    candidates: Vec<Candidate>,
    oa_status: Option<String>,
    note: Option<String>,
) -> ApiInspection {
    let status = if candidates.is_empty() {
        ApiStatus::Empty
    } else {
        ApiStatus::Found
    };
    ApiInspection {
        source,
        status,
        learned,
        candidates,
        oa_status,
        note,
        retry_after_s: None,
    }
}

/// `Empty` with a note (the API answered, there is nothing usable).
pub(super) fn nothing(source: Source, note: &str) -> ApiInspection {
    ApiInspection {
        note: Some(note.to_owned()),
        ..super::empty(source, ApiStatus::Empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_upgrades_and_filters() {
        assert_eq!(
            https("http://arxiv.org/abs/1706.03762#x").as_deref(),
            Some("https://arxiv.org/abs/1706.03762")
        );
        assert_eq!(
            https(" https://a.org/p?q=1 ").as_deref(),
            Some("https://a.org/p?q=1")
        );
        assert_eq!(
            https("http://a.org:8080/p").as_deref(),
            Some("http://a.org:8080/p")
        );
        for bad in ["", "ftp://a.org/x", "/relative", "javascript:;", "s3://b/k"] {
            assert_eq!(https(bad), None, "{bad}");
        }
    }

    #[test]
    fn pdf_heuristic() {
        assert!(looks_like_pdf(
            "https://www.nature.com/articles/nature12373.pdf"
        ));
        assert!(looks_like_pdf("https://arxiv.org/pdf/1706.03762"));
        assert!(!looks_like_pdf(
            "https://doi.org/10.1371/journal.pmed.1000097"
        ));
    }

    #[test]
    fn set_dedupes_and_upgrades_to_pdf() {
        let mut set = CandidateSet::default();
        set.push(candidate(
            Source::Openalex,
            CandKind::Landing,
            "http://a.org/x.pdf",
        ));
        let mut pdf = candidate(Source::Openalex, CandKind::Pdf, "https://a.org/x.pdf");
        if let Some(c) = pdf.as_mut() {
            c.license = Some("cc-by".to_owned());
        }
        set.push(pdf);
        set.push(candidate(
            Source::Openalex,
            CandKind::Pdf,
            "https://a.org/x.pdf?v=2",
        ));
        set.push(candidate(Source::Openalex, CandKind::Pdf, "not a url"));
        let items = set.into_vec();
        assert_eq!(items.len(), 2);
        assert_eq!(
            (items[0].kind, items[0].license.as_deref()),
            (CandKind::Pdf, Some("cc-by"))
        );
    }
}
