//! Identifier normalisation. Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! Every function is total: any text, however hostile (NULs, 10 KB of junk, unicode look-alikes), yields `None` or a
//! normal value, never a panic. Cleaners return the canonical spelling the rest of the plugin compares and stores:
//! DOIs bare and lower-case, PMIDs as digits without leading zeros, PMCIDs as `PMC<digits>`, arXiv ids without a
//! version suffix.
use crate::model::{Ids, ItemFields, LearnedIds};
use crate::urls::{self, percent_decode};
use std::collections::BTreeMap;

/// Build [`Ids`] from the identifier-bearing fields of an item (phase 0 of every run).
///
/// Rules:
/// * DOI: `fields.doi`, else the first `DOI:` line of `fields.extra`, else a DOI embedded in `fields.url`
///   (`doi.org/...`, `dx.doi.org/...`), else a DOI-looking `archive_id` (preprints). Always [`clean_doi`]-normalised.
/// * PMID / PMCID: their fields, else `PMID:` / `PMCID:` lines of Extra.
/// * arXiv: `10.48550/arXiv.X` DOI, `archive_id` (`arXiv:X`), an `arxiv.org/abs|pdf/X` URL, or Extra.
/// * `rxiv` is true when the DOI starts with `10.1101/`.
/// * `url` keeps the item URL only when it is http(s).
/// * `sources` records, per id name, where it came from (`"field"`, `"extra"`, `"url"`, `"archive_id"`, `"doi"` for an
///   arXiv id derived from the DOI).
///
/// Empty or whitespace-only fields are ignored and no identifier is ever invented.
pub fn normalise(fields: &ItemFields) -> Ids {
    let mut ids = Ids::default();

    let doi = first_hit([
        (clean_doi(&fields.doi), "field"),
        (
            extra_field(&fields.extra, "DOI")
                .as_deref()
                .and_then(clean_doi),
            "extra",
        ),
        (doi_from_url(&fields.url), "url"),
        (clean_doi(&fields.archive_id), "archive_id"),
    ]);
    ids.doi = record(&mut ids, "doi", doi);
    ids.rxiv = ids.doi.as_deref().is_some_and(is_rxiv_doi);

    let pmid = first_hit([
        (clean_pmid(&fields.pmid), "field"),
        (
            extra_field(&fields.extra, "PMID")
                .as_deref()
                .and_then(clean_pmid),
            "extra",
        ),
    ]);
    ids.pmid = record(&mut ids, "pmid", pmid);

    let pmcid = first_hit([
        (clean_pmcid(&fields.pmcid), "field"),
        (
            extra_field(&fields.extra, "PMCID")
                .as_deref()
                .and_then(clean_pmcid),
            "extra",
        ),
    ]);
    ids.pmcid = record(&mut ids, "pmcid", pmcid);

    let arxiv = first_hit([
        (ids.doi.as_deref().and_then(arxiv_from_doi), "doi"),
        (clean_arxiv(&fields.archive_id), "archive_id"),
        (clean_arxiv(&fields.url), "url"),
        (
            extra_field(&fields.extra, "arXiv")
                .as_deref()
                .and_then(clean_arxiv),
            "extra",
        ),
        (
            extra_field(&fields.extra, "arXiv ID")
                .as_deref()
                .and_then(clean_arxiv),
            "extra",
        ),
    ]);
    ids.arxiv = record(&mut ids, "arxiv", arxiv);

    ids.url = urls::parse_http(&fields.url).map(|_| fields.url.trim().to_owned());
    ids
}

