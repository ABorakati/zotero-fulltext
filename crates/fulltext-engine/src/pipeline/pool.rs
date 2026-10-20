//! `CandidatePool`: merges candidates from all sources, dedupes (including the query string), tracks what was
//! tried, learns identifiers, and ranks. Synchronous and unit-tested natively. The scoring, tiering and merging
//! rules live in `fulltext_core::rank` (`merge`, `rank`, `tier`); this type owns the per-item state around them.
//!
//! Ranking (research/SUMMARY (d)): version preference `OaConfig.version_order`; then tier (PMC S3 PDF > Europe PMC
//! getPdf > arXiv > medRxiv early > publisher gold/hybrid `pdf_url` with matching record DOI > repository >
//! landing pages > challenge-walled copies); candidates whose `record_doi` differs from the requested DOI from an
//! unknown source are down-weighted by core (OpenAlex merges records).
//!
//! Engine-side additions around core's ranking:
//! * `OaConfig` toggles filter at read time (`ranked`): Europe PMC `getPdf`, the bioRxiv early path, arXiv.
//! * Crossref links (`Source::Doi`) are publisher URLs that usually need a subscription. They stay in the pool for
//!   the institutional phase but are never "strong" and a duplicate from an OA source takes the OA source over.
//! * A small per-URL bonus (set by landing extraction) orders candidates of one page inside a tier; core's own
//!   `rank` recomputes `score` and would lose the extractor's order.
//! * `extracted` tracks landing pages whose candidates were already taken (a page is never extracted twice).
//! * A preprint's published DOI (`published_doi:` note) is exposed once through `take_published_doi`; afterwards
//!   `requested_for` verifies candidates of that record against the published DOI instead of the preprint's.
use crate::pipeline::pure;
use crate::proto::OaConfig;
use fulltext_core::model::*;
use fulltext_core::rank::{self, RankPrefs, tier};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

/// Note prefix core puts on an `ApiInspection` that names the journal version of a preprint.
const PUBLISHED_NOTE: &str = "published_doi:";

/// "Strong" = worth starting to download right away (PMC S3, Europe PMC getPdf, arXiv, medRxiv early, publisher
/// gold/hybrid PDF). Tier 0 means "not ranked yet". Candidates that only name where an institution's copy lives
/// (Crossref links, Alma resolver entries) are never strong.
pub(crate) fn is_strong(c: &Candidate) -> bool {
    c.kind == CandKind::Pdf
        && (1..=tier::DIRECT_PUBLISHER).contains(&c.tier)
        && !matches!(c.source, Source::Doi | Source::Alma)
}

/// Does `oa` allow attempting this candidate?
fn allowed(c: &Candidate, oa: &OaConfig) -> bool {
    let host = pure::host_of(&c.url).unwrap_or_default();
    let path = c.url.to_ascii_lowercase();
    let is_get_pdf = host == "europepmc.org" && path.contains("/api/getpdf");
    (oa.europepmc_get_pdf || !is_get_pdf)
        && (oa.medrxiv_early || c.signal.as_deref() != Some("biorxiv:early_path"))
        && (oa.arxiv || c.source != Source::Arxiv)
}

#[derive(Clone, Debug)]
pub(crate) struct CandidatePool {
    ids: Ids,
    candidates: Vec<Candidate>,
    /// `canonical_key` of every URL ever tried (never fetch or extract twice).
    tried: BTreeSet<String>,
    /// `canonical_key` of every landing page whose candidates were already extracted.
    extracted: BTreeSet<String>,
    /// `canonical_key` -> ordering bonus inside a tier.
    bonus: BTreeMap<String, i32>,
    oa_status: Option<String>,
    /// Published DOI of a preprint that nobody has picked up yet.
    published_pending: Option<String>,
    /// Published DOI of a preprint (kept for `requested_for`).
    published: Option<String>,
    /// A lookup failed because the network is down.
    offline: bool,
}

impl CandidatePool {
    pub(crate) fn new(ids: Ids) -> Self {
        Self {
            ids,
            candidates: Vec::new(),
            tried: BTreeSet::new(),
            extracted: BTreeSet::new(),
            bonus: BTreeMap::new(),
            oa_status: None,
            published_pending: None,
            published: None,
            offline: false,
        }
    }

