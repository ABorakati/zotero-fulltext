//! OA discovery: parallel lookups (OpenAlex, Europe PMC, bioRxiv, Zotero OA, optional Unpaywall / Semantic
//! Scholar / CORE), PMC S3 chained once a PMCID is known, settle early on a strong candidate.
//!
//! # Contract (`discover_oa`)
//! * Builds one future per applicable source (`requests::api_requests`, `OaConfig` toggles / keys) into a
//!   `FuturesUnordered`; each is `timeout(OaApi, acquire_api(src).await -> cx.http(..))`. A throttled answer
//!   (429 / 5xx / `ApiStatus::Throttled`) retries at most once after `min(Retry-After, 10 s)`; a second failure
//!   drops the source for this item (and a throttled answer that survived its retry switches that API off for a
//!   cool-down through the gate).
//! * Sources with more than one request (bioRxiv then medRxiv, PMC S3 metadata then bucket listing) try the next
//!   request while the answer is "not found" / "empty".
//! * Every parsed answer is `pool.merge`d; learned ids are folded into `Ids`; when a PMCID becomes known and
//!   `oa.pmc_s3` is on, the PMC S3 metadata lookup is pushed into the SAME set.
//! * Finishes when the set is empty, `DISCOVERY_BUDGET_MS` elapsed, or `STRONG_SETTLE_MS` passed since the first
//!   strong candidate appeared. Dropping the set cancels stragglers (`Effect::Cancel`).
//! * Preprints: when an `ApiInspection.note` is `published_doi:<doi>` (core), run ONE more round of lookups for
//!   the published DOI (same budget, sources that already failed are not retried); its candidates rank by version.
//! * Crossref answers arrive as `Source::Doi`; they are asked only when the institutional route can use them
//!   (`inst.enabled` and a Full run).
//! * Deterministic URLs need no lookup: an arXiv id adds `https://arxiv.org/pdf/<id>` directly, and an item that
//!   has no DOI or PMCID contributes its own URL as a landing page.
//! * Emits `l10n::id::SEARCHING_OA` once at the start. Records a `trace_step` per source.
//! * A lookup that failed with `OpErrKind::Offline` marks the pool (`saw_offline`) so that `item.rs` can tell
//!   "nothing found" from "no network".
use crate::cx::ItemCx;
use crate::l10n::{self, id};
use crate::ops::{Sleep, TimedOut};
use crate::pipeline::pool::CandidatePool;
use crate::pipeline::{pure, requests};
use crate::proto::*;
use crate::sched::policy::{self, FetchClass};
use crate::sched::{Permit, source_name};
use fulltext_core::model::*;
use fulltext_core::rank::new_candidate;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use std::collections::BTreeSet;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;

/// Longest `Retry-After` a lookup honours.
const RETRY_AFTER_CAP_S: u32 = 10;
/// Wait used when a throttled answer names no `Retry-After`.
const RETRY_AFTER_DEFAULT_S: u32 = 2;

/// The lookups of the first round, in the order their requests are issued.
const FIRST_ROUND: [Source; 9] = [
    Source::Openalex,
    Source::Europepmc,
    Source::Biorxiv,
    Source::ZoteroOa,
    Source::Unpaywall,
    Source::SemanticScholar,
    Source::Core,
    Source::Doi,
    Source::PmcS3,
];

/// The lookups repeated for the published DOI of a preprint.
const PUBLISHED_ROUND: [Source; 6] = [
    Source::Openalex,
    Source::Europepmc,
    Source::ZoteroOa,
    Source::Unpaywall,
    Source::SemanticScholar,
    Source::Core,
];

/// Why a lookup produced no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fail {
    Timeout,
    Network,
    Offline,
    Throttled,
    Unparseable,
    Unusable,
}

impl Fail {
    fn name(self) -> &'static str {
        match self {
            Fail::Timeout => "timeout",
            Fail::Network => "network",
            Fail::Offline => "offline",
            Fail::Throttled => "throttled",
            Fail::Unparseable => "unparseable",
            Fail::Unusable => "unusable",
        }
    }
}

fn fail_for(e: &OpErr) -> Fail {
    match e.kind {
        OpErrKind::Timeout => Fail::Timeout,
        OpErrKind::Offline => Fail::Offline,
        OpErrKind::Network | OpErrKind::Tls => Fail::Network,
        _ => Fail::Unusable,
    }
}

/// Tell the API's gate that the lookup failed with `why`. A network that is down says nothing about the API, so it
/// does not count towards the circuit breaker (otherwise the retries of an offline item would switch every API off).
fn report_failure(permit: &mut Permit, why: Fail) {
    if why != Fail::Offline {
        permit.report(false);
    }
}

/// What one lookup came back with.
enum Answer {
    Api(ApiInspection),
    Failed(Fail),
    /// The gate refused (API switched off, engine shutting down).
    Skipped,
}