/// The first `(Some(value), origin)` pair, in order.
fn first_hit<const N: usize>(
    candidates: [(Option<String>, &'static str); N],
) -> Option<(String, &'static str)> {
    candidates
        .into_iter()
        .find_map(|(value, origin)| value.map(|v| (v, origin)))
}

/// Remember where `hit` came from under `name` in `ids.sources` and hand back the value.
fn record(ids: &mut Ids, name: &str, hit: Option<(String, &'static str)>) -> Option<String> {
    let (value, origin) = hit?;
    ids.sources.insert(name.to_owned(), origin.to_owned());
    Some(value)
}

/// A DOI inside a `doi.org` / `dx.doi.org` URL (other hosts may embed DOIs, but only the resolver is certain).
fn doi_from_url(url: &str) -> Option<String> {
    let host = urls::host_of(url)?;
    matches!(
        host.strip_prefix("www.").unwrap_or(&host),
        "doi.org" | "dx.doi.org"
    )
    .then(|| clean_doi(url))?
}

/// Normalise a DOI from any common written form to the bare lower-case registrant/suffix string.
///
/// Accepts `10.1371/journal.pmed.1000097`, `doi:10...`, `DOI: 10...`, `https://doi.org/10...`, `http://dx.doi.org/10...`,
/// percent-encoded slashes (`10.1002%2Fbjs.11646`), URLs with query/fragment, surrounding whitespace and trailing
/// sentence punctuation. Returns `None` when no `10.<4-9 digits>/<suffix>` core is present. Balanced trailing
/// parentheses are kept (`10.1002/(sici)1097-0258(19980430)17:8<873::aid-sim780>3.0.co;2-i` style DOIs).
pub fn clean_doi(raw: &str) -> Option<String> {
    let s = raw.trim();
    let start = find_doi_start(s)?;
    let tail = s.get(start..)?;
    let url_like = s.contains("://") || s.contains("%2F") || s.contains("%2f");
    let end = tail
        .find(|c: char| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '"' | '<' | '>')
                || (url_like && matches!(c, '?' | '#'))
        })
        .unwrap_or(tail.len());
    let mut doi = tail.get(..end)?.to_owned();
    if url_like && doi.contains('%') {
        doi = percent_decode(&doi);
    }
    let doi = trim_trailing(&doi).to_ascii_lowercase();
    let (prefix, suffix) = doi.split_once('/')?;
    (prefix.starts_with("10.") && !suffix.is_empty()).then_some(doi)
}

/// Byte offset of the `10.` that starts a DOI core inside `s`, if any.
fn find_doi_start(s: &str) -> Option<usize> {
    for (i, _) in s.match_indices("10.") {
        let before_ok = s
            .get(..i)
            .and_then(|p| p.chars().next_back())
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        if !before_ok {
            continue;
        }
        let Some(rest) = s.get(i + 3..) else { continue };
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if !(4..=9).contains(&digits) {
            continue;
        }
        let after = rest.get(digits..).unwrap_or("");
        let slash = after.starts_with('/')
            || after
                .get(..3)
                .is_some_and(|p| p.eq_ignore_ascii_case("%2f"));
        if slash {
            return Some(i);
        }
    }
    None
}

/// Strip trailing sentence punctuation and unbalanced closing brackets.
fn trim_trailing(s: &str) -> &str {
    // Bracket counts are taken once and kept in step as brackets are dropped, so long runs stay linear.
    let count = |c: char| s.matches(c).count();
    let (open_paren, mut close_paren, open_bracket, mut close_bracket) =
        (count('('), count(')'), count('['), count(']'));
    let mut t = s;
    loop {
        t = t.trim_end_matches(['.', ',', ';', ':', '\'']);
        if t.ends_with(')') && open_paren < close_paren {
            close_paren -= 1;
        } else if t.ends_with(']') && open_bracket < close_bracket {
            close_bracket -= 1;
        } else {
            return t;
        }
        // The dropped bracket is one byte.
        t = t.get(..t.len() - 1).unwrap_or(t);
    }
}

/// Separators that may sit between a label (`PMID`, `arXiv`, ...) and its value.
fn is_label_sep(c: char) -> bool {
    c == ':' || c == '=' || c == '#' || c.is_whitespace()
}

/// `s` without a leading ASCII-case-insensitive `label` and the separators after it; `None` when `s` does not start
/// with the label.
fn strip_label<'a>(s: &'a str, label: &str) -> Option<&'a str> {
    let head = s.get(..label.len())?;
    head.eq_ignore_ascii_case(label).then(|| {
        s.get(label.len()..)
            .unwrap_or("")
            .trim_start_matches(is_label_sep)
    })
}

/// `Some(s)` when `s` is a non-empty run of ASCII digits.
fn all_digits(s: &str) -> Option<&str> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then_some(s)
}

/// Digits without leading zeros, at most `max_len` of them, not zero.
fn plain_number(digits: &str, max_len: usize) -> Option<String> {
    let digits = all_digits(digits)?.trim_start_matches('0');
    (!digits.is_empty() && digits.len() <= max_len).then(|| digits.to_owned())
}

