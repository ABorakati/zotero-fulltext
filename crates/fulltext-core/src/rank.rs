//! Candidate merging, dedupe, scoring and tiering. Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! Every OA parser and the page extractor produce [`Candidate`]s; the engine pools them per item with [`merge`] and
//! calls [`rank`] to get the attempt order.
//!
//! [`rank`] is a pure function of the SET of candidates: input order never matters. That is deliberate. Candidates
//! arrive in the order concurrent API calls complete, and the same pool must always produce the same attempt order
//! (reproducible runs, stable simulation tests). Ties are therefore broken by the URL and the source, not by arrival
//! position, and `rank(rank(x)) == rank(x)`.
use crate::model::{CandKind, Candidate, HostKind, Ids, Source, Version};
use crate::{ids, oa, urls};
use std::cmp::Ordering;
use url::Url;

/// Attempt tiers written to `Candidate.tier`. Lower is tried earlier; the engine runs tiers sequentially and stops
/// at the first verified `%PDF`. Mirrors research/SUMMARY.md section (d) "Phase 2".
pub mod tier {
    /// PMC Cloud Service S3 PDF (no bot wall).
    pub const PMC_S3: u8 = 1;
    /// `https://europepmc.org/api/getPdf?pmcid=PMC<n>`.
    pub const EUROPEPMC_GETPDF: u8 = 2;
    /// `https://arxiv.org/pdf/<id>`.
    pub const ARXIV: u8 = 3;
    /// bioRxiv / medRxiv `early/YYYY/MM/DD/<suffix>.full.pdf`.
    pub const RXIV_EARLY: u8 = 4;
    /// Direct `pdf_url` on a publisher host (gold / hybrid first).
    pub const DIRECT_PUBLISHER: u8 = 5;
    /// Direct `pdf_url` on a repository / aggregator host.
    pub const DIRECT_REPOSITORY: u8 = 6;
    /// Landing page to run the extractor / translators on.
    pub const LANDING: u8 = 7;
    /// Only reachable through a real browser (challenge-walled hosts such as the PMC website PDF).
    pub const BROWSER_ONLY: u8 = 8;
    /// Paid OpenAlex content API (needs the user's key).
    pub const PAID_API: u8 = 9;
}

/// Inputs to ranking that are not on the candidates themselves.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RankPrefs {
    /// Preferred version order (`OaConfig.version_order`); versions missing from it rank last. Empty = published,
    /// accepted, submitted, unknown.
    pub version_order: Vec<Version>,
    /// What the user asked for; a candidate whose `record_doi` differs from `requested.doi` (and whose source is
    /// not a recognised repository) is down-weighted, never dropped (OpenAlex merges records).
    pub requested: Ids,
    /// Keep at most this many candidates (0 = unlimited).
    pub max_candidates: usize,
}

/// A candidate with every optional field at its default (`score` 0, `tier` 0, `Version::Unknown`, `HostKind::Other`).
pub fn new_candidate(url: impl Into<String>, kind: CandKind, source: Source) -> Candidate {
    Candidate {
        url: url.into(),
        kind,
        source,
        version: Version::Unknown,
        license: None,
        host_kind: Default::default(),
        record_doi: None,
        landing_url: None,
        signal: None,
        score: 0,
        tier: 0,
        needs_browser: false,
    }
}

const DEFAULT_VERSION_ORDER: [Version; 4] = [
    Version::Published,
    Version::Accepted,
    Version::Submitted,
    Version::Unknown,
];

fn version_order(prefs: &RankPrefs) -> &[Version] {
    if prefs.version_order.is_empty() {
        &DEFAULT_VERSION_ORDER
    } else {
        &prefs.version_order
    }
}

/// Position of `v` in `order`; versions that are not listed rank after all listed ones.
fn version_rank(order: &[Version], v: Version) -> usize {
    order.iter().position(|o| *o == v).unwrap_or(order.len())
}

