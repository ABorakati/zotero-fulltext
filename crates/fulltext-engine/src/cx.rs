//! Shared engine state and the contexts that pipeline futures hold.
//!
//! # Borrow discipline (read this before touching anything that awaits)
//! All mutable engine state lives in one `State` behind `Shared` (`Rc<RefCell<State>>`). The ONLY way to touch it
//! is `Shared::with(|state| ...)`. The closure gets `&mut State` and nothing else, so it cannot reach the cell
//! again; therefore re-entrant borrows cannot happen as long as the rules below hold:
//!
//! 1. Never hold a borrow across `.await`. (`with` takes a closure; nothing can be kept.)
//! 2. Inside a `with` closure never drop a value whose `Drop` calls `with`: `Op` futures, `Permit`s, `Body`,
//!    `LedgerTicket`, `Executor` tasks. Those live in futures / locals, never inside `State`. `State` stores
//!    only plain data and `Waker`s (dropping a `Waker` only touches the executor's ready queue).
//! 3. Waking (`Waker::wake`) inside a `with` closure is allowed: it only pushes onto the executor's ready queue
//!    (an `Arc<Mutex<VecDeque>>` that nothing else locks).
//! 4. `Executor` methods that can drop futures (`cancel_where`, `retire`, dropping the executor) are called from
//!    `Engine::step` / `run.rs` outside any `with`.
//! 5. `with` uses `try_borrow_mut`; a failed borrow returns `None`, bumps `Shared::reentries`, and `Engine::step`
//!    turns a non-zero count into `EngineError(reentered)`. It never panics (panic = abort in release).
use crate::executor::Executor;
use crate::login::LoginGate;
use crate::ops::Reactor;
use crate::proto::*;
use crate::run::RunTable;
use crate::sched::Sched;
use fulltext_core::model::*;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::rc::Rc;

/// A boxed, non-`Send` future: what the executor stores (single-threaded wasm).
pub(crate) type LocalFuture = Pin<Box<dyn Future<Output = ()>>>;

/// How `run.rs` builds the task body of one item. Production value: `pipeline::item_runner`.
/// Tests pass their own (`Engine::with_runner`) to exercise the run manager without the real pipeline.
pub(crate) type ItemRunner = fn(ItemCx) -> LocalFuture;

/// Virtual clocks as of the current step. `now` is monotonic ms (timers, gaps); `wall` is epoch ms (ledger,
/// halts, cache TTLs only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Clock {
    pub now: u64,
    pub wall: u64,
}

impl Clock {
    /// Set the clocks from `StepIn`. `now` never moves backwards (a smaller value is clamped to the old one).
    pub(crate) fn advance(&mut self, now: u64, wall: u64) {
        self.now = self.now.max(now);
        self.wall = wall;
    }
}

/// Who owns a task. Cancellation is by owner: `CancelRun` drops every `Item`/`Run` task of that run,
/// `CancelItem` drops one `Item` task, `Shutdown` drops everything including `System`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Owner {
    Item {
        run: RunId,
        item: ItemId,
    },
    /// Run-level helper tasks (the batch Confirm).
    Run(RunId),
    /// Engine-wide helper tasks that must survive a run's cancellation (the login window driver).
    System,
}

impl Owner {
    #[cfg(test)]
    pub(crate) fn run(&self) -> Option<RunId> {
        match self {
            Owner::Item { run, .. } | Owner::Run(run) => Some(*run),
            Owner::System => None,
        }
    }
}

/// A task that logic running inside `State` wants started (it cannot call the executor from inside `with`).
/// `Engine::step` drains `State::spawns` after every poll round.
pub(crate) struct SpawnReq {
    pub owner: Owner,
    pub fut: LocalFuture,
}

/// Result of one finished item task, queued for the run manager (`run::collect_done`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ItemDone {
    pub run: RunId,
    pub item: ItemId,
    pub outcome: Outcome,
    pub trace: Vec<TraceStep>,
}

/// All mutable engine state. Fields are owned by the module named in the comment; other modules go through
/// that module's methods.
pub(crate) struct State {
    pub clock: Clock,
    /// Latest `Configure` config (each run snapshots it at `StartRun`).
    pub cfg: Option<Rc<EngineConfig>>,
    /// ops.rs: op slots, virtual timers, ignored completions.
    pub reactor: Reactor,
    /// sched/: gates, token buckets, licence ledger, halts, host knowledge, RNG.
    pub sched: Sched,
    /// login.rs: session states per realm, interactive window gate.
    pub login: LoginGate,
    /// run.rs: live runs and their per-item records.
    pub runs: RunTable,
    /// Effects produced during this step, in emission order. Order matters: a `Persist` pushed before an
    /// `Http{barrier:true}` must stay before it. Drained into `StepOut.effects` by `Engine::step`.
    pub outbox: Vec<Effect>,
    /// Finished item tasks (drained by `run::collect_done`).
    pub done: Vec<ItemDone>,
    /// Tasks to spawn after the current poll round.
    pub spawns: Vec<SpawnReq>,
}