/// PMID: `"19621072"`, `"PMID: 19621072"`, `"pmid:19621072"`, `https://pubmed.ncbi.nlm.nih.gov/19621072/`.
/// One to nine digits; leading zeros are dropped (`"007"` -> `"7"`); anything else is `None`.
pub fn clean_pmid(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.contains("://") {
        let url = urls::parse_http(s)?;
        let host = url.host_str()?.to_ascii_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host);
        if !matches!(
            host,
            "pubmed.ncbi.nlm.nih.gov" | "ncbi.nlm.nih.gov" | "pubmed.gov"
        ) {
            return None;
        }
        // `/19621072/`, `/pubmed/19621072`: the last non-empty path segment.
        let id = url.path_segments()?.rfind(|seg| !seg.is_empty())?;
        return plain_number(id, 9);
    }
    let value = strip_label(s, "pmid")
        .or_else(|| strip_label(s, "pubmed"))
        .unwrap_or(s);
    plain_number(value.trim_end_matches('/'), 9)
}

/// PMCID: `"PMC2707599"`, `"pmc2707599"`, `"2707599"` (digits only are accepted when the caller knows the field is a
/// PMCID), `"PMC2707599.1"`, `"PMCID: PMC2707599"`, a `/pmc/articles/PMC2707599/` URL. Returns `"PMC<digits>"`;
/// a DOI or other text is `None`.
pub fn clean_pmcid(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.contains("://") || s.starts_with('/') {
        return pmcid_from_url_path(s);
    }
    let rest = strip_label(s, "pmcid").unwrap_or(s);
    let digits = match strip_label(rest, "pmc") {
        // With the `PMC` label a `.<version>` suffix and trailing text are fine.
        Some(after) => {
            let n = after.bytes().take_while(u8::is_ascii_digit).count();
            let (digits, tail) = after.split_at_checked(n)?;
            let tail_ok = tail.is_empty()
                || tail.starts_with(char::is_whitespace)
                || tail
                    .strip_prefix('.')
                    .is_some_and(|v| all_digits(v.trim_end_matches('/')).is_some());
            tail_ok.then_some(digits)?
        }
        None => rest,
    };
    plain_number(digits, 10).map(|n| format!("PMC{n}"))
}

/// The PMCID in a PMC / Europe PMC article URL or a bare `/pmc/articles/...` path.
fn pmcid_from_url_path(s: &str) -> Option<String> {
    let (path, pmc_host) = if s.starts_with('/') {
        (s.split(['?', '#']).next()?.to_owned(), false)
    } else {
        let url = urls::parse_http(s)?;
        let host = url.host_str()?.to_ascii_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host);
        if !matches!(
            host,
            "pmc.ncbi.nlm.nih.gov" | "ncbi.nlm.nih.gov" | "europepmc.org"
        ) {
            return None;
        }
        (url.path().to_owned(), true)
    };
    let lower = path.to_ascii_lowercase();
    // Legacy `/pmc/articles/2707599` carries bare digits; a bare `/articles/<digits>` is only trusted on a PMC host.
    let (after, legacy) = match lower.split_once("/pmc/articles/") {
        Some((_, after)) => (after, true),
        None => (lower.split_once("/articles/")?.1, false),
    };
    let segment = after.split('/').next()?;
    let digits = match segment.strip_prefix("pmc") {
        Some(digits) => digits,
        None if legacy || pmc_host => segment,
        None => return None,
    };
    plain_number(digits.split('.').next()?, 10).map(|n| format!("PMC{n}"))
}

