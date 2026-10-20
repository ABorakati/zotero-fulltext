//! The per-item pipeline: one `async fn` per item (`item::run_item`), built from the stage modules below.
//!
//! ```text
//! item_runner(cx) = run_item(cx) -> cx.finish(outcome)
//!
//! run_item:  wait_admitted + item slot        mod.rs (Slot), run.rs (admission)
//!            precheck (skip rules)            precheck.rs
//!            identify (Ids)                   pure.rs
//!            discover_oa (parallel lookups)   oa.rs       -> CandidatePool (pool.rs)
//!            attempt OA candidates            attempt.rs  (tiers: http -> classify -> browser -> hedge)
//!              landing pages                  landing.rs  (Rust extractor + translators)
//!            institutional route              inst.rs     (direct / Alma / OpenAthens / EZproxy / login gate)
//!            validate + save                  save.rs     -> Outcome
//! ```
//! Parking: an item holds an item slot only while it is doing work. Before awaiting the login gate it releases
//! the slot (`parked`) and re-acquires with `Prio::Resumed`, so a sign-in prompt never starves OA work of the
//! other items. The time an item spends parked is remembered by its `Slot` and added to the item's time budget
//! (`item::run_item` gives up after `FetchClass::Item` of *working* time).
pub(crate) mod attempt;
pub(crate) mod handoff;
pub(crate) mod inst;
pub(crate) mod item;
pub(crate) mod landing;
pub(crate) mod oa;
pub(crate) mod pool;
pub(crate) mod precheck;
pub(crate) mod pure;
pub(crate) mod requests;
pub(crate) mod save;

use crate::cx::{ItemCx, LocalFuture};
use crate::sched::{ItemPermit, Prio, Refused};
use std::cell::Cell;
use std::future::Future;
use std::rc::Rc;

/// The production `ItemRunner`: the whole life of one item task.
pub(crate) fn item_runner(cx: ItemCx) -> LocalFuture {
    Box::pin(async move {
        let outcome = item::run_item(&cx).await;
        cx.finish(outcome);
    })
}

/// The item slot an item currently holds (or not, while parked).
pub(crate) struct Slot {
    permit: Option<ItemPermit>,
    /// Total ms this item spent parked (login waits, re-acquiring a slot afterwards).
    parked_ms: Rc<Cell<u64>>,
}

impl Slot {
    /// Wait for a slot.
    pub(crate) async fn acquire(cx: &ItemCx, prio: Prio) -> Result<Slot, Refused> {
        Ok(Slot {
            permit: Some(cx.acquire_item_slot(prio).await?),
            parked_ms: Rc::new(Cell::new(0)),
        })
    }

    /// Give the slot back (idempotent).
    pub(crate) fn release(&mut self) {
        self.permit = None;
    }

    /// Take a slot again (no-op when held).
    pub(crate) async fn reacquire(&mut self, cx: &ItemCx, prio: Prio) -> Result<(), Refused> {
        if self.permit.is_none() {
            self.permit = Some(cx.acquire_item_slot(prio).await?);
        }
        Ok(())
    }

    /// Shared counter of the ms spent parked; `item::run_item` reads it to extend the item's time budget.
    pub(crate) fn parked_clock(&self) -> Rc<Cell<u64>> {
        Rc::clone(&self.parked_ms)
    }
}

/// Run `fut` (typically a login-gate wait) without holding the item slot; re-acquire with `Prio::Resumed`
/// afterwards. A failed re-acquire (engine shutting down) is returned alongside the value. The whole parked time
/// (the wait and the re-acquire) is added to the slot's parked clock.
pub(crate) async fn parked<T>(
    cx: &ItemCx,
    slot: &mut Slot,
    fut: impl Future<Output = T>,
) -> (T, Result<(), Refused>) {
    slot.release();
    let started = cx.now();
    let value = fut.await;
    let back = slot.reacquire(cx, Prio::Resumed).await;
    let spent = cx.now().saturating_sub(started);
    slot.parked_ms
        .set(slot.parked_ms.get().saturating_add(spent));
    (value, back)
}

/// Harness for the async pipeline tests: a `SimHost` with a scripted environment and one item task running the
/// real pipeline (`item::run_admitted`), no run manager involved.
#[cfg(test)]
pub(crate) mod testkit {
    use crate::cx::{ItemCx, Owner};
    use crate::pipeline::item;
    use crate::proto::*;
    use crate::sim::fixtures::{self, ScriptResponder};
    use crate::sim::{Responder, SimEnd, SimHost};
    use fulltext_core::model::*;
    use std::cell::RefCell;
    use std::future::Future;
    use std::rc::Rc;

    /// A configured host plus the outcome slot of the item task.
    pub(crate) struct Rig {
        pub host: SimHost,
        outcome: Rc<RefCell<Option<Outcome>>>,
        cfg: Rc<EngineConfig>,
    }

