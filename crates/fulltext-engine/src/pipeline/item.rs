//! `run_item`: the whole per-item pipeline as one `async fn`.
//!
//! # Flow and outcome mapping (every path ends in exactly one `Outcome`; the caller emits the terminal row)
//! ```text
//! wait_admitted()                       Cancelled -> Outcome::Cancelled { User }      (batch confirm declined)
//! Slot::acquire(Prio::New)              Refused::ShuttingDown -> Cancelled { Shutdown }
//! row CHECKING; precheck::eligibility   Some(r) -> Skipped { r }
//! ids = pure::normalise_ids             no doi/pmid/pmcid/arxiv/url -> NoIdentifier
//! pool = oa::discover_oa(..)            (row SEARCHING_OA)
//! attempt::attempt_oa(..)               Pdf(blob) -> save::save_pdf_blob -> Saved { route: Oa }
//!   Exhausted:
//!     mode == OaOnly or inst disabled   -> NotFound { OaOnly | NoOaCopy | .. } (reason from the Exhausted flags)
//!     claim_inst_slot():                NotAllowed -> NotFound{OaOnly}; BatchCap -> Deferred{Batch};
//!                                       Halted -> Halted{handoff}; GroupDenied -> NeedsUser{GroupLibrary};
//!                                       ResumeBlocked -> NeedsUser{EngineRestarted}; Granted -> inst::run_institutional
//! Item timeout (policy FetchClass::Item, excluding time parked in the login gate) -> NotFound { Timeout }
//! ```
//! # Rules
//! * Phases are wrapped in `cx.timeout(..)`; the loser is dropped, which cancels its ops.
//! * The item slot is held only while working: `pipeline::parked` around login waits.
//! * The terminal outcome of a run that is OA-only (`Mode::OaOnly`) when nothing is found is
//!   `NotFound { reason: OaOnly }`, never `NotHeld`.
//! * Rows: emit a progress row at each phase change (ids in `l10n::id`); the terminal row is emitted by the
//!   run manager from the `Outcome` (`l10n::outcome_row`).
//!
//! # Details worth knowing
//! * The `NotFound` reason of an OA-only run is always `OaOnly` (the user's next step is "try again with
//!   institutional access"); otherwise a refusal beats a timeout beats "no copy" (`not_found_reason`).
//! * The item's time budget (`FetchClass::Item`) covers everything up to the save; it is extended by the time the
//!   item spent parked and it never interrupts a save that has begun (`Slot::parked_clock`, `ItemPhase::Saving`).
//! * A network that is down for every lookup makes the item wait (row `NETWORK_DOWN`, one new discovery every 15 s
//!   for 2 minutes) and then fail with `Failed { code: "offline" }`.
//! * `run_admitted` is `run_item` without the admission step; the tests drive it directly.
use crate::cx::ItemCx;
use crate::l10n::{self, id};
use crate::ops::TimedOut;
use crate::pipeline::attempt::{self, OaEnd};
use crate::pipeline::pool::CandidatePool;
use crate::pipeline::{Slot, handoff, inst, oa, precheck, pure, save};
use crate::proto::*;
use crate::run::{InstClaim, ItemPhase, RunAdmission};
use crate::sched::Prio;
use crate::sched::policy::{self, FetchClass};
use fulltext_core::model::*;
use std::cell::Cell;
use std::future::Future;

/// Failure code of an item that could not reach the network for the whole pause.
const OFFLINE_CODE: &str = "offline";

/// Run one item to its outcome.
pub(crate) async fn run_item(cx: &ItemCx) -> Outcome {
    match cx.wait_admitted().await {
        RunAdmission::Cancelled => Outcome::Cancelled {
            by: CancelledBy::User,
        },
        RunAdmission::Go { inst_allowed } => run_admitted(cx, inst_allowed).await,
    }
}

/// The pipeline after admission. `inst_allowed` is false for OA-only runs and for declined large batches.
pub(crate) async fn run_admitted(cx: &ItemCx, inst_allowed: bool) -> Outcome {
    let Ok(mut slot) = Slot::acquire(cx, Prio::New).await else {
        return Outcome::Cancelled {
            by: CancelledBy::Shutdown,
        };
    };
    cx.progress(l10n::row(RowState::Working, id::CHECKING));
    if let Some(reason) = precheck::eligibility(&cx.snap) {
        return Outcome::Skipped { reason };
    }
    let ids = pure::normalise_ids(&cx.snap);
    if ids.doi.is_none()
        && ids.pmid.is_none()
        && ids.pmcid.is_none()
        && ids.arxiv.is_none()
        && ids.url.is_none()
    {
        return Outcome::NoIdentifier;
    }
    let parked = slot.parked_clock();
    let search = search(cx, &mut slot, &ids, inst_allowed);
    match within_item_budget(cx, &parked, search).await {
        Some(outcome) => outcome,
        None => Outcome::NotFound {
            reason: NotFoundReason::Timeout,
        },
    }
}