/// Add `more` to `pool`, merging candidates with the same [`crate::urls::canonical_key`]: keep the better version
/// (published first), the licence if either has one, the lower non-zero tier and the higher score; the candidate
/// already in the pool keeps its `source` on ties.
pub fn merge(pool: &mut Vec<Candidate>, more: Vec<Candidate>) {
    for c in more {
        let key = urls::canonical_key(&c.url);
        match pool
            .iter_mut()
            .find(|existing| urls::canonical_key(&existing.url) == key)
        {
            Some(existing) => absorb(existing, c, &DEFAULT_VERSION_ORDER, None),
            None => pool.push(c),
        }
    }
}

/// Fold `other` (same URL) into `base`.
fn absorb(base: &mut Candidate, other: Candidate, order: &[Version], requested: Option<&Ids>) {
    if version_rank(order, other.version) < version_rank(order, base.version) {
        base.version = other.version;
    }
    base.license = base.license.take().or(other.license);
    base.tier = match (base.tier, other.tier) {
        (0, t) | (t, 0) => t,
        (a, b) => a.min(b),
    };
    base.score = base.score.max(other.score);
    if other.kind == CandKind::Pdf {
        base.kind = CandKind::Pdf;
    }
    base.needs_browser |= other.needs_browser;
    if base.host_kind == HostKind::Other {
        base.host_kind = other.host_kind;
    }
    base.landing_url = base.landing_url.take().or(other.landing_url);
    base.signal = base.signal.take().or(other.signal);
    // A record DOI that confirms the request beats one that does not.
    let wanted = requested.and_then(|r| r.doi.as_deref());
    let confirms = |doi: &Option<String>| matches!((doi.as_deref(), wanted), (Some(d), Some(w)) if ids::same_doi(d, w));
    if base.record_doi.is_none() || (!confirms(&base.record_doi) && confirms(&other.record_doi)) {
        base.record_doi = other.record_doi;
    }
}

/// Score one candidate (higher is better). Pure: it reads the candidate's fields, never its stored `score`. Used by
/// [`rank`] and exposed for the extractor's own sorting.
///
/// Base 100; version bonus by position in the preferred order (+20, +10, +4, then 0; unlisted versions -5);
/// Creative Commons licence +5; publisher host +10; aggregator host -30; a `record_doi` that differs from the
/// requested DOI on an aggregator or unrecognised host (the OpenAlex merged-work mirror case) -40; browser-only -10;
/// a Crossref text-mining link -15; a bioRxiv canonical URL (often WAF-blocked) -5.
pub fn score(c: &Candidate, prefs: &RankPrefs) -> i32 {
    let mut s = 100;
    s += match version_order(prefs).iter().position(|v| *v == c.version) {
        Some(0) => 20,
        Some(1) => 10,
        Some(2) => 4,
        Some(_) => 0,
        None => -5,
    };
    if c.license.as_deref().is_some_and(|l| l.starts_with("cc")) {
        s += 5;
    }
    match c.host_kind {
        HostKind::Publisher => s += 10,
        HostKind::Aggregator => s -= 30,
        _ => {}
    }
    let foreign_record = match (
        c.record_doi.as_deref().and_then(ids::clean_doi),
        prefs.requested.doi.as_deref().and_then(ids::clean_doi),
    ) {
        (Some(record), Some(wanted)) => record != wanted,
        _ => false,
    };
    if foreign_record && matches!(c.host_kind, HostKind::Aggregator | HostKind::Other) {
        s -= 40;
    }
    if c.needs_browser || oa::is_browser_walled(&c.url) {
        s -= 10;
    }
    match c.signal.as_deref() {
        Some("crossref:link_tdm") => s -= 15,
        Some("biorxiv:canonical") => s -= 5,
        _ => {}
    }
    s
}