    impl Rig {
        /// Configured with `fixtures::test_config()` (trace on).
        pub(crate) fn new(responder: impl Responder + 'static) -> Self {
            Self::with_config(fixtures::test_config(), responder)
        }

        pub(crate) fn with_config(cfg: EngineConfig, responder: impl Responder + 'static) -> Self {
            let mut host = SimHost::new(Box::new(responder));
            host.configure(cfg.clone(), Persisted::default(), 7)
                .expect("configure");
            Self {
                host,
                outcome: Rc::new(RefCell::new(None)),
                cfg: Rc::new(cfg),
            }
        }

        fn item_cx(&self, snap: ItemSnapshot, mode: Mode) -> ItemCx {
            let shared = Rc::clone(self.host.engine.shared());
            ItemCx::new(shared, 1, mode, true, Rc::clone(&self.cfg), Rc::new(snap))
        }

        /// Run `make(cx)` as the item task of `snap` (run 1, `Mode::Full`) and return its result: for unit tests of
        /// one pipeline stage.
        pub(crate) fn exec<T: 'static, F: Future<Output = T> + 'static>(
            &mut self,
            snap: ItemSnapshot,
            make: impl FnOnce(ItemCx) -> F,
        ) -> Option<T> {
            let item_id = snap.item;
            let fut = make(self.item_cx(snap, Mode::Full));
            let out: Rc<RefCell<Option<T>>> = Rc::new(RefCell::new(None));
            let slot = Rc::clone(&out);
            self.host
                .engine
                .spawn_test(
                    Owner::Item {
                        run: 1,
                        item: item_id,
                    },
                    move |_| {
                        Box::pin(async move {
                            *slot.borrow_mut() = Some(fut.await);
                        })
                    },
                )
                .expect("spawn the stage task");
            self.run();
            out.borrow_mut().take()
        }

        /// Spawn the pipeline of `snap` as an item task of run 1 (no admission step, institutional route allowed).
        pub(crate) fn spawn(&mut self, snap: ItemSnapshot, mode: Mode) {
            let item_id = snap.item;
            let cx = self.item_cx(snap, mode);
            let slot = Rc::clone(&self.outcome);
            self.host
                .engine
                .spawn_test(
                    Owner::Item {
                        run: 1,
                        item: item_id,
                    },
                    move |_| {
                        Box::pin(async move {
                            let outcome = item::run_admitted(&cx, true).await;
                            *slot.borrow_mut() = Some(outcome);
                        })
                    },
                )
                .expect("spawn the item task");
        }

        /// Run until nothing can happen any more (one virtual hour at most).
        pub(crate) fn run(&mut self) -> SimEnd {
            self.host.run_until_idle(3_600_000).expect("simulation")
        }

        /// Spawn, run to the end and return the outcome.
        pub(crate) fn run_item(&mut self, snap: ItemSnapshot, mode: Mode) -> Option<Outcome> {
            self.spawn(snap, mode);
            self.run();
            self.outcome()
        }

        pub(crate) fn outcome(&self) -> Option<Outcome> {
            self.outcome.borrow().clone()
        }

        /// One more engine step, so that effects pushed by `Drop`s after the task ended (a `Release`) are logged.
        pub(crate) fn flush(&mut self) {
            self.host.step(Vec::new()).expect("flush step");
        }

        /// Every logged effect, in order.
        pub(crate) fn effects(&self) -> Vec<Effect> {
            self.host.effects().cloned().collect()
        }

        /// Logged effects with their virtual time.
        pub(crate) fn timeline(&self) -> Vec<(u64, Effect)> {
            self.host
                .log
                .iter()
                .map(|l| (l.at, l.effect.clone()))
                .collect()
        }

        /// The `Http` effects (url, ua, retain) in order.
        pub(crate) fn http(&self) -> Vec<(String, Ua, Retain)> {
            self.effects()
                .into_iter()
                .filter_map(|e| match e {
                    Effect::Http { req, retain, .. } => Some((req.url, req.ua, retain)),
                    _ => None,
                })
                .collect()
        }

        /// How many logged effects match.
        pub(crate) fn count(&self, f: impl Fn(&Effect) -> bool) -> usize {
            self.host.effects().filter(|e| f(e)).count()
        }

        /// The ids of the progress rows emitted so far.
        pub(crate) fn rows(&self) -> Vec<String> {
            self.effects()
                .into_iter()
                .filter_map(|e| match e {
                    Effect::Progress { row, .. } => Some(row.l10n),
                    _ => None,
                })
                .collect()
        }
    }

    /// The responder most tests start from: everything fails fast unless a rule says otherwise.
    pub(crate) fn script() -> ScriptResponder {
        ScriptResponder::new()
    }
}