impl State {
    pub(crate) fn new() -> Self {
        Self {
            clock: Clock::default(),
            cfg: None,
            reactor: Reactor::new(),
            sched: Sched::new(),
            login: LoginGate::new(),
            runs: RunTable::new(),
            outbox: Vec::new(),
            done: Vec::new(),
            spawns: Vec::new(),
        }
    }
}

/// The single cell around [`State`]. See the module docs for the borrow rules.
pub(crate) struct Shared {
    state: RefCell<State>,
    reentries: Cell<u32>,
}

impl Shared {
    pub(crate) fn new() -> Rc<Shared> {
        Rc::new(Shared {
            state: RefCell::new(State::new()),
            reentries: Cell::new(0),
        })
    }

    /// Run `f` with exclusive access to the state. `None` means the cell was already borrowed (engine bug).
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> Option<R> {
        match self.state.try_borrow_mut() {
            Ok(mut st) => Some(f(&mut st)),
            Err(_) => {
                self.reentries.set(self.reentries.get().saturating_add(1));
                None
            }
        }
    }

    /// Number of failed `with` calls so far; `Engine::step` fails the run if this is non-zero.
    pub(crate) fn reentries(&self) -> u32 {
        self.reentries.get()
    }
}

/// What `run.rs` functions receive: the shared state plus the executor (needed to spawn / cancel tasks).
pub(crate) struct Ctl<'a> {
    pub shared: &'a Rc<Shared>,
    pub exec: &'a mut Executor,
}

/// Base context for any task that performs ops: owner + ids for effect fields. Cheap to clone.
/// Op wrappers (`http`, `browser_load`, `login_window`, `run_translators`, `zotero_oa`, `save_pdf`, `confirm`,
/// `sleep`, `timeout`) are inherent methods added in ops.rs.
#[derive(Clone)]
pub(crate) struct Cx {
    pub(crate) shared: Rc<Shared>,
    pub(crate) owner: Owner,
    /// Value for the `run` field of effects (0 for `Owner::System`).
    pub(crate) run: RunId,
    /// Value for the `item` field of effects (0 when not item-scoped).
    pub(crate) item: ItemId,
}

impl Cx {
    pub(crate) fn new(shared: Rc<Shared>, owner: Owner) -> Self {
        let (run, item) = match owner {
            Owner::Item { run, item } => (run, item),
            Owner::Run(run) => (run, 0),
            Owner::System => (0, 0),
        };
        Self {
            shared,
            owner,
            run,
            item,
        }
    }

    /// Same owner, different `run` / `item` labels on emitted effects (the login window driver, an `Owner::System`
    /// task, labels its `LoginWindow` effect with the first waiter's run and item).
    #[cfg(test)]
    pub(crate) fn with_ids(&self, run: RunId, item: ItemId) -> Cx {
        Cx {
            run,
            item,
            ..self.clone()
        }
    }

    /// Monotonic ms of the current step.
    pub(crate) fn now(&self) -> u64 {
        self.shared.with(|s| s.clock.now).unwrap_or_default()
    }

    /// Epoch ms of the current step.
    #[cfg(test)]
    pub(crate) fn wall(&self) -> u64 {
        self.shared.with(|s| s.clock.wall).unwrap_or_default()
    }

    /// Append an effect to the outbox.
    pub(crate) fn emit(&self, effect: Effect) {
        let _ = self.shared.with(|s| s.outbox.push(effect));
    }

    #[cfg(test)]
    pub(crate) fn log(&self, level: LogLevel, msg: impl Into<String>) {
        self.emit(Effect::Log {
            level,
            msg: msg.into(),
        });
    }
}

