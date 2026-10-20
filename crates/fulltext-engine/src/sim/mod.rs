//! Deterministic simulation harness (tests only). `SimHost` plays the part of JS: it owns a virtual clock, feeds
//! `Engine::step`, performs effects by asking a scripted [`Responder`] what each op should answer and when, and
//! checks [`Invariant`]s after every step. No real time, no threads, no randomness except the seeded one.
//!
//! Layout: this file = the driver (work package engine-executor-ops); `fixtures.rs` = builders for configs,
//! items and canned completions (engine-executor-ops); `invariants.rs` + `scenarios/` = the checks and the
//! failure-catalogue scenarios (engine-run-sim).
//!
//! # Driver contract
//! * `step(commands)` builds a `StepIn { now, wall, commands, completions }` from the replies that are due at the
//!   current virtual time (sorted by `(due, op)`), calls `Engine::step`, records every effect with its timestamp,
//!   registers new ops in `pending` (asking the responder for a `Reply` once, in emission order), removes ops that
//!   got `Effect::Cancel` (from `pending` and `scheduled`), applies `Persist` effects to `persisted`, collects
//!   `RunFinished` reports, answers `LoginUpdate` at once with `ok: {}` (what JS does; the engine reports the
//!   completion in `StepOut.ignored`), and then runs the invariants. A failing invariant is
//!   `Err(EngineError { code: "invariant", message })` naming the invariant, the virtual time and the last 30
//!   effects.
//! * `run_until_idle(max_ms)` repeats `step` while anything can still happen: `StepOut.more`, due replies, an
//!   engine timer (`next_timer`), or a reply scheduled in the future; it jumps the clock to the earliest of those.
//!   It stops with `SimEnd::Idle` when nothing is scheduled and the engine has no timer (ops answered
//!   `Reply::Never` stay in `pending` for the scenario to assert on, or to `answer` later), or with
//!   `SimEnd::TimeLimit` when the next event is more than `max_ms` of virtual time away (the clock is then moved
//!   to the limit, so a later call continues from there).
//! * `answer(op, reply)` scripts the answer of a pending op by hand (the user closing a login window, say).
//! * `finish()` runs `Invariant::finish` once the scenario is over.
//! * Everything is deterministic: same seed + same responder = same effect log.
pub(crate) mod fixtures;
pub(crate) mod invariants;
mod scenarios;

use crate::engine::Engine;
use crate::error::EngineError;
use crate::proto::*;
use fulltext_core::model::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// `EngineError::code` of a failed invariant.
pub(crate) const INVARIANT_CODE: &str = "invariant";
/// `EngineError::code` when `run_until_idle` takes more steps than any sane scenario needs.
pub(crate) const LIVELOCK_CODE: &str = "sim_livelock";
/// Upper bound on engine steps inside one `run_until_idle`.
const MAX_STEPS: u32 = 50_000;
/// How many trailing effects an invariant failure prints.
const FAILURE_TAIL: usize = 30;

/// An effect with an op id that JS (the simulator) has to answer.
#[derive(Clone, Debug)]
pub(crate) struct PendingOp {
    pub effect: Effect,
    pub issued_at: u64,
}

/// What the scripted environment answers.
#[derive(Clone, Debug)]
pub(crate) enum Reply {
    /// Complete with `ok` after `after_ms` of virtual time.
    Ok { after_ms: u64, value: Value },
    /// Complete with `err` after `after_ms`.
    Err { after_ms: u64, err: OpErr },
    /// Never answer (hang): used for slow hosts, login windows left open, cancellation tests.
    Never,
}

/// The scripted environment: given a new op, decide its answer. Called once per op, in emission order.
pub(crate) trait Responder {
    fn respond(&mut self, op: &PendingOp, now: u64, wall: u64) -> Reply;
}