/// Drop unusable candidates, normalise URL aliases, merge duplicates, assign `tier` and `score`, remove candidates
/// that a better one supersedes, sort and truncate to `prefs.max_candidates`.
///
/// In order: (1) candidates without an http(s) URL are dropped; (2) aliases are rewritten (`http` -> `https`,
/// `www.ncbi.nlm.nih.gov/pmc/articles/N` -> `pmc.ncbi.nlm.nih.gov/articles/PMC<N>/`, `arxiv.org/abs/X` ->
/// `arxiv.org/pdf/X` as a `Pdf`); (3) duplicates are merged by canonical URL (query included, so two PLOS
/// `file?id=..&type=` variants stay apart; arXiv versions of one id merge); (4) `tier` is assigned; (5) the walled
/// PMC / Europe PMC PDFs are dropped when a PMC S3 or Europe PMC `getPdf` candidate exists, and `core.ac.uk`
/// candidates when a non-aggregator PDF exists; (6) `score` is assigned; (7) the list is sorted by tier, descending
/// score, preferred version order, then URL; (8) it is truncated.
pub fn rank(pool: Vec<Candidate>, prefs: &RankPrefs) -> Vec<Candidate> {
    let order = version_order(prefs);

    // (merge key, is the exact canonical URL, candidate). Within a key the exact URL comes first, so the unversioned
    // arXiv URL is the one that survives.
    let mut items: Vec<(String, bool, Candidate)> = pool
        .into_iter()
        .filter_map(normalise)
        .map(|c| {
            let key = rank_key(&c.url);
            let exact = urls::canonical_key(&c.url) == key;
            (key, exact, c)
        })
        .collect();
    items.sort_by(|(ka, ea, a), (kb, eb, b)| {
        ka.cmp(kb).then(eb.cmp(ea)).then_with(|| total_order(a, b))
    });
    let mut merged: Vec<Candidate> = Vec::new();
    let mut previous_key: Option<String> = None;
    for (key, _, c) in items {
        match merged.last_mut() {
            Some(base) if previous_key.as_deref() == Some(key.as_str()) => {
                absorb(base, c, order, Some(&prefs.requested))
            }
            _ => merged.push(c),
        }
        previous_key = Some(key);
    }

    for c in &mut merged {
        c.needs_browser |= oa::is_browser_walled(&c.url);
        c.tier = tier_of(c);
    }
    let has_open_pmc = merged
        .iter()
        .any(|c| matches!(c.tier, tier::PMC_S3 | tier::EUROPEPMC_GETPDF));
    let has_real_pdf = merged
        .iter()
        .any(|c| c.kind == CandKind::Pdf && c.host_kind != HostKind::Aggregator);
    let superseded = |c: &Candidate| {
        (has_open_pmc && oa::is_browser_walled(&c.url)) || (has_real_pdf && is_core_ac_uk(&c.url))
    };
    merged.retain(|c| !superseded(c));
    for c in &mut merged {
        c.score = score(c, prefs);
    }

    let mut ordered: Vec<(String, Candidate)> =
        merged.into_iter().map(|c| (rank_key(&c.url), c)).collect();
    ordered.sort_by(|(ka, a), (kb, b)| {
        a.tier
            .cmp(&b.tier)
            .then(b.score.cmp(&a.score))
            .then(version_rank(order, a.version).cmp(&version_rank(order, b.version)))
            .then_with(|| ka.cmp(kb))
            .then_with(|| total_order(a, b))
    });
    let mut ranked: Vec<Candidate> = ordered.into_iter().map(|(_, c)| c).collect();
    if prefs.max_candidates > 0 {
        ranked.truncate(prefs.max_candidates);
    }
    ranked
}

/// Every field, so that sorting never depends on input order.
fn total_order(a: &Candidate, b: &Candidate) -> Ordering {
    fn plain(c: &Candidate) -> (Source, CandKind, Version, HostKind, bool, i32, u8) {
        (
            c.source,
            c.kind,
            c.version,
            c.host_kind,
            c.needs_browser,
            c.score,
            c.tier,
        )
    }
    fn texts(c: &Candidate) -> (&str, Option<&str>, Option<&str>, Option<&str>, Option<&str>) {
        (
            &c.url,
            c.license.as_deref(),
            c.record_doi.as_deref(),
            c.landing_url.as_deref(),
            c.signal.as_deref(),
        )
    }
    plain(a)
        .cmp(&plain(b))
        .then_with(|| texts(a).cmp(&texts(b)))
}