struct Lookup {
    src: Source,
    answer: Answer,
    ms: u32,
}

fn status_name(status: ApiStatus) -> &'static str {
    match status {
        ApiStatus::Found => "found",
        ApiStatus::NotFound => "not_found",
        ApiStatus::Empty => "empty",
        ApiStatus::Throttled => "throttled",
        ApiStatus::Unparseable => "unparseable",
    }
}

/// One HTTP lookup with the single throttle retry.
async fn one_request(cx: &ItemCx, src: Source, req: HttpReq, spec: InspectSpec) -> Answer {
    let timeout = u64::from(policy::timeout_ms(FetchClass::OaApi, false));
    let mut retried = false;
    loop {
        let Ok(mut permit) = cx.acquire_api(src).await else {
            return Answer::Skipped;
        };
        let res = cx
            .timeout(
                timeout,
                cx.http(Route::Oa, false, req.clone(), Retain::None, spec.clone()),
            )
            .await;
        let resp = match res {
            Err(TimedOut) => {
                permit.report(false);
                return Answer::Failed(Fail::Timeout);
            }
            Ok(Err(e)) => {
                let why = fail_for(&e);
                report_failure(&mut permit, why);
                return Answer::Failed(why);
            }
            Ok(Ok(resp)) => resp,
        };
        let Inspection::Api(api) = resp.inspect else {
            permit.report(false);
            return Answer::Failed(Fail::Unusable);
        };
        let throttled =
            api.status == ApiStatus::Throttled || resp.status == 429 || resp.status >= 500;
        if !throttled {
            return match api.status {
                ApiStatus::Found | ApiStatus::NotFound | ApiStatus::Empty => {
                    permit.report(true);
                    Answer::Api(api)
                }
                ApiStatus::Throttled | ApiStatus::Unparseable => {
                    permit.report(false);
                    Answer::Failed(Fail::Unparseable)
                }
            };
        }
        permit.report(false);
        drop(permit);
        if retried {
            if api.status == ApiStatus::Throttled || resp.status == 429 {
                cx.disable_api(src);
            }
            return Answer::Failed(Fail::Throttled);
        }
        retried = true;
        let header_wait = resp
            .headers
            .get("retry-after")
            .and_then(|v| v.trim().parse::<u32>().ok());
        let wait_s = api
            .retry_after_s
            .or(header_wait)
            .unwrap_or(RETRY_AFTER_DEFAULT_S)
            .min(RETRY_AFTER_CAP_S);
        cx.sleep(u64::from(wait_s) * 1_000).await;
    }
}

/// Zotero's own OA service (an effect, not an HTTP request) as an `ApiInspection`.
async fn zotero_lookup(cx: &ItemCx, ids: &Ids) -> Answer {
    let Some(doi) = ids.doi.as_deref() else {
        return Answer::Skipped;
    };
    let Ok(mut permit) = cx.acquire_api(Source::ZoteroOa).await else {
        return Answer::Skipped;
    };
    let timeout = u64::from(policy::timeout_ms(FetchClass::OaApi, false));
    match cx.timeout(timeout, cx.zotero_oa(doi)).await {
        Err(TimedOut) => {
            permit.report(false);
            Answer::Failed(Fail::Timeout)
        }
        Ok(Err(e)) => {
            let why = fail_for(&e);
            report_failure(&mut permit, why);
            Answer::Failed(why)
        }
        Ok(Ok(hits)) => {
            permit.report(true);
            let candidates: Vec<Candidate> = hits
                .iter()
                .flat_map(|h| {
                    pure::zotero_hit_candidates(&h.url, h.page_url.as_deref(), h.version.as_deref())
                })
                .collect();
            let status = if candidates.is_empty() {
                ApiStatus::Empty
            } else {
                ApiStatus::Found
            };
            Answer::Api(ApiInspection {
                source: Source::ZoteroOa,
                status,
                learned: LearnedIds::default(),
                candidates,
                oa_status: None,
                note: None,
                retry_after_s: None,
            })
        }
    }
}

/// All requests of one source, in order, until one gives a definite answer.
async fn answer_for(cx: &ItemCx, src: Source, ids: &Ids) -> Answer {
    if src == Source::ZoteroOa {
        return zotero_lookup(cx, ids).await;
    }
    let mut last = Answer::Skipped;
    for (req, spec) in requests::api_requests(src, ids, &cx.cfg.oa) {
        match one_request(cx, src, req, spec).await {
            Answer::Api(api) if matches!(api.status, ApiStatus::NotFound | ApiStatus::Empty) => {
                last = Answer::Api(api)
            }
            other => return other,
        }
    }
    last
}

/// One timed lookup (a single future type, so a `FuturesUnordered` can hold them all).
async fn lookup(cx: &ItemCx, src: Source, ids: Ids) -> Lookup {
    let started = cx.now();
    let answer = answer_for(cx, src, &ids).await;
    Lookup {
        src,
        answer,
        ms: u32::try_from(cx.now().saturating_sub(started)).unwrap_or(u32::MAX),
    }
}

