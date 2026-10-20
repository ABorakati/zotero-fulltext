//! `Engine`: ties the executor, reactor, scheduler, login gate and run manager into `step`.
//!
//! # `Engine::step(StepIn) -> StepOut` (the only entry point; never reentrant, never blocks, never does I/O)
//! 1. **Clock**: `state.clock.advance(now, wall)` (monotonic clamp).
//! 2. **Commands**, in order:
//!    * `Configure` - protocol must equal `proto::PROTOCOL` (else `EngineError(protocol_mismatch)`); stores
//!      `Rc<EngineConfig>` in `state.cfg`, `sched.configure(cfg, persisted, seed, wall)`. May be re-sent: the new
//!      config replaces the old one, `persisted` is applied by the first one only (inside `sched.configure`).
//!    * `StartRun` -> `run::start_run`; `CancelRun` / `CancelItem` -> `run::cancel_*`; `UserAction` ->
//!      `run::user_action`; `DumpState` -> `run::dump_state`; `Shutdown` -> `run::shutdown`.
//!      `StartRun` before any `Configure` -> `EngineError(not_configured)`. The other commands need no config and
//!      ignore unknown runs / items (the run manager logs and carries on), so they are accepted at any time.
//! 3. **Completions**: for each message `reactor.complete(msg)` inside one `with`; the returned wakers are woken
//!    AFTER the closure ends. Unknown ops are reported in `StepOut.ignored`.
//! 4. **Timers**: `reactor.due_timers(now)` -> wake.
//! 5. **Poll rounds**: `exec.run(remaining)` (total budget `policy::POLL_BUDGET` per step); after each round drain
//!    `state.spawns` into the executor (`ExecFull` -> the future is dropped and a `Log` effect says so) and call
//!    `run::collect_done`; repeat while tasks are ready and budget remains. `more = true` when the budget ran out
//!    with tasks still ready.
//! 6. **Finish**: `run::finish_runs` (one `RunFinished` per completed run).
//! 7. **Stall check** (only when `!more`): tasks alive, none ready, no op in flight, no timer, nobody waiting for
//!    the user (limiter retry instants are timers, so a gate waiter that time would serve is not a stall) -> one
//!    `exec.wake_all()` sweep and one more poll round with its own budget. If the sweep made progress (the outbox
//!    grew, the task count changed, ops or timers appeared) a lost wake is counted in `Stats.lost_wakes`
//!    (cumulative over the engine's life); if it did not, `Stats.stalled = true` (JS fails the run and sends
//!    `DumpState`).
//! 8. **Reentry check**: `shared.reentries() > 0` -> `EngineError(reentered)`. Sticky: the state may be
//!    inconsistent, so the host resets the engine.
//! 9. **Output**: `effects = take(outbox)`, `next_timer = reactor.next_deadline()` (also the earliest limiter
//!    retry instant armed as a timer), `ignored = reactor.take_ignored()`, `stats`.
//!
//! Effect ordering is part of the contract: `outbox` order is emission order, and a `Persist { flush: true }`
//! pushed before an `Http { barrier: true }` in the same step must stay before it. Effects pushed between steps
//! (a `Cancel` from dropping a future outside `step`) are delivered by the next `step`.
//!
//! Everything here is deterministic: no clock reads, no randomness except the seeded RNG inside `sched`.
use crate::cx::{Ctl, ItemRunner, Shared, SpawnReq, State};
use crate::error::{EngineError, code};
use crate::executor::Executor;
use crate::pipeline;
use crate::proto::*;
use crate::run::{self, StartRunCmd};
use crate::sched::policy;
use fulltext_core::model::CancelledBy;
use std::rc::Rc;
use std::task::Waker;

/// Upper bound on spawn/poll/collect rounds inside one phase. The poll budget already bounds the work; this is
/// a belt-and-braces guard against a spawn loop that never polls.
const MAX_ROUNDS: u32 = 4 * policy::POLL_BUDGET;

/// The engine. One per wasm instance; owned by the `fx_step` export's global.
pub struct Engine {
    shared: Rc<Shared>,
    exec: Executor,
    runner: ItemRunner,
    /// A `Configure` was applied.
    configured: bool,
    /// `Stats.lost_wakes`: sweeps that revived a task, cumulative.
    lost_wakes: u32,
}

/// What the stall check compares before and after the sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mark {
    outbox: usize,
    tasks: usize,
    ops: usize,
    timers: usize,
}

impl Mark {
    /// Did anything happen between `before` and `self`?
    fn progressed_since(&self, before: &Mark) -> bool {
        self.outbox > before.outbox
            || self.tasks != before.tasks
            || self.ops > before.ops
            || self.timers > before.timers
    }
}

