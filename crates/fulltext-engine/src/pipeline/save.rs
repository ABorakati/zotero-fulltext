//! PDF validation and saving, shared by OA and institutional paths.
//!
//! # Contract (`save_pdf_blob`)
//! 1. `validate(&blob.verdict)` (reject non-PDF / HTML / tiny). `trim_prefix = verdict.junk_prefix` so the stored
//!    file starts with `%PDF-` at offset 0 (JS trims and re-asserts the 5 bytes).
//! 2. Group library + institutional route -> `cx.group_consent()` once per run (policy `Ask`), `Never` -> hand-off
//!    outcome (decided by the caller via `claim_inst_slot`; this is the late re-check).
//! 3. `cx.mark_saving()` (cancellation no longer drops this item), `acquire_save()`, row `SAVING`,
//!    `cx.save_pdf(SaveReq { url_to_store: pure::storable_url(..), route, source, version, .. })`.
//! 4. Map the result: `Ok` -> `Outcome::Saved`, commit the ledger ticket with the real size;
//!    `AlreadyHasPdf` -> `Skipped(PdfAddedMeanwhile)`; `NotEditable` -> `Skipped(ReadOnly)`;
//!    `ParentGone` -> `Cancelled(ParentGone)`; `Io` -> `Failed { code: "io" }`. The `Body` is dropped (released)
//!    only after the op finished.
//!
//! # Licence ledger
//! An institutional PDF was downloaded whatever happens next, so its ticket is committed with the real size after
//! the save op (any result) and after a late "group library refused". It is rolled back only when the bytes turn
//! out not to be a PDF at all (`validate` fails).
use crate::cx::ItemCx;
use crate::pipeline::attempt::PdfBlob;
use crate::pipeline::{handoff, pure};
use crate::proto::*;
use crate::sched::source_name;
use fulltext_core::model::*;

/// Why bytes that looked like a PDF are not acceptable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BadPdf {
    NotPdf,
    LooksHtml,
    TooShort,
}

/// Bytes below this are never a real article PDF.
pub(crate) const MIN_PDF_BYTES: u64 = 1024;

/// Accept / reject a PDF verdict. `Ok(trim_prefix)`. A missing `%%EOF` (`has_eof == false`) is only a warning:
/// truncated downloads are caught by the byte count / Content-Length check in the fetch tier, not here.
pub(crate) fn validate(v: &PdfVerdict) -> Result<u32, BadPdf> {
    if !v.is_pdf {
        return Err(BadPdf::NotPdf);
    }
    if v.looks_html {
        return Err(BadPdf::LooksHtml);
    }
    if v.len < MIN_PDF_BYTES {
        return Err(BadPdf::TooShort);
    }
    Ok(v.junk_prefix)
}

/// Count an institutional download (any outcome after the bytes arrived) with its real size.
fn commit_ticket(blob: &mut PdfBlob) {
    if let Some(ticket) = blob.ticket.take() {
        ticket.commit(blob.verdict.len);
    }
}

/// Is saving into this library allowed? Only an institutional PDF in a group library needs consent
/// (`Ask`: one answer per run), `Never` refuses, `Allow` goes ahead.
async fn group_allows(cx: &ItemCx, route: Route) -> bool {
    if route != Route::Inst || cx.snap.library.kind != LibraryKind::Group {
        return true;
    }
    match cx.cfg.inst.group_libraries {
        GroupPolicy::Allow => true,
        GroupPolicy::Never => false,
        GroupPolicy::Ask => cx.group_consent().await,
    }
}

/// What a failed save op stands for.
fn outcome_for_save_error(e: &OpErr) -> Outcome {
    match e.kind {
        OpErrKind::AlreadyHasPdf => Outcome::Skipped {
            reason: SkipReason::PdfAddedMeanwhile,
        },
        OpErrKind::NotEditable => Outcome::Skipped {
            reason: SkipReason::ReadOnly,
        },
        OpErrKind::ParentGone => Outcome::Cancelled {
            by: CancelledBy::ParentGone,
        },
        OpErrKind::Cancelled => Outcome::Cancelled {
            by: CancelledBy::Shutdown,
        },
        OpErrKind::Io => Outcome::Failed {
            code: "io".to_owned(),
        },
        OpErrKind::Timeout
        | OpErrKind::Network
        | OpErrKind::TooLarge
        | OpErrKind::Tls
        | OpErrKind::Unsupported
        | OpErrKind::Offline
        | OpErrKind::Internal => {
            let code = serde_json::to_value(e.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "save".to_owned());
            Outcome::Failed { code }
        }
    }
}