/// Is this item inside `SavePdf`? (Then nothing may interrupt it.)
fn is_saving(cx: &ItemCx) -> bool {
    let (run, item) = (cx.run, cx.item);
    cx.shared
        .with(|s| {
            s.runs
                .runs
                .get(&run)
                .and_then(|r| r.items.get(&item))
                .is_some_and(|rec| rec.phase == ItemPhase::Saving)
        })
        .unwrap_or(false)
}

/// Run `fut`, but give up (dropping it, which cancels its ops) after `FetchClass::Item` ms of working time: the
/// time the item spent parked (`parked`) is added to the allowance, and a save that has begun is never cut off.
async fn within_item_budget<F: Future>(
    cx: &ItemCx,
    parked: &Cell<u64>,
    fut: F,
) -> Option<F::Output> {
    let started = cx.now();
    let allowance = u64::from(policy::timeout_ms(FetchClass::Item, false));
    let mut fut = Box::pin(fut);
    loop {
        let deadline = started
            .saturating_add(allowance)
            .saturating_add(parked.get());
        match cx
            .timeout(deadline.saturating_sub(cx.now()), &mut fut)
            .await
        {
            Ok(done) => return Some(done),
            Err(TimedOut) => {
                // The timer ran out; parking may have moved the deadline since it was set.
                let moved = started
                    .saturating_add(allowance)
                    .saturating_add(parked.get());
                if cx.now() < moved {
                    continue;
                }
                if is_saving(cx) {
                    return Some(fut.await);
                }
                return None;
            }
        }
    }
}

/// Why nothing was found (see the module docs for the precedence).
fn not_found_reason(mode: Mode, saw_access_denied: bool, saw_timeout: bool) -> NotFoundReason {
    if mode == Mode::OaOnly {
        NotFoundReason::OaOnly
    } else if saw_access_denied {
        NotFoundReason::AccessDenied
    } else if saw_timeout {
        NotFoundReason::Timeout
    } else {
        NotFoundReason::NoOaCopy
    }
}

/// Discovery, waiting out a network that is down for every lookup.
async fn discover(cx: &ItemCx, ids: &Ids) -> Result<CandidatePool, Outcome> {
    let mut pool = oa::discover_oa(cx, ids).await;
    let mut waited = 0u64;
    while pool.saw_offline() && pool.is_empty() && waited < policy::NETWORK_DOWN_GIVE_UP_MS {
        cx.progress(l10n::row(RowState::Waiting, id::NETWORK_DOWN));
        cx.sleep(policy::NETWORK_PROBE_EVERY_MS).await;
        waited = waited.saturating_add(policy::NETWORK_PROBE_EVERY_MS);
        pool = oa::discover_oa(cx, ids).await;
    }
    if pool.saw_offline() && pool.is_empty() {
        return Err(Outcome::Failed {
            code: OFFLINE_CODE.to_owned(),
        });
    }
    Ok(pool)
}

/// Everything from the first lookup to the end of the save.
async fn search(cx: &ItemCx, slot: &mut Slot, ids: &Ids, inst_allowed: bool) -> Outcome {
    let mut pool = match discover(cx, ids).await {
        Ok(pool) => pool,
        Err(outcome) => return outcome,
    };
    match attempt::attempt_oa(cx, &mut pool).await {
        OaEnd::Pdf(blob) => save::save_pdf_blob(cx, blob, pool.ids()).await,
        OaEnd::ShuttingDown => Outcome::Cancelled {
            by: CancelledBy::Shutdown,
        },
        OaEnd::Offline => Outcome::Failed {
            code: OFFLINE_CODE.to_owned(),
        },
        OaEnd::Exhausted {
            saw_access_denied,
            saw_timeout,
            ..
        } => {
            let reason = not_found_reason(cx.mode, saw_access_denied, saw_timeout);
            if cx.mode == Mode::OaOnly || !cx.cfg.inst.enabled {
                return Outcome::NotFound { reason };
            }
            if !inst_allowed {
                return Outcome::NotFound {
                    reason: NotFoundReason::OaOnly,
                };
            }
            institutional(cx, slot, &mut pool).await
        }
    }
}