/// The merge key: canonical URL, with arXiv version suffixes removed (`/pdf/1706.03762v5` is `/pdf/1706.03762`).
fn rank_key(url: &str) -> String {
    let key = urls::canonical_key(url);
    match key.strip_prefix("https://arxiv.org/pdf/") {
        Some(id) => {
            ids::clean_arxiv(id).map_or(key.clone(), |base| format!("https://arxiv.org/pdf/{base}"))
        }
        None => key,
    }
}

fn is_core_ac_uk(url: &str) -> bool {
    urls::host_of(url).is_some_and(|h| h == "core.ac.uk" || h.ends_with(".core.ac.uk"))
}

/// Steps (1) and (2) of [`rank`] for one candidate; `None` drops it.
fn normalise(mut c: Candidate) -> Option<Candidate> {
    let mut url = urls::parse_http(&c.url)?;
    if url.scheme() == "http" && url.port().is_none() {
        url.set_scheme("https").ok()?;
    }
    url.set_fragment(None);
    if let Some(pmc) = pmc_alias(&url) {
        c.url = pmc;
    } else if let Some(id) = arxiv_id_of_page(&url) {
        // An abstract page and the PDF are one document: ask for the PDF, remember the page.
        if c.landing_url.is_none() && url.path().starts_with("/abs/") {
            c.landing_url = Some(format!("https://arxiv.org/abs/{id}"));
        }
        c.url = format!("https://arxiv.org/pdf/{id}");
        c.kind = CandKind::Pdf;
    } else {
        c.url = url.into();
    }
    if c.host_kind == HostKind::Other {
        c.host_kind = oa::host_kind(&c.url);
    }
    Some(c)
}

/// `www.ncbi.nlm.nih.gov/pmc/articles/PMC123/...` and `.../pmc/articles/123` -> `pmc.ncbi.nlm.nih.gov/articles/PMC123/...`.
fn pmc_alias(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let after = match host {
        "ncbi.nlm.nih.gov" => url.path().strip_prefix("/pmc/articles/")?,
        "pmc.ncbi.nlm.nih.gov" => url.path().strip_prefix("/articles/")?,
        _ => return None,
    };
    let (id, tail) = after.split_once('/').unwrap_or((after, ""));
    let digits = id
        .get(..3)
        .filter(|p| p.eq_ignore_ascii_case("pmc"))
        .and_then(|_| id.get(3..))
        .unwrap_or(id);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut out = format!("https://pmc.ncbi.nlm.nih.gov/articles/PMC{digits}/{tail}");
    if let Some(query) = url.query().filter(|_| !tail.is_empty()) {
        out.push('?');
        out.push_str(query);
    }
    Some(out)
}

/// The id of an `arxiv.org/abs/<id>` or `arxiv.org/pdf/<id>[.pdf]` URL (version suffix kept).
fn arxiv_id_of_page(url: &Url) -> Option<String> {
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
    let id = id.trim_end_matches('/');
    let id = id.strip_suffix(".pdf").unwrap_or(id);
    (!id.is_empty()).then(|| id.to_owned())
}