/// A property that must hold after every step (and at the end).
pub(crate) trait Invariant {
    fn name(&self) -> &'static str;
    /// Look at the new step output and the host state; `Err(description)` fails the test with the effect log.
    fn check(&mut self, host: &SimHost, out: &StepOut) -> Result<(), String>;
    /// Called once when the scenario ends.
    fn finish(&mut self, host: &SimHost) -> Result<(), String> {
        let _ = host;
        Ok(())
    }
}

/// An effect together with the virtual time of the step that produced it.
#[derive(Clone, Debug)]
pub(crate) struct Logged {
    pub at: u64,
    pub effect: Effect,
}

/// How a `run_until_idle` ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SimEnd {
    /// Nothing pending, nothing scheduled.
    Idle,
    /// `max_ms` of virtual time elapsed first.
    TimeLimit,
}

/// The op an effect asks JS to perform (`None` for fire-and-forget effects, `Cancel`, and `LoginUpdate`, whose
/// `op` is a throw-away id).
pub(crate) fn effect_op(effect: &Effect) -> Option<OpId> {
    match effect {
        Effect::Http { op, .. }
        | Effect::BrowserLoad { op, .. }
        | Effect::LoginWindow { op, .. }
        | Effect::RunTranslators { op, .. }
        | Effect::ZoteroOa { op, .. }
        | Effect::SavePdf { op, .. }
        | Effect::Confirm { op, .. } => Some(*op),
        _ => None,
    }
}

pub(crate) struct SimHost {
    pub(crate) engine: Engine,
    pub(crate) now: u64,
    pub(crate) wall: u64,
    pub(crate) pending: BTreeMap<OpId, PendingOp>,
    /// Replies scheduled for the future: (due time, op id, completion).
    pub(crate) scheduled: Vec<(u64, OpId, CompletionMsg)>,
    pub(crate) log: Vec<Logged>,
    pub(crate) reports: Vec<RunReport>,
    pub(crate) persisted: BTreeMap<PersistKey, Value>,
    /// Every op id the engine reported in `StepOut.ignored`, in order (LoginUpdate completions land here).
    pub(crate) ignored: Vec<OpId>,
    /// Engine steps taken so far.
    pub(crate) steps: u32,
    responder: Box<dyn Responder>,
    invariants: Vec<Box<dyn Invariant>>,
}

impl SimHost {
    /// Production engine, given responder, no invariants (add with `with_invariants`).
    pub(crate) fn new(responder: Box<dyn Responder>) -> Self {
        Self::with_engine(Engine::new(), responder)
    }

    pub(crate) fn with_engine(engine: Engine, responder: Box<dyn Responder>) -> Self {
        Self {
            engine,
            now: 1_000,
            wall: 1_792_496_400_000,
            pending: BTreeMap::new(),
            scheduled: Vec::new(),
            log: Vec::new(),
            reports: Vec::new(),
            persisted: BTreeMap::new(),
            ignored: Vec::new(),
            steps: 0,
            responder,
            invariants: Vec::new(),
        }
    }

    pub(crate) fn with_invariants(mut self, invariants: Vec<Box<dyn Invariant>>) -> Self {
        self.invariants = invariants;
        self
    }

    /// Send `Command::Configure`.
    pub(crate) fn configure(
        &mut self,
        config: EngineConfig,
        persisted: Persisted,
        seed: u64,
    ) -> Result<(), EngineError> {
        let cmd = Command::Configure {
            protocol: PROTOCOL,
            config: Box::new(config),
            persisted,
            seed,
        };
        self.step(vec![cmd]).map(|_| ())
    }

    /// Send `Command::StartRun` (interactive, no resume hints).
    pub(crate) fn start_run(
        &mut self,
        run: RunId,
        mode: Mode,
        items: Vec<ItemSnapshot>,
    ) -> Result<(), EngineError> {
        let cmd = Command::StartRun {
            run,
            mode,
            interactive: true,
            items,
            resume: Vec::new(),
        };
        self.step(vec![cmd]).map(|_| ())
    }