    pub(crate) fn ids(&self) -> &Ids {
        &self.ids
    }

    /// Fold in one API answer: learned ids (`ids::apply_learned`, never overwrites a known id), candidates
    /// (`rank::merge`), `oa_status`, a preprint's published DOI. Returns true when new ids were learned.
    pub(crate) fn merge(&mut self, api: &ApiInspection, source_name: &str) -> bool {
        let learned = pure::apply_learned(&mut self.ids, &api.learned, source_name);
        self.fold_in(api.candidates.clone());
        if self.oa_status.is_none() {
            self.oa_status = api.oa_status.clone();
        }
        if let Some(doi) = api
            .note
            .as_deref()
            .and_then(|n| n.strip_prefix(PUBLISHED_NOTE))
        {
            self.note_published(doi);
        }
        learned
    }

    /// Add one candidate (landing-page extraction, deterministic arXiv URL). Returns true when the pool grew.
    pub(crate) fn add(&mut self, c: Candidate) -> bool {
        let before = self.candidates.len();
        self.fold_in(vec![c]);
        self.candidates.len() > before
    }

    /// [`CandidatePool::add`] with an ordering bonus (added to the score when `ranked` orders one tier).
    pub(crate) fn add_boosted(&mut self, c: Candidate, bonus: i32) -> bool {
        let key = pure::canonical_key(&c.url);
        let grew = self.add(c);
        let entry = self.bonus.entry(key).or_insert(bonus);
        *entry = (*entry).max(bonus);
        grew
    }

    /// `rank::merge`, except that an OA source takes over a duplicate that Crossref reported first.
    fn fold_in(&mut self, incoming: Vec<Candidate>) {
        for c in &incoming {
            if c.source == Source::Doi {
                continue;
            }
            let key = pure::canonical_key(&c.url);
            if let Some(existing) = self
                .candidates
                .iter_mut()
                .find(|e| e.source == Source::Doi && pure::canonical_key(&e.url) == key)
            {
                existing.source = c.source;
            }
        }
        rank::merge(&mut self.candidates, incoming);
    }

    fn note_published(&mut self, doi: &str) {
        let Some(doi) = pure::clean_doi(doi) else {
            return;
        };
        let is_requested = self
            .ids
            .doi
            .as_deref()
            .is_some_and(|d| pure::same_doi(d, &doi));
        if !is_requested && self.published.is_none() {
            self.published_pending = Some(doi.clone());
            self.published = Some(doi);
        }
    }

    /// The published DOI of a preprint, the first time somebody asks (then `None`).
    pub(crate) fn take_published_doi(&mut self) -> Option<String> {
        self.published_pending.take()
    }

    /// The identifiers a response for `cand` must match: the item's, or the published record's when the candidate
    /// comes from the journal version of a preprint.
    pub(crate) fn requested_for(&self, cand: &Candidate) -> Ids {
        let from_published = match (cand.record_doi.as_deref(), self.published.as_deref()) {
            (Some(record), Some(published)) => pure::same_doi(record, published),
            _ => false,
        };
        if from_published {
            Ids {
                doi: self.published.clone(),
                rxiv: false,
                ..self.ids.clone()
            }
        } else {
            self.ids.clone()
        }
    }

    /// The published DOI of a preprint, if one was learned.
    pub(crate) fn published_doi(&self) -> Option<&str> {
        self.published.as_deref()
    }

    /// PMCID known so far (item field or learned): triggers the chained PMC S3 lookup.
    pub(crate) fn pmcid(&self) -> Option<&str> {
        self.ids.pmcid.as_deref()
    }

    /// True once a strong candidate is present (ranked first: raw candidates carry tier 0).
    pub(crate) fn has_strong(&self) -> bool {
        let prefs = RankPrefs {
            version_order: Vec::new(),
            requested: self.ids.clone(),
            max_candidates: 0,
        };
        rank::rank(self.candidates.clone(), &prefs)
            .iter()
            .any(is_strong)
    }

