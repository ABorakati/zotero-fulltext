//! Run-level scenarios: F17 shutdown mid-batch, F18 items that already have a PDF, F20 group libraries,
//! F21 duplicate concurrent runs, F22 user cancel, F23 offline, F27 failures at save time.
use super::*;
use crate::l10n::id;
use crate::sim::SimHost;
use crate::sim::fixtures::{is_type, item, login_ok, save_ok};
use std::cell::Cell;
use std::rc::Rc;

fn doc(doi: &str, oa: bool) -> Doc {
    Doc {
        doi: doi.to_owned(),
        oa,
        held: true,
        direct: false,
    }
}

fn six_wide() -> EngineConfig {
    let mut cfg = test_config();
    cfg.run.item_concurrency = 6;
    cfg
}

fn pending_ops(host: &SimHost, ty: &'static str) -> Vec<OpId> {
    host.pending_where(is_type(ty))
}

fn cancels_in(out: &StepOut) -> Vec<OpId> {
    out.effects
        .iter()
        .filter_map(|e| {
            if let Effect::Cancel { op } = e {
                Some(*op)
            } else {
                None
            }
        })
        .collect()
}

fn new_requests_in(out: &StepOut) -> usize {
    out.effects
        .iter()
        .filter(|e| {
            matches!(
                e,
                Effect::Http { .. }
                    | Effect::BrowserLoad { .. }
                    | Effect::LoginWindow { .. }
                    | Effect::ZoteroOa { .. }
                    | Effect::RunTranslators { .. }
                    | Effect::Confirm { .. }
                    | Effect::SavePdf { .. }
            )
        })
        .count()
}

fn landed() -> Reply {
    Reply::ok(
        0,
        login_ok(
            LoginResult::Landed,
            PUBLISHER_PAGE,
            &Inspection::None,
            None,
            None,
        ),
    )
}

// ----------------------------------------------------------------------------------------------------- F17