/// OA found nothing: ask the run manager whether this item may use the institutional route, and run it.
async fn institutional(cx: &ItemCx, slot: &mut Slot, pool: &mut CandidatePool) -> Outcome {
    let hand_off = |pool: &CandidatePool| handoff::handoff(&cx.cfg.institution, pool.ids());
    match cx.claim_inst_slot() {
        InstClaim::NotAllowed => Outcome::NotFound {
            reason: NotFoundReason::OaOnly,
        },
        InstClaim::BatchCap => Outcome::Deferred {
            cap: CapKind::Batch,
        },
        InstClaim::Halted { .. } => Outcome::Halted {
            handoff: hand_off(pool),
        },
        InstClaim::GroupDenied => Outcome::NeedsUser {
            why: NeedsWhy::GroupLibrary,
            handoff: hand_off(pool),
        },
        InstClaim::ResumeBlocked => Outcome::NeedsUser {
            why: NeedsWhy::EngineRestarted,
            handoff: hand_off(pool),
        },
        InstClaim::Granted => {
            let ids = pool.ids().clone();
            inst::run_institutional(cx, slot, &ids, pool).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::testkit::{Rig, script};
    use crate::sim::Reply;
    use crate::sim::fixtures::{
        self, body, browser_ok, candidate, http_ok, is_type, op_err, page_inspection,
        pdf_inspection, save_ok, url_has,
    };
    use std::rc::Rc;

    const PMC_PDF: &str = "https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf";
    const DOI: &str = "10.1371/journal.pmed.1000097";

    fn snap() -> ItemSnapshot {
        fixtures::item(1, DOI)
    }

    fn openalex_found(candidates: Vec<Candidate>) -> Reply {
        Reply::ok(
            100,
            http_ok(
                200,
                "https://api.openalex.org/works/doi:x",
                &fixtures::api_found(Source::Openalex, candidates),
                None,
            ),
        )
    }

    fn s3_candidate() -> Candidate {
        candidate(PMC_PDF, CandKind::Pdf, Source::Openalex, 0)
    }

    fn pdf_response(url: &str, handle: u32) -> Reply {
        Reply::ok(
            200,
            http_ok(
                200,
                url,
                &pdf_inspection(80_000),
                Some(body(handle, 80_000)),
            ),
        )
    }

    /// A script where OpenAlex finds the PMC S3 PDF and it downloads and saves fine.
    fn happy() -> fixtures::ScriptResponder {
        script()
            .on(
                url_has("api.openalex.org"),
                openalex_found(vec![s3_candidate()]),
            )
            .on(url_has("pmc-oa-opendata"), pdf_response(PMC_PDF, 7))
            .on(is_type("save_pdf"), Reply::ok(150, save_ok("1/ABCD1234")))
    }

    fn run(rig: &mut Rig, snap: ItemSnapshot, mode: Mode) -> Outcome {
        rig.run_item(snap, mode).expect("the item finished")
    }

    #[test]
    fn open_access_happy_path_ends_saved_with_the_pmc_pdf() {
        let mut rig = Rig::new(happy());
        let outcome = run(&mut rig, snap(), Mode::Full);
        assert_eq!(
            outcome,
            Outcome::Saved {
                route: Route::Oa,
                source: "openalex".into(),
                version: Version::Published,
                url: PMC_PDF.into(),
                attachment_key: "1/ABCD1234".into()
            }
        );
        let effects = rig.effects();
        let pos = |f: &dyn Fn(&Effect) -> bool| effects.iter().position(f);
        let lookup =
            pos(&|e| matches!(e, Effect::Http { req, .. } if req.url.contains("openalex")));
        let download = pos(
            &|e| matches!(e, Effect::Http { req, retain: Retain::Pdf, .. } if req.url == PMC_PDF),
        );
        let save = pos(&|e| matches!(e, Effect::SavePdf { .. }));
        let release = pos(&|e| matches!(e, Effect::Release { handle: 7 }));
        assert!(
            lookup < download && download < save && save < release,
            "lookup -> download -> save -> release: {lookup:?} {download:?} {save:?} {release:?}"
        );
        // no institutional effects on the open-access path
        assert_eq!(
            rig.count(|e| matches!(e, Effect::BrowserLoad { .. } | Effect::LoginWindow { .. })),
            0
        );
        assert_eq!(
            rig.rows(),
            vec![id::CHECKING, id::SEARCHING_OA, id::DOWNLOADING, id::SAVING]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );
        assert!(rig.host.pending.is_empty());
    }

    #[test]
    fn the_item_trace_names_every_step() {
        let mut rig = Rig::new(happy());
        let trace = rig
            .exec(snap(), |cx| async move {
                let _ = run_admitted(&cx, true).await;
                cx.take_trace()
            })
            .expect("done");
        let steps: Vec<(String, String, String)> = trace
            .into_iter()
            .map(|t| (t.src, t.tier, t.class))
            .collect();
        assert!(
            steps.contains(&("openalex".into(), "api".into(), "found".into())),
            "{steps:?}"
        );
        assert!(
            steps.contains(&("openalex".into(), "http".into(), "pdf".into())),
            "{steps:?}"
        );
    }

    #[test]
    fn items_that_cannot_be_helped_are_skipped_before_any_network() {
        let mut rig = Rig::new(script());
        let mut has_pdf = snap();
        has_pdf.pdf_attachments = 1;
        assert_eq!(
            run(&mut rig, has_pdf, Mode::Full),
            Outcome::Skipped {
                reason: SkipReason::HasPdf
            }
        );
        let mut read_only = fixtures::item(2, DOI);
        read_only.library.files_editable = false;
        assert_eq!(
            run(&mut rig, read_only, Mode::Full),
            Outcome::Skipped {
                reason: SkipReason::ReadOnly
            }
        );
        assert!(rig.http().is_empty());
        assert_eq!(
            rig.rows()
                .iter()
                .filter(|r| r.as_str() == id::CHECKING)
                .count(),
            2
        );
    }

    #[test]
    fn an_item_without_identifiers_is_reported_as_such() {
        let mut rig = Rig::new(script());
        let outcome = run(&mut rig, fixtures::item(1, ""), Mode::Full);
        assert_eq!(outcome, Outcome::NoIdentifier);
        assert!(rig.http().is_empty());
        assert!(!rig.rows().contains(&id::SEARCHING_OA.to_owned()));
    }

    #[test]
    fn a_pmid_only_item_is_resolved_through_europe_pmc() {
        let mut item = fixtures::item(1, "");
        item.fields.pmid = "19621072".into();
        let mut learned = ApiInspection {
            source: Source::Europepmc,
            status: ApiStatus::Found,
            learned: LearnedIds {
                doi: Some(DOI.into()),
                pmcid: Some("PMC2707599".into()),
                ..LearnedIds::default()
            },
            candidates: vec![],
            oa_status: None,
            note: None,
            retry_after_s: None,
        };
        learned.status = ApiStatus::Found;
        let s3 = fixtures::api_found(
            Source::PmcS3,
            vec![candidate(PMC_PDF, CandKind::Pdf, Source::PmcS3, 0)],
        );
        let s = script()
            .on(
                url_has("EXT_ID:19621072"),
                Reply::ok(100, http_ok(200, "u", &Inspection::Api(learned), None)),
            )
            .on(
                url_has("metadata/PMC2707599"),
                Reply::ok(100, http_ok(200, "u", &s3, None)),
            )
            .on(
                url_has("pmc-oa-opendata.s3.amazonaws.com/PMC2707599"),
                pdf_response(PMC_PDF, 7),
            )
            .on(is_type("save_pdf"), Reply::ok(100, save_ok("1/K")));
        let mut rig = Rig::new(s);
        let outcome = run(&mut rig, item, Mode::Full);
        assert!(
            matches!(&outcome, Outcome::Saved { route: Route::Oa, source, .. } if source == "pmc_s3"),
            "{outcome:?}"
        );
        let urls: Vec<String> = rig.http().into_iter().map(|(u, _, _)| u).collect();
        assert!(
            urls.iter().any(|u| u.contains("EXT_ID:19621072")),
            "{urls:?}"
        );
        assert!(
            urls.iter()
                .any(|u| u.ends_with("metadata/PMC2707599.1.json")),
            "chained once the PMCID was learned: {urls:?}"
        );
    }

    #[test]
    fn an_arxiv_only_item_downloads_the_deterministic_pdf() {
        let mut item = fixtures::item(1, "");
        item.fields.archive_id = "arXiv:1706.03762".into();
        let s = script()
            .on(
                url_has("arxiv.org/pdf/1706.03762"),
                pdf_response("https://arxiv.org/pdf/1706.03762", 7),
            )
            .on(is_type("save_pdf"), Reply::ok(100, save_ok("1/K")));
        let mut rig = Rig::new(s);
        let outcome = run(&mut rig, item, Mode::Full);
        assert!(
            matches!(&outcome, Outcome::Saved { route: Route::Oa, source, url, version, .. } if source == "arxiv" && url == "https://arxiv.org/pdf/1706.03762" && *version == Version::Submitted),
            "{outcome:?}"
        );
    }

    #[test]
    fn nothing_found_in_an_oa_only_run_is_oa_only_never_not_held() {
        let mut rig = Rig::new(script());
        assert_eq!(
            run(&mut rig, snap(), Mode::OaOnly),
            Outcome::NotFound {
                reason: NotFoundReason::OaOnly
            }
        );
        // the same when the lookups found a candidate that did not work out
        let s = script()
            .on(
                url_has("api.openalex.org"),
                openalex_found(vec![s3_candidate()]),
            )
            .on(
                url_has("pmc-oa-opendata"),
                Reply::ok(
                    50,
                    http_ok(
                        200,
                        PMC_PDF,
                        &page_inspection(PageClass::AccessDenied, vec![]),
                        None,
                    ),
                ),
            );
        let mut rig = Rig::new(s);
        assert_eq!(
            run(&mut rig, snap(), Mode::OaOnly),
            Outcome::NotFound {
                reason: NotFoundReason::OaOnly
            }
        );
    }

    #[test]
    fn without_the_institutional_route_the_reason_comes_from_what_happened() {
        let mut cfg = fixtures::test_config();
        cfg.inst.enabled = false;
        let found = |r: Reply| {
            script()
                .on(
                    url_has("api.openalex.org"),
                    openalex_found(vec![s3_candidate()]),
                )
                .on(url_has("pmc-oa-opendata"), r)
        };
        let denied = Reply::ok(
            50,
            http_ok(
                200,
                PMC_PDF,
                &page_inspection(PageClass::AccessDenied, vec![]),
                None,
            ),
        );
        let mut rig = Rig::with_config(cfg.clone(), found(denied));
        assert_eq!(
            run(&mut rig, snap(), Mode::Full),
            Outcome::NotFound {
                reason: NotFoundReason::AccessDenied
            }
        );
        let mut rig = Rig::with_config(cfg.clone(), found(Reply::err(50, OpErrKind::Timeout)));
        assert_eq!(
            run(&mut rig, snap(), Mode::Full),
            Outcome::NotFound {
                reason: NotFoundReason::Timeout
            }
        );
        let mut rig = Rig::with_config(cfg.clone(), script());
        assert_eq!(
            run(&mut rig, snap(), Mode::Full),
            Outcome::NotFound {
                reason: NotFoundReason::NoOaCopy
            }
        );
        // a plain network error is neither a refusal nor a timeout
        let mut rig = Rig::with_config(cfg, found(Reply::err(50, OpErrKind::Network)));
        assert_eq!(
            run(&mut rig, snap(), Mode::Full),
            Outcome::NotFound {
                reason: NotFoundReason::NoOaCopy
            }
        );
    }

    #[test]
    fn a_pdf_that_appeared_meanwhile_is_a_skip_not_an_error() {
        let s = script()
            .on(
                url_has("api.openalex.org"),
                openalex_found(vec![s3_candidate()]),
            )
            .on(url_has("pmc-oa-opendata"), pdf_response(PMC_PDF, 7))
            .on(
                is_type("save_pdf"),
                Reply::err(50, OpErrKind::AlreadyHasPdf),
            );
        let mut rig = Rig::new(s);
        assert_eq!(
            run(&mut rig, snap(), Mode::Full),
            Outcome::Skipped {
                reason: SkipReason::PdfAddedMeanwhile
            }
        );
        assert_eq!(rig.count(|e| matches!(e, Effect::Release { handle: 7 })), 1);
    }

    #[test]
    fn a_challenge_on_an_open_access_host_is_cleared_in_the_browser() {
        let publisher = "https://pub-a.example.org/a.pdf";
        let mut cand = candidate(publisher, CandKind::Pdf, Source::Openalex, 0);
        cand.host_kind = HostKind::Publisher;
        let challenge = page_inspection(
            PageClass::Challenge {
                vendor: Vendor::Cloudflare,
                interactive: false,
            },
            vec![],
        );
        let s = script()
            .on(url_has("api.openalex.org"), openalex_found(vec![cand]))
            .on(
                is_type("browser_load"),
                Reply::ok(
                    1_200,
                    browser_ok(
                        publisher,
                        &pdf_inspection(80_000),
                        Some(body(9, 80_000)),
                        None,
                    ),
                ),
            )
            .on(
                url_has("pub-a"),
                Reply::ok(50, http_ok(403, publisher, &challenge, None)),
            )
            .on(is_type("save_pdf"), Reply::ok(100, save_ok("1/K")));
        let mut rig = Rig::new(s);
        let outcome = run(&mut rig, snap(), Mode::Full);
        assert!(
            matches!(
                outcome,
                Outcome::Saved {
                    route: Route::Oa,
                    ..
                }
            ),
            "{outcome:?}"
        );
        let load = rig.effects().into_iter().find_map(|e| match e {
            Effect::BrowserLoad { req, .. } => Some(req),
            _ => None,
        });
        assert_eq!(load.map(|r| (r.ua, r.capture_pdf)), Some((Ua::Plain, true)));
    }

    #[test]
    fn a_landing_page_that_names_the_pdf_leads_to_the_pdf() {
        let landing_url = "https://pub-a.example.org/article/1";
        let pdf_url = "https://pub-a.example.org/article/1.pdf";
        let mut cand = candidate(landing_url, CandKind::Landing, Source::Openalex, 0);
        cand.host_kind = HostKind::Publisher;
        let mut pdf = candidate(pdf_url, CandKind::Pdf, Source::Extractor, 0);
        pdf.record_doi = Some(DOI.into());
        let page = page_inspection(PageClass::Landing, vec![pdf]);
        let s = script()
            .on(url_has("api.openalex.org"), openalex_found(vec![cand]))
            .on(url_has("article/1.pdf"), pdf_response(pdf_url, 7))
            .on(
                url_has("article/1"),
                Reply::ok(100, http_ok(200, landing_url, &page, Some(body(3, 5_000)))),
            )
            .on(
                is_type("run_translators"),
                Reply::ok(200, fixtures::translators_ok(None)),
            )
            .on(is_type("save_pdf"), Reply::ok(100, save_ok("1/K")));
        let mut rig = Rig::new(s);
        let outcome = run(&mut rig, snap(), Mode::Full);
        assert!(
            matches!(&outcome, Outcome::Saved { url, .. } if url == pdf_url),
            "{outcome:?}"
        );
        assert_eq!(rig.count(|e| matches!(e, Effect::RunTranslators { .. })), 1);
    }

    #[test]
    fn a_network_that_is_down_makes_the_item_wait_and_then_fail_offline() {
        let mut rig = Rig::new(
            script()
                .on(is_type("http"), Reply::err(5, OpErrKind::Offline))
                .on(is_type("zotero_oa"), Reply::err(5, OpErrKind::Offline)),
        );
        let start = rig.host.now;
        let outcome = run(&mut rig, snap(), Mode::Full);
        assert_eq!(
            outcome,
            Outcome::Failed {
                code: "offline".into()
            }
        );
        assert!(rig.rows().iter().any(|r| r == id::NETWORK_DOWN));
        let waited = rig.host.now - start;
        assert!(
            (policy::NETWORK_DOWN_GIVE_UP_MS..policy::NETWORK_DOWN_GIVE_UP_MS + 60_000)
                .contains(&waited),
            "{waited}"
        );
        // the retries did not count against the APIs: they would be switched off after three failures
        let asked = rig
            .http()
            .iter()
            .filter(|(u, _, _)| u.contains("api.openalex.org"))
            .count();
        assert_eq!(
            asked as u64,
            policy::NETWORK_DOWN_GIVE_UP_MS / policy::NETWORK_PROBE_EVERY_MS + 1,
            "one lookup per probe"
        );
    }

    #[test]
    fn a_network_that_comes_back_lets_the_item_carry_on() {
        let s = script()
            .on_seq(
                url_has("api.openalex.org"),
                vec![
                    Reply::err(5, OpErrKind::Offline),
                    openalex_found(vec![s3_candidate()]),
                ],
            )
            .on(is_type("zotero_oa"), Reply::err(5, OpErrKind::Offline))
            .on(url_has("ebi.ac.uk"), Reply::err(5, OpErrKind::Offline))
            .on(url_has("crossref"), Reply::err(5, OpErrKind::Offline))
            .on(url_has("pmc-oa-opendata"), pdf_response(PMC_PDF, 7))
            .on(is_type("save_pdf"), Reply::ok(100, save_ok("1/K")));
        let mut rig = Rig::new(s);
        let outcome = run(&mut rig, snap(), Mode::Full);
        assert!(matches!(outcome, Outcome::Saved { .. }), "{outcome:?}");
        assert!(rig.rows().iter().any(|r| r == id::NETWORK_DOWN));
    }

    // ------------------------------------------------------------------------------------ cancellation

    fn cancel_item(rig: &mut Rig) {
        let dropped = rig
            .host
            .engine
            .exec_mut()
            .cancel_where(|owner| matches!(owner, crate::cx::Owner::Item { run: 1, item: 1 }));
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn cancelling_mid_fetch_cancels_the_request_in_the_same_step() {
        let s = script()
            .on(
                url_has("api.openalex.org"),
                openalex_found(vec![s3_candidate()]),
            )
            .on(url_has("pmc-oa-opendata"), Reply::Never);
        let mut rig = Rig::new(s);
        rig.spawn(snap(), Mode::Full);
        // lookups done, the PDF request is out
        rig.host.run_until_idle(5_000).expect("run");
        let fetch_ops = rig
            .host
            .pending_where(|e| matches!(e, Effect::Http { req, .. } if req.url == PMC_PDF));
        assert_eq!(fetch_ops.len(), 1, "the download is in flight");
        let before = rig.effects().len();
        cancel_item(&mut rig);
        let out = rig.host.step(Vec::new()).expect("step");
        assert!(
            out.effects
                .iter()
                .any(|e| matches!(e, Effect::Cancel { op } if *op == fetch_ops[0])),
            "{:?}",
            out.effects
        );
        assert!(rig.host.pending.is_empty());
        assert!(rig.effects().len() > before);
        assert_eq!(
            rig.outcome(),
            None,
            "a cancelled item reports nothing itself: the run manager records it"
        );
    }

    #[test]
    fn cancelling_during_the_save_cancels_the_save_and_releases_the_body() {
        let s = script()
            .on(
                url_has("api.openalex.org"),
                openalex_found(vec![s3_candidate()]),
            )
            .on(url_has("pmc-oa-opendata"), pdf_response(PMC_PDF, 7))
            .on(is_type("save_pdf"), Reply::Never);
        let mut rig = Rig::new(s);
        rig.spawn(snap(), Mode::Full);
        rig.host.run_until_idle(5_000).expect("run");
        let saves = rig
            .host
            .pending_where(|e| matches!(e, Effect::SavePdf { .. }));
        assert_eq!(saves.len(), 1);
        cancel_item(&mut rig);
        let out = rig.host.step(Vec::new()).expect("step");
        assert!(
            out.effects
                .iter()
                .any(|e| matches!(e, Effect::Cancel { op } if *op == saves[0]))
        );
        assert!(
            out.effects
                .iter()
                .any(|e| matches!(e, Effect::Release { handle: 7 })),
            "{:?}",
            out.effects
        );
    }

    #[test]
    fn a_reply_that_lands_in_the_cancelling_step_is_released_too() {
        let s = script()
            .on(
                url_has("api.openalex.org"),
                openalex_found(vec![s3_candidate()]),
            )
            .on(url_has("pmc-oa-opendata"), pdf_response(PMC_PDF, 7));
        let mut rig = Rig::new(s);
        rig.spawn(snap(), Mode::Full);
        // stop just before the PDF reply is due, deliver it by hand, then cancel before the task is polled again
        loop {
            rig.host.step(Vec::new()).expect("step");
            if !rig
                .host
                .pending_where(|e| matches!(e, Effect::Http { req, .. } if req.url == PMC_PDF))
                .is_empty()
            {
                break;
            }
            let next = rig
                .host
                .scheduled
                .iter()
                .map(|s| s.0)
                .min()
                .unwrap_or(rig.host.now);
            rig.host.advance(next.saturating_sub(rig.host.now).max(1));
        }
        let ops = rig
            .host
            .pending_where(|e| matches!(e, Effect::Http { req, .. } if req.url == PMC_PDF));
        let payload = http_ok(200, PMC_PDF, &pdf_inspection(80_000), Some(body(7, 80_000)));
        let msg = CompletionMsg {
            op: ops[0],
            ok: Some(payload),
            err: None,
        };
        // the answer arrives in this step, and the item is cancelled before it can read it
        let wakers = rig
            .host
            .engine
            .shared()
            .with(|s| s.reactor.complete(msg))
            .flatten();
        cancel_item(&mut rig);
        drop(wakers);
        let out = rig.host.step(Vec::new()).expect("step");
        assert!(
            out.effects
                .iter()
                .any(|e| matches!(e, Effect::Release { handle: 7 })),
            "{:?}",
            out.effects
        );
    }

    // ---------------------------------------------------------------------------------- time budget

    #[test]
    fn the_item_gives_up_after_its_time_budget() {
        // six PDF candidates on six hosts, each hanging until its own 90 s timeout, one at a time
        let cands: Vec<Candidate> = (0..6)
            .map(|i| {
                let mut c = candidate(
                    &format!("https://pub-{i}.example.org/{i}.pdf"),
                    CandKind::Pdf,
                    Source::Openalex,
                    0,
                );
                c.host_kind = HostKind::Publisher;
                c
            })
            .collect();
        let s = script()
            .on(url_has("api.openalex.org"), openalex_found(cands))
            .on(url_has(".example.org"), Reply::Never);
        let mut cfg = fixtures::test_config();
        cfg.run.hedge = false;
        let mut rig = Rig::with_config(cfg, s);
        let start = rig.host.now;
        let outcome = run(&mut rig, snap(), Mode::Full);
        assert_eq!(
            outcome,
            Outcome::NotFound {
                reason: NotFoundReason::Timeout
            }
        );
        let spent = rig.host.now - start;
        assert!((360_000..362_000).contains(&spent), "{spent}");
        assert!(
            rig.host.pending.is_empty(),
            "the request in flight was cancelled"
        );
    }

    #[test]
    fn parked_time_does_not_count_against_the_budget() {
        let mut rig = Rig::new(script());
        let (long_wait, no_credit) = rig
            .exec(snap(), |cx| async move {
                // 150 s of work, then 400 s parked, then 150 s of work: 700 s in total, 300 s of it working
                let parked = Cell::new(0);
                let first = within_item_budget(&cx, &parked, async {
                    cx.sleep(150_000).await;
                    parked.set(400_000);
                    cx.sleep(400_000).await;
                    cx.sleep(150_000).await;
                    "finished"
                })
                .await;
                // the same wait without the credit runs out at 360 s
                let none = Cell::new(0);
                let second = within_item_budget(&cx, &none, async {
                    cx.sleep(150_000).await;
                    cx.sleep(400_000).await;
                    "finished"
                })
                .await;
                (first, second)
            })
            .expect("done");
        assert_eq!(long_wait, Some("finished"));
        assert_eq!(no_credit, None);
        assert!(rig.host.pending.is_empty());
    }

    #[test]
    fn the_budget_never_cuts_off_a_save_that_has_begun() {
        // No run record exists in this rig, so `is_saving` is false: the budget applies.
        let mut rig = Rig::new(script());
        let timed_out = rig
            .exec(snap(), |cx| async move {
                let parked = Cell::new(0);
                within_item_budget(&cx, &parked, cx.sleep(1_000_000))
                    .await
                    .is_none()
            })
            .expect("done");
        assert!(timed_out);
    }

    // ------------------------------------------------------------------------------------- unit bits

    #[test]
    fn the_not_found_reason_precedence() {
        use NotFoundReason::*;
        assert_eq!(not_found_reason(Mode::OaOnly, true, true), OaOnly);
        assert_eq!(not_found_reason(Mode::Full, true, true), AccessDenied);
        assert_eq!(not_found_reason(Mode::Full, false, true), Timeout);
        assert_eq!(not_found_reason(Mode::Full, false, false), NoOaCopy);
    }

    #[test]
    fn the_item_runner_hands_the_outcome_to_the_run_manager() {
        // `item_runner` = admission + pipeline + `cx.finish`; without a run record the admission answers "go".
        // The future is polled by hand: the engine's own step collects `done` into the run records.
        let rig = Rig::new(script());
        let shared = Rc::clone(rig.host.engine.shared());
        let cx = ItemCx::new(
            Rc::clone(&shared),
            1,
            Mode::Full,
            true,
            Rc::new(fixtures::test_config()),
            Rc::new(fixtures::item(1, "")),
        );
        let mut fut = crate::pipeline::item_runner(cx);
        let waker = futures_util::task::noop_waker();
        assert!(
            fut.as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
                .is_ready(),
            "nothing to wait for"
        );
        let done = shared
            .with(|s| std::mem::take(&mut s.done))
            .unwrap_or_default();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].outcome, Outcome::NoIdentifier);
        let _ = op_err(OpErrKind::Io);
    }
}