/// Does `src` have anything to ask for this item?
fn applies(cx: &ItemCx, src: Source, ids: &Ids) -> bool {
    match src {
        Source::ZoteroOa => cx.cfg.oa.zotero_oa && ids.doi.is_some(),
        Source::Doi => cx.cfg.inst.enabled && cx.mode == Mode::Full && ids.doi.is_some(),
        _ => !requests::api_requests(src, ids, &cx.cfg.oa).is_empty(),
    }
}

/// Fold one finished lookup into the pool.
fn absorb(cx: &ItemCx, pool: &mut CandidatePool, failed: &mut BTreeSet<Source>, done: Lookup) {
    let name = source_name(done.src);
    match done.answer {
        Answer::Api(api) => {
            cx.trace_step(&name, "api", status_name(api.status), done.ms);
            pool.merge(&api, &name);
        }
        Answer::Failed(why) => {
            cx.trace_step(&name, "api", why.name(), done.ms);
            failed.insert(done.src);
            if why == Fail::Offline {
                pool.note_offline();
            }
        }
        Answer::Skipped => {
            cx.trace_step(&name, "api", "skipped", done.ms);
            failed.insert(done.src);
        }
    }
}

/// How a round ends.
enum Event {
    Done(Option<Lookup>),
    Stop,
}

/// One round of lookups for `ids`: start every applicable source that has not failed, chain PMC S3 once a PMCID
/// is known, stop on an empty set, the budget, or the settle timer.
async fn run_round(
    cx: &ItemCx,
    pool: &mut CandidatePool,
    ids: &Ids,
    sources: &[Source],
    failed: &mut BTreeSet<Source>,
) {
    let mut set = FuturesUnordered::new();
    let mut asked: BTreeSet<Source> = BTreeSet::new();
    for &src in sources {
        if !failed.contains(&src) && applies(cx, src, ids) && asked.insert(src) {
            set.push(lookup(cx, src, ids.clone()));
        }
    }
    let mut budget = cx.sleep(policy::DISCOVERY_BUDGET_MS);
    let mut settle: Option<Sleep> = None;
    loop {
        if cx.cfg.oa.pmc_s3
            && pool.pmcid().is_some()
            && !failed.contains(&Source::PmcS3)
            && asked.insert(Source::PmcS3)
        {
            set.push(lookup(cx, Source::PmcS3, pool.ids().clone()));
        }
        if set.is_empty() {
            break;
        }
        let event = poll_fn(|task| {
            if let Poll::Ready(done) = set.poll_next_unpin(task) {
                return Poll::Ready(Event::Done(done));
            }
            if Pin::new(&mut budget).poll(task).is_ready() {
                return Poll::Ready(Event::Stop);
            }
            if let Some(s) = settle.as_mut()
                && Pin::new(s).poll(task).is_ready()
            {
                return Poll::Ready(Event::Stop);
            }
            Poll::Pending
        })
        .await;
        match event {
            Event::Stop | Event::Done(None) => break,
            Event::Done(Some(done)) => {
                absorb(cx, pool, failed, done);
                if settle.is_none() && pool.has_strong() {
                    settle = Some(cx.sleep(policy::STRONG_SETTLE_MS));
                }
            }
        }
    }
    // Dropping `set` here cancels whatever is still in flight.
}

/// Candidates that need no lookup: the arXiv PDF of an arXiv id, the item's own URL when nothing else identifies it.
fn seed(cx: &ItemCx, pool: &mut CandidatePool) {
    let ids = pool.ids().clone();
    if cx.cfg.oa.arxiv
        && let Some(arxiv) = ids.arxiv.as_deref()
    {
        let mut c = new_candidate(
            format!("https://arxiv.org/pdf/{arxiv}"),
            CandKind::Pdf,
            Source::Arxiv,
        );
        c.version = Version::Submitted;
        c.host_kind = HostKind::Preprint;
        c.landing_url = Some(format!("https://arxiv.org/abs/{arxiv}"));
        pool.add(c);
        cx.trace_step("arxiv", "seed", "pdf_url", 0);
    }
    if ids.doi.is_none()
        && ids.pmcid.is_none()
        && let Some(url) = ids.url.as_deref()
    {
        let mut c = new_candidate(url, CandKind::Landing, Source::ItemUrl);
        c.host_kind = pure::host_kind(url);
        pool.add(c);
        cx.trace_step("item_url", "seed", "landing", 0);
    }
}