    /// Candidates not yet tried, best first (`rank::rank` with the user's version order, then the `OaConfig`
    /// toggles, then the extractor's bonus inside a tier).
    pub(crate) fn ranked(&self, oa: &OaConfig) -> Vec<Candidate> {
        let prefs = RankPrefs {
            version_order: oa.version_order.clone(),
            requested: self.ids.clone(),
            max_candidates: 0,
        };
        let mut out: Vec<Candidate> = rank::rank(self.candidates.clone(), &prefs)
            .into_iter()
            .filter(|c| !self.tried.contains(&pure::canonical_key(&c.url)) && allowed(c, oa))
            .collect();
        // Stable: equal keys keep core's (deterministic) order.
        out.sort_by_key(|c| {
            (
                c.tier,
                Reverse(c.score.saturating_add(self.bonus_of(&c.url))),
            )
        });
        out
    }

    fn bonus_of(&self, url: &str) -> i32 {
        self.bonus
            .get(&pure::canonical_key(url))
            .copied()
            .unwrap_or(0)
    }

    /// Record that a URL was fetched (never re-fetch it in this item). True when it was new.
    pub(crate) fn mark_tried(&mut self, url: &str) -> bool {
        self.tried.insert(pure::canonical_key(url))
    }

    pub(crate) fn was_tried(&self, url: &str) -> bool {
        self.tried.contains(&pure::canonical_key(url))
    }

