//! Scheduler scenarios: one institutional request per publisher family, two overall, PDF spacing, the licence
//! ledger across an engine restart, reserve-then-commit ordering, cancelled-after-sent tickets, item slots
//! released while parked. The invariants (`HostConcurrency`, `PdfSpacing`, `LedgerCaps`, ...) fail the test when
//! the limits are broken; the assertions here make sure the scenario really put the limits under pressure.
use super::*;
use crate::sim::SimHost;
use crate::sim::fixtures::is_type;

/// `n` DOIs of one publisher family (BMJ).
fn one_family(tag: &str, n: u32) -> Vec<String> {
    (1..=n).map(|i| format!("10.1136/{tag}.{i}")).collect()
}

/// An institutional page or PDF fetch (the requests the host gates are about).
fn gated_inst(e: &Effect) -> bool {
    match e {
        Effect::Http {
            route: Route::Inst,
            inspect,
            ..
        }
        | Effect::BrowserLoad {
            route: Route::Inst,
            inspect,
            ..
        } => {
            matches!(inspect, InspectSpec::Page { .. } | InspectSpec::Pdf { .. })
        }
        _ => false,
    }
}

fn is_pdf_fetch(e: &Effect) -> bool {
    matches!(
        e,
        Effect::Http {
            route: Route::Inst,
            inspect: InspectSpec::Pdf { .. },
            ..
        }
    )
}

/// Step the host to the end like `run_until_idle`, calling `sample` after every step.
fn drive_sampling(host: &mut SimHost, mut sample: impl FnMut(&SimHost)) {
    for _ in 0..100_000 {
        let out = host.step(Vec::new()).expect("step");
        sample(host);
        if out.more || host.scheduled.iter().any(|(at, _, _)| *at <= host.now) {
            continue;
        }
        match host
            .scheduled
            .iter()
            .map(|(at, _, _)| *at)
            .chain(out.next_timer)
            .min()
        {
            None => return,
            Some(at) if at <= host.now => {}
            Some(at) => host.advance(at - host.now),
        }
    }
    panic!("the simulation did not settle");
}

/// Run a campus-access batch (PDFs are fetched over plain HTTP, one page + one PDF per item) and return the host
/// plus the peak number of institutional fetches in flight, in total and per publisher family.
fn campus_batch(dois: &[String], cfg: EngineConfig, pdf_ms: u64) -> (SimHost, usize, usize) {
    let mut world = World::on_campus(dois);
    world.pdf_ms = pdf_ms;
    let mut host = run_scenario_with(cfg, Persisted::default(), world);
    start(&mut host, 1, Mode::Full, true, items(dois));
    let (mut peak_total, mut peak_family) = (0, 0);
    drive_sampling(&mut host, |h| {
        let flying: Vec<String> = h
            .pending
            .values()
            .filter(|p| gated_inst(&p.effect))
            .filter_map(|p| match &p.effect {
                Effect::Http {
                    inspect:
                        InspectSpec::Page { requested, .. } | InspectSpec::Pdf { requested, .. },
                    ..
                }
                | Effect::BrowserLoad {
                    inspect:
                        InspectSpec::Page { requested, .. } | InspectSpec::Pdf { requested, .. },
                    ..
                } => requested
                    .doi
                    .as_deref()
                    .map(|doi| crate::sched::hosts::InstKey::for_doi(doi).0),
                _ => None,
            })
            .collect();
        peak_total = peak_total.max(flying.len());
        for family in &flying {
            peak_family = peak_family.max(flying.iter().filter(|f| *f == family).count());
        }
    });
    host.finish().expect("end-of-scenario invariants");
    (host, peak_total, peak_family)
}

fn pdf_fetch_times(host: &SimHost) -> Vec<u64> {
    host.log
        .iter()
        .filter(|l| is_pdf_fetch(&l.effect))
        .map(|l| l.at)
        .collect()
}

#[test]
fn sched_one_institutional_request_per_publisher_family() {
    let d = one_family("s1", 6);
    let (host, _, peak_family) = campus_batch(&d, test_config(), 6_000);
    assert_eq!(peak_family, 1, "six items of one publisher never overlap");
    assert_eq!(report(&host, 1).counts.saved, 6);
    assert_eq!(count(&host, is_pdf_fetch), 6);
    // Serialised: six page fetches (2 s apart at least) and six PDFs (5 s apart at least) take a while.
    assert!(host.now - 1_000 >= 6 * 5_000, "{} ms", host.now - 1_000);
}