    /// One engine step with `commands` and every reply that is due now, then the bookkeeping and the invariants
    /// described in the module docs.
    pub(crate) fn step(&mut self, commands: Vec<Command>) -> Result<StepOut, EngineError> {
        let completions = self.take_due();
        let input = StepIn {
            now: self.now,
            wall: self.wall,
            commands,
            completions,
        };
        let out = self.engine.step(input)?;
        self.steps = self.steps.saturating_add(1);
        self.absorb(&out);
        self.check(&out)?;
        Ok(out)
    }

    /// Advance the virtual clocks without stepping.
    pub(crate) fn advance(&mut self, ms: u64) {
        self.now = self.now.saturating_add(ms);
        self.wall = self.wall.saturating_add(ms);
    }

    /// Run until nothing can happen any more (see module docs). `max_ms` is virtual time from now.
    pub(crate) fn run_until_idle(&mut self, max_ms: u64) -> Result<SimEnd, EngineError> {
        let limit = self.now.saturating_add(max_ms);
        for _ in 0..MAX_STEPS {
            let out = self.step(Vec::new())?;
            if out.more || self.scheduled.iter().any(|(at, _, _)| *at <= self.now) {
                continue; // more work at this very instant
            }
            let next = self
                .scheduled
                .iter()
                .map(|(at, _, _)| *at)
                .chain(out.next_timer)
                .min();
            match next {
                None => return Ok(SimEnd::Idle),
                // An engine timer that is already due fires at the start of the next step.
                Some(at) if at <= self.now => {}
                Some(at) if at > limit => {
                    self.advance(limit.saturating_sub(self.now));
                    return Ok(SimEnd::TimeLimit);
                }
                Some(at) => self.advance(at - self.now),
            }
        }
        Err(EngineError::new(
            LIVELOCK_CODE,
            format!(
                "run_until_idle took {MAX_STEPS} steps without going idle (virtual time {})",
                self.now
            ),
        ))
    }

    /// Script the answer of an op that is still pending (typically one the responder left at `Reply::Never`).
    /// Returns `false` when the op is not pending (never issued, already answered, or cancelled).
    pub(crate) fn answer(&mut self, op: OpId, reply: Reply) -> bool {
        if !self.pending.contains_key(&op) {
            return false;
        }
        self.schedule(op, reply);
        true
    }

    /// Ids of the pending ops whose effect matches, in op order.
    pub(crate) fn pending_where(&self, matcher: impl Fn(&Effect) -> bool) -> Vec<OpId> {
        self.pending
            .iter()
            .filter(|(_, p)| matcher(&p.effect))
            .map(|(op, _)| *op)
            .collect()
    }

    /// Run `Invariant::finish` on every invariant (call once, when the scenario is over).
    pub(crate) fn finish(&mut self) -> Result<(), EngineError> {
        let mut invariants = std::mem::take(&mut self.invariants);
        let failure = invariants
            .iter_mut()
            .find_map(|inv| inv.finish(self).err().map(|detail| (inv.name(), detail)));
        self.invariants = invariants;
        match failure {
            Some((name, detail)) => Err(self.invariant_error(name, &detail)),
            None => Ok(()),
        }
    }

    /// Effects logged so far.
    pub(crate) fn effects(&self) -> impl Iterator<Item = &Effect> {
        self.log.iter().map(|l| &l.effect)
    }

    /// Replies due now, in `(due, op)` order, as completions; they leave `pending` as they are delivered.
    fn take_due(&mut self) -> Vec<CompletionMsg> {
        let now = self.now;
        let (mut due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.scheduled)
            .into_iter()
            .partition(|(at, _, _)| *at <= now);
        self.scheduled = later;
        due.sort_by_key(|(at, op, _)| (*at, *op));
        due.into_iter()
            .map(|(_, op, msg)| {
                self.pending.remove(&op);
                msg
            })
            .collect()
    }