/// Items in every stage when the user quits Zotero: two downloads in flight, three items parked at one sign-in
/// window, one save in flight. Shutdown cancels every request in its own step (including the window), flushes
/// the persisted state, starts nothing new, and the item that is saving still finishes.
#[test]
fn f17_shutdown_mid_batch_cancels_everything_and_lets_the_save_finish() {
    let d = dois("f17", 6);
    let docs: Vec<Doc> = d
        .iter()
        .enumerate()
        .map(|(i, doi)| doc(doi, i < 3))
        .collect();
    let hang = [oa_pdf_url(&d[0]), oa_pdf_url(&d[1])];
    let world = World::new(docs).hook(move |op, _| match &op.effect {
        Effect::Http { req, .. } if hang.contains(&req.url) => Some(Reply::Never),
        Effect::SavePdf { .. } | Effect::LoginWindow { .. } => Some(Reply::Never),
        _ => None,
    });
    let mut host = run_scenario_with(six_wide(), Persisted::default(), world);
    start(&mut host, 1, Mode::Full, true, items(&d));
    run_for(&mut host, 30_000);
    assert_eq!(pending_ops(&host, "http").len(), 2, "two downloads hang");
    assert_eq!(
        pending_ops(&host, "login_window").len(),
        1,
        "one window for three items"
    );
    let saves = pending_ops(&host, "save_pdf");
    assert_eq!(saves.len(), 1, "one save is in flight");

    let out = host.step(vec![Command::Shutdown]).expect("shutdown");
    assert_eq!(
        cancels_in(&out).len(),
        3,
        "both downloads and the sign-in window: {:?}",
        out.effects
    );
    assert_eq!(
        host.pending.keys().copied().collect::<Vec<_>>(),
        saves,
        "only the save is still out"
    );
    let flushed: Vec<PersistKey> = out
        .effects
        .iter()
        .filter_map(|e| {
            if let Effect::Persist {
                key, flush: true, ..
            } = e
            {
                Some(*key)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        flushed,
        vec![
            PersistKey::Ledger,
            PersistKey::Halts,
            PersistKey::Ezproxy,
            PersistKey::Alma
        ]
    );
    assert_eq!(
        new_requests_in(&out),
        0,
        "a shut-down engine starts nothing"
    );
    assert!(host.reports.is_empty(), "the run waits for the save");

    assert!(host.answer(saves[0], Reply::ok(0, save_ok("1/K0003"))));
    settle(&mut host);
    let r = report(&host, 1);
    assert_eq!((r.counts.saved, r.counts.cancelled), (1, 5));
    assert!(is_saved(&outcome(&host, 1, 3), Route::Oa));
    for item in [1, 2, 4, 5, 6] {
        assert_eq!(
            outcome(&host, 1, item),
            Outcome::Cancelled {
                by: CancelledBy::Shutdown
            },
            "item {item}"
        );
    }
    assert_eq!(host.reports.len(), 1);
}

// ----------------------------------------------------------------------------------------------------- F18

/// An item that has a PDF, one whose PDF is only synced, and one that gets a PDF while we search.
#[test]
fn f18_items_with_a_pdf_are_skipped_before_any_network_and_a_late_pdf_is_discarded() {
    let d = dois("f18", 3);
    let mut has = item(1, &d[0]);
    has.pdf_attachments = 1;
    let mut synced = item(2, &d[1]);
    synced.pdf_attachments = 1;
    synced.best_attachment = BestAttachment {
        kind: AttachKind::Pdf,
        exists: Some(false),
    };
    let late = item(3, &d[2]);
    let world = World::open_access(&d).hook(|op, _| match &op.effect {
        Effect::SavePdf { .. } => Some(Reply::err(80, OpErrKind::AlreadyHasPdf)),
        _ => None,
    });
    let mut host = run_scenario(world);
    start(&mut host, 1, Mode::Full, true, vec![has, synced, late]);
    settle(&mut host);
    assert_eq!(
        outcome(&host, 1, 1),
        Outcome::Skipped {
            reason: SkipReason::HasPdf
        }
    );
    assert_eq!(
        outcome(&host, 1, 2),
        Outcome::Skipped {
            reason: SkipReason::PdfNotDownloaded
        }
    );
    assert_eq!(
        outcome(&host, 1, 3),
        Outcome::Skipped {
            reason: SkipReason::PdfAddedMeanwhile
        }
    );
    assert_eq!(report(&host, 1).counts.skipped, 3);
    for item in [1, 2] {
        let touched = count(
            &host,
            |e| matches!(e, Effect::Http { item: i, .. } | Effect::ZoteroOa { item: i, .. } if *i == item),
        );
        assert_eq!(
            touched, 0,
            "item {item} was decided from the snapshot alone"
        );
    }
    assert_eq!(
        rows(&host, 1, 2).last().map(String::as_str),
        Some(id::SKIPPED_NOT_DOWNLOADED)
    );
    assert_eq!(
        rows(&host, 1, 3).last().map(String::as_str),
        Some(id::SKIPPED_HAS_PDF),
        "the late PDF is reported like an existing one"
    );
    assert_eq!(count(&host, |e| matches!(e, Effect::SavePdf { .. })), 1);
}

// ----------------------------------------------------------------------------------------------------- F20

fn group_item(id: ItemId, doi: &str) -> ItemSnapshot {
    let mut it = item(id, doi);
    it.library = LibraryInfo {
        id: 5,
        kind: LibraryKind::Group,
        files_editable: true,
        name: "Lab group".into(),
    };
    it
}

fn group_prompts(host: &SimHost) -> Vec<ConfirmReq> {
    host.effects()
        .filter_map(|e| {
            if let Effect::Confirm { req, .. } = e {
                Some(req.clone())
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn f20_group_library_ask_asks_once_for_the_whole_run() {
    let d = dois("f20a", 4);
    let batch = vec![
        group_item(1, &d[0]),
        group_item(2, &d[1]),
        group_item(3, &d[2]),
        item(4, &d[3]),
    ];
    let mut host = run_scenario(World::inst_only(&d));
    start(&mut host, 1, Mode::Full, true, batch);
    settle(&mut host);
    assert_eq!(
        group_prompts(&host),
        vec![ConfirmReq {
            kind: ConfirmKind::GroupLibrary,
            n: 3,
            cap: 0,
            library: "Lab group".into()
        }],
        "one prompt for three items"
    );
    for item in 1..=4 {
        assert!(
            is_saved(&outcome(&host, 1, item), Route::Inst),
            "item {item}: {:?}",
            outcome(&host, 1, item)
        );
    }
    assert_eq!(count(&host, |e| matches!(e, Effect::LoginWindow { .. })), 1);
}

#[test]
fn f20_group_library_declined_hands_the_item_off_without_downloading() {
    let d = dois("f20b", 3);
    let batch = vec![group_item(1, &d[0]), group_item(2, &d[1]), item(3, &d[2])];
    let mut world = World::inst_only(&d);
    world.confirm = ConfirmChoice::OaOnly;
    let mut host = run_scenario(world);
    start(&mut host, 1, Mode::Full, true, batch);
    settle(&mut host);
    assert_eq!(group_prompts(&host).len(), 1);
    for item in [1, 2] {
        match outcome(&host, 1, item) {
            Outcome::NeedsUser {
                why: NeedsWhy::GroupLibrary,
                handoff,
            } => assert!(
                handoff.redirector_url.is_some() && handoff.doi_url.is_some(),
                "{handoff:?}"
            ),
            other => panic!("item {item}: {other:?}"),
        }
    }
    assert!(
        is_saved(&outcome(&host, 1, 3), Route::Inst),
        "the user library is unaffected"
    );
    assert_eq!(
        ledger_counts(&host),
        (1, 0),
        "only the user-library PDF was downloaded and counted"
    );
}

#[test]
fn f20_group_library_never_hands_off_but_open_access_still_saves() {
    let d = dois("f20c", 3);
    let docs = vec![doc(&d[0], true), doc(&d[1], false), doc(&d[2], false)];
    let mut cfg = test_config();
    cfg.inst.group_libraries = GroupPolicy::Never;
    let mut host = run_scenario_with(cfg, Persisted::default(), World::new(docs));
    start(
        &mut host,
        1,
        Mode::Full,
        true,
        vec![group_item(1, &d[0]), group_item(2, &d[1]), item(3, &d[2])],
    );
    settle(&mut host);
    assert!(group_prompts(&host).is_empty(), "policy Never never asks");
    assert!(
        is_saved(&outcome(&host, 1, 1), Route::Oa),
        "an open-access copy goes into the group library without any prompt"
    );
    assert!(matches!(
        outcome(&host, 1, 2),
        Outcome::NeedsUser {
            why: NeedsWhy::GroupLibrary,
            ..
        }
    ));
    assert!(is_saved(&outcome(&host, 1, 3), Route::Inst));
}

// ----------------------------------------------------------------------------------------------------- F21

#[test]
fn f21_duplicate_concurrent_runs_leave_the_shared_item_to_the_first_run() {
    let d = dois("f21", 4);
    let all = items(&d);
    let mut host = run_scenario(World::open_access(&d));
    start(&mut host, 1, Mode::Full, true, all[..2].to_vec());
    let out = start(&mut host, 2, Mode::Full, true, all[1..].to_vec());
    assert!(
        out.effects.iter().any(|e| matches!(e, Effect::Progress { run: 2, item: 2, row } if row.l10n == id::SKIPPED_IN_PROGRESS && row.state == RowState::Skipped)),
        "the second run learns it at once"
    );
    settle(&mut host);
    assert_eq!(
        outcome(&host, 2, 2),
        Outcome::Skipped {
            reason: SkipReason::InProgressElsewhere
        }
    );
    assert!(
        is_saved(&outcome(&host, 1, 2), Route::Oa),
        "the first run is undisturbed"
    );
    assert!(
        is_saved(&outcome(&host, 2, 3), Route::Oa) && is_saved(&outcome(&host, 2, 4), Route::Oa)
    );
    let labelled = count(&host, |e| {
        matches!(
            e,
            Effect::Http {
                run: 2,
                item: 2,
                ..
            } | Effect::ZoteroOa {
                run: 2,
                item: 2,
                ..
            } | Effect::SavePdf {
                run: 2,
                item: 2,
                ..
            }
        )
    });
    assert_eq!(labelled, 0, "no request was made for the duplicate");
    assert_eq!(
        count(&host, |e| matches!(e, Effect::SavePdf { item: 2, .. })),
        1,
        "saved once"
    );

    // The claim ended with run 1: the item can be searched for again.
    start(&mut host, 3, Mode::Full, true, vec![all[1].clone()]);
    settle(&mut host);
    assert!(is_saved(&outcome(&host, 3, 2), Route::Oa));
}

// ----------------------------------------------------------------------------------------------------- F22

/// Run 1 is cancelled while its items download, wait at the shared sign-in window and save; run 2 shares the
/// window and carries on.
#[test]
fn f22_user_cancel_cancels_every_op_of_the_run_in_the_same_step_and_spares_the_other_run() {
    let d1 = dois("f22a", 5);
    let d2 = dois("f22b", 2);
    let mut docs: Vec<Doc> = d1
        .iter()
        .enumerate()
        .map(|(i, doi)| doc(doi, i == 0 || i == 1 || i == 4))
        .collect();
    docs.extend([doc(&d2[0], false), doc(&d2[1], true)]);
    let hang = [oa_pdf_url(&d1[0]), oa_pdf_url(&d1[1])];
    let world = World::new(docs).hook(move |op, _| match &op.effect {
        Effect::Http { req, .. } if hang.contains(&req.url) => Some(Reply::Never),
        Effect::SavePdf { req, .. } if req.item == 5 => Some(Reply::Never),
        Effect::LoginWindow { .. } => Some(Reply::Never),
        _ => None,
    });
    let session = Rc::clone(&world.session);
    let mut host = run_scenario_with(six_wide(), Persisted::default(), world);
    let run1 = items(&d1);
    let run2 = vec![item(11, &d2[0]), item(12, &d2[1])];
    host.step(vec![
        Command::StartRun {
            run: 1,
            mode: Mode::Full,
            interactive: true,
            items: run1,
            resume: Vec::new(),
        },
        Command::StartRun {
            run: 2,
            mode: Mode::Full,
            interactive: true,
            items: run2,
            resume: Vec::new(),
        },
    ])
    .expect("start");
    run_for(&mut host, 30_000);
    assert_eq!(
        pending_ops(&host, "http").len(),
        2,
        "two downloads of run 1 hang"
    );
    assert_eq!(
        pending_ops(&host, "login_window").len(),
        1,
        "run 1 (items 3, 4) and run 2 (item 11) wait at one window"
    );
    assert_eq!(pending_ops(&host, "save_pdf").len(), 1, "item 5 is saving");

    let before = host.pending.len();
    let out = host
        .step(vec![Command::CancelRun { run: 1 }])
        .expect("cancel");
    // The window was opened for run 1's item 3 (the only item that asked for it; the others wait for its
    // outcome), so it closes with the run, in the same step.
    assert_eq!(
        cancels_in(&out).len(),
        3,
        "two downloads and the window, in the step of the command: {:?}",
        out.effects
    );
    let run1_ops: Vec<&PendingOp> = host
        .pending
        .values()
        .filter(|p| {
            matches!(
                &p.effect,
                Effect::Http { run: 1, .. }
                    | Effect::BrowserLoad { run: 1, .. }
                    | Effect::LoginWindow { run: 1, .. }
            )
        })
        .collect();
    assert!(
        run1_ops.is_empty(),
        "nothing of run 1 is left but its save: {run1_ops:?}"
    );
    assert_eq!(pending_ops(&host, "save_pdf").len(), 1);
    assert!(before >= 4);
    for item in [1, 2, 3, 4] {
        assert!(
            out.effects.iter().any(|e| matches!(e, Effect::Progress { run: 1, item: i, row } if *i == item && row.state == RowState::Cancelled)),
            "item {item} got its terminal row in that step"
        );
    }
    assert!(
        host.reports.iter().all(|r| r.run != 1),
        "run 1 waits for item 5's save"
    );

    // Run 2's item 11 takes over and asks for a window of its own; the user signs in, the save of item 5 completes.
    run_for(&mut host, 10_000);
    let windows = pending_ops(&host, "login_window");
    assert_eq!(windows.len(), 1, "one window again, this time for run 2");
    session.set(true);
    assert!(host.answer(windows[0], landed()));
    let saves = pending_ops(&host, "save_pdf");
    assert!(host.answer(saves[0], Reply::ok(0, save_ok("1/K0005"))));
    settle(&mut host);
    assert_eq!(report(&host, 1).counts.cancelled, 4);
    assert!(
        is_saved(&outcome(&host, 1, 5), Route::Oa),
        "a save that has begun is never cut off"
    );
    assert!(
        is_saved(&outcome(&host, 2, 11), Route::Inst)
            && is_saved(&outcome(&host, 2, 12), Route::Oa),
        "run 2 is untouched"
    );
}

// ----------------------------------------------------------------------------------------------------- F23

fn offline_until(ms: u64, start: Rc<Cell<u64>>) -> impl FnMut(&PendingOp, u64) -> Option<Reply> {
    move |op, now| match &op.effect {
        Effect::Http { .. } | Effect::ZoteroOa { .. } if now.saturating_sub(start.get()) < ms => {
            Some(Reply::err(5, OpErrKind::Offline))
        }
        _ => None,
    }
}

#[test]
fn f23_offline_pauses_probes_and_then_fails_the_items() {
    let d = dois("f23a", 3);
    let begin = Rc::new(Cell::new(0));
    let world = World::open_access(&d).hook(offline_until(u64::MAX, Rc::clone(&begin)));
    let mut host = run_scenario(world);
    begin.set(host.now);
    let t0 = host.now;
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    let waited = host.now - t0;
    assert!(
        (120_000..180_000).contains(&waited),
        "the engine probes for two minutes and gives up: {waited} ms"
    );
    for item in 1..=3 {
        assert_eq!(
            outcome(&host, 1, item),
            Outcome::Failed {
                code: "offline".into()
            }
        );
        assert!(
            rows(&host, 1, item).iter().any(|r| r == id::NETWORK_DOWN),
            "item {item} shows that it waits for the network"
        );
    }
    assert_eq!(report(&host, 1).counts.failed, 3);
    assert_eq!(
        count(&host, |e| matches!(
            e,
            Effect::SavePdf { .. } | Effect::BrowserLoad { .. } | Effect::LoginWindow { .. }
        )),
        0
    );
}

#[test]
fn f23_the_network_comes_back_and_the_items_carry_on() {
    let d = dois("f23b", 3);
    let begin = Rc::new(Cell::new(0));
    let world = World::open_access(&d).hook(offline_until(40_000, Rc::clone(&begin)));
    let mut host = run_scenario(world);
    begin.set(host.now);
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    for item in 1..=3 {
        assert!(
            is_saved(&outcome(&host, 1, item), Route::Oa),
            "item {item}: {:?}",
            outcome(&host, 1, item)
        );
        assert!(rows(&host, 1, item).iter().any(|r| r == id::NETWORK_DOWN));
    }
}

// ----------------------------------------------------------------------------------------------------- F27

#[test]
fn f27_failures_at_save_time_end_each_item_cleanly() {
    let d = dois("f27", 5);
    let mut docs: Vec<Doc> = d.iter().map(|doi| doc(doi, true)).collect();
    docs[4].oa = false; // the fifth comes through the institution
    let world = World::new(docs).hook(|op, _| match &op.effect {
        Effect::SavePdf { req, .. } => match req.item {
            1 => Some(Reply::err(50, OpErrKind::ParentGone)),
            2 => Some(Reply::err(50, OpErrKind::NotEditable)),
            3 => Some(Reply::err(50, OpErrKind::Io)),
            5 => Some(Reply::err(50, OpErrKind::ParentGone)),
            _ => None,
        },
        _ => None,
    });
    let mut host = run_scenario(world);
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    assert_eq!(
        outcome(&host, 1, 1),
        Outcome::Cancelled {
            by: CancelledBy::ParentGone
        }
    );
    assert_eq!(
        outcome(&host, 1, 2),
        Outcome::Skipped {
            reason: SkipReason::ReadOnly
        }
    );
    assert_eq!(outcome(&host, 1, 3), Outcome::Failed { code: "io".into() });
    assert!(is_saved(&outcome(&host, 1, 4), Route::Oa));
    assert_eq!(
        outcome(&host, 1, 5),
        Outcome::Cancelled {
            by: CancelledBy::ParentGone
        },
        "the same through the institution"
    );
    assert_eq!(
        ledger_counts(&host),
        (1, 0),
        "the institutional download happened and counts, saved or not"
    );
    let r = report(&host, 1);
    assert_eq!(
        (
            r.counts.saved,
            r.counts.skipped,
            r.counts.cancelled,
            r.counts.failed
        ),
        (1, 1, 2, 1)
    );
    assert_eq!(count(&host, |e| matches!(e, Effect::SavePdf { .. })), 5);
}