#[test]
fn sched_at_most_two_institutional_requests_overall() {
    let d = dois("s2", 10);
    let mut cfg = test_config();
    cfg.run.item_concurrency = 6;
    let (host, peak_total, peak_family) = campus_batch(&d, cfg, 20_000);
    assert_eq!(
        peak_total, 2,
        "ten families would like to run side by side; two is the limit"
    );
    assert_eq!(peak_family, 1);
    assert_eq!(report(&host, 1).counts.saved, 10);
}

#[test]
fn sched_institutional_pdfs_of_one_family_start_at_least_five_seconds_apart() {
    let d = one_family("s3", 5);
    let (host, _, _) = campus_batch(&d, test_config(), 400);
    let starts = pdf_fetch_times(&host);
    assert_eq!(starts.len(), 5);
    for pair in starts.windows(2) {
        assert!(
            pair[1] - pair[0] >= 5_000,
            "PDF fetches {} ms apart: {starts:?}",
            pair[1] - pair[0]
        );
    }
    // The user can only make the gap longer.
    let mut slow = test_config();
    slow.inst.min_gap_sec = 12;
    let (host, _, _) = campus_batch(&d, slow, 400);
    let starts = pdf_fetch_times(&host);
    assert!(
        starts.windows(2).all(|p| p[1] - p[0] >= 12_000),
        "{starts:?}"
    );
}

/// Persisted state as JS would hand it back after a restart: what the last `Persist` effects carried.
fn restart_state(host: &SimHost) -> Persisted {
    let part = |key: PersistKey| host.persisted.get(&key).cloned().unwrap_or_default();
    Persisted {
        ledger: serde_json::from_value(part(PersistKey::Ledger)).unwrap_or_default(),
        halts: serde_json::from_value(part(PersistKey::Halts)).unwrap_or_default(),
        ezproxy: serde_json::from_value(part(PersistKey::Ezproxy)).unwrap_or_default(),
        alma: serde_json::from_value(part(PersistKey::Alma)).unwrap_or_default(),
    }
}

#[test]
fn sched_ledger_caps_hold_across_an_engine_restart() {
    let mut cfg = test_config();
    cfg.inst.hourly_cap = 4;
    let first = dois("s4a", 6);
    let mut host = run_scenario_with(cfg.clone(), Persisted::default(), World::on_campus(&first));
    start(&mut host, 1, Mode::Full, true, items(&first));
    settle(&mut host);
    assert_eq!(report(&host, 1).counts.saved, 4);
    assert_eq!(report(&host, 1).counts.deferred, 2);
    assert_eq!(ledger_counts(&host), (4, 0));
    let elapsed = host.now - 1_000;

    // The browser restarts: a new engine gets the persisted ledger and the clock moves on a little.
    let more = dois("s4b", 3);
    let mut host2 = run_scenario_with(cfg.clone(), restart_state(&host), World::on_campus(&more));
    host2.advance(elapsed + 5 * 60_000);
    start(&mut host2, 1, Mode::Full, true, items(&more));
    settle(&mut host2);
    assert_eq!(
        report(&host2, 1).counts.deferred,
        3,
        "the four downloads of the first engine still count: {:?}",
        outcomes(&host2, 1)
    );
    assert_eq!(report(&host2, 1).ledger.hour, 4);

    // An hour after the first downloads, the budget is back (and the ledger invariant keeps watching).
    host2.advance(60 * 60_000);
    start(&mut host2, 2, Mode::Full, true, items(&more));
    settle(&mut host2);
    assert_eq!(report(&host2, 2).counts.saved, 3);
}