/// Save a verified PDF and map the result to an `Outcome`.
pub(crate) async fn save_pdf_blob(cx: &ItemCx, mut blob: PdfBlob, ids: &Ids) -> Outcome {
    let Ok(trim_prefix) = validate(&blob.verdict) else {
        // Not a PDF after all: nothing was downloaded that counts against the licence.
        if let Some(ticket) = blob.ticket.take() {
            ticket.rollback();
        }
        return Outcome::Failed {
            code: "invalid_pdf".to_owned(),
        };
    };

    if !group_allows(cx, blob.route).await {
        commit_ticket(&mut blob);
        return Outcome::NeedsUser {
            why: NeedsWhy::GroupLibrary,
            handoff: handoff::handoff(&cx.cfg.institution, ids),
        };
    }

    // From here on a cancellation lets the save finish.
    cx.mark_saving();
    let Ok(_save_slot) = cx.acquire_save().await else {
        commit_ticket(&mut blob);
        return Outcome::Cancelled {
            by: CancelledBy::Shutdown,
        };
    };
    cx.progress(crate::l10n::row(RowState::Working, crate::l10n::id::SAVING).on_route(blob.route));

    let url_to_store = pure::storable_url(
        &blob.url,
        blob.landing_url.as_deref(),
        ids,
        &cx.cfg.institution,
    );
    let source = source_name(blob.source);
    let req = SaveReq {
        item: cx.item,
        body: blob.body.body_ref(),
        trim_prefix,
        version: blob.version,
        url_to_store: url_to_store.clone(),
        route: blob.route,
        source: source.clone(),
    };
    // `blob.body` stays alive across the op: JS reads the bytes through the handle until it answers.
    let saved = cx.save_pdf(req).await;
    commit_ticket(&mut blob);
    match saved {
        Ok(done) => Outcome::Saved {
            route: blob.route,
            source,
            version: blob.version,
            url: url_to_store,
            attachment_key: done.attachment_key,
        },
        Err(e) => outcome_for_save_error(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::Body;
    use crate::pipeline::attempt::{FetchEnd, FetchPolicy, fetch_candidate};
    use crate::pipeline::pool::CandidatePool;
    use crate::pipeline::testkit::{Rig, script};
    use crate::sched::InstKey;
    use crate::sim::Reply;
    use crate::sim::fixtures::{self, body, candidate, http_ok, is_type, pdf_inspection, save_ok};
    use std::rc::Rc;

    fn verdict() -> PdfVerdict {
        PdfVerdict {
            is_pdf: true,
            junk_prefix: 3,
            has_eof: true,
            encrypted: false,
            looks_html: false,
            len: 50_000,
        }
    }

    fn snap() -> ItemSnapshot {
        fixtures::item(1, "10.1234/x")
    }

    fn ids() -> Ids {
        Ids {
            doi: Some("10.1234/x".into()),
            ..Ids::default()
        }
    }

    fn blob(cx: &ItemCx, route: Route, v: PdfVerdict, url: &str) -> PdfBlob {
        PdfBlob {
            body: Body::new(Rc::clone(&cx.shared), body(7, v.len)),
            verdict: v,
            url: url.to_owned(),
            final_url: url.to_owned(),
            landing_url: None,
            source: Source::PmcS3,
            version: Version::Published,
            route,
            ticket: None,
        }
    }

    fn save_req(rig: &Rig) -> Option<SaveReq> {
        rig.effects().into_iter().find_map(|e| match e {
            Effect::SavePdf { req, .. } => Some(req),
            _ => None,
        })
    }

    const PMC: &str = "https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf";

    #[test]
    fn validates() {
        assert_eq!(validate(&verdict()), Ok(3));
        assert_eq!(
            validate(&PdfVerdict {
                is_pdf: false,
                ..verdict()
            }),
            Err(BadPdf::NotPdf)
        );
        assert_eq!(
            validate(&PdfVerdict {
                looks_html: true,
                ..verdict()
            }),
            Err(BadPdf::LooksHtml)
        );
        assert_eq!(
            validate(&PdfVerdict {
                len: 10,
                ..verdict()
            }),
            Err(BadPdf::TooShort)
        );
        assert_eq!(
            validate(&PdfVerdict {
                has_eof: false,
                ..verdict()
            }),
            Ok(3)
        );
    }

    #[test]
    fn a_verified_pdf_is_saved_and_its_body_released_afterwards() {
        let mut rig =
            Rig::new(script().on(is_type("save_pdf"), Reply::ok(200, save_ok("1/ABCD1234"))));
        let outcome = rig
            .exec(snap(), |cx| async move {
                let b = blob(&cx, Route::Oa, verdict(), PMC);
                save_pdf_blob(&cx, b, &ids()).await
            })
            .expect("done");
        assert_eq!(
            outcome,
            Outcome::Saved {
                route: Route::Oa,
                source: "pmc_s3".into(),
                version: Version::Published,
                url: PMC.into(),
                attachment_key: "1/ABCD1234".into()
            }
        );
        let req = save_req(&rig).expect("save request");
        assert_eq!(
            (req.item, req.trim_prefix, req.route, req.version),
            (1, 3, Route::Oa, Version::Published)
        );
        assert_eq!(
            (req.body, req.source.as_str(), req.url_to_store.as_str()),
            (fixtures::body(7, 50_000), "pmc_s3", PMC)
        );
        let effects = rig.effects();
        let save_at = effects
            .iter()
            .position(|e| matches!(e, Effect::SavePdf { .. }));
        let release_at = effects
            .iter()
            .position(|e| matches!(e, Effect::Release { handle: 7 }));
        assert!(
            save_at.is_some() && release_at.is_some() && save_at < release_at,
            "released only after the save: {save_at:?} {release_at:?}"
        );
        assert!(rig.rows().contains(&crate::l10n::id::SAVING.to_owned()));
    }

    #[test]
    fn the_stored_url_is_the_un_proxied_clean_one() {
        let mut rig = Rig::new(script().on(is_type("save_pdf"), Reply::ok(10, save_ok("1/K"))));
        let outcome = rig
            .exec(snap(), |cx| async move {
                let b = blob(&cx, Route::Inst, verdict(), "https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71.full.pdf?utm_source=x");
                save_pdf_blob(&cx, b, &ids()).await
            })
            .expect("done");
        assert!(
            matches!(&outcome, Outcome::Saved { url, route: Route::Inst, .. } if url == "https://www.bmj.com/content/372/bmj.n71.full.pdf"),
            "{outcome:?}"
        );
        assert_eq!(
            save_req(&rig).map(|r| r.url_to_store).as_deref(),
            Some("https://www.bmj.com/content/372/bmj.n71.full.pdf")
        );
    }

    #[test]
    fn save_errors_map_to_outcomes() {
        let cases = [
            (
                OpErrKind::AlreadyHasPdf,
                Outcome::Skipped {
                    reason: SkipReason::PdfAddedMeanwhile,
                },
            ),
            (
                OpErrKind::NotEditable,
                Outcome::Skipped {
                    reason: SkipReason::ReadOnly,
                },
            ),
            (
                OpErrKind::ParentGone,
                Outcome::Cancelled {
                    by: CancelledBy::ParentGone,
                },
            ),
            (OpErrKind::Io, Outcome::Failed { code: "io".into() }),
            (
                OpErrKind::Cancelled,
                Outcome::Cancelled {
                    by: CancelledBy::Shutdown,
                },
            ),
            (
                OpErrKind::Internal,
                Outcome::Failed {
                    code: "internal".into(),
                },
            ),
        ];
        for (kind, want) in cases {
            let mut rig = Rig::new(script().on(is_type("save_pdf"), Reply::err(20, kind)));
            let outcome = rig
                .exec(snap(), |cx| async move {
                    let b = blob(&cx, Route::Oa, verdict(), PMC);
                    save_pdf_blob(&cx, b, &ids()).await
                })
                .expect("done");
            assert_eq!(outcome, want, "{kind:?}");
            assert_eq!(
                rig.count(|e| matches!(e, Effect::Release { handle: 7 })),
                1,
                "{kind:?}: the body is released once"
            );
        }
    }

    #[test]
    fn a_blob_that_fails_validation_is_not_saved_and_gives_the_reservation_back() {
        let mut rig = Rig::new(script());
        let outcome = rig
            .exec(snap(), |cx| async move {
                let mut b = blob(
                    &cx,
                    Route::Inst,
                    PdfVerdict {
                        is_pdf: false,
                        ..verdict()
                    },
                    PMC,
                );
                b.ticket = cx.reserve_licence(1 << 20).ok();
                save_pdf_blob(&cx, b, &ids()).await
            })
            .expect("done");
        assert_eq!(
            outcome,
            Outcome::Failed {
                code: "invalid_pdf".into()
            }
        );
        assert!(save_req(&rig).is_none());
        let ledger = rig
            .host
            .persisted
            .get(&PersistKey::Ledger)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            (
                ledger["reserved"].as_array().map(Vec::len),
                ledger["committed"].as_array().map(Vec::len)
            ),
            (Some(0), Some(0))
        );
    }

    #[test]
    fn an_institutional_download_is_committed_with_its_real_size() {
        let pdf = http_ok(
            200,
            "https://pub-a.example.org/a.pdf",
            &pdf_inspection(1_234_567),
            Some(body(5, 1_234_567)),
        );
        let s = script()
            .on(is_type("http"), Reply::ok(300, pdf))
            .on(is_type("save_pdf"), Reply::ok(100, save_ok("1/K")));
        let mut rig = Rig::new(s);
        let outcome = rig
            .exec(snap(), |cx| async move {
                let mut pool = CandidatePool::new(ids());
                let cand = candidate(
                    "https://pub-a.example.org/a.pdf",
                    CandKind::Pdf,
                    Source::Redirector,
                    0,
                );
                match fetch_candidate(
                    &cx,
                    &cand,
                    &FetchPolicy::inst(InstKey("pub-a".into())),
                    &mut pool,
                )
                .await
                {
                    FetchEnd::Pdf(b) => save_pdf_blob(&cx, b, &ids()).await,
                    _ => Outcome::Failed {
                        code: "no pdf".into(),
                    },
                }
            })
            .expect("done");
        assert!(
            matches!(
                outcome,
                Outcome::Saved {
                    route: Route::Inst,
                    ..
                }
            ),
            "{outcome:?}"
        );
        let ledger = rig
            .host
            .persisted
            .get(&PersistKey::Ledger)
            .cloned()
            .unwrap_or_default();
        assert_eq!(ledger["reserved"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            ledger["committed"][0][1],
            serde_json::json!(1_234_567),
            "the real size, not the 4 MiB estimate"
        );
        // reserve (persist) -> request -> commit (persist): the ledger was written before the barrier fetch
        let effects = rig.effects();
        let first_persist = effects.iter().position(|e| {
            matches!(
                e,
                Effect::Persist {
                    key: PersistKey::Ledger,
                    ..
                }
            )
        });
        let http = effects
            .iter()
            .position(|e| matches!(e, Effect::Http { barrier: true, .. }));
        assert!(first_persist < http);
    }

    #[test]
    fn a_failed_save_still_counts_the_download() {
        let pdf = http_ok(
            200,
            "https://pub-a.example.org/a.pdf",
            &pdf_inspection(777_000),
            Some(body(5, 777_000)),
        );
        let s = script().on(is_type("http"), Reply::ok(300, pdf)).on(
            is_type("save_pdf"),
            Reply::err(100, OpErrKind::AlreadyHasPdf),
        );
        let mut rig = Rig::new(s);
        let outcome = rig
            .exec(snap(), |cx| async move {
                let mut pool = CandidatePool::new(ids());
                let cand = candidate(
                    "https://pub-a.example.org/a.pdf",
                    CandKind::Pdf,
                    Source::Redirector,
                    0,
                );
                match fetch_candidate(
                    &cx,
                    &cand,
                    &FetchPolicy::inst(InstKey("pub-a".into())),
                    &mut pool,
                )
                .await
                {
                    FetchEnd::Pdf(b) => save_pdf_blob(&cx, b, &ids()).await,
                    _ => Outcome::Failed {
                        code: "no pdf".into(),
                    },
                }
            })
            .expect("done");
        assert_eq!(
            outcome,
            Outcome::Skipped {
                reason: SkipReason::PdfAddedMeanwhile
            }
        );
        let ledger = rig
            .host
            .persisted
            .get(&PersistKey::Ledger)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            ledger["committed"][0][1],
            serde_json::json!(777_000),
            "the publisher served it: it counts"
        );
    }

    #[test]
    fn group_libraries_follow_the_policy_for_institutional_pdfs_only() {
        let group = |mut s: ItemSnapshot| {
            s.library = LibraryInfo {
                id: 2,
                kind: LibraryKind::Group,
                files_editable: true,
                name: "Lab".into(),
            };
            s
        };
        // Never: hand-off, nothing saved, the download is still counted
        let mut cfg = fixtures::test_config();
        cfg.inst.group_libraries = GroupPolicy::Never;
        let mut rig = Rig::with_config(cfg.clone(), script());
        let outcome = rig
            .exec(group(snap()), |cx| async move {
                let mut b = blob(
                    &cx,
                    Route::Inst,
                    verdict(),
                    "https://pub-a.example.org/a.pdf",
                );
                b.ticket = cx.reserve_licence(1 << 20).ok();
                save_pdf_blob(&cx, b, &ids()).await
            })
            .expect("done");
        assert!(
            matches!(&outcome, Outcome::NeedsUser { why: NeedsWhy::GroupLibrary, handoff } if handoff.doi_url.as_deref() == Some("https://doi.org/10.1234/x")),
            "{outcome:?}"
        );
        assert!(save_req(&rig).is_none());
        let ledger = rig
            .host
            .persisted
            .get(&PersistKey::Ledger)
            .cloned()
            .unwrap_or_default();
        assert_eq!(ledger["committed"].as_array().map(Vec::len), Some(1));

        // Allow: saved
        cfg.inst.group_libraries = GroupPolicy::Allow;
        let mut rig = Rig::with_config(
            cfg.clone(),
            script().on(is_type("save_pdf"), Reply::ok(10, save_ok("2/K"))),
        );
        let outcome = rig
            .exec(group(snap()), |cx| async move {
                let b = blob(
                    &cx,
                    Route::Inst,
                    verdict(),
                    "https://pub-a.example.org/a.pdf",
                );
                save_pdf_blob(&cx, b, &ids()).await
            })
            .expect("done");
        assert!(matches!(outcome, Outcome::Saved { .. }));

        // An open-access PDF never needs consent, even under `Never`
        cfg.inst.group_libraries = GroupPolicy::Never;
        let mut rig = Rig::with_config(
            cfg,
            script().on(is_type("save_pdf"), Reply::ok(10, save_ok("2/K"))),
        );
        let outcome = rig
            .exec(group(snap()), |cx| async move {
                let b = blob(&cx, Route::Oa, verdict(), PMC);
                save_pdf_blob(&cx, b, &ids()).await
            })
            .expect("done");
        assert!(matches!(
            outcome,
            Outcome::Saved {
                route: Route::Oa,
                ..
            }
        ));
    }

    #[test]
    fn saves_run_one_at_a_time() {
        let mut rig = Rig::new(script().on(is_type("save_pdf"), Reply::ok(500, save_ok("1/K"))));
        let outcomes = rig
            .exec(snap(), |cx| async move {
                let (a, b) = (
                    blob(&cx, Route::Oa, verdict(), PMC),
                    blob(
                        &cx,
                        Route::Oa,
                        verdict(),
                        "https://pmc-oa-opendata.s3.amazonaws.com/PMC2.1/PMC2.1.pdf",
                    ),
                );
                let ids = ids();
                futures_util::future::join(save_pdf_blob(&cx, a, &ids), save_pdf_blob(&cx, b, &ids))
                    .await
            })
            .expect("done");
        assert!(matches!(
            outcomes,
            (Outcome::Saved { .. }, Outcome::Saved { .. })
        ));
        let starts: Vec<u64> = rig
            .timeline()
            .into_iter()
            .filter(|(_, e)| matches!(e, Effect::SavePdf { .. }))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(starts.len(), 2);
        assert!(
            starts[1] >= starts[0] + 500,
            "the second save waits for the first: {starts:?}"
        );
    }
}