impl Engine {
    /// A fresh, unconfigured engine running the production pipeline.
    pub fn new() -> Self {
        Self::with_runner(pipeline::item_runner)
    }

    /// Engine with a custom item task body (tests of the run manager use this).
    pub(crate) fn with_runner(runner: ItemRunner) -> Self {
        Self {
            shared: Shared::new(),
            exec: Executor::new(policy::MAX_TASKS),
            runner,
            configured: false,
            lost_wakes: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn shared(&self) -> &Rc<Shared> {
        &self.shared
    }

    #[cfg(test)]
    pub(crate) fn exec_mut(&mut self) -> &mut Executor {
        &mut self.exec
    }

    /// Advance the engine by one JS event-loop turn. See the module docs.
    pub fn step(&mut self, input: StepIn) -> Result<StepOut, EngineError> {
        let StepIn {
            now,
            wall,
            commands,
            completions,
        } = input;

        // 1. clock
        self.state(|s| s.clock.advance(now, wall))?;

        // 2. commands
        for command in commands {
            self.apply(command)?;
        }

        // 3. completions: complete everything under one borrow, wake after it ends.
        let wakers = self.state(|s| {
            completions
                .into_iter()
                .filter_map(|msg| s.reactor.complete(msg))
                .collect::<Vec<Waker>>()
        })?;
        wakers.into_iter().for_each(Waker::wake);

        // 4. timers
        let wakers = self.state(|s| {
            let now = s.clock.now;
            s.reactor.due_timers(now)
        })?;
        wakers.into_iter().for_each(Waker::wake);

        // 5. poll rounds
        let mut polls = 0u32;
        let mut more = self.poll_rounds(&mut polls)?;

        // 6. finish
        self.with_ctl(|ctl, _| run::finish_runs(ctl));

        // 7. stall check
        let mut stalled = false;
        if !more && self.stall_candidate()? {
            let before = self.mark()?;
            self.exec.wake_all();
            more = self.poll_rounds(&mut polls)?;
            self.with_ctl(|ctl, _| run::finish_runs(ctl));
            if self.mark()?.progressed_since(&before) {
                self.lost_wakes = self.lost_wakes.saturating_add(1);
            } else {
                stalled = true;
            }
        }

        // 8. reentry check
        if self.shared.reentries() > 0 {
            return Err(EngineError::new(
                code::REENTERED,
                "engine state was borrowed re-entrantly",
            ));
        }

        // 9. output
        let tasks = u32::try_from(self.exec.live()).unwrap_or(u32::MAX);
        let lost_wakes = self.lost_wakes;
        self.state(|s| {
            let stats = Stats {
                tasks,
                ops_inflight: u32::try_from(s.reactor.inflight()).unwrap_or(u32::MAX),
                timers: u32::try_from(s.reactor.timer_count()).unwrap_or(u32::MAX),
                waiting_user: s.login.waiting_user(),
                polls,
                lost_wakes,
                stalled,
                mem_hint_kb: 0,
            };
            StepOut {
                effects: std::mem::take(&mut s.outbox),
                next_timer: s.reactor.next_deadline(),
                more,
                ignored: s.reactor.take_ignored(),
                stats,
            }
        })
    }

    /// Run `f` on the state; a failed borrow is the `reentered` engine error.
    fn state<R>(&self, f: impl FnOnce(&mut State) -> R) -> Result<R, EngineError> {
        self.shared.with(f).ok_or_else(|| {
            EngineError::new(code::REENTERED, "engine state was borrowed re-entrantly")
        })
    }

    /// Hand `f` a `Ctl` (shared state + executor) and the item runner.
    fn with_ctl<R>(&mut self, f: impl FnOnce(&mut Ctl<'_>, ItemRunner) -> R) -> R {
        let runner = self.runner;
        let mut ctl = Ctl {
            shared: &self.shared,
            exec: &mut self.exec,
        };
        f(&mut ctl, runner)
    }

    /// Phase 2, one command.
    fn apply(&mut self, command: Command) -> Result<(), EngineError> {
        match command {
            Command::Configure {
                protocol,
                config,
                persisted,
                seed,
            } => {
                if protocol != PROTOCOL {
                    return Err(EngineError::new(
                        code::PROTOCOL_MISMATCH,
                        format!("engine speaks protocol {PROTOCOL}, host sent {protocol}"),
                    ));
                }
                let cfg = Rc::new(*config);
                self.state(|s| {
                    let wall = s.clock.wall;
                    s.sched.configure(&cfg, persisted, seed, wall);
                    s.cfg = Some(cfg);
                })?;
                self.configured = true;
                Ok(())
            }
            Command::StartRun {
                run,
                mode,
                interactive,
                items,
                resume,
            } => {
                if !self.configured {
                    return Err(EngineError::new(
                        code::NOT_CONFIGURED,
                        "StartRun before Configure",
                    ));
                }
                let cmd = StartRunCmd {
                    run,
                    mode,
                    interactive,
                    items,
                    resume,
                };
                self.with_ctl(|ctl, runner| run::start_run(ctl, runner, cmd))
            }
            Command::CancelRun { run } => {
                self.with_ctl(|ctl, _| run::cancel_run(ctl, run, CancelledBy::User));
                Ok(())
            }
            Command::CancelItem { run, item } => {
                self.with_ctl(|ctl, _| run::cancel_item(ctl, run, item));
                Ok(())
            }
            Command::UserAction { run, kind } => {
                self.with_ctl(|ctl, _| run::user_action(ctl, run, kind));
                Ok(())
            }
            Command::DumpState => {
                self.with_ctl(|ctl, _| run::dump_state(ctl));
                Ok(())
            }
            Command::Shutdown => {
                self.with_ctl(|ctl, _| run::shutdown(ctl));
                Ok(())
            }
        }
    }

    /// Phase 5: poll, spawn, collect, repeat while work is ready and the budget lasts. `polls` accumulates the
    /// polls of this step. Returns `more`.
    fn poll_rounds(&mut self, polls: &mut u32) -> Result<bool, EngineError> {
        let mut remaining = policy::POLL_BUDGET;
        for _ in 0..MAX_ROUNDS {
            let stats = self.exec.run(remaining);
            *polls = polls.saturating_add(stats.polls);
            remaining = remaining.saturating_sub(stats.polls);
            self.drain_spawns()?;
            self.with_ctl(run::collect_done);
            if remaining == 0 || !self.work_ready() {
                break;
            }
        }
        Ok(self.work_ready())
    }

    fn work_ready(&self) -> bool {
        self.exec.ready_len() > 0 || self.exec.sweep_pending()
    }

    /// Move tasks that logic inside `State` asked for into the executor.
    fn drain_spawns(&mut self) -> Result<(), EngineError> {
        let spawns = self.state(|s| std::mem::take(&mut s.spawns))?;
        for SpawnReq { owner, fut } in spawns {
            // On `ExecFull` the future is dropped inside `spawn`, outside any borrow of the state.
            if self.exec.spawn(owner, fut).is_err() {
                let msg = format!(
                    "executor full ({} tasks): dropped a task of {owner:?}",
                    self.exec.max_tasks()
                );
                self.state(|s| {
                    s.outbox.push(Effect::Log {
                        level: LogLevel::Error,
                        msg,
                    })
                })?;
            }
        }
        Ok(())
    }

    /// All the conditions of a possible stall except the sweep itself.
    fn stall_candidate(&self) -> Result<bool, EngineError> {
        let m = self.mark()?;
        let waiting_user = self.state(|s| s.login.waiting_user())?;
        Ok(m.tasks > 0
            && m.ops == 0
            && m.timers == 0
            && waiting_user == 0
            && self.exec.ready_len() == 0)
    }

    fn mark(&self) -> Result<Mark, EngineError> {
        let tasks = self.exec.live();
        self.state(|s| Mark {
            outbox: s.outbox.len(),
            tasks,
            ops: s.reactor.inflight(),
            timers: s.reactor.timer_count(),
        })
    }
}

#[cfg(test)]
impl Engine {
    /// Test hook: spawn an arbitrary task for `owner` without the run manager. `make` receives a `Cx` for that
    /// owner. The next `step` polls it. Used by executor / ops / sched / pipeline unit tests.
    pub(crate) fn spawn_test(
        &mut self,
        owner: crate::cx::Owner,
        make: impl FnOnce(crate::cx::Cx) -> crate::cx::LocalFuture,
    ) -> Result<crate::executor::TaskKey, crate::executor::ExecFull> {
        let cx = crate::cx::Cx::new(Rc::clone(&self.shared), owner);
        self.exec.spawn(owner, make(cx))
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::{Cx, LocalFuture, Owner};
    use crate::sim::fixtures::test_config;
    use fulltext_core::model::*;
    use serde_json::json;
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    fn at(now: u64) -> StepIn {
        StepIn {
            now,
            wall: 1_800_000_000_000 + now,
            commands: Vec::new(),
            completions: Vec::new(),
        }
    }

    fn configure(protocol: u32) -> Command {
        Command::Configure {
            protocol,
            config: Box::new(test_config()),
            persisted: Persisted::default(),
            seed: 7,
        }
    }

    fn done(op: OpId, value: serde_json::Value) -> CompletionMsg {
        CompletionMsg {
            op,
            ok: Some(value),
            err: None,
        }
    }

    /// Yields to the executor `n` times, waking itself each time.
    struct YieldNow(u32);
    impl Future for YieldNow {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 == 0 {
                return Poll::Ready(());
            }
            self.0 -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    fn boxed(f: impl Future<Output = ()> + 'static) -> LocalFuture {
        Box::pin(f)
    }

    fn task(engine: &mut Engine, make: impl FnOnce(Cx) -> LocalFuture) {
        engine.spawn_test(Owner::System, make).expect("spawn");
    }

    #[test]
    fn protocol_mismatch_is_an_engine_error() {
        let mut e = Engine::new();
        let mut input = at(10);
        input.commands.push(configure(PROTOCOL + 1));
        let err = e.step(input).expect_err("mismatch");
        assert_eq!(err.code, code::PROTOCOL_MISMATCH);
        assert!(err.to_string().starts_with("protocol_mismatch: "));
        // Nothing was configured: a run still cannot start.
        let mut input = at(11);
        input.commands.push(Command::StartRun {
            run: 1,
            mode: Mode::Full,
            interactive: true,
            items: Vec::new(),
            resume: Vec::new(),
        });
        assert_eq!(
            e.step(input).expect_err("not configured").code,
            code::NOT_CONFIGURED
        );
    }

    #[test]
    fn start_run_before_configure_is_not_configured() {
        let mut e = Engine::new();
        let mut input = at(10);
        input.commands.push(Command::StartRun {
            run: 1,
            mode: Mode::Full,
            interactive: true,
            items: Vec::new(),
            resume: Vec::new(),
        });
        let err = e.step(input).expect_err("not configured");
        assert_eq!(err.code, code::NOT_CONFIGURED);
        assert_eq!(err.to_err_body().code, "not_configured");
    }

    #[test]
    fn commands_for_unknown_runs_are_not_errors() {
        let mut e = Engine::new();
        let mut input = at(10);
        input.commands = vec![
            Command::CancelRun { run: 99 },
            Command::CancelItem { run: 99, item: 1 },
            Command::UserAction {
                run: 99,
                kind: UserActionKind::KeepWaiting,
            },
            Command::DumpState,
        ];
        assert!(e.step(input).is_ok());
    }

    #[test]
    fn configure_stores_the_config_and_may_be_resent() {
        let mut e = Engine::new();
        let mut input = at(10);
        input.commands.push(configure(PROTOCOL));
        e.step(input).expect("configure");
        assert!(e.configured);
        let first = e
            .shared()
            .with(|s| s.cfg.clone())
            .flatten()
            .expect("cfg stored");
        assert_eq!(first.inst.hourly_cap, 30);

        let mut cfg = test_config();
        cfg.inst.hourly_cap = 5;
        let mut input = at(20);
        input.commands.push(Command::Configure {
            protocol: PROTOCOL,
            config: Box::new(cfg),
            persisted: Persisted::default(),
            seed: 9,
        });
        e.step(input).expect("re-configure");
        let second = e
            .shared()
            .with(|s| s.cfg.clone())
            .flatten()
            .expect("cfg replaced");
        assert_eq!(second.inst.hourly_cap, 5);
        assert!(!Rc::ptr_eq(&first, &second));
        // An already running snapshot is untouched.
        assert_eq!(first.inst.hourly_cap, 30);
    }

    #[test]
    fn start_run_after_configure_reaches_the_run_manager() {
        let mut e = Engine::new();
        let mut input = at(10);
        input.commands.push(configure(PROTOCOL));
        input.commands.push(Command::StartRun {
            run: 1,
            mode: Mode::Full,
            interactive: true,
            items: Vec::new(),
            resume: Vec::new(),
        });
        // Whatever the run manager answers, it is not `not_configured`.
        if let Err(err) = e.step(input) {
            assert_ne!(err.code, code::NOT_CONFIGURED);
        }
    }

    #[test]
    fn sleeping_task_yields_next_timer_and_finishes_when_due() {
        let mut e = Engine::new();
        let finished = Rc::new(Cell::new(false));
        let f = Rc::clone(&finished);
        // `now` is 1000 when the task is first polled: it sleeps until 1100.
        e.step(at(1_000)).expect("prime the clock");
        task(&mut e, |cx| {
            boxed(async move {
                cx.sleep(100).await;
                f.set(true);
            })
        });
        let out = e.step(at(1_000)).expect("step");
        assert_eq!(out.next_timer, Some(1_100));
        assert_eq!(
            (out.stats.tasks, out.stats.timers, out.stats.ops_inflight),
            (1, 1, 0)
        );
        assert!(!out.more && !out.stats.stalled);
        assert!(!finished.get());

        let out = e.step(at(1_099)).expect("early step");
        assert_eq!(out.stats.polls, 0, "nothing is due yet");
        assert_eq!(out.next_timer, Some(1_100));
        assert!(!finished.get());

        let out = e.step(at(1_100)).expect("due step");
        assert!(finished.get());
        assert_eq!(
            (out.next_timer, out.stats.tasks, out.stats.timers),
            (None, 0, 0)
        );
        assert_eq!(out.stats.polls, 1);
    }

    #[test]
    fn op_round_trip_through_step_in_completions() {
        let mut e = Engine::new();
        let got: Rc<RefCell<Option<Result<usize, OpErrKind>>>> = Rc::default();
        let g = Rc::clone(&got);
        task(&mut e, |cx| {
            boxed(async move {
                let r = cx.zotero_oa("10.1371/journal.pmed.1000097").await;
                *g.borrow_mut() = Some(r.map(|hits| hits.len()).map_err(|e| e.kind));
            })
        });
        let out = e.step(at(5)).expect("first step");
        assert_eq!(out.effects.len(), 1);
        let Effect::ZoteroOa { op, doi, .. } = &out.effects[0] else {
            panic!("{:?}", out.effects)
        };
        assert_eq!((*op, doi.as_str()), (1, "10.1371/journal.pmed.1000097"));
        assert_eq!((out.stats.tasks, out.stats.ops_inflight), (1, 1));
        assert!(out.ignored.is_empty());
        assert!(got.borrow().is_none());

        let mut input = at(6);
        input.completions.push(done(
            1,
            json!([{"url": "https://x/y.pdf"}, {"url": "https://x/z.pdf"}]),
        ));
        let out = e.step(input).expect("second step");
        assert_eq!(*got.borrow(), Some(Ok(2)));
        assert_eq!((out.stats.tasks, out.stats.ops_inflight), (0, 0));
        assert!(out.effects.is_empty() && out.ignored.is_empty());
    }

    #[test]
    fn completion_errors_reach_the_task() {
        let mut e = Engine::new();
        let got = Rc::new(Cell::new(None));
        let g = Rc::clone(&got);
        task(&mut e, |cx| {
            boxed(async move {
                g.set(cx.zotero_oa("x").await.err().map(|e| (e.kind, e.after_ms)));
            })
        });
        e.step(at(1)).expect("step");
        let mut input = at(2);
        input.completions.push(CompletionMsg {
            op: 1,
            ok: None,
            err: Some(OpErr {
                kind: OpErrKind::Timeout,
                message: String::new(),
                after_ms: Some(9000),
                status: None,
            }),
        });
        e.step(input).expect("step");
        assert_eq!(got.get(), Some((OpErrKind::Timeout, Some(9000))));
    }

    #[test]
    fn unknown_and_duplicate_completions_are_reported_in_ignored() {
        let mut e = Engine::new();
        task(&mut e, |cx| {
            boxed(async move {
                let _ = cx.zotero_oa("x").await;
            })
        });
        e.step(at(1)).expect("step");
        let mut input = at(2);
        input.completions = vec![done(1, json!([])), done(1, json!([])), done(42, json!({}))];
        let out = e.step(input).expect("step");
        assert_eq!(out.ignored, vec![1, 42]);
        assert_eq!(out.stats.tasks, 0);
        // The list is per step.
        assert!(e.step(at(3)).expect("step").ignored.is_empty());
    }

    #[test]
    fn a_cancelled_op_completing_late_is_ignored_and_cancel_is_delivered_next_step() {
        let mut e = Engine::new();
        task(&mut e, |cx| {
            boxed(async move {
                let _ = cx.zotero_oa("x").await;
            })
        });
        e.step(at(1)).expect("step");
        // Cancel outside `step` (as the run manager does inside a command): the drop pushes into the outbox.
        let gone = e.exec_mut().cancel_where(|_| true);
        assert_eq!(gone, vec![Owner::System]);
        let mut input = at(2);
        input.completions.push(done(1, json!([])));
        let out = e.step(input).expect("step");
        assert_eq!(out.effects, vec![Effect::Cancel { op: 1 }]);
        assert_eq!(out.ignored, vec![1]);
        assert_eq!((out.stats.tasks, out.stats.ops_inflight), (0, 0));
    }

    #[test]
    fn poll_budget_sets_more_until_the_task_is_done() {
        let mut e = Engine::new();
        let finished = Rc::new(Cell::new(false));
        let f = Rc::clone(&finished);
        task(&mut e, |_cx| {
            boxed(async move {
                YieldNow(1_000).await;
                f.set(true);
            })
        });
        let budget = policy::POLL_BUDGET;
        let mut steps = 0;
        let mut total = 0;
        loop {
            steps += 1;
            let out = e.step(at(10)).expect("step");
            total += out.stats.polls;
            if out.more {
                assert_eq!(
                    out.stats.polls, budget,
                    "a step that reports more used its whole budget"
                );
                assert!(!out.stats.stalled);
            } else {
                break;
            }
            assert!(steps < 20, "must terminate");
        }
        assert!(finished.get());
        assert_eq!(total, 1_001, "1000 yields + the final poll");
        assert_eq!(steps, 1_001_u32.div_ceil(budget));
    }

    #[test]
    fn many_ready_tasks_are_spread_over_steps() {
        let mut e = Engine::new();
        e.exec = Executor::new(policy::POLL_BUDGET + 64); // the production table (256) is smaller than the budget + 10
        let count = Rc::new(Cell::new(0u32));
        for _ in 0..(policy::POLL_BUDGET + 10) {
            let c = Rc::clone(&count);
            e.spawn_test(Owner::System, |_| boxed(async move { c.set(c.get() + 1) }))
                .expect("spawn");
        }
        let out = e.step(at(1)).expect("step");
        assert!(out.more);
        assert_eq!(count.get(), policy::POLL_BUDGET);
        let out = e.step(at(1)).expect("step");
        assert!(!out.more);
        assert_eq!(count.get(), policy::POLL_BUDGET + 10);
        assert_eq!(out.stats.tasks, 0);
    }

    #[test]
    fn a_leaf_that_never_registers_a_wake_is_reported_as_stalled() {
        let mut e = Engine::new();
        let polls = Rc::new(Cell::new(0u32));
        let p = Rc::clone(&polls);
        task(&mut e, |_| {
            boxed(std::future::poll_fn(move |_cx| {
                p.set(p.get() + 1);
                Poll::<()>::Pending
            }))
        });
        let out = e.step(at(1)).expect("step");
        assert!(out.stats.stalled);
        assert_eq!(out.stats.lost_wakes, 0);
        assert_eq!(polls.get(), 2, "first poll, then the sweep poll");
        assert_eq!(out.stats.polls, 2);
        assert_eq!(out.stats.tasks, 1);
        // Still stalled on the next step (the condition persists), and not counted as a lost wake.
        let out = e.step(at(2)).expect("step");
        assert!(out.stats.stalled && out.stats.lost_wakes == 0);
    }

    #[test]
    fn a_wake_that_only_a_sweep_delivers_counts_as_a_lost_wake() {
        let mut e = Engine::new();
        let polls = Rc::new(Cell::new(0u32));
        let p = Rc::clone(&polls);
        task(&mut e, |cx| {
            boxed(async move {
                // The first poll parks without a waker; the sweep poll makes progress.
                std::future::poll_fn(move |_cx| {
                    p.set(p.get() + 1);
                    if p.get() >= 2 {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                cx.log(LogLevel::Info, "revived");
            })
        });
        let out = e.step(at(1)).expect("step");
        assert!(!out.stats.stalled);
        assert_eq!(out.stats.lost_wakes, 1);
        assert_eq!(
            out.effects,
            vec![Effect::Log {
                level: LogLevel::Info,
                msg: "revived".into()
            }]
        );
        assert_eq!(out.stats.tasks, 0);
        // The counter is cumulative.
        assert_eq!(e.step(at(2)).expect("step").stats.lost_wakes, 1);
    }

    #[test]
    fn waiting_on_an_op_or_a_timer_is_not_a_stall() {
        let mut e = Engine::new();
        task(&mut e, |cx| {
            boxed(async move {
                let _ = cx.zotero_oa("x").await;
            })
        });
        task(&mut e, |cx| {
            boxed(async move {
                cx.sleep(1_000).await;
            })
        });
        let out = e.step(at(1)).expect("step");
        assert!(!out.stats.stalled);
        assert_eq!(out.stats.polls, 2, "no sweep poll");
        assert_eq!(
            (
                out.stats.ops_inflight,
                out.stats.timers,
                out.stats.lost_wakes
            ),
            (1, 1, 0)
        );
    }

    #[test]
    fn nested_with_returns_none_and_step_reports_reentered() {
        let mut e = Engine::new();
        let inner_was_none = Rc::new(Cell::new(false));
        let flag = Rc::clone(&inner_was_none);
        task(&mut e, |cx| {
            boxed(async move {
                let shared = Rc::clone(&cx.shared);
                let outer = cx.shared.with(|_state| shared.with(|_| ()).is_none());
                flag.set(outer == Some(true));
            })
        });
        let err = e.step(at(1)).expect_err("re-entry must fail the step");
        assert_eq!(err.code, code::REENTERED);
        assert!(inner_was_none.get());
        // Sticky: the engine stays failed until the host resets it.
        assert_eq!(e.step(at(2)).expect_err("sticky").code, code::REENTERED);
    }

    #[test]
    fn effect_order_is_emission_order() {
        let mut e = Engine::new();
        task(&mut e, |cx| {
            boxed(async move {
                cx.emit(Effect::Persist {
                    key: PersistKey::Ledger,
                    value: json!({"reserved": [1]}),
                    flush: true,
                });
                let _ = cx
                    .http(
                        Route::Inst,
                        true,
                        crate::sim::fixtures::http_req("https://doi.org/10.1/x"),
                        Retain::None,
                        InspectSpec::None,
                    )
                    .await;
            })
        });
        task(&mut e, |cx| {
            boxed(async move {
                cx.log(LogLevel::Debug, "second task");
            })
        });
        let out = e.step(at(1)).expect("step");
        let kinds: Vec<&str> = out
            .effects
            .iter()
            .map(|e| match e {
                Effect::Persist { .. } => "persist",
                Effect::Http { barrier: true, .. } => "http_barrier",
                Effect::Log { .. } => "log",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["persist", "http_barrier", "log"]);
    }

    #[test]
    fn spawn_requests_beyond_the_task_table_are_dropped_with_a_log() {
        let mut e = Engine::new();
        e.exec = Executor::new(1);
        let ran = Rc::new(Cell::new(0u32));
        let reqs: Vec<SpawnReq> = (0..2)
            .map(|_| {
                let r = Rc::clone(&ran);
                SpawnReq {
                    owner: Owner::System,
                    fut: boxed(async move { r.set(r.get() + 1) }),
                }
            })
            .collect();
        e.shared().with(|s| s.spawns.extend(reqs));
        let out = e.step(at(1)).expect("step");
        assert_eq!(ran.get(), 1);
        assert!(
            matches!(out.effects.as_slice(), [Effect::Log { level: LogLevel::Error, msg }] if msg.contains("executor full"))
        );
    }

    #[test]
    fn tasks_spawned_through_state_run_in_the_same_step() {
        let mut e = Engine::new();
        let ran = Rc::new(Cell::new(false));
        let r = Rc::clone(&ran);
        // A running task asks for a helper task; the step drains the request after the poll round and polls it.
        task(&mut e, |cx| {
            boxed(async move {
                let r = Rc::clone(&r);
                let _ = cx.shared.with(|s| {
                    s.spawns.push(SpawnReq {
                        owner: Owner::System,
                        fut: boxed(async move { r.set(true) }),
                    })
                });
            })
        });
        let out = e.step(at(1)).expect("step");
        assert!(ran.get());
        assert_eq!(out.stats.tasks, 0);
        assert_eq!(out.stats.polls, 2);
    }

    #[test]
    fn the_clock_never_moves_backwards() {
        let mut e = Engine::new();
        e.step(at(500)).expect("step");
        let seen = Rc::new(Cell::new(0u64));
        let s = Rc::clone(&seen);
        task(&mut e, |cx| boxed(async move { s.set(cx.now()) }));
        e.step(at(300)).expect("step with an older clock");
        assert_eq!(seen.get(), 500);
    }

    /// One scripted run: sleepers, op users and a chatty task, fed fixed inputs. Returns the JSON of each StepOut.
    fn scripted_run() -> Vec<String> {
        let mut e = Engine::new();
        for n in 0..3u64 {
            task(&mut e, move |cx| {
                boxed(async move {
                    cx.sleep(100 * (n + 1)).await;
                    cx.log(LogLevel::Info, format!("slept {n}"));
                    let r = cx.zotero_oa(&format!("10.1/{n}")).await;
                    cx.log(LogLevel::Info, format!("op {n}: {}", r.is_ok()));
                })
            });
        }
        task(&mut e, |cx| {
            boxed(async move {
                YieldNow(5).await;
                cx.log(LogLevel::Debug, "yielded");
            })
        });
        let mut outs = Vec::new();
        let mut steps = vec![at(0), at(100), at(200), at(300)];
        steps[2].completions.push(done(1, json!([])));
        steps[3].completions = vec![done(2, json!([])), done(9, json!(null)), done(3, json!([]))];
        for input in steps {
            let out = e.step(input).expect("step");
            outs.push(serde_json::to_string(&out).expect("serialize"));
        }
        outs
    }

    #[test]
    fn identical_inputs_give_identical_outputs() {
        let a = scripted_run();
        let b = scripted_run();
        assert_eq!(a, b);
        // The script is not trivially empty.
        assert!(a.iter().any(|s| s.contains("slept 2")));
        // Completions are applied before timers: op 3 does not exist yet when its completion arrives.
        assert!(a.iter().any(|s| s.contains("\"ignored\":[9,3]")), "{a:#?}");
    }

    /// Tiny deterministic generator for the stress test (the engine's own `Rng` belongs to the scheduler).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    /// 30 tasks, each awaiting three sequential ops under a 400 ms timeout. The "JS" below answers a random
    /// subset of the pending ops in random order at random times. Whatever the order: every task finishes,
    /// nothing stalls, no wake is lost, and a `Cancel` only ever names an op that was still pending.
    #[test]
    fn random_completion_orders_never_stall_or_lose_wakes() {
        use std::collections::BTreeSet;
        for seed in 1..=25u64 {
            let mut rng = Lcg(seed);
            let mut e = Engine::new();
            let results: Rc<RefCell<Vec<(u32, u32, bool)>>> = Rc::default();
            for n in 0..30u32 {
                let results = Rc::clone(&results);
                task(&mut e, move |cx| {
                    boxed(async move {
                        for k in 0..3u32 {
                            let r = cx
                                .timeout(400, cx.zotero_oa(&format!("10.1/{n}.{k}")))
                                .await;
                            results.borrow_mut().push((n, k, matches!(r, Ok(Ok(_)))));
                        }
                    })
                });
            }
            let mut pending: BTreeSet<OpId> = BTreeSet::new();
            let mut now = 0u64;
            let mut finished = false;
            for _ in 0..5_000 {
                let mut chosen: Vec<OpId> = pending
                    .iter()
                    .copied()
                    .filter(|_| matches!(rng.next() % 3, 0))
                    .collect();
                for i in (1..chosen.len()).rev() {
                    let j = (rng.next() % (i as u64 + 1)) as usize;
                    chosen.swap(i, j);
                }
                for op in &chosen {
                    pending.remove(op);
                }
                let mut input = at(now);
                input.completions = chosen.into_iter().map(|op| done(op, json!([]))).collect();
                let out = e.step(input).expect("step");
                assert!(!out.stats.stalled, "seed {seed}: stalled at {now}");
                assert_eq!(out.stats.lost_wakes, 0, "seed {seed}: lost wake at {now}");
                assert!(
                    out.ignored.is_empty(),
                    "seed {seed}: ignored {:?}",
                    out.ignored
                );
                for effect in out.effects {
                    match effect {
                        Effect::ZoteroOa { op, .. } => {
                            assert!(pending.insert(op), "seed {seed}: op {op} issued twice")
                        }
                        Effect::Cancel { op } => assert!(
                            pending.remove(&op),
                            "seed {seed}: cancel of op {op}, which was not pending"
                        ),
                        other => panic!("seed {seed}: unexpected effect {other:?}"),
                    }
                }
                if out.stats.tasks == 0 {
                    finished = true;
                    break;
                }
                now += rng.next() % 200;
            }
            assert!(finished, "seed {seed}: tasks still running");
            assert!(
                pending.is_empty(),
                "seed {seed}: ops left pending: {pending:?}"
            );
            let results = results.borrow();
            assert_eq!(results.len(), 90, "seed {seed}");
            // Per task the three results arrive in order.
            for n in 0..30u32 {
                let ks: Vec<u32> = results
                    .iter()
                    .filter(|(t, _, _)| *t == n)
                    .map(|(_, k, _)| *k)
                    .collect();
                assert_eq!(ks, vec![0, 1, 2], "seed {seed} task {n}");
            }
        }
    }

    #[test]
    fn engine_default_is_new() {
        let mut e = Engine::default();
        let out = e.step(at(0)).expect("step");
        assert_eq!(out.stats, Stats::default());
        assert!(out.effects.is_empty() && out.next_timer.is_none() && !out.more);
    }
}