#[test]
fn sched_every_barrier_fetch_is_preceded_by_its_flushed_ledger_write() {
    let d = dois("s5", 5);
    let mut host = run_scenario(World::on_campus(&d));
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    let mut flushed = 0;
    let mut barriers = 0;
    for e in host.effects() {
        match e {
            Effect::Persist {
                key: PersistKey::Ledger,
                flush: true,
                ..
            } => flushed += 1,
            Effect::Http {
                route: Route::Inst,
                barrier: true,
                ..
            }
            | Effect::BrowserLoad {
                route: Route::Inst,
                barrier: true,
                ..
            } => {
                barriers += 1;
                assert!(
                    flushed >= barriers,
                    "fetch #{barriers} left before its reservation was written ({flushed} writes so far)"
                );
            }
            _ => {}
        }
    }
    assert_eq!(barriers, 5);
    assert_eq!(
        ledger_counts(&host),
        (5, 0),
        "reserved, then committed with the real size"
    );
    let state: LedgerState = serde_json::from_value(
        host.persisted
            .get(&PersistKey::Ledger)
            .cloned()
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    assert!(
        state.committed.iter().all(|(_, bytes)| *bytes == 80_000),
        "{:?}",
        state.committed
    );
}

#[test]
fn sched_a_fetch_cancelled_after_it_was_sent_still_counts_against_the_licence() {
    let d = dois("s6", 2);
    let world = World::on_campus(&d).hook(|op, _| match &op.effect {
        Effect::Http {
            inspect: InspectSpec::Pdf { .. },
            route: Route::Inst,
            ..
        } => Some(Reply::Never),
        _ => None,
    });
    let mut host = run_scenario(world);
    start(&mut host, 1, Mode::Full, true, items(&d));
    run_for(&mut host, 30_000);
    assert_eq!(
        host.pending_where(is_pdf_fetch).len(),
        2,
        "both PDF requests are out"
    );
    assert_eq!(
        ledger_counts(&host),
        (0, 2),
        "reserved before they were sent"
    );
    let out = host
        .step(vec![Command::CancelRun { run: 1 }])
        .expect("cancel");
    assert!(
        out.effects
            .iter()
            .filter(|e| matches!(e, Effect::Cancel { .. }))
            .count()
            >= 2
    );
    settle(&mut host);
    assert_eq!(
        ledger_counts(&host),
        (2, 0),
        "the publisher saw the requests: they are counted, with the estimate"
    );
    assert_eq!(report(&host, 1).counts.cancelled, 2);
    assert_eq!(report(&host, 1).ledger.hour, 2);
}

#[test]
fn sched_a_fetch_cancelled_before_it_was_sent_is_not_counted() {
    let d = dois("s7", 1);
    // the item waits for the publisher's gap; the user cancels before the PDF request is sent
    let mut host = run_scenario(World::on_campus(&d));
    start(&mut host, 1, Mode::Full, true, items(&d));
    host.step(Vec::new()).expect("step");
    host.step(vec![Command::CancelRun { run: 1 }])
        .expect("cancel");
    settle(&mut host);
    assert_eq!(ledger_counts(&host), (0, 0));
    assert_eq!(report(&host, 1).counts.cancelled, 1);
}

#[test]
fn sched_parked_items_release_their_slots_so_open_access_work_goes_on() {
    let inst = dois("s8a", 3);
    let oa = dois("s8b", 6);
    let mut docs: Vec<Doc> = inst
        .iter()
        .map(|d| Doc {
            doi: d.clone(),
            oa: false,
            held: true,
            direct: false,
        })
        .collect();
    docs.extend(oa.iter().map(|d| Doc {
        doi: d.clone(),
        oa: true,
        held: true,
        direct: false,
    }));
    // nobody answers the sign-in window
    let world = World::new(docs)
        .hook(|op, _| matches!(op.effect, Effect::LoginWindow { .. }).then_some(Reply::Never));
    let mut cfg = test_config();
    cfg.run.item_concurrency = 2;
    let mut host = run_scenario_with(cfg, Persisted::default(), world);
    let mut all = inst.clone();
    all.extend(oa.clone());
    start(&mut host, 1, Mode::Full, true, items(&all));
    run_for(&mut host, 60_000);
    assert_eq!(
        host.pending_where(is_type("login_window")).len(),
        1,
        "the window is open"
    );
    assert_eq!(
        count(&host, |e| matches!(e, Effect::SavePdf { .. })),
        6,
        "all six open-access items finished while the sign-in is pending"
    );
    let gates = host
        .engine
        .shared()
        .with(|s| {
            s.sched
                .limiters
                .get(&crate::sched::GateKey::Items)
                .map(|l| l.in_use)
        })
        .flatten();
    assert_eq!(gates, Some(0), "the parked items hold no item slot");
    // End the scenario.
    host.step(vec![Command::CancelRun { run: 1 }])
        .expect("cancel");
    settle(&mut host);
    assert_eq!(
        (
            report(&host, 1).counts.saved,
            report(&host, 1).counts.cancelled
        ),
        (6, 3)
    );
}