    /// Book-keeping for the effects of one step, in emission order.
    fn absorb(&mut self, out: &StepOut) {
        self.ignored.extend(out.ignored.iter().copied());
        for effect in &out.effects {
            self.log.push(Logged {
                at: self.now,
                effect: effect.clone(),
            });
            match effect {
                Effect::Cancel { op } => {
                    self.pending.remove(op);
                    self.scheduled
                        .retain(|(_, scheduled_op, _)| scheduled_op != op);
                }
                Effect::LoginUpdate { op, .. } => {
                    let msg = CompletionMsg {
                        op: *op,
                        ok: Some(json!({})),
                        err: None,
                    };
                    self.scheduled.push((self.now, *op, msg));
                }
                Effect::Persist { key, value, .. } => {
                    self.persisted.insert(*key, value.clone());
                }
                Effect::RunFinished { report, .. } => self.reports.push(report.clone()),
                other => {
                    if let Some(op) = effect_op(other) {
                        let pending = PendingOp {
                            effect: other.clone(),
                            issued_at: self.now,
                        };
                        let reply = self.responder.respond(&pending, self.now, self.wall);
                        self.pending.insert(op, pending);
                        self.schedule(op, reply);
                    }
                }
            }
        }
    }

    /// Queue the completion for `reply` (nothing for `Never`).
    fn schedule(&mut self, op: OpId, reply: Reply) {
        match reply {
            Reply::Ok { after_ms, value } => {
                let msg = CompletionMsg {
                    op,
                    ok: Some(value),
                    err: None,
                };
                self.scheduled
                    .push((self.now.saturating_add(after_ms), op, msg));
            }
            Reply::Err { after_ms, err } => {
                let msg = CompletionMsg {
                    op,
                    ok: None,
                    err: Some(err),
                };
                self.scheduled
                    .push((self.now.saturating_add(after_ms), op, msg));
            }
            Reply::Never => {}
        }
    }

    /// Run every invariant against the step that just finished.
    fn check(&mut self, out: &StepOut) -> Result<(), EngineError> {
        // The invariants read the host, so they are taken out of it while they run.
        let mut invariants = std::mem::take(&mut self.invariants);
        let failure = invariants.iter_mut().find_map(|inv| {
            inv.check(self, out)
                .err()
                .map(|detail| (inv.name(), detail))
        });
        self.invariants = invariants;
        match failure {
            Some((name, detail)) => Err(self.invariant_error(name, &detail)),
            None => Ok(()),
        }
    }