/// Run the discovery phase for `ids` and return the pool of candidates it found.
pub(crate) async fn discover_oa(cx: &ItemCx, ids: &Ids) -> CandidatePool {
    cx.progress(l10n::row(RowState::Working, id::SEARCHING_OA).on_route(Route::Oa));
    let mut pool = CandidatePool::new(ids.clone());
    seed(cx, &mut pool);
    let mut failed = BTreeSet::new();
    run_round(cx, &mut pool, ids, &FIRST_ROUND, &mut failed).await;
    if let Some(published) = pool.take_published_doi() {
        let published_ids = Ids {
            doi: Some(published),
            ..Ids::default()
        };
        run_round(cx, &mut pool, &published_ids, &PUBLISHED_ROUND, &mut failed).await;
    }
    pool
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::testkit::{Rig, script};
    use crate::sim::Reply;
    use crate::sim::fixtures::{self, candidate, http_ok, is_type, url_has};
    use std::collections::BTreeMap;

    const PMC_PDF: &str = "https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf";

    fn snap() -> ItemSnapshot {
        fixtures::item(1, "10.1371/journal.pmed.1000097")
    }

    fn ids() -> Ids {
        Ids {
            doi: Some("10.1371/journal.pmed.1000097".into()),
            ..Ids::default()
        }
    }

    fn api(source: Source, status: ApiStatus, candidates: Vec<Candidate>) -> Inspection {
        Inspection::Api(ApiInspection {
            source,
            status,
            learned: LearnedIds::default(),
            candidates,
            oa_status: None,
            note: None,
            retry_after_s: None,
        })
    }

    fn reply(after: u64, url: &str, inspect: &Inspection) -> Reply {
        Reply::ok(after, http_ok(200, url, inspect, None))
    }

    fn s3_pdf() -> Candidate {
        candidate(PMC_PDF, CandKind::Pdf, Source::PmcS3, 0)
    }

    /// Run `discover_oa` for the default item and return the pool.
    fn discover(rig: &mut Rig) -> CandidatePool {
        discover_ids(rig, snap(), ids())
    }

    fn discover_ids(rig: &mut Rig, snap: ItemSnapshot, ids: Ids) -> CandidatePool {
        rig.exec(snap, move |cx| async move { discover_oa(&cx, &ids).await })
            .expect("done")
    }

    fn urls(rig: &Rig) -> Vec<String> {
        rig.http().into_iter().map(|(u, _, _)| u).collect()
    }

    fn only_openalex_and_europepmc() -> OaConfig {
        let mut oa = fixtures::test_config().oa;
        oa.zotero_oa = false;
        oa
    }

    #[test]
    fn a_strong_candidate_settles_discovery_and_cancels_the_stragglers() {
        let found = api(Source::Openalex, ApiStatus::Found, vec![s3_pdf()]);
        let s = script()
            .on(
                url_has("api.openalex.org"),
                reply(100, "https://api.openalex.org/works/doi:x", &found),
            )
            .on(url_has("ebi.ac.uk"), Reply::Never)
            .on(is_type("zotero_oa"), Reply::Never);
        let mut rig = Rig::new(s);
        let start = rig.host.now;
        let pool = discover(&mut rig);
        assert!(pool.has_strong());
        assert_eq!(
            rig.host.now - start,
            100 + policy::STRONG_SETTLE_MS,
            "settled 1.5 s after the strong candidate"
        );
        let cancelled = rig.count(|e| matches!(e, Effect::Cancel { .. }));
        assert_eq!(cancelled, 2, "Europe PMC and Zotero OA were still running");
        assert!(rig.host.pending.is_empty());
        assert_eq!(
            rig.rows()
                .iter()
                .filter(|r| r.as_str() == id::SEARCHING_OA)
                .count(),
            1
        );
    }

    #[test]
    fn nothing_strong_runs_until_every_source_answered() {
        let empty = |src| api(src, ApiStatus::Empty, vec![]);
        let s = script()
            .on(
                url_has("api.openalex.org"),
                reply(300, "u", &empty(Source::Openalex)),
            )
            .on(
                url_has("ebi.ac.uk"),
                reply(900, "u", &empty(Source::Europepmc)),
            )
            .on(is_type("zotero_oa"), Reply::ok(400, serde_json::json!([])))
            .on(
                url_has("api.crossref.org"),
                reply(200, "u", &empty(Source::Doi)),
            );
        let mut rig = Rig::new(s);
        let start = rig.host.now;
        let pool = discover(&mut rig);
        assert!(pool.is_empty());
        assert_eq!(rig.host.now - start, 900, "the slowest source decides");
        assert_eq!(rig.count(|e| matches!(e, Effect::Cancel { .. })), 0);
    }

    #[test]
    fn each_source_gets_the_request_the_requests_module_builds() {
        let mut rig = Rig::new(script());
        let _ = discover(&mut rig);
        let mut got = urls(&rig);
        got.sort();
        assert_eq!(
            got,
            vec![
                "https://api.crossref.org/works/10.1371/journal.pmed.1000097",
                "https://api.openalex.org/works/doi:10.1371/journal.pmed.1000097",
                "https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=DOI:%2210.1371/journal.pmed.1000097%22&resultType=core&format=json",
            ]
        );
        assert_eq!(rig.count(|e| matches!(e, Effect::ZoteroOa { doi, .. } if doi == "10.1371/journal.pmed.1000097")), 1);
        let spec = rig.effects().into_iter().find_map(|e| match e {
            Effect::Http {
                inspect,
                retain,
                route,
                req,
                ..
            } if req.url.contains("openalex") => Some((inspect, retain, route, req.timeout_ms)),
            _ => None,
        });
        let (inspect, retain, route, timeout) = spec.expect("openalex request");
        assert_eq!((retain, route, timeout), (Retain::None, Route::Oa, 9_000));
        assert!(matches!(
            inspect,
            InspectSpec::Api {
                source: Source::Openalex,
                ..
            }
        ));
    }

    #[test]
    fn crossref_is_asked_only_when_the_institutional_route_can_use_it() {
        let mut cfg = fixtures::test_config();
        cfg.inst.enabled = false;
        let mut rig = Rig::with_config(cfg, script());
        let _ = discover(&mut rig);
        assert!(!urls(&rig).iter().any(|u| u.contains("crossref")));
        // an OA-only run
        let mut rig = Rig::new(script());
        let pool = rig
            .exec(snap(), |cx| async move {
                let oa_only = ItemCx::new(
                    std::rc::Rc::clone(&cx.shared),
                    1,
                    Mode::OaOnly,
                    true,
                    std::rc::Rc::clone(&cx.cfg),
                    std::rc::Rc::clone(&cx.snap),
                );
                discover_oa(&oa_only, &ids()).await
            })
            .expect("done");
        assert!(pool.is_empty());
        assert!(!urls(&rig).iter().any(|u| u.contains("crossref")));
    }

    #[test]
    fn a_throttled_answer_is_retried_once_after_its_retry_after() {
        let throttled = Inspection::Api(ApiInspection {
            source: Source::Openalex,
            status: ApiStatus::Throttled,
            learned: LearnedIds::default(),
            candidates: vec![],
            oa_status: None,
            note: Some("http_429".into()),
            retry_after_s: Some(2),
        });
        let found = api(Source::Openalex, ApiStatus::Found, vec![s3_pdf()]);
        let s = script().on_seq(
            url_has("api.openalex.org"),
            vec![
                Reply::ok(50, http_ok(429, "u", &throttled, None)),
                reply(50, "u", &found),
            ],
        );
        let mut rig = Rig::with_config(
            {
                let mut c = fixtures::test_config();
                c.oa = only_openalex_and_europepmc();
                c
            },
            s,
        );
        let start = rig.host.now;
        let pool = discover(&mut rig);
        let openalex: Vec<u64> = rig
            .timeline()
            .into_iter()
            .filter_map(|(at, e)| match e {
                Effect::Http { req, .. } if req.url.contains("openalex") => Some(at - start),
                _ => None,
            })
            .collect();
        // First try at 0; the answer takes 50 ms; the retry waits the announced 2 s.
        assert_eq!(openalex, vec![0, 50 + 2_000]);
        assert!(pool.has_strong());
    }

    #[test]
    fn a_second_throttle_drops_the_source_and_switches_the_api_off() {
        let throttled = Inspection::Api(ApiInspection {
            source: Source::Openalex,
            status: ApiStatus::Throttled,
            learned: LearnedIds::default(),
            candidates: vec![],
            oa_status: None,
            note: Some("http_429".into()),
            retry_after_s: Some(30),
        });
        let s = script().on(
            url_has("api.openalex.org"),
            Reply::ok(10, http_ok(429, "u", &throttled, None)),
        );
        let mut rig = Rig::new(s);
        let start = rig.host.now;
        let (pool, refused) = rig
            .exec(snap(), |cx| async move {
                let pool = discover_oa(&cx, &ids()).await;
                let refused = cx.acquire_api(Source::Openalex).await.is_err();
                (pool, refused)
            })
            .expect("done");
        assert!(pool.is_empty());
        assert!(refused, "the API is off for the cool-down");
        let openalex = urls(&rig).iter().filter(|u| u.contains("openalex")).count();
        assert_eq!(openalex, 2, "one retry, no more");
        // The wait is capped at 10 s although the API asked for 30.
        assert!(rig.host.now - start < 15_000, "{}", rig.host.now - start);
    }

    #[test]
    fn server_errors_are_retried_like_throttles() {
        let unparseable = Inspection::Api(ApiInspection {
            source: Source::Openalex,
            status: ApiStatus::Unparseable,
            learned: LearnedIds::default(),
            candidates: vec![],
            oa_status: None,
            note: Some("http_503".into()),
            retry_after_s: None,
        });
        let found = api(Source::Openalex, ApiStatus::Found, vec![s3_pdf()]);
        let s = script().on_seq(
            url_has("api.openalex.org"),
            vec![
                Reply::ok(10, http_ok(503, "u", &unparseable, None)),
                reply(10, "u", &found),
            ],
        );
        let mut rig = Rig::new(s);
        let pool = discover(&mut rig);
        assert!(pool.has_strong());
        assert_eq!(
            urls(&rig).iter().filter(|u| u.contains("openalex")).count(),
            2
        );
    }

    #[test]
    fn unusable_answers_fail_the_source_only() {
        let s = script()
            .on(
                url_has("api.openalex.org"),
                Reply::ok(
                    10,
                    http_ok(200, "u", &Inspection::Failed { reason: "x".into() }, None),
                ),
            )
            .on(url_has("ebi.ac.uk"), Reply::err(10, OpErrKind::Network))
            .on(is_type("zotero_oa"), Reply::err(10, OpErrKind::Timeout))
            .on(
                url_has("crossref"),
                reply(10, "u", &api(Source::Doi, ApiStatus::Unparseable, vec![])),
            );
        let mut rig = Rig::new(s);
        let (pool, trace) = rig
            .exec(snap(), |cx| async move {
                let pool = discover_oa(&cx, &ids()).await;
                (pool, cx.take_trace())
            })
            .expect("done");
        assert!(pool.is_empty() && !pool.saw_offline());
        let classes: BTreeMap<String, String> =
            trace.into_iter().map(|t| (t.src, t.class)).collect();
        assert_eq!(
            classes.get("openalex").map(String::as_str),
            Some("unusable")
        );
        assert_eq!(
            classes.get("europepmc").map(String::as_str),
            Some("network")
        );
        assert_eq!(
            classes.get("zotero_oa").map(String::as_str),
            Some("timeout")
        );
        assert_eq!(classes.get("doi").map(String::as_str), Some("unparseable"));
    }

    #[test]
    fn offline_marks_the_pool() {
        let mut rig = Rig::new(script().on(is_type("http"), Reply::err(5, OpErrKind::Offline)));
        let pool = discover(&mut rig);
        assert!(pool.saw_offline() && pool.is_empty());
    }

    #[test]
    fn a_learned_pmcid_chains_the_pmc_s3_lookup_into_the_same_set() {
        let mut learned = ApiInspection {
            source: Source::Europepmc,
            status: ApiStatus::Found,
            learned: LearnedIds::default(),
            candidates: vec![],
            oa_status: None,
            note: None,
            retry_after_s: None,
        };
        learned.learned.pmcid = Some("PMC2707599".into());
        let s3 = api(Source::PmcS3, ApiStatus::Found, vec![s3_pdf()]);
        let s = script()
            .on(
                url_has("ebi.ac.uk"),
                reply(100, "u", &Inspection::Api(learned)),
            )
            .on(url_has("pmc-oa-opendata"), reply(80, "u", &s3));
        let mut rig = Rig::with_config(
            {
                let mut c = fixtures::test_config();
                c.oa = only_openalex_and_europepmc();
                c
            },
            s,
        );
        let start = rig.host.now;
        let pool = discover(&mut rig);
        assert!(pool.has_strong());
        assert_eq!(pool.pmcid(), Some("PMC2707599"));
        assert!(urls(&rig).contains(
            &"https://pmc-oa-opendata.s3.amazonaws.com/metadata/PMC2707599.1.json".to_owned()
        ));
        let s3_at = rig.timeline().into_iter().find_map(|(at, e)| match e {
            Effect::Http { req, .. } if req.url.contains("pmc-oa-opendata") => Some(at - start),
            _ => None,
        });
        assert_eq!(s3_at, Some(100), "pushed as soon as the PMCID was known");
    }

    #[test]
    fn a_known_pmcid_starts_pmc_s3_at_once_and_a_missing_object_falls_back_to_the_listing() {
        let not_found = api(Source::PmcS3, ApiStatus::NotFound, vec![]);
        let listing = api(Source::PmcS3, ApiStatus::Found, vec![s3_pdf()]);
        let s = script().on_seq(
            url_has("pmc-oa-opendata"),
            vec![reply(30, "u", &not_found), reply(30, "u", &listing)],
        );
        let mut rig = Rig::new(s);
        let mut item = snap();
        item.fields.pmcid = "PMC2707599".into();
        let ids = Ids {
            pmcid: Some("PMC2707599".into()),
            ..ids()
        };
        let pool = discover_ids(&mut rig, item, ids);
        assert!(pool.has_strong());
        let s3: Vec<String> = urls(&rig)
            .into_iter()
            .filter(|u| u.contains("pmc-oa-opendata"))
            .collect();
        assert_eq!(
            s3,
            vec![
                "https://pmc-oa-opendata.s3.amazonaws.com/metadata/PMC2707599.1.json",
                "https://pmc-oa-opendata.s3.amazonaws.com/?list-type=2&prefix=PMC2707599."
            ]
        );
    }

    #[test]
    fn biorxiv_tries_medrxiv_when_the_first_server_has_no_posts() {
        let none = api(Source::Biorxiv, ApiStatus::Empty, vec![]);
        let early = {
            let mut c = candidate(
                "https://www.medrxiv.org/content/medrxiv/early/2023/06/01/2023.05.31.23290770.full.pdf",
                CandKind::Pdf,
                Source::Biorxiv,
                0,
            );
            c.signal = Some("biorxiv:early_path".into());
            c
        };
        let found = api(Source::Biorxiv, ApiStatus::Found, vec![early]);
        let s = script().on_seq(
            url_has("api.biorxiv.org"),
            vec![reply(20, "u", &none), reply(20, "u", &found)],
        );
        let mut rig = Rig::new(s);
        let item = fixtures::item(1, "10.1101/2023.05.31.23290770");
        let ids = Ids {
            doi: Some("10.1101/2023.05.31.23290770".into()),
            rxiv: true,
            ..Ids::default()
        };
        let pool = discover_ids(&mut rig, item, ids);
        let servers: Vec<String> = urls(&rig)
            .into_iter()
            .filter(|u| u.contains("biorxiv.org/details"))
            .collect();
        assert_eq!(
            servers,
            vec![
                "https://api.biorxiv.org/details/biorxiv/10.1101/2023.05.31.23290770/na/json",
                "https://api.biorxiv.org/details/medrxiv/10.1101/2023.05.31.23290770/na/json"
            ]
        );
        assert!(pool.has_strong());
    }

    #[test]
    fn a_preprint_triggers_one_more_round_for_the_published_doi() {
        let mut preprint = ApiInspection {
            source: Source::Biorxiv,
            status: ApiStatus::Found,
            learned: LearnedIds::default(),
            candidates: vec![],
            oa_status: None,
            note: Some("published_doi:10.1371/journal.pone.0298402".into()),
            retry_after_s: None,
        };
        preprint.status = ApiStatus::Found;
        let published = api(
            Source::Openalex,
            ApiStatus::Found,
            vec![candidate(
                "https://journals.plos.org/plosone/article/file?id=10.1371/journal.pone.0298402&type=printable",
                CandKind::Pdf,
                Source::Openalex,
                0,
            )],
        );
        let s = script()
            .on(
                url_has("api.biorxiv.org"),
                reply(20, "u", &Inspection::Api(preprint)),
            )
            .on(
                url_has("doi:10.1371/journal.pone.0298402"),
                reply(30, "u", &published),
            )
            .on(
                url_has("api.openalex.org"),
                reply(10, "u", &api(Source::Openalex, ApiStatus::NotFound, vec![])),
            )
            .on(url_has("ebi.ac.uk"), Reply::err(10, OpErrKind::Network));
        let mut rig = Rig::new(s);
        let item = fixtures::item(1, "10.1101/2023.05.31.23290770");
        let ids = Ids {
            doi: Some("10.1101/2023.05.31.23290770".into()),
            rxiv: true,
            ..Ids::default()
        };
        let pool = discover_ids(&mut rig, item, ids);
        let all = urls(&rig);
        let openalex: Vec<&String> = all.iter().filter(|u| u.contains("openalex")).collect();
        assert_eq!(
            openalex.len(),
            2,
            "the preprint and the published DOI: {all:?}"
        );
        assert!(
            openalex
                .iter()
                .any(|u| u.ends_with("doi:10.1371/journal.pone.0298402"))
        );
        // Europe PMC failed in round one for the preprint and is not asked again.
        assert_eq!(all.iter().filter(|u| u.contains("ebi.ac.uk")).count(), 1);
        assert_eq!(pool.published_doi(), Some("10.1371/journal.pone.0298402"));
        assert_eq!(
            pool.ranked(&fixtures::test_config().oa).len(),
            1,
            "the journal's PDF is in the pool"
        );
    }

    #[test]
    fn zotero_oa_hits_become_candidates_and_the_toggle_turns_it_off() {
        let hits = vec![
            ZoteroOaHit {
                url: "https://arxiv.org/pdf/1304.1068".into(),
                page_url: Some("http://arxiv.org/abs/1304.1068".into()),
                version: Some("submittedVersion".into()),
            },
            ZoteroOaHit {
                url: "https://www.nature.com/articles/nature12373.pdf".into(),
                page_url: None,
                version: Some("publishedVersion".into()),
            },
        ];
        let reply = Reply::ok(60, serde_json::to_value(&hits).expect("hits are json"));
        let mut rig = Rig::new(script().on(is_type("zotero_oa"), reply));
        let pool = discover(&mut rig);
        let urls: Vec<String> = pool
            .ranked(&fixtures::test_config().oa)
            .into_iter()
            .map(|c| c.url)
            .collect();
        assert!(
            urls.contains(&"https://arxiv.org/pdf/1304.1068".to_owned()),
            "{urls:?}"
        );
        assert!(urls.contains(&"https://www.nature.com/articles/nature12373.pdf".to_owned()));

        let mut cfg = fixtures::test_config();
        cfg.oa.zotero_oa = false;
        let mut rig = Rig::with_config(cfg, script());
        let _ = discover(&mut rig);
        assert_eq!(rig.count(|e| matches!(e, Effect::ZoteroOa { .. })), 0);
    }

    #[test]
    fn an_arxiv_id_needs_no_lookup_for_its_pdf() {
        let item = fixtures::item(1, "10.48550/arXiv.1706.03762");
        let ids = Ids {
            doi: Some("10.48550/arxiv.1706.03762".into()),
            arxiv: Some("1706.03762".into()),
            ..Ids::default()
        };
        let mut cfg = fixtures::test_config();
        cfg.inst.enabled = false;
        cfg.oa.zotero_oa = false;
        let mut rig = Rig::with_config(cfg, script());
        let pool = discover_ids(&mut rig, item.clone(), ids.clone());
        let ranked = pool.ranked(&fixtures::test_config().oa);
        assert_eq!(
            ranked.first().map(|c| (c.url.as_str(), c.tier, c.version)),
            Some(("https://arxiv.org/pdf/1706.03762", 3, Version::Submitted))
        );
        assert!(
            !urls(&rig).iter().any(|u| u.contains("arxiv.org")),
            "no arXiv API call"
        );

        let mut cfg = fixtures::test_config();
        cfg.oa.arxiv = false;
        let mut rig = Rig::with_config(cfg, script());
        let pool = discover_ids(&mut rig, item, ids);
        assert!(pool.ranked(&fixtures::test_config().oa).is_empty());
    }

    #[test]
    fn items_without_a_doi_use_their_other_identifiers() {
        // PMID only: Europe PMC by EXT_ID, OpenAlex by pmid
        let mut rig = Rig::new(script());
        let mut item = fixtures::item(1, "");
        item.fields.pmid = "19621072".into();
        let ids = Ids {
            pmid: Some("19621072".into()),
            ..Ids::default()
        };
        let _ = discover_ids(&mut rig, item, ids);
        let mut got = urls(&rig);
        got.sort();
        assert_eq!(
            got,
            vec![
                "https://api.openalex.org/works/pmid:19621072",
                "https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=EXT_ID:19621072%20AND%20SRC:MED&resultType=core&format=json",
            ]
        );
        assert_eq!(
            rig.count(|e| matches!(e, Effect::ZoteroOa { .. })),
            0,
            "Zotero's service needs a DOI"
        );

        // URL only: the page itself is the candidate
        let mut rig = Rig::new(script());
        let mut item = fixtures::item(2, "");
        item.fields.url = "https://www.example.org/article/42".into();
        let ids = Ids {
            url: Some("https://www.example.org/article/42".into()),
            ..Ids::default()
        };
        let pool = discover_ids(&mut rig, item, ids);
        assert!(rig.http().is_empty());
        let ranked = pool.ranked(&fixtures::test_config().oa);
        assert_eq!(
            ranked.first().map(|c| (c.url.as_str(), c.kind, c.source)),
            Some((
                "https://www.example.org/article/42",
                CandKind::Landing,
                Source::ItemUrl
            ))
        );
    }

    #[test]
    fn the_budget_ends_discovery_and_cancels_what_never_answered() {
        let mut rig = Rig::new(
            script()
                .on(is_type("http"), Reply::Never)
                .on(is_type("zotero_oa"), Reply::Never),
        );
        let start = rig.host.now;
        let pool = discover(&mut rig);
        assert!(pool.is_empty());
        // The per-lookup timeout (9 s) fires before the 12 s budget; nothing waits longer than the budget.
        assert!(
            rig.host.now - start <= policy::DISCOVERY_BUDGET_MS,
            "{}",
            rig.host.now - start
        );
        assert!(rig.host.pending.is_empty(), "every straggler was cancelled");
    }

    #[test]
    fn key_holders_get_the_optional_sources() {
        let mut cfg = fixtures::test_config();
        cfg.oa.unpaywall_email = "me@kcl.ac.uk".into();
        cfg.oa.semantic_scholar_key = "s2".into();
        cfg.oa.core_key = "core".into();
        cfg.oa.openalex_key = "oa".into();
        let mut rig = Rig::with_config(cfg, script());
        let _ = discover(&mut rig);
        let all = urls(&rig);
        for needle in [
            "api.unpaywall.org",
            "api.semanticscholar.org",
            "api.core.ac.uk/v3/discover",
        ] {
            assert!(all.iter().any(|u| u.contains(needle)), "{needle}: {all:?}");
        }
        let bearer = rig.effects().into_iter().find_map(|e| match e {
            Effect::Http { req, .. } if req.url.contains("openalex") => {
                req.headers.get("Authorization").cloned()
            }
            _ => None,
        });
        assert_eq!(bearer.as_deref(), Some("Bearer oa"));
    }
}