/// arXiv id without version suffix: `"arXiv:1706.03762v5"`, `"1706.03762"`, `"cs/0112017"`, `"hep-th/9901001v2"`,
/// `"arXiv:1706.03762 [cs.CL]"`, `https://arxiv.org/abs/1706.03762v5`, `https://arxiv.org/pdf/1706.03762.pdf`.
/// Returns `1706.03762` / `cs/0112017`. New-style ids are `YYMM.NNNN[N]`; old-style ids are `archive[.XX]/YYMMNNN`.
pub fn clean_arxiv(raw: &str) -> Option<String> {
    let s = raw.trim();
    let candidate: String = if s.contains("://") || has_arxiv_host_prefix(s) {
        let full = if s.contains("://") {
            s.to_owned()
        } else {
            format!("https://{s}")
        };
        let url = urls::parse_http(&full)?;
        let host = url.host_str()?.to_ascii_lowercase();
        if !matches!(
            host.as_str(),
            "arxiv.org" | "www.arxiv.org" | "export.arxiv.org"
        ) {
            return None;
        }
        let path = url.path();
        let id = path
            .strip_prefix("/abs/")
            .or_else(|| path.strip_prefix("/pdf/"))?;
        id.trim_matches('/').to_owned()
    } else {
        // `arXiv:1706.03762v5 [cs.CL]`: the label is optional, the first token is the id.
        strip_label(s, "arxiv")
            .unwrap_or(s)
            .split_whitespace()
            .next()?
            .to_owned()
    };
    let id = strip_suffix_ci(&candidate, ".pdf");
    arxiv_id(strip_version(id))
}

/// `arxiv.org/abs/...` written without a scheme.
fn has_arxiv_host_prefix(s: &str) -> bool {
    ["arxiv.org/", "www.arxiv.org/"]
        .iter()
        .any(|p| s.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p)))
}

fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> &'a str {
    match s.len().checked_sub(suffix.len()).and_then(|n| s.get(n..)) {
        Some(tail) if tail.eq_ignore_ascii_case(suffix) => {
            s.get(..s.len() - suffix.len()).unwrap_or(s)
        }
        _ => s,
    }
}

/// Drop a trailing `v<digits>`.
fn strip_version(id: &str) -> &str {
    let Some(pos) = id.rfind(['v', 'V']) else {
        return id;
    };
    let (head, tail) = id.split_at_checked(pos).unwrap_or((id, ""));
    match tail.get(1..) {
        Some(digits) if !head.is_empty() && all_digits(digits).is_some() => head,
        _ => id,
    }
}

/// Validate an unversioned arXiv id and return its canonical spelling.
fn arxiv_id(id: &str) -> Option<String> {
    if let Some((yymm, num)) = id.split_once('.')
        && yymm.len() == 4
        && all_digits(yymm).is_some()
        && (4..=5).contains(&num.len())
        && all_digits(num).is_some()
    {
        return Some(id.to_owned());
    }
    // Old style: `hep-th/9901001`, `math.GT/0309136`.
    let (archive, number) = id.split_once('/')?;
    if number.len() != 7 || all_digits(number).is_none() {
        return None;
    }
    let (name, class) = match archive.split_once('.') {
        Some((name, class)) => (name, Some(class)),
        None => (archive, None),
    };
    let name = name.to_ascii_lowercase();
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
        return None;
    }
    match class {
        None => Some(format!("{name}/{number}")),
        Some(c) if c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic()) => {
            Some(format!("{name}.{}/{number}", c.to_ascii_uppercase()))
        }
        Some(_) => None,
    }
}

/// `10.48550/arxiv.1706.03762` -> `1706.03762` (the DOI may be in any case).
pub fn arxiv_from_doi(doi: &str) -> Option<String> {
    let prefix = "10.48550/arxiv.";
    let head = doi.trim().get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| doi.trim().get(prefix.len()..))
        .flatten()
        .and_then(clean_arxiv)
}

/// bioRxiv and medRxiv DOIs share the `10.1101/` prefix.
pub fn is_rxiv_doi(doi: &str) -> bool {
    doi.starts_with("10.1101/")
}