    fn invariant_error(&self, name: &str, detail: &str) -> EngineError {
        let skip = self.log.len().saturating_sub(FAILURE_TAIL);
        let tail: String = self
            .log
            .iter()
            .skip(skip)
            .map(|l| {
                format!(
                    "\n  [{}] {}",
                    l.at,
                    serde_json::to_string(&l.effect).unwrap_or_else(|_| format!("{:?}", l.effect))
                )
            })
            .collect();
        EngineError::new(
            INVARIANT_CODE,
            format!(
                "{name}: {detail} (virtual time {} ms); last effects:{tail}",
                self.now
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{ScriptResponder, is_type, op_err};
    use super::*;
    use crate::cx::Owner;
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::rc::Rc;

    struct Silent;
    impl Responder for Silent {
        fn respond(&mut self, _op: &PendingOp, _now: u64, _wall: u64) -> Reply {
            Reply::Never
        }
    }

    fn boxed(f: impl Future<Output = ()> + 'static) -> crate::cx::LocalFuture {
        Box::pin(f)
    }

    fn host_with(responder: impl Responder + 'static) -> SimHost {
        SimHost::new(Box::new(responder))
    }

    #[test]
    fn empty_step_is_ok() {
        let mut host = SimHost::new(Box::new(Silent));
        let out = host.step(Vec::new()).expect("empty step");
        assert!(out.effects.is_empty());
        assert!(!out.more);
        assert_eq!(host.run_until_idle(1_000), Ok(SimEnd::Idle));
    }

    #[test]
    fn the_clock_jumps_between_events_instead_of_ticking() {
        let mut host = host_with(Silent);
        let done = Rc::new(Cell::new(false));
        let d = Rc::clone(&done);
        host.engine
            .spawn_test(Owner::System, |cx| {
                boxed(async move {
                    cx.sleep(500).await;
                    cx.sleep(700).await;
                    d.set(true);
                })
            })
            .expect("spawn");
        let start = host.now;
        assert_eq!(host.run_until_idle(60_000), Ok(SimEnd::Idle));
        assert!(done.get());
        assert_eq!(host.now, start + 1_200);
        assert_eq!(
            host.wall,
            1_792_496_400_000 + 1_200,
            "wall moves with the monotonic clock"
        );
        assert!(host.steps <= 4, "{} steps for two sleeps", host.steps);
    }

    #[test]
    fn time_limit_stops_before_a_far_event_and_a_later_call_continues() {
        let mut host = host_with(Silent);
        let done = Rc::new(Cell::new(false));
        let d = Rc::clone(&done);
        host.engine
            .spawn_test(Owner::System, |cx| {
                boxed(async move {
                    cx.sleep(10_000).await;
                    d.set(true);
                })
            })
            .expect("spawn");
        let start = host.now;
        assert_eq!(host.run_until_idle(1_000), Ok(SimEnd::TimeLimit));
        assert_eq!((host.now, done.get()), (start + 1_000, false));
        assert_eq!(host.run_until_idle(20_000), Ok(SimEnd::Idle));
        assert_eq!((host.now, done.get()), (start + 10_000, true));
    }

    #[test]
    fn replies_arrive_at_their_due_time_in_due_then_op_order() {
        let script = ScriptResponder::new().on_seq(
            is_type("zotero_oa"),
            vec![
                Reply::ok(30, json!([])),
                Reply::ok(10, json!([])),
                Reply::ok(10, json!([])),
            ],
        );
        let mut host = host_with(script);
        let order = Rc::new(RefCell::new(Vec::new()));
        for n in 1..=3u32 {
            let o = Rc::clone(&order);
            host.engine
                .spawn_test(Owner::System, |cx| {
                    boxed(async move {
                        let _ = cx.zotero_oa(&format!("10.1/{n}")).await;
                        o.borrow_mut().push((n, cx.now()));
                    })
                })
                .expect("spawn");
        }
        let start = host.now;
        assert_eq!(host.run_until_idle(1_000), Ok(SimEnd::Idle));
        // Ops 2 and 3 answer after 10 ms (op order within a tie), op 1 after 30 ms.
        assert_eq!(
            *order.borrow(),
            vec![(2, start + 10), (3, start + 10), (1, start + 30)]
        );
        assert!(host.pending.is_empty() && host.scheduled.is_empty());
        assert!(host.ignored.is_empty());
    }

    #[test]
    fn an_error_reply_reaches_the_task() {
        let script =
            ScriptResponder::new().on(is_type("zotero_oa"), Reply::err(5, OpErrKind::Offline));
        let mut host = host_with(script);
        let got = Rc::new(Cell::new(None));
        let g = Rc::clone(&got);
        host.engine
            .spawn_test(Owner::System, |cx| {
                boxed(async move {
                    g.set(cx.zotero_oa("x").await.err().map(|e| e.kind));
                })
            })
            .expect("spawn");
        host.run_until_idle(100).expect("run");
        assert_eq!(got.get(), Some(OpErrKind::Offline));
    }

    #[test]
    fn cancel_removes_the_op_from_pending_and_scheduled() {
        // The reply is due after 200 ms but the task gives up after 50 ms.
        let script = ScriptResponder::new().on(is_type("zotero_oa"), Reply::ok(200, json!([])));
        let mut host = host_with(script);
        let outcome = Rc::new(Cell::new(None));
        let o = Rc::clone(&outcome);
        host.engine
            .spawn_test(Owner::System, |cx| {
                boxed(async move {
                    o.set(Some(cx.timeout(50, cx.zotero_oa("x")).await.is_err()));
                })
            })
            .expect("spawn");
        let start = host.now;
        let out = host.step(Vec::new()).expect("step");
        assert!(matches!(
            out.effects.as_slice(),
            [Effect::ZoteroOa { op: 1, .. }]
        ));
        assert_eq!((host.pending.len(), host.scheduled.len()), (1, 1));

        host.advance(50);
        let out = host.step(Vec::new()).expect("step at the timeout");
        assert_eq!(out.effects, vec![Effect::Cancel { op: 1 }]);
        assert_eq!(outcome.get(), Some(true));
        assert!(host.pending.is_empty() && host.scheduled.is_empty());

        assert_eq!(host.run_until_idle(10_000), Ok(SimEnd::Idle));
        assert_eq!(
            host.now,
            start + 50,
            "the cancelled reply no longer holds the clock back"
        );
        assert!(host.ignored.is_empty(), "it was never delivered");
    }

    #[test]
    fn login_update_is_answered_at_once_and_lands_in_ignored() {
        let mut host = host_with(Silent);
        host.engine
            .spawn_test(Owner::System, |cx| {
                boxed(async move {
                    let op = cx.shared.with(|s| s.reactor.alloc_id()).unwrap_or_default();
                    cx.emit(Effect::LoginUpdate { op, waiting: 3 });
                })
            })
            .expect("spawn");
        assert_eq!(host.run_until_idle(1_000), Ok(SimEnd::Idle));
        assert_eq!(host.ignored, vec![1]);
        assert!(host.pending.is_empty(), "LoginUpdate is never a pending op");
        assert!(
            host.effects()
                .any(|e| matches!(e, Effect::LoginUpdate { op: 1, waiting: 3 }))
        );
    }

    #[test]
    fn never_replies_stay_pending_until_answered_by_hand() {
        let mut host = host_with(Silent);
        let got = Rc::new(RefCell::new(None));
        let g = Rc::clone(&got);
        host.engine
            .spawn_test(Owner::System, |cx| {
                boxed(async move {
                    *g.borrow_mut() = cx.zotero_oa("x").await.ok().map(|hits| hits.len());
                })
            })
            .expect("spawn");
        assert_eq!(host.run_until_idle(1_000), Ok(SimEnd::Idle));
        assert_eq!(host.pending.keys().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(host.pending_where(is_type("zotero_oa")), vec![1]);
        assert!(host.pending_where(is_type("http")).is_empty());
        assert_eq!(host.pending.get(&1).map(|p| p.issued_at), Some(1_000));

        assert!(!host.answer(77, Reply::Never), "unknown op");
        assert!(host.answer(1, Reply::ok(25, json!([{"url": "https://x/y.pdf"}]))));
        assert_eq!(host.run_until_idle(1_000), Ok(SimEnd::Idle));
        assert_eq!(*got.borrow(), Some(1));
        assert!(host.pending.is_empty());
        assert!(!host.answer(1, Reply::Never), "already answered");
    }

    #[test]
    fn persist_and_run_finished_are_collected() {
        let mut host = host_with(Silent);
        host.engine
            .spawn_test(Owner::System, |cx| {
                boxed(async move {
                    cx.emit(Effect::Persist {
                        key: PersistKey::Ledger,
                        value: json!({"v": 1}),
                        flush: false,
                    });
                    cx.emit(Effect::Persist {
                        key: PersistKey::Ledger,
                        value: json!({"v": 2}),
                        flush: true,
                    });
                    cx.emit(Effect::Persist {
                        key: PersistKey::Halts,
                        value: json!({}),
                        flush: true,
                    });
                    let report = RunReport {
                        run: 4,
                        counts: ReportCounts::default(),
                        items: Vec::new(),
                        ledger: LedgerUsage::default(),
                        halted: None,
                        engine_restarts: 0,
                    };
                    cx.emit(Effect::RunFinished { run: 4, report });
                })
            })
            .expect("spawn");
        host.run_until_idle(10).expect("run");
        assert_eq!(
            host.persisted.get(&PersistKey::Ledger),
            Some(&json!({"v": 2})),
            "the latest value wins"
        );
        assert_eq!(host.persisted.len(), 2);
        assert_eq!(host.reports.len(), 1);
        assert_eq!(host.reports[0].run, 4);
    }

    #[test]
    fn engine_errors_propagate_from_step() {
        let mut host = host_with(Silent);
        let err = host
            .start_run(1, Mode::Full, Vec::new())
            .expect_err("no Configure yet");
        assert_eq!(err.code, crate::error::code::NOT_CONFIGURED);
    }

    #[test]
    fn configure_goes_through_the_engine() {
        let mut host = host_with(Silent);
        host.configure(fixtures::test_config(), Persisted::default(), 42)
            .expect("configure");
        assert_eq!(host.steps, 1);
        // A run without items is accepted or not by the run manager, but never for lack of a config.
        if let Err(e) = host.start_run(1, Mode::Full, Vec::new()) {
            assert_ne!(e.code, crate::error::code::NOT_CONFIGURED);
        }
    }

    /// Fails as soon as it sees a `Log` effect; remembers how often it ran.
    struct NoLogs {
        checks: Rc<Cell<u32>>,
        finished: Rc<Cell<bool>>,
    }
    impl Invariant for NoLogs {
        fn name(&self) -> &'static str {
            "NoLogs"
        }
        fn check(&mut self, _host: &SimHost, out: &StepOut) -> Result<(), String> {
            self.checks.set(self.checks.get() + 1);
            match out.effects.iter().find(|e| matches!(e, Effect::Log { .. })) {
                Some(e) => Err(format!("saw {e:?}")),
                None => Ok(()),
            }
        }
        fn finish(&mut self, host: &SimHost) -> Result<(), String> {
            self.finished.set(true);
            if host.pending.is_empty() {
                Ok(())
            } else {
                Err("ops still pending".into())
            }
        }
    }

    #[test]
    fn invariants_run_after_every_step_and_failures_carry_the_effect_tail() {
        let (checks, finished) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(false)));
        let inv = NoLogs {
            checks: Rc::clone(&checks),
            finished: Rc::clone(&finished),
        };
        let mut host = host_with(Silent).with_invariants(vec![Box::new(inv)]);
        host.engine
            .spawn_test(Owner::System, |cx| {
                boxed(async move {
                    cx.emit(Effect::Persist {
                        key: PersistKey::Alma,
                        value: json!(null),
                        flush: false,
                    });
                    cx.sleep(10).await;
                    cx.log(LogLevel::Warn, "boom");
                })
            })
            .expect("spawn");
        let err = host
            .run_until_idle(1_000)
            .expect_err("the invariant fails at the log");
        assert_eq!(err.code, INVARIANT_CODE);
        assert!(err.message.starts_with("NoLogs: saw "), "{}", err.message);
        assert!(
            err.message.contains("virtual time 1010 ms"),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("\"type\":\"persist\""),
            "the tail lists earlier effects: {}",
            err.message
        );
        assert!(err.message.contains("\"msg\":\"boom\""), "{}", err.message);
        assert_eq!(
            checks.get(),
            2,
            "once per step: the persist step passed, the log step failed"
        );
        assert!(!finished.get());
        host.finish().expect("no pending ops");
        assert!(finished.get());
    }

    #[test]
    fn finish_reports_invariant_failures() {
        let inv = NoLogs {
            checks: Rc::default(),
            finished: Rc::default(),
        };
        let mut host = host_with(Silent).with_invariants(vec![Box::new(inv)]);
        host.engine
            .spawn_test(Owner::System, |cx| {
                boxed(async move {
                    let _ = cx.zotero_oa("x").await;
                })
            })
            .expect("spawn");
        host.run_until_idle(10).expect("run");
        let err = host.finish().expect_err("an op is still pending");
        assert_eq!(err.code, INVARIANT_CODE);
        assert!(
            err.message.contains("NoLogs: ops still pending"),
            "{}",
            err.message
        );
    }

    /// One scripted scenario: parallel tasks with ops of different latencies and a cancel.
    fn scripted_log() -> Vec<String> {
        let script = ScriptResponder::new()
            .on_seq(
                is_type("zotero_oa"),
                vec![
                    Reply::ok(40, json!([])),
                    Reply::ok(15, json!([])),
                    Reply::err(5, OpErrKind::Timeout),
                ],
            )
            .fallback(Reply::Err {
                after_ms: 1,
                err: op_err(OpErrKind::Network),
            });
        let mut host = host_with(script);
        for n in 0..3u32 {
            host.engine
                .spawn_test(Owner::System, |cx| {
                    boxed(async move {
                        let r = cx.timeout(30, cx.zotero_oa(&format!("10.1/{n}"))).await;
                        cx.log(
                            LogLevel::Info,
                            format!("task {n}: {:?}", r.map(|r| r.is_ok())),
                        );
                    })
                })
                .expect("spawn");
        }
        host.run_until_idle(10_000).expect("run");
        host.log
            .iter()
            .map(|l| {
                format!(
                    "{} {}",
                    l.at,
                    serde_json::to_string(&l.effect).unwrap_or_default()
                )
            })
            .collect()
    }

    #[test]
    fn the_effect_log_is_deterministic() {
        let a = scripted_log();
        let b = scripted_log();
        assert_eq!(a, b);
        // Op 1 (40 ms) timed out at 30 ms and was cancelled; ops 2 and 3 answered in time.
        assert!(
            a.iter()
                .any(|l| l.contains("\"type\":\"cancel\"") && l.contains("\"op\":1")),
            "{a:#?}"
        );
        assert!(
            a.iter().any(|l| l.contains("task 0: Err(TimedOut)")),
            "{a:#?}"
        );
        assert!(a.iter().any(|l| l.contains("task 1: Ok(true)")), "{a:#?}");
        assert!(a.iter().any(|l| l.contains("task 2: Ok(false)")), "{a:#?}");
    }

    #[test]
    fn effect_op_covers_every_op_carrying_effect() {
        let samples = [
            Effect::Http {
                op: 1,
                run: 1,
                item: 1,
                route: Route::Oa,
                barrier: false,
                req: fixtures::http_req("https://example.org"),
                retain: Retain::None,
                inspect: InspectSpec::None,
            },
            Effect::BrowserLoad {
                op: 2,
                run: 1,
                item: 1,
                route: Route::Inst,
                barrier: true,
                req: fixtures::browser_req("https://example.org"),
                inspect: InspectSpec::None,
            },
            Effect::LoginWindow {
                op: 3,
                run: 1,
                item: 1,
                req: fixtures::login_req("https://go.openathens.net/redirector/kcl.ac.uk"),
            },
            Effect::RunTranslators {
                op: 4,
                run: 1,
                item: 1,
                page: fixtures::body(1, 1),
                url: "u".into(),
            },
            Effect::ZoteroOa {
                op: 5,
                run: 1,
                item: 1,
                doi: "d".into(),
            },
            Effect::Confirm {
                op: 7,
                run: 1,
                req: ConfirmReq {
                    kind: ConfirmKind::LargeBatch,
                    n: 1,
                    cap: 1,
                    library: String::new(),
                },
            },
        ];
        let ops: Vec<Option<OpId>> = samples.iter().map(effect_op).collect();
        assert_eq!(
            ops,
            vec![Some(1), Some(2), Some(3), Some(4), Some(5), Some(7)]
        );
        assert_eq!(effect_op(&Effect::Cancel { op: 9 }), None);
        assert_eq!(effect_op(&Effect::LoginUpdate { op: 9, waiting: 1 }), None);
        assert_eq!(effect_op(&Effect::Release { handle: 9 }), None);
    }
}