/// The context of one item's task: `Cx` plus the run's config snapshot and the item snapshot.
/// `Deref<Target = Cx>` so `cx.http(..)`, `cx.sleep(..)` work directly.
/// Pipeline-facing sched/login/run methods are inherent methods added in sched/acquire.rs, login.rs, run.rs.
#[derive(Clone)]
pub(crate) struct ItemCx {
    pub(crate) cx: Cx,
    /// Config snapshot taken at `StartRun` (pref changes never affect a running batch).
    pub(crate) cfg: Rc<EngineConfig>,
    pub(crate) snap: Rc<ItemSnapshot>,
    pub(crate) mode: Mode,
    pub(crate) interactive: bool,
    /// Attempt trace for the report. Local to this item's future: borrowed only inside the methods below,
    /// never across an await, never from another task.
    trace: Rc<RefCell<Vec<TraceStep>>>,
}

impl Deref for ItemCx {
    type Target = Cx;
    fn deref(&self) -> &Cx {
        &self.cx
    }
}

impl ItemCx {
    pub(crate) fn new(
        shared: Rc<Shared>,
        run: RunId,
        mode: Mode,
        interactive: bool,
        cfg: Rc<EngineConfig>,
        snap: Rc<ItemSnapshot>,
    ) -> Self {
        let owner = Owner::Item {
            run,
            item: snap.item,
        };
        Self {
            cx: Cx::new(shared, owner),
            cfg,
            snap,
            mode,
            interactive,
            trace: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Emit a progress row for this item.
    pub(crate) fn progress(&self, row: ProgressRow) {
        self.emit(Effect::Progress {
            run: self.run,
            item: self.item,
            row,
        });
    }

    /// Record one attempt in the item's trace (shown in the report; capped at 64 entries).
    pub(crate) fn trace_step(&self, src: &str, tier: &str, class: &str, ms: u32) {
        if let Ok(mut t) = self.trace.try_borrow_mut()
            && t.len() < 64
        {
            t.push(TraceStep {
                src: src.to_owned(),
                tier: tier.to_owned(),
                class: class.to_owned(),
                ms,
            });
        }
    }

    /// Emit a structured trace event when `cfg.trace` is on (otherwise a no-op).
    pub(crate) fn trace_event(&self, ev: Value) {
        if self.cfg.trace {
            self.emit(Effect::Trace {
                run: self.run,
                item: self.item,
                ev,
            });
        }
    }

    pub(crate) fn take_trace(&self) -> Vec<TraceStep> {
        self.trace
            .try_borrow_mut()
            .map(|mut t| std::mem::take(&mut *t))
            .unwrap_or_default()
    }

    /// Hand the final outcome to the run manager. Called exactly once, by the item runner, as the last step of
    /// the task body. (A cancelled task never gets here; `run.rs` records `Cancelled` itself.)
    pub(crate) fn finish(&self, outcome: Outcome) {
        let done = ItemDone {
            run: self.run,
            item: self.item,
            outcome,
            trace: self.take_trace(),
        };
        let _ = self.shared.with(|s| s.done.push(done));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::fixtures::{item, test_config};
    use serde_json::json;

    fn item_cx(trace: bool) -> (Rc<Shared>, ItemCx) {
        let shared = Shared::new();
        let mut cfg = test_config();
        cfg.trace = trace;
        let cx = ItemCx::new(
            Rc::clone(&shared),
            3,
            Mode::Full,
            true,
            Rc::new(cfg),
            Rc::new(item(12, "10.1/x")),
        );
        (shared, cx)
    }

    fn outbox(shared: &Shared) -> Vec<Effect> {
        shared
            .with(|s| std::mem::take(&mut s.outbox))
            .unwrap_or_default()
    }

    #[test]
    fn nested_with_fails_softly_and_is_counted() {
        let shared = Shared::new();
        assert_eq!(shared.reentries(), 0);
        let inner = shared.with(|_| shared.with(|_| 1));
        assert_eq!(
            inner,
            Some(None),
            "the outer call ran, the nested one was refused"
        );
        assert_eq!(shared.reentries(), 1);
        // The cell is usable again afterwards.
        assert_eq!(shared.with(|s| s.clock.now), Some(0));
        let _ = shared.with(|_| shared.with(|_| ()));
        assert_eq!(shared.reentries(), 2);
    }

    #[test]
    fn clock_never_moves_backwards_but_wall_follows_the_host() {
        let mut c = Clock::default();
        c.advance(100, 5_000);
        c.advance(40, 5_050);
        assert_eq!(
            c,
            Clock {
                now: 100,
                wall: 5_050
            }
        );
        c.advance(150, 5_100);
        assert_eq!(c.now, 150);
    }

    #[test]
    fn owners_map_to_run_and_effect_ids() {
        let item_owner = Owner::Item { run: 4, item: 9 };
        assert_eq!(item_owner.run(), Some(4));
        assert_eq!(Owner::Run(5).run(), Some(5));
        assert_eq!(Owner::System.run(), None);

        let shared = Shared::new();
        let cx = Cx::new(Rc::clone(&shared), item_owner);
        assert_eq!((cx.run, cx.item), (4, 9));
        let cx = Cx::new(Rc::clone(&shared), Owner::Run(5));
        assert_eq!((cx.run, cx.item), (5, 0));
        let cx = Cx::new(Rc::clone(&shared), Owner::System);
        assert_eq!((cx.run, cx.item), (0, 0));
        let relabelled = cx.with_ids(6, 7);
        assert_eq!(
            (relabelled.owner, relabelled.run, relabelled.item),
            (Owner::System, 6, 7)
        );
    }

    #[test]
    fn cx_reads_the_clock_and_emits_in_order() {
        let shared = Shared::new();
        let cx = Cx::new(Rc::clone(&shared), Owner::System);
        let _ = shared.with(|s| s.clock.advance(250, 9_000));
        assert_eq!((cx.now(), cx.wall()), (250, 9_000));
        cx.log(LogLevel::Info, "one");
        cx.emit(Effect::Release { handle: 3 });
        cx.log(LogLevel::Warn, String::from("two"));
        let out = outbox(&shared);
        assert_eq!(
            out,
            vec![
                Effect::Log {
                    level: LogLevel::Info,
                    msg: "one".into()
                },
                Effect::Release { handle: 3 },
                Effect::Log {
                    level: LogLevel::Warn,
                    msg: "two".into()
                },
            ]
        );
    }

    #[test]
    fn item_cx_labels_progress_with_its_ids_and_derefs_to_cx() {
        let (shared, cx) = item_cx(false);
        assert_eq!(
            (cx.run, cx.item, cx.owner),
            (3, 12, Owner::Item { run: 3, item: 12 })
        );
        let row = ProgressRow {
            state: RowState::Working,
            l10n: "zotero-fulltext-status-searching-oa".into(),
            args: Default::default(),
            route: None,
            actions: Vec::new(),
        };
        cx.progress(row.clone());
        assert_eq!(
            outbox(&shared),
            vec![Effect::Progress {
                run: 3,
                item: 12,
                row
            }]
        );
    }

    #[test]
    fn trace_steps_are_capped_and_taken_once() {
        let (_shared, cx) = item_cx(false);
        for i in 0..100u32 {
            cx.trace_step("openalex", "api", "found", i);
        }
        let trace = cx.take_trace();
        assert_eq!(trace.len(), 64);
        assert_eq!(trace.first().map(|t| t.ms), Some(0));
        assert_eq!(trace.last().map(|t| t.ms), Some(63));
        assert!(cx.take_trace().is_empty(), "take_trace drains");
    }

    #[test]
    fn trace_events_only_when_the_run_config_asks_for_them() {
        let (shared, off) = item_cx(false);
        off.trace_event(json!({"ev": "x"}));
        assert!(outbox(&shared).is_empty());
        let (shared, on) = item_cx(true);
        on.trace_event(json!({"ev": "x"}));
        assert_eq!(
            outbox(&shared),
            vec![Effect::Trace {
                run: 3,
                item: 12,
                ev: json!({"ev": "x"})
            }]
        );
    }

    #[test]
    fn finish_hands_the_outcome_and_trace_to_the_run_manager() {
        let (shared, cx) = item_cx(false);
        cx.trace_step("pmc_s3", "http", "pdf", 40);
        cx.finish(Outcome::Skipped {
            reason: SkipReason::HasPdf,
        });
        let done = shared
            .with(|s| std::mem::take(&mut s.done))
            .unwrap_or_default();
        assert_eq!(done.len(), 1);
        assert_eq!((done[0].run, done[0].item), (3, 12));
        assert_eq!(
            done[0].outcome,
            Outcome::Skipped {
                reason: SkipReason::HasPdf
            }
        );
        assert_eq!(
            done[0].trace,
            vec![TraceStep {
                src: "pmc_s3".into(),
                tier: "http".into(),
                class: "pdf".into(),
                ms: 40
            }]
        );
    }

    #[test]
    fn a_fresh_state_is_empty() {
        let shared = Shared::new();
        let empty = shared
            .with(|s| {
                s.cfg.is_none()
                    && s.outbox.is_empty()
                    && s.done.is_empty()
                    && s.spawns.is_empty()
                    && s.clock == Clock::default()
            })
            .unwrap_or(false);
        assert!(empty);
    }
}