/// Value of a `Key: value` line in a Zotero Extra field (case-insensitive key, first non-empty match wins).
///
/// A line only counts when it STARTS with the key followed by a colon, so prose that merely mentions `DOI:` is
/// ignored; `Key:value` without a space is accepted; the value is trimmed.
pub fn extra_field(extra: &str, key: &str) -> Option<String> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    extra.split(['\n', '\r']).find_map(|line| {
        let rest = strip_key(line.trim_start(), key)?;
        let value = rest.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

/// `line` after `<key>:` when it starts with the key (ASCII case-insensitive).
fn strip_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let head = line.get(..key.len())?;
    head.eq_ignore_ascii_case(key)
        .then(|| line.get(key.len()..))??
        .strip_prefix(':')
}

/// Fill identifiers that are still missing in `ids` from what an API taught us; never overwrite a known id.
/// Learned values go through the same cleaners as item fields (invalid ones are ignored). Records `source` (for
/// example `"openalex"`) in `ids.sources` and sets `rxiv` for a learned `10.1101/` DOI. Returns true when anything
/// changed.
pub fn apply_learned(ids: &mut Ids, learned: &LearnedIds, source: &str) -> bool {
    let mut changed = false;
    changed |= fill(
        &mut ids.doi,
        &mut ids.sources,
        "doi",
        learned.doi.as_deref().and_then(clean_doi),
        source,
    );
    changed |= fill(
        &mut ids.pmid,
        &mut ids.sources,
        "pmid",
        learned.pmid.as_deref().and_then(clean_pmid),
        source,
    );
    changed |= fill(
        &mut ids.pmcid,
        &mut ids.sources,
        "pmcid",
        learned.pmcid.as_deref().and_then(clean_pmcid),
        source,
    );
    changed |= fill(
        &mut ids.arxiv,
        &mut ids.sources,
        "arxiv",
        learned.arxiv.as_deref().and_then(clean_arxiv),
        source,
    );
    ids.rxiv = ids.rxiv || ids.doi.as_deref().is_some_and(is_rxiv_doi);
    changed
}

/// Put `value` into an empty `slot` and note its origin; a filled slot is never touched.
fn fill(
    slot: &mut Option<String>,
    sources: &mut BTreeMap<String, String>,
    name: &str,
    value: Option<String>,
    origin: &str,
) -> bool {
    match (slot.is_none(), value) {
        (true, Some(value)) => {
            *slot = Some(value);
            sources.insert(name.to_owned(), origin.to_owned());
            true
        }
        _ => false,
    }
}

/// Case-insensitive DOI comparison after [`clean_doi`] on both sides.
pub fn same_doi(a: &str, b: &str) -> bool {
    matches!((clean_doi(a), clean_doi(b)), (Some(x), Some(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_doi_forms() {
        let want = Some("10.1371/journal.pmed.1000097".to_owned());
        for raw in [
            "10.1371/journal.pmed.1000097",
            " DOI: 10.1371/JOURNAL.PMED.1000097. ",
            "doi:10.1371/journal.pmed.1000097",
            "https://doi.org/10.1371/journal.pmed.1000097",
            "http://dx.doi.org/10.1371/journal.pmed.1000097?foo=bar#frag",
            "https://doi.org/10.1371%2Fjournal.pmed.1000097",
        ] {
            assert_eq!(clean_doi(raw), want, "{raw}");
        }
    }

    #[test]
    fn clean_doi_rejects_non_dois() {
        for raw in ["", "hello", "10.12/short", "110.1234/x", "10.1234/"] {
            assert_eq!(clean_doi(raw), None, "{raw}");
        }
    }

    #[test]
    fn clean_doi_keeps_balanced_parentheses() {
        assert_eq!(
            clean_doi("10.1016/s0140-6736(20)30183-5").as_deref(),
            Some("10.1016/s0140-6736(20)30183-5")
        );
        assert_eq!(
            clean_doi("(10.1016/s0140-6736(20)30183-5)").as_deref(),
            Some("10.1016/s0140-6736(20)30183-5")
        );
    }

    #[test]
    fn rxiv_and_same_doi() {
        assert!(is_rxiv_doi("10.1101/2023.05.31.23290770"));
        assert!(same_doi(
            "DOI:10.1097/SLA.0000000000005603",
            "https://doi.org/10.1097/sla.0000000000005603"
        ));
    }

    #[test]
    fn pmid_pmcid_arxiv_basics() {
        assert_eq!(
            clean_pmid("PMID: 0019621072").as_deref(),
            Some("19621072"),
            "leading zeros are dropped"
        );
        assert_eq!(clean_pmid("pmid:19621072").as_deref(), Some("19621072"));
        assert_eq!(
            clean_pmcid("PMCID: PMC2707599.1").as_deref(),
            Some("PMC2707599")
        );
        assert_eq!(clean_pmcid("10.1371/journal.pmed.1000097"), None);
        assert_eq!(
            clean_arxiv("arXiv:1706.03762v5 [cs.CL]").as_deref(),
            Some("1706.03762")
        );
        assert_eq!(
            arxiv_from_doi("10.48550/arXiv.1706.03762").as_deref(),
            Some("1706.03762")
        );
    }
}