/// Step (4): the attempt tier of one candidate (first matching rule wins).
fn tier_of(c: &Candidate) -> u8 {
    let parsed = urls::parse_http(&c.url);
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = parsed
        .as_ref()
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_default();
    let signal = c.signal.as_deref().unwrap_or("");
    let is_pdf = c.kind == CandKind::Pdf;
    let rxiv_host = host == "biorxiv.org"
        || host.ends_with(".biorxiv.org")
        || host == "medrxiv.org"
        || host.ends_with(".medrxiv.org");

    if host == "pmc-oa-opendata.s3.amazonaws.com" {
        tier::PMC_S3
    } else if host == "europepmc.org" && path.starts_with("/api/getpdf") {
        tier::EUROPEPMC_GETPDF
    } else if is_pdf && host == "arxiv.org" && path.starts_with("/pdf/") {
        tier::ARXIV
    } else if signal.contains("biorxiv:early_path")
        || signal.contains("biorxiv:canonical")
        || (is_pdf && rxiv_host)
    {
        tier::RXIV_EARLY
    } else if host == "content.openalex.org" || signal.ends_with(":content_api") {
        tier::PAID_API
    } else if c.needs_browser {
        tier::BROWSER_ONLY
    } else if is_pdf && c.host_kind == HostKind::Publisher {
        tier::DIRECT_PUBLISHER
    } else if is_pdf {
        tier::DIRECT_REPOSITORY
    } else {
        tier::LANDING
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf(url: &str) -> Candidate {
        new_candidate(url, CandKind::Pdf, Source::Openalex)
    }

    #[test]
    fn new_candidate_has_neutral_defaults() {
        let c = new_candidate(
            "https://arxiv.org/pdf/1706.03762",
            CandKind::Pdf,
            Source::Arxiv,
        );
        assert_eq!((c.score, c.tier, c.version), (0, 0, Version::Unknown));
        assert!(!c.needs_browser);
    }

    #[test]
    fn aliases() {
        let prefs = RankPrefs::default();
        let out = rank(
            vec![
                new_candidate(
                    "http://www.ncbi.nlm.nih.gov/pmc/articles/2707599",
                    CandKind::Landing,
                    Source::Openalex,
                ),
                new_candidate(
                    "https://www.ncbi.nlm.nih.gov/pmc/articles/PMC2707599/",
                    CandKind::Landing,
                    Source::Unpaywall,
                ),
                new_candidate(
                    "http://arxiv.org/abs/1706.03762",
                    CandKind::Landing,
                    Source::Openalex,
                ),
            ],
            &prefs,
        );
        let urls: Vec<&str> = out.iter().map(|c| c.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://arxiv.org/pdf/1706.03762",
                "https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/"
            ]
        );
        assert_eq!(out[0].kind, CandKind::Pdf);
        assert_eq!(
            out[0].landing_url.as_deref(),
            Some("https://arxiv.org/abs/1706.03762")
        );
    }

    #[test]
    fn merge_keeps_the_better_of_each_field() {
        let mut a = pdf("https://a.org/x.pdf");
        a.version = Version::Submitted;
        a.tier = 6;
        a.score = 3;
        let mut b = pdf("http://a.org/x.pdf#frag");
        b.version = Version::Published;
        b.license = Some("cc-by".to_owned());
        b.tier = 5;
        b.score = 9;
        let mut pool = vec![a];
        merge(&mut pool, vec![b, pdf("https://a.org/y.pdf")]);
        assert_eq!(pool.len(), 2);
        assert_eq!(
            (
                pool[0].version,
                pool[0].license.as_deref(),
                pool[0].tier,
                pool[0].score
            ),
            (Version::Published, Some("cc-by"), 5, 9)
        );
    }

    #[test]
    fn tiers_follow_the_documented_rules() {
        let mut walled = pdf("https://pmc.ncbi.nlm.nih.gov/articles/PMC1/pdf/a.pdf");
        walled.needs_browser = true;
        let mut rxiv = pdf("https://www.medrxiv.org/content/medrxiv/early/2023/06/01/x.full.pdf");
        rxiv.host_kind = HostKind::Preprint;
        let mut publisher = pdf("https://www.nature.com/articles/n.pdf");
        publisher.host_kind = HostKind::Publisher;
        let cases = [
            (
                pdf("https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf"),
                tier::PMC_S3,
            ),
            (
                pdf("https://europepmc.org/api/getPdf?pmcid=PMC1"),
                tier::EUROPEPMC_GETPDF,
            ),
            (pdf("https://arxiv.org/pdf/1706.03762"), tier::ARXIV),
            (rxiv, tier::RXIV_EARLY),
            (publisher, tier::DIRECT_PUBLISHER),
            (
                pdf("https://iris.example.it/a.pdf"),
                tier::DIRECT_REPOSITORY,
            ),
            (
                new_candidate("https://doi.org/10.1/x", CandKind::Landing, Source::Doi),
                tier::LANDING,
            ),
            (walled, tier::BROWSER_ONLY),
            (
                pdf("https://content.openalex.org/works/W1.pdf"),
                tier::PAID_API,
            ),
        ];
        for (c, want) in cases {
            let url = c.url.clone();
            let out = rank(vec![c], &RankPrefs::default());
            assert_eq!(out.first().map(|c| c.tier), Some(want), "{url}");
        }
    }
}