    /// Record that a landing page was extracted. True when it was new (the caller extracts), false when its
    /// candidates were taken before.
    pub(crate) fn mark_extracted(&mut self, url: &str) -> bool {
        self.extracted.insert(pure::canonical_key(url))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.candidates.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn oa_status(&self) -> Option<&str> {
        self.oa_status.as_deref()
    }

    /// A lookup failed because the network is down (`OpErrKind::Offline`).
    pub(crate) fn note_offline(&mut self) {
        self.offline = true;
    }

    pub(crate) fn saw_offline(&self) -> bool {
        self.offline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fulltext_core::rank::new_candidate;

    fn oa_cfg() -> OaConfig {
        crate::sim::fixtures::test_config().oa
    }

    fn api(source: Source, candidates: Vec<Candidate>) -> ApiInspection {
        ApiInspection {
            source,
            status: ApiStatus::Found,
            learned: LearnedIds::default(),
            candidates,
            oa_status: None,
            note: None,
            retry_after_s: None,
        }
    }

    fn pdf(url: &str, source: Source) -> Candidate {
        new_candidate(url, CandKind::Pdf, source)
    }

    fn doi_ids(doi: &str) -> Ids {
        Ids {
            doi: Some(doi.to_owned()),
            ..Ids::default()
        }
    }

    #[test]
    fn dedupes_including_the_query_string() {
        let mut pool = CandidatePool::new(doi_ids("10.1371/journal.pmed.1000097"));
        let a = "https://journals.plos.org/plosmedicine/article/file?id=10.1371/journal.pmed.1000097&type=printable";
        let b = "https://journals.plos.org/plosmedicine/article/file?id=10.1371/journal.pmed.1000097&type=manuscript";
        pool.merge(
            &api(Source::Openalex, vec![pdf(a, Source::Openalex)]),
            "openalex",
        );
        pool.merge(
            &api(
                Source::Unpaywall,
                vec![pdf(a, Source::Unpaywall), pdf(b, Source::Unpaywall)],
            ),
            "unpaywall",
        );
        assert_eq!(pool.len(), 2, "same URL merged, different query kept apart");
        assert!(
            !pool.add(pdf(&format!("{a}#frag"), Source::Core)),
            "a fragment does not make a new candidate"
        );
        assert!(pool.add(pdf("https://example.org/other.pdf", Source::Core)));
    }

    #[test]
    fn learned_ids_never_overwrite_known_ones() {
        let mut ids = doi_ids("10.1371/journal.pmed.1000097");
        ids.pmid = Some("111".into());
        let mut pool = CandidatePool::new(ids);
        let mut a = api(Source::Europepmc, vec![]);
        a.learned = LearnedIds {
            doi: Some("10.9999/other".into()),
            pmid: Some("222".into()),
            pmcid: Some("PMC2707599".into()),
            arxiv: None,
        };
        assert!(pool.merge(&a, "europepmc"), "the PMCID was new");
        assert_eq!(
            pool.ids().doi.as_deref(),
            Some("10.1371/journal.pmed.1000097")
        );
        assert_eq!(pool.ids().pmid.as_deref(), Some("111"));
        assert_eq!(pool.pmcid(), Some("PMC2707599"));
        assert!(!pool.merge(&a, "europepmc"), "nothing new the second time");
    }

    #[test]
    fn strong_means_a_ranked_pdf_in_the_first_five_tiers() {
        let mut pool = CandidatePool::new(doi_ids("10.1/x"));
        assert!(!pool.has_strong());
        pool.add(new_candidate(
            "https://journals.plos.org/article?id=10.1/x",
            CandKind::Landing,
            Source::Openalex,
        ));
        pool.add(pdf("https://iris.example.it/a.pdf", Source::Openalex));
        assert!(
            !pool.has_strong(),
            "a landing page and a repository PDF are not strong"
        );
        pool.add(pdf(
            "https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf",
            Source::PmcS3,
        ));
        assert!(pool.has_strong());
        // tier numbers are the contract
        let mut c = pdf("https://arxiv.org/pdf/1706.03762", Source::Arxiv);
        c.tier = tier::ARXIV;
        assert!(is_strong(&c));
        c.tier = tier::DIRECT_REPOSITORY;
        assert!(!is_strong(&c));
        c.tier = tier::LANDING;
        c.kind = CandKind::Landing;
        assert!(!is_strong(&c));
    }

    #[test]
    fn crossref_links_are_kept_but_are_not_strong() {
        let mut pool = CandidatePool::new(doi_ids("10.1/x"));
        let mut c = pdf(
            "https://www.nature.com/articles/nature12373.pdf",
            Source::Doi,
        );
        c.host_kind = HostKind::Publisher;
        pool.merge(&api(Source::Doi, vec![c]), "doi");
        assert_eq!(pool.len(), 1);
        assert!(
            !pool.has_strong(),
            "a publisher link from Crossref needs a subscription"
        );
        assert_eq!(
            pool.ranked(&oa_cfg()).len(),
            1,
            "it is still offered to the institutional phase"
        );
        // the same URL from an OA source takes the source over
        let mut d = pdf(
            "https://www.nature.com/articles/nature12373.pdf",
            Source::Openalex,
        );
        d.host_kind = HostKind::Publisher;
        pool.merge(&api(Source::Openalex, vec![d]), "openalex");
        assert_eq!(pool.len(), 1);
        assert!(pool.has_strong());
        assert_eq!(
            pool.ranked(&oa_cfg()).first().map(|c| c.source),
            Some(Source::Openalex)
        );
    }

    #[test]
    fn ranked_skips_tried_urls_and_orders_by_tier() {
        let mut pool = CandidatePool::new(doi_ids("10.1/x"));
        pool.add(new_candidate(
            "https://journals.plos.org/article?id=10.1/x",
            CandKind::Landing,
            Source::Openalex,
        ));
        pool.add(pdf("https://arxiv.org/pdf/1706.03762", Source::Arxiv));
        pool.add(pdf(
            "https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf",
            Source::PmcS3,
        ));
        let oa = oa_cfg();
        let order: Vec<u8> = pool.ranked(&oa).iter().map(|c| c.tier).collect();
        assert_eq!(order, vec![tier::PMC_S3, tier::ARXIV, tier::LANDING]);
        assert!(
            pool.mark_tried("https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf#x"),
            "fragment ignored"
        );
        assert!(
            !pool.mark_tried("http://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf"),
            "scheme ignored"
        );
        assert!(pool.was_tried("https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf"));
        let order: Vec<u8> = pool.ranked(&oa).iter().map(|c| c.tier).collect();
        assert_eq!(order, vec![tier::ARXIV, tier::LANDING]);
    }

    #[test]
    fn oa_toggles_filter_what_is_offered() {
        let mut pool = CandidatePool::new(doi_ids("10.1/x"));
        let mut early = pdf(
            "https://www.medrxiv.org/content/medrxiv/early/2023/06/01/x.full.pdf",
            Source::Biorxiv,
        );
        early.signal = Some("biorxiv:early_path".into());
        pool.add(early);
        let mut get_pdf = pdf(
            "https://europepmc.org/api/getPdf?pmcid=PMC1",
            Source::Europepmc,
        );
        get_pdf.signal = Some("europepmc:getpdf".into());
        pool.add(get_pdf);
        pool.add(pdf("https://arxiv.org/pdf/1706.03762", Source::Arxiv));
        let mut oa = oa_cfg();
        assert_eq!(pool.ranked(&oa).len(), 3);
        oa.europepmc_get_pdf = false;
        assert_eq!(pool.ranked(&oa).len(), 2);
        oa.medrxiv_early = false;
        assert_eq!(pool.ranked(&oa).len(), 1);
        oa.arxiv = false;
        assert!(pool.ranked(&oa).is_empty());
    }

    #[test]
    fn the_bonus_orders_candidates_inside_a_tier_only() {
        let mut pool = CandidatePool::new(doi_ids("10.1/x"));
        let mut a = pdf("https://pub.example.org/a.pdf", Source::Extractor);
        a.host_kind = HostKind::Publisher;
        let mut b = pdf("https://pub.example.org/b.pdf", Source::Extractor);
        b.host_kind = HostKind::Publisher;
        let a_url = a.url.clone();
        let b_url = b.url.clone();
        pool.add_boosted(a, 0);
        pool.add_boosted(b, 15);
        let urls: Vec<String> = pool.ranked(&oa_cfg()).into_iter().map(|c| c.url).collect();
        assert_eq!(
            urls,
            vec![b_url.clone(), a_url.clone()],
            "the boosted candidate goes first"
        );
        pool.add(pdf(
            "https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf",
            Source::PmcS3,
        ));
        let first = pool.ranked(&oa_cfg()).into_iter().next().map(|c| c.url);
        assert_eq!(
            first.as_deref(),
            Some("https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf"),
            "a better tier still wins"
        );
    }

    #[test]
    fn published_doi_note_is_exposed_once() {
        let mut pool = CandidatePool::new(doi_ids("10.1101/2023.05.31.23290770"));
        let mut a = api(Source::Biorxiv, vec![]);
        a.note = Some("published_doi:10.1371/Journal.pone.0298402".into());
        pool.merge(&a, "biorxiv");
        assert_eq!(
            pool.take_published_doi().as_deref(),
            Some("10.1371/journal.pone.0298402")
        );
        assert_eq!(pool.take_published_doi(), None, "taken once");
        assert_eq!(pool.published_doi(), Some("10.1371/journal.pone.0298402"));
        // candidates of the journal record are verified against the published DOI
        let mut c = pdf(
            "https://journals.plos.org/plosone/article/file?id=10.1371/journal.pone.0298402&type=printable",
            Source::Openalex,
        );
        c.record_doi = Some("10.1371/journal.pone.0298402".into());
        assert_eq!(
            pool.requested_for(&c).doi.as_deref(),
            Some("10.1371/journal.pone.0298402")
        );
        let other = pdf("https://www.medrxiv.org/x.pdf", Source::Biorxiv);
        assert_eq!(
            pool.requested_for(&other).doi.as_deref(),
            Some("10.1101/2023.05.31.23290770")
        );
        // the item's own DOI is never "published"
        let mut again = api(Source::Biorxiv, vec![]);
        again.note = Some("published_doi:10.1101/2023.05.31.23290770".into());
        let mut fresh = CandidatePool::new(doi_ids("10.1101/2023.05.31.23290770"));
        fresh.merge(&again, "biorxiv");
        assert_eq!(fresh.take_published_doi(), None);
    }

    #[test]
    fn extraction_and_offline_bookkeeping() {
        let mut pool = CandidatePool::new(Ids::default());
        assert!(pool.mark_extracted("https://pub.example.org/article"));
        assert!(
            !pool.mark_extracted("http://pub.example.org/article#top"),
            "the same page is never extracted twice"
        );
        assert!(!pool.saw_offline());
        pool.note_offline();
        assert!(pool.saw_offline());
        assert!(pool.is_empty());
        assert_eq!(pool.oa_status(), None);
    }
}
