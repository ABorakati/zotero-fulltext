//! Run manager: `StartRun`, cancellation, user actions, confirmations, per-item records, reports.
//!
//! # Contract
//! * `start_run`: needs a prior `Configure` (else `EngineError(not_configured)`). Snapshots `State::cfg` for the
//!   run. Items whose id is already claimed by another live run are recorded immediately as
//!   `Skipped(InProgressElsewhere)` and get a terminal row; every other item gets a `Queued` row and one task
//!   (`Owner::Item`) built by the `ItemRunner`. Items beyond the task table (`exec.max_tasks`, less
//!   [`HELPER_HEADROOM`] slots kept for run helpers and login windows) stay `Queued` in the run and are spawned
//!   as slots free up (never an error). `resume` hints are recorded (`inst_blocked`) and bump
//!   `RunReport.engine_restarts` by 1 when non-empty. A run id that is already live is `EngineError(bad_command)`.
//! * Admission: in `Mode::Full` with `inst.enabled` and `interactive`, when the number of items that could use
//!   the institutional route (not duplicates, passing `precheck::eligibility`) exceeds `inst.confirm_above`, a
//!   run-level task (`Owner::Run`) emits `Confirm { LargeBatch, n, cap = min(batch_max, CEIL_BATCH_MAX) }` and
//!   every item task waits in `wait_admitted()` meanwhile (rows show `WAITING_CONFIRM`; OA work does NOT start
//!   before the answer, it is one quick prompt). `Yes` -> institutional allowed for the first `cap` items to ask;
//!   `OaOnly` -> run is OA-only; `Cancel` (or a failed prompt) -> `cancel_run`. Non-interactive runs never
//!   prompt: large batches get the first `cap` institutional slots, group libraries are treated as
//!   `GroupPolicy::Never` when the policy is `Ask`.
//! * `claim_inst_slot` is how items enter the institutional phase: it enforces mode/enabled, `resume`
//!   blocks, the batch cap (`Deferred { Batch }`), halts, and group-library policy. The group-library prompt
//!   (`Ask`) itself is asked lazily by `ItemCx::group_consent` (inst.rs and save.rs await it): one
//!   `Confirm { GroupLibrary }` per run, answered once for all items; a declined answer makes later claims
//!   `GroupDenied`.
//! * `CancelRun` / `CancelItem` / `Shutdown`: the executor drops the matching tasks first (their `Drop`s emit
//!   `Cancel`/`Release` in this same step), then the manager records `Outcome::Cancelled { by }` and a terminal
//!   `Cancelled` row for each dropped item. Items in `ItemPhase::Saving` (`mark_saving`) are NOT cancelled: the
//!   save is short and atomic; they finish and report normally. Unknown runs and items are logged and ignored.
//! * `collect_done` moves `State::done` into the run records, emits the terminal `Progress` row
//!   (`l10n::outcome_row`), frees the item's claim, and spawns queued items while the table has room.
//! * `finish_runs`: a run with every item terminal emits exactly one `Effect::RunFinished { run, report }`
//!   (counts, items in snapshot order, ledger usage, `halted` reason, `engine_restarts`), then forgets the run
//!   (`login.on_run_gone`). A run helper task that is still waiting for the user when the last item ends (a group
//!   prompt nobody needs any more) is cancelled first. An item whose task vanished without reporting an outcome
//!   (a bug) is recorded as `Failed { code: "task_lost" }` so a run can never hang.
//! * `DumpState`: one `Effect::Log { Info, json }` with tasks, ops, timers, gates, login states, runs.
//! * `Shutdown`: cancel everything (`CancelledBy::Shutdown`), emit `Persist` for all keys with `flush = true`,
//!   then `RunFinished` for each live run that has no saving item left.
//!
//! # Waiting protocol
//! `WaitAdmitted` and `GroupConsent` park their task's waker in `RunState::waiters`; every decision wakes all of
//! them and each future re-reads the state (spurious wake-ups are harmless). No borrow is ever held across a poll.
use crate::cx::{
    Ctl, Cx, ItemCx, ItemDone, ItemRunner, LocalFuture, Owner, Shared, SpawnReq, State,
};
use crate::error::{EngineError, code};
use crate::executor::Executor;
use crate::l10n::{self, id};
use crate::login;
use crate::pipeline::precheck;
use crate::proto::*;
use crate::sched::policy;
use fulltext_core::model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Task-table slots that item tasks never use: run helpers (the two confirm prompts) and the login window
/// drivers must always find room.
const HELPER_HEADROOM: usize = 8;

/// `Command::StartRun` unpacked.
#[derive(Clone, Debug)]
pub(crate) struct StartRunCmd {
    pub run: RunId,
    pub mode: Mode,
    pub interactive: bool,
    pub items: Vec<ItemSnapshot>,
    pub resume: Vec<ResumeHint>,
}

/// Lifecycle of one item inside a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemPhase {
    /// Created, no task yet (the task table is full).
    Queued,
    /// The task exists (waiting for admission, an item slot, or working).
    Active,
    /// Inside `SavePdf`: not cancellable.
    Saving,
    Done,
}

#[derive(Clone, Debug)]
pub(crate) struct ItemRec {
    pub snap: Rc<ItemSnapshot>,
    pub phase: ItemPhase,
    pub outcome: Option<Outcome>,
    pub trace: Vec<TraceStep>,
    pub inst_blocked: bool,
    /// The latest row of this item carries a `SignIn` button (cleared again by `SkipSignIn` / `RetrySignIn`).
    pub sign_in_button: bool,
}

/// Answer to "may the item tasks start?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunAdmission {
    /// Go. `inst_allowed` is false for OA-only runs and for declined large batches.
    Go {
        inst_allowed: bool,
    },
    Cancelled,
}

/// Answer to `claim_inst_slot`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InstClaim {
    Granted,
    /// Run is OA-only (mode, pref off, or batch declined).
    NotAllowed,
    /// Beyond the batch cap: outcome `Deferred { cap: Batch }`.
    BatchCap,
    /// Halted: `RunReport.halted` carries the reason.
    Halted {
        reason: String,
    },
    /// Group library and policy `Never` (or non-interactive `Ask`, or a declined prompt): outcome
    /// `NeedsUser(GroupLibrary)`.
    GroupDenied,
    /// `resume.inst_blocked`: outcome `NeedsUser(EngineRestarted)`.
    ResumeBlocked,
}

#[derive(Debug)]
pub(crate) struct RunState {
    pub mode: Mode,
    pub interactive: bool,
    pub cfg: Rc<EngineConfig>,
    pub items: BTreeMap<ItemId, ItemRec>,
    /// Snapshot order, for the report.
    pub order: Vec<ItemId>,
    pub admission: Option<RunAdmission>,
    pub inst_quota_left: u32,
    pub halted: Option<String>,
    pub engine_restarts: u32,
    /// Group-library consent: `None` = not answered yet, `Some(allow)` = answered.
    pub group_consent: Option<bool>,
    /// The group-library prompt has been started (its answer is still to come when `group_consent` is `None`).
    pub group_asked: bool,
    pub waiters: Vec<Waker>,
    pub finished: bool,
}

#[derive(Debug, Default)]
pub(crate) struct RunTable {
    pub(crate) runs: BTreeMap<RunId, RunState>,
    /// item id -> run that currently owns it (F21: duplicate concurrent runs).
    pub(crate) claimed: BTreeMap<ItemId, RunId>,
    /// Cancellations asked for by run helper tasks (they cannot cancel tasks from inside a poll); executed by
    /// `collect_done`.
    cancel_requests: Vec<(RunId, CancelledBy)>,
}

impl RunTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_live(&self, run: RunId) -> bool {
        self.runs.get(&run).is_some_and(|r| !r.finished)
    }
}

fn reentered() -> EngineError {
    EngineError::new(code::REENTERED, "engine state was borrowed re-entrantly")
}

/// Wake every parked waiter of a run (they re-read the run state).
fn wake_waiters(rs: &mut RunState) {
    std::mem::take(&mut rs.waiters)
        .into_iter()
        .for_each(Waker::wake);
}

/// Remember `waker` once.
fn park(waiters: &mut Vec<Waker>, waker: &Waker) {
    if !waiters.iter().any(|w| w.will_wake(waker)) {
        waiters.push(waker.clone());
    }
}

fn log(s: &mut State, level: LogLevel, msg: String) {
    s.outbox.push(Effect::Log { level, msg });
}

/// How many task-table slots item tasks may use.
fn item_capacity(exec: &Executor) -> usize {
    usize::try_from(exec.max_tasks())
        .unwrap_or(usize::MAX)
        .saturating_sub(HELPER_HEADROOM)
        .max(1)
}

// ------------------------------------------------------------------------------ engine-facing entry points

/// Handle `Command::StartRun`.
pub(crate) fn start_run(
    ctl: &mut Ctl<'_>,
    runner: ItemRunner,
    cmd: StartRunCmd,
) -> Result<(), EngineError> {
    let run = cmd.run;
    let begun = ctl
        .shared
        .with(|s| begin_run(s, cmd))
        .ok_or_else(reentered)??;
    if let Some(prompt) = begun {
        let cx = Cx::new(Rc::clone(ctl.shared), Owner::Run(run));
        let task: LocalFuture = Box::pin(batch_confirm(cx, prompt));
        if ctl.exec.spawn(Owner::Run(run), task).is_err() {
            // No room for the prompt: run without the institutional route rather than hang.
            let _ = ctl.shared.with(|s| {
                log(
                    s,
                    LogLevel::Error,
                    format!(
                        "run {run}: no task slot for the batch prompt; the run is open-access only"
                    ),
                );
                admit(
                    s,
                    run,
                    RunAdmission::Go {
                        inst_allowed: false,
                    },
                );
            });
        }
    }
    spawn_queued(ctl, runner);
    Ok(())
}

/// What the batch prompt shows.
#[derive(Clone, Copy, Debug)]
struct BatchPrompt {
    n: u32,
    cap: u32,
}

/// Create the run record, claim the items and emit the first rows. Returns the batch prompt to ask, if any.
fn begin_run(s: &mut State, cmd: StartRunCmd) -> Result<Option<BatchPrompt>, EngineError> {
    let StartRunCmd {
        run,
        mode,
        interactive,
        items,
        resume,
    } = cmd;
    let Some(cfg) = s.cfg.clone() else {
        return Err(EngineError::new(
            code::NOT_CONFIGURED,
            "StartRun before Configure",
        ));
    };
    if s.runs.runs.contains_key(&run) {
        return Err(EngineError::new(
            code::BAD_COMMAND,
            format!("run {run} already exists"),
        ));
    }
    let blocked: BTreeSet<ItemId> = resume
        .iter()
        .filter(|h| h.inst_blocked)
        .map(|h| h.item)
        .collect();
    let cap = cfg.inst.batch_max.min(policy::CEIL_BATCH_MAX);
    let inst_possible = mode == Mode::Full && cfg.inst.enabled;
    let mut rs = RunState {
        mode,
        interactive,
        cfg: Rc::clone(&cfg),
        items: BTreeMap::new(),
        order: Vec::new(),
        admission: None,
        inst_quota_left: cap,
        halted: None,
        engine_restarts: u32::from(!resume.is_empty()),
        group_consent: None,
        group_asked: false,
        waiters: Vec::new(),
        finished: false,
    };

    let mut rows: Vec<(ItemId, ProgressRow)> = Vec::new();
    for snap in items {
        let item = snap.item;
        if rs.items.contains_key(&item) {
            continue; // listed twice in one request
        }
        let elsewhere = s
            .runs
            .claimed
            .get(&item)
            .is_some_and(|other| s.runs.is_live(*other));
        let mut rec = ItemRec {
            snap: Rc::new(snap),
            phase: ItemPhase::Queued,
            outcome: None,
            trace: Vec::new(),
            inst_blocked: blocked.contains(&item),
            sign_in_button: false,
        };
        if elsewhere {
            let outcome = Outcome::Skipped {
                reason: SkipReason::InProgressElsewhere,
            };
            rows.push((item, l10n::outcome_row(&outcome)));
            rec.phase = ItemPhase::Done;
            rec.outcome = Some(outcome);
        } else {
            s.runs.claimed.insert(item, run);
        }
        rs.order.push(item);
        rs.items.insert(item, rec);
    }

    let eligible = rs
        .items
        .values()
        .filter(|r| r.phase != ItemPhase::Done && precheck::eligibility(&r.snap).is_none())
        .count();
    let eligible = u32::try_from(eligible).unwrap_or(u32::MAX);
    let ask = inst_possible && interactive && eligible > cfg.inst.confirm_above;
    let queued_row = if ask {
        l10n::row(RowState::Waiting, id::WAITING_CONFIRM)
    } else {
        l10n::row(RowState::Queued, id::QUEUED)
    };
    for item in &rs.order {
        if rs
            .items
            .get(item)
            .is_some_and(|r| r.phase == ItemPhase::Queued)
        {
            rows.push((*item, queued_row.clone()));
        }
    }
    rs.admission = if ask {
        None
    } else {
        Some(RunAdmission::Go {
            inst_allowed: inst_possible,
        })
    };
    s.runs.runs.insert(run, rs);
    for (item, row) in rows {
        s.outbox.push(Effect::Progress { run, item, row });
    }
    Ok(ask.then_some(BatchPrompt { n: eligible, cap }))
}

/// Run helper: ask whether a large batch may use the institutional route.
async fn batch_confirm(cx: Cx, prompt: BatchPrompt) {
    let req = ConfirmReq {
        kind: ConfirmKind::LargeBatch,
        n: prompt.n,
        cap: prompt.cap,
        library: String::new(),
    };
    let answer = cx.confirm(req).await;
    let run = cx.run;
    let _ = cx.shared.with(|s| match answer.map(|ok| ok.choice) {
        Ok(ConfirmChoice::Yes) => admit(s, run, RunAdmission::Go { inst_allowed: true }),
        Ok(ConfirmChoice::OaOnly) => admit(
            s,
            run,
            RunAdmission::Go {
                inst_allowed: false,
            },
        ),
        Ok(ConfirmChoice::Cancel) | Err(_) => {
            admit(s, run, RunAdmission::Cancelled);
            s.runs.cancel_requests.push((run, CancelledBy::User));
        }
    });
}

/// Record the admission decision, refresh the rows of the items that were waiting for it, wake them.
fn admit(s: &mut State, run: RunId, admission: RunAdmission) {
    let Some(rs) = s.runs.runs.get_mut(&run) else {
        return;
    };
    rs.admission = Some(admission);
    wake_waiters(rs);
    if matches!(admission, RunAdmission::Go { .. }) {
        let waiting: Vec<ItemId> = rs
            .order
            .iter()
            .copied()
            .filter(|i| rs.items.get(i).is_some_and(|r| r.phase != ItemPhase::Done))
            .collect();
        for item in waiting {
            s.outbox.push(Effect::Progress {
                run,
                item,
                row: l10n::row(RowState::Queued, id::QUEUED),
            });
        }
    }
}

/// Handle `Command::CancelRun` (also used by Shutdown with `CancelledBy::Shutdown`).
pub(crate) fn cancel_run(ctl: &mut Ctl<'_>, run: RunId, by: CancelledBy) {
    cancel_items(ctl, run, None, by);
}

/// Handle `Command::CancelItem`.
pub(crate) fn cancel_item(ctl: &mut Ctl<'_>, run: RunId, item: ItemId) {
    cancel_items(ctl, run, Some(item), CancelledBy::User);
}

/// Is this item inside `SavePdf`? Safe to call outside `Shared::with`.
fn is_saving(shared: &Rc<Shared>, run: RunId, item: ItemId) -> bool {
    shared
        .with(|s| {
            s.runs
                .runs
                .get(&run)
                .and_then(|r| r.items.get(&item))
                .is_some_and(|rec| rec.phase == ItemPhase::Saving)
        })
        .unwrap_or(false)
}

/// Cancel a whole run (`only == None`) or one item. The tasks are dropped first, outside any borrow of the
/// state, so the `Cancel` / `Release` effects of their futures come out in this very step.
fn cancel_items(ctl: &mut Ctl<'_>, run: RunId, only: Option<ItemId>, by: CancelledBy) {
    let known = ctl
        .shared
        .with(|s| {
            drain_done(s);
            match (s.runs.runs.get(&run), only) {
                (Some(r), None) => !r.finished,
                (Some(r), Some(item)) => r
                    .items
                    .get(&item)
                    .is_some_and(|rec| rec.phase != ItemPhase::Done),
                (None, _) => false,
            }
        })
        .unwrap_or(false);
    if !known {
        let what = only.map_or_else(
            || format!("run {run}"),
            |item| format!("item {item} of run {run}"),
        );
        let _ = ctl.shared.with(|s| {
            log(
                s,
                LogLevel::Info,
                format!("cancel ignored: {what} is not active"),
            )
        });
        return;
    }

    let shared = ctl.shared;
    ctl.exec.cancel_where(|owner| match owner {
        Owner::Item { run: r, item } => {
            *r == run && only.is_none_or(|o| o == *item) && !is_saving(shared, run, *item)
        }
        Owner::Run(r) => *r == run && only.is_none(),
        Owner::System => false,
    });

    let _ = shared.with(|s| {
        let doomed: Vec<ItemId> = s
            .runs
            .runs
            .get(&run)
            .map(|r| {
                r.order
                    .iter()
                    .copied()
                    .filter(|item| only.is_none_or(|o| o == *item))
                    .filter(|item| {
                        r.items.get(item).is_some_and(|rec| {
                            matches!(rec.phase, ItemPhase::Queued | ItemPhase::Active)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        for item in doomed {
            finish_item(s, run, item, Outcome::Cancelled { by }, Vec::new());
        }
        if only.is_none() {
            s.login.on_run_gone(run);
        }
    });
}

/// Handle `Command::UserAction`: forwards to `login::on_user_action` and clears row actions.
pub(crate) fn user_action(ctl: &mut Ctl<'_>, run: RunId, kind: UserActionKind) {
    login::on_user_action(ctl, run, kind);
    if !matches!(
        kind,
        UserActionKind::SkipSignIn | UserActionKind::RetrySignIn
    ) {
        return;
    }
    // The "Sign in" button is moot now: re-issue the waiting row of every item that still shows it.
    let _ = ctl.shared.with(|s| {
        let Some(rs) = s.runs.runs.get_mut(&run) else {
            return;
        };
        let mut rows = Vec::new();
        for item in &rs.order {
            if let Some(rec) = rs.items.get_mut(item)
                && rec.phase != ItemPhase::Done
                && std::mem::take(&mut rec.sign_in_button)
            {
                rows.push(*item);
            }
        }
        for item in rows {
            let row = l10n::row(RowState::Waiting, id::WAITING_LOGIN).on_route(Route::Inst);
            s.outbox.push(Effect::Progress { run, item, row });
        }
    });
}

/// Handle `Command::Shutdown`.
pub(crate) fn shutdown(ctl: &mut Ctl<'_>) {
    let runs: Vec<RunId> = ctl
        .shared
        .with(|s| {
            s.runs
                .runs
                .iter()
                .filter(|(_, r)| !r.finished)
                .map(|(run, _)| *run)
                .collect()
        })
        .unwrap_or_default();
    for run in runs {
        cancel_run(ctl, run, CancelledBy::Shutdown);
    }
    login::on_shutdown(ctl);
    let _ = ctl.shared.with(|s| {
        let persist = s.sched.persist_all(true);
        s.outbox.extend(persist);
    });
    finish_runs(ctl);
}

/// Handle `Command::DumpState`: one `Log` with everything the engine knows, in a stable key order.
pub(crate) fn dump_state(ctl: &mut Ctl<'_>) {
    let owners: Vec<String> = ctl.exec.owners().iter().map(owner_label).collect();
    let (live, max) = (ctl.exec.live(), ctl.exec.max_tasks());
    let _ = ctl.shared.with(|s| {
        let ops: Vec<Value> = s
            .reactor
            .waiting()
            .into_iter()
            .map(|(op, owner, kind)| json!({ "op": op, "owner": owner_label(&owner), "kind": format!("{kind:?}") }))
            .collect();
        let runs: serde_json::Map<String, Value> = s.runs.runs.iter().map(|(run, rs)| (run.to_string(), dump_run(rs))).collect();
        let dump = json!({
            "clock": { "now": s.clock.now, "wall": s.clock.wall },
            "tasks": { "live": live, "max": max, "owners": owners },
            "ops": ops,
            "timers": { "count": s.reactor.timer_count(), "next": s.reactor.next_deadline() },
            "sched": s.sched.dump(s.clock.wall),
            "login": s.login.dump(),
            "runs": runs,
            "claimed": s.runs.claimed.iter().map(|(item, run)| (item.to_string(), *run)).collect::<BTreeMap<String, RunId>>(),
        });
        log(s, LogLevel::Info, dump.to_string());
    });
}

fn owner_label(owner: &Owner) -> String {
    match owner {
        Owner::Item { run, item } => format!("item:{run}:{item}"),
        Owner::Run(run) => format!("run:{run}"),
        Owner::System => "system".to_owned(),
    }
}

fn phase_label(phase: ItemPhase) -> &'static str {
    match phase {
        ItemPhase::Queued => "queued",
        ItemPhase::Active => "active",
        ItemPhase::Saving => "saving",
        ItemPhase::Done => "done",
    }
}

fn dump_run(rs: &RunState) -> Value {
    let items: serde_json::Map<String, Value> = rs
        .items
        .iter()
        .map(|(item, rec)| (item.to_string(), json!(phase_label(rec.phase))))
        .collect();
    json!({
        "mode": format!("{:?}", rs.mode),
        "interactive": rs.interactive,
        "admission": rs.admission.map(|a| format!("{a:?}")),
        "inst_quota_left": rs.inst_quota_left,
        "halted": rs.halted,
        "group_consent": rs.group_consent,
        "waiters": rs.waiters.len(),
        "finished": rs.finished,
        "items": items,
    })
}

// ----------------------------------------------------------------------------------------- done / finished

/// Move finished item tasks from `State::done` into run records and emit their terminal rows. Also runs the
/// cancellations that helper tasks asked for, and spawns queued items while the table has room.
pub(crate) fn collect_done(ctl: &mut Ctl<'_>, runner: ItemRunner) {
    let requests = ctl
        .shared
        .with(|s| {
            drain_done(s);
            std::mem::take(&mut s.runs.cancel_requests)
        })
        .unwrap_or_default();
    for (run, by) in requests {
        cancel_run(ctl, run, by);
    }
    spawn_queued(ctl, runner);
}

fn drain_done(s: &mut State) {
    for ItemDone {
        run,
        item,
        outcome,
        trace,
    } in std::mem::take(&mut s.done)
    {
        finish_item(s, run, item, outcome, trace);
    }
}

/// Record the outcome of one item: terminal row, claim freed. Items that are already terminal are left alone.
fn finish_item(s: &mut State, run: RunId, item: ItemId, outcome: Outcome, trace: Vec<TraceStep>) {
    let Some(rec) = s
        .runs
        .runs
        .get_mut(&run)
        .and_then(|r| r.items.get_mut(&item))
    else {
        return;
    };
    if rec.phase == ItemPhase::Done {
        return;
    }
    let row = l10n::outcome_row(&outcome);
    rec.phase = ItemPhase::Done;
    rec.outcome = Some(outcome);
    rec.trace = trace;
    rec.sign_in_button = false;
    s.outbox.push(Effect::Progress { run, item, row });
    if s.runs.claimed.get(&item) == Some(&run) {
        s.runs.claimed.remove(&item);
    }
}

/// Spawn the tasks of queued items while the item part of the task table has room.
fn spawn_queued(ctl: &mut Ctl<'_>, runner: ItemRunner) {
    let room = item_capacity(ctl.exec).saturating_sub(ctl.exec.live());
    if room == 0 {
        return;
    }
    let shared = Rc::clone(ctl.shared);
    let picks: Vec<ItemCx> = ctl
        .shared
        .with(|s| pick_queued(s, &shared, room))
        .unwrap_or_default();
    for cx in picks {
        let (owner, run, item) = (cx.owner, cx.run, cx.item);
        // On `ExecFull` the future is dropped here, outside any borrow; it was never polled.
        if ctl.exec.spawn(owner, runner(cx)).is_err() {
            let _ = ctl.shared.with(|s| {
                if let Some(rec) = s
                    .runs
                    .runs
                    .get_mut(&run)
                    .and_then(|r| r.items.get_mut(&item))
                {
                    rec.phase = ItemPhase::Queued;
                }
            });
        }
    }
}

/// Up to `room` item contexts for queued items, oldest run first, in snapshot order; they become `Active`.
fn pick_queued(s: &mut State, shared: &Rc<Shared>, room: usize) -> Vec<ItemCx> {
    let mut picks = Vec::new();
    for (run, rs) in &mut s.runs.runs {
        if rs.finished {
            continue;
        }
        for item in &rs.order {
            if picks.len() >= room {
                return picks;
            }
            let Some(rec) = rs.items.get_mut(item) else {
                continue;
            };
            if rec.phase != ItemPhase::Queued {
                continue;
            }
            rec.phase = ItemPhase::Active;
            picks.push(ItemCx::new(
                Rc::clone(shared),
                *run,
                rs.mode,
                rs.interactive,
                Rc::clone(&rs.cfg),
                Rc::clone(&rec.snap),
            ));
        }
    }
    picks
}

/// Emit `RunFinished` for runs whose items are all terminal.
pub(crate) fn finish_runs(ctl: &mut Ctl<'_>) {
    let live: BTreeSet<Owner> = ctl.exec.owners().into_iter().collect();
    let ready: Vec<RunId> = ctl
        .shared
        .with(|s| {
            note_sign_in_buttons(s);
            fail_lost_items(s, &live);
            s.runs
                .runs
                .iter()
                .filter(|(_, r)| {
                    !r.finished && r.items.values().all(|i| i.phase == ItemPhase::Done)
                })
                .map(|(run, _)| *run)
                .collect()
        })
        .unwrap_or_default();
    for run in ready {
        // Whatever helper is still waiting for the user has nothing left to serve.
        ctl.exec.cancel_where(|owner| *owner == Owner::Run(run));
        let unspawned = ctl
            .shared
            .with(|s| {
                let (mine, rest): (Vec<SpawnReq>, Vec<SpawnReq>) = std::mem::take(&mut s.spawns)
                    .into_iter()
                    .partition(|r| r.owner == Owner::Run(run));
                s.spawns = rest;
                mine
            })
            .unwrap_or_default();
        drop(unspawned); // never polled: dropping them touches no state
        let _ = ctl.shared.with(|s| report_run(s, run));
    }
}

/// Remember which items show a `SignIn` button, from the rows emitted during this step.
fn note_sign_in_buttons(s: &mut State) {
    for effect in &s.outbox {
        if let Effect::Progress { run, item, row } = effect
            && let Some(rec) = s.runs.runs.get_mut(run).and_then(|r| r.items.get_mut(item))
            && rec.phase != ItemPhase::Done
        {
            rec.sign_in_button = row.actions.contains(&RowAction::SignIn);
        }
    }
}

/// Items that are active but own no task and left no outcome behind: record a failure instead of waiting for ever.
fn fail_lost_items(s: &mut State, live: &BTreeSet<Owner>) {
    let reported: BTreeSet<(RunId, ItemId)> = s.done.iter().map(|d| (d.run, d.item)).collect();
    let lost: Vec<(RunId, ItemId)> = s
        .runs
        .runs
        .iter()
        .flat_map(|(run, rs)| {
            rs.items
                .iter()
                .filter(|(item, rec)| {
                    matches!(rec.phase, ItemPhase::Active | ItemPhase::Saving)
                        && !live.contains(&Owner::Item {
                            run: *run,
                            item: **item,
                        })
                        && !reported.contains(&(*run, **item))
                })
                .map(|(item, _)| (*run, *item))
        })
        .collect();
    for (run, item) in lost {
        log(
            s,
            LogLevel::Error,
            format!("item {item} of run {run}: its task ended without an outcome"),
        );
        finish_item(
            s,
            run,
            item,
            Outcome::Failed {
                code: "task_lost".to_owned(),
            },
            Vec::new(),
        );
    }
}

/// Count the outcomes of a finished run the way the report wants them.
fn count(counts: &mut ReportCounts, outcome: &Outcome) {
    let field = match outcome {
        Outcome::Saved { .. } => &mut counts.saved,
        Outcome::Skipped { .. } => &mut counts.skipped,
        Outcome::NotFound { .. } | Outcome::NoIdentifier => &mut counts.not_found,
        Outcome::NeedsUser { .. } | Outcome::NotEntitled { .. } | Outcome::Halted { .. } => {
            &mut counts.needs_user
        }
        Outcome::Blocked { .. } => &mut counts.blocked,
        Outcome::Deferred { .. } => &mut counts.deferred,
        Outcome::Cancelled { .. } => &mut counts.cancelled,
        Outcome::Failed { .. } => &mut counts.failed,
    };
    *field = field.saturating_add(1);
}

/// Build the report, emit `RunFinished` and forget the run.
fn report_run(s: &mut State, run: RunId) {
    let wall = s.clock.wall;
    let Some(rs) = s.runs.runs.remove(&run) else {
        return;
    };
    let mut counts = ReportCounts::default();
    let mut items = Vec::with_capacity(rs.order.len());
    for item in &rs.order {
        let Some(rec) = rs.items.get(item) else {
            continue;
        };
        let outcome = rec.outcome.clone().unwrap_or(Outcome::Failed {
            code: "task_lost".to_owned(),
        });
        count(&mut counts, &outcome);
        items.push(ItemReport {
            item: *item,
            key: rec.snap.key.clone(),
            outcome,
            trace: rec.trace.clone(),
        });
    }
    // A halt that started during the run counts even when no later item asked for the institutional route.
    let route_used = matches!(rs.admission, Some(RunAdmission::Go { inst_allowed: true }));
    let halted = rs.halted.clone().or_else(|| {
        route_used
            .then(|| s.sched.halts.inst_halted(wall).map(|(reason, _)| reason))
            .flatten()
    });
    let report = RunReport {
        run,
        counts,
        items,
        ledger: s.sched.usage(wall),
        halted,
        engine_restarts: rs.engine_restarts,
    };
    s.outbox.push(Effect::RunFinished { run, report });
    s.runs.claimed.retain(|_, owner| *owner != run);
    s.login.on_run_gone(run);
}

// ------------------------------------------------------------------------------------ pipeline-facing API

/// Future of `ItemCx::wait_admitted`.
pub(crate) struct WaitAdmitted {
    cx: ItemCx,
}

impl Future for WaitAdmitted {
    type Output = RunAdmission;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<RunAdmission> {
        let run = self.cx.run;
        let waker = task.waker();
        let polled = self.cx.shared.with(|s| match s.runs.runs.get_mut(&run) {
            // Not part of a run (a stage test drives the item directly): open-access only.
            None => Some(RunAdmission::Go {
                inst_allowed: false,
            }),
            Some(rs) => {
                if rs.admission.is_none() {
                    park(&mut rs.waiters, waker);
                }
                rs.admission
            }
        });
        match polled {
            Some(Some(admission)) => Poll::Ready(admission),
            Some(None) => Poll::Pending,
            None => Poll::Ready(RunAdmission::Cancelled),
        }
    }
}

/// Future of `ItemCx::group_consent`.
pub(crate) struct GroupConsent {
    cx: ItemCx,
}

impl Future for GroupConsent {
    type Output = bool;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<bool> {
        let (run, snap) = (self.cx.run, Rc::clone(&self.cx.snap));
        let shared = Rc::clone(&self.cx.shared);
        let waker = task.waker();
        let polled = self
            .cx
            .shared
            .with(|s| group_answer(s, &shared, run, &snap, waker));
        match polled {
            Some(Some(answer)) => Poll::Ready(answer),
            Some(None) => Poll::Pending,
            None => Poll::Ready(false),
        }
    }
}

/// The group-library answer for a run, starting the prompt the first time it is needed.
fn group_answer(
    s: &mut State,
    shared: &Rc<Shared>,
    run: RunId,
    snap: &ItemSnapshot,
    waker: &Waker,
) -> Option<bool> {
    let Some(rs) = s.runs.runs.get_mut(&run) else {
        return Some(false);
    };
    if let Some(answer) = rs.group_consent {
        return Some(answer);
    }
    if !rs.interactive {
        return Some(false);
    }
    park(&mut rs.waiters, waker);
    if !rs.group_asked {
        rs.group_asked = true;
        let n = rs
            .items
            .values()
            .filter(|r| r.phase != ItemPhase::Done && r.snap.library.kind == LibraryKind::Group)
            .count();
        let cx = Cx::new(Rc::clone(shared), Owner::Run(run));
        let fut: LocalFuture = Box::pin(group_confirm(
            cx,
            u32::try_from(n).unwrap_or(u32::MAX),
            snap.library.name.clone(),
        ));
        s.spawns.push(SpawnReq {
            owner: Owner::Run(run),
            fut,
        });
    }
    None
}

/// Run helper: ask once whether institutional PDFs may go into a group library.
async fn group_confirm(cx: Cx, n: u32, library: String) {
    let answer = cx
        .confirm(ConfirmReq {
            kind: ConfirmKind::GroupLibrary,
            n,
            cap: 0,
            library,
        })
        .await;
    let allow = matches!(answer.map(|ok| ok.choice), Ok(ConfirmChoice::Yes));
    let run = cx.run;
    let _ = cx.shared.with(|s| {
        if let Some(rs) = s.runs.runs.get_mut(&run) {
            rs.group_consent = Some(allow);
            wake_waiters(rs);
        }
    });
}

impl ItemCx {
    /// First thing every item task awaits: resolves when the run is admitted (batch confirm answered) or
    /// cancelled.
    pub(crate) fn wait_admitted(&self) -> WaitAdmitted {
        WaitAdmitted { cx: self.clone() }
    }

    /// Enter the institutional phase? Consumes one unit of the run's batch quota on `Granted`.
    pub(crate) fn claim_inst_slot(&self) -> InstClaim {
        let halted = self.inst_halted();
        let (run, item) = (self.run, self.item);
        let policy = self.cfg.inst.group_libraries;
        let group = self.snap.library.kind == LibraryKind::Group;
        self.shared
            .with(|s| {
                claim(
                    s,
                    run,
                    item,
                    halted.map(|(reason, _)| reason),
                    group.then_some(policy),
                )
            })
            .unwrap_or(InstClaim::NotAllowed)
    }

    /// Group library + `GroupPolicy::Ask`: resolves with the one answer for the run (true = allow).
    pub(crate) fn group_consent(&self) -> GroupConsent {
        GroupConsent { cx: self.clone() }
    }

    /// The item is about to call `save_pdf`: from now on cancellation lets it finish.
    pub(crate) fn mark_saving(&self) {
        let (run, item) = (self.run, self.item);
        let _ = self.shared.with(|s| {
            if let Some(rec) = s
                .runs
                .runs
                .get_mut(&run)
                .and_then(|r| r.items.get_mut(&item))
                && rec.phase == ItemPhase::Active
            {
                rec.phase = ItemPhase::Saving;
            }
        });
    }
}

/// The decision of `claim_inst_slot`, in the documented order. `group` is the group-library policy when the item
/// lives in a group library.
fn claim(
    s: &mut State,
    run: RunId,
    item: ItemId,
    halted: Option<String>,
    group: Option<GroupPolicy>,
) -> InstClaim {
    let Some(rs) = s.runs.runs.get_mut(&run) else {
        return InstClaim::NotAllowed;
    };
    let allowed = rs.mode == Mode::Full
        && rs.cfg.inst.enabled
        && matches!(rs.admission, Some(RunAdmission::Go { inst_allowed: true }));
    if !allowed {
        return InstClaim::NotAllowed;
    }
    if rs.items.get(&item).is_some_and(|rec| rec.inst_blocked) {
        return InstClaim::ResumeBlocked;
    }
    if rs.inst_quota_left == 0 {
        return InstClaim::BatchCap;
    }
    if let Some(reason) = halted {
        rs.halted = Some(reason.clone());
        return InstClaim::Halted { reason };
    }
    let denied = match group {
        None | Some(GroupPolicy::Allow) => false,
        Some(GroupPolicy::Never) => true,
        Some(GroupPolicy::Ask) => !rs.interactive || rs.group_consent == Some(false),
    };
    if denied {
        return InstClaim::GroupDenied;
    }
    rs.inst_quota_left -= 1;
    InstClaim::Granted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::sim::fixtures::{self, ScriptResponder, is_type, item, test_config};
    use crate::sim::{Reply, SimEnd, SimHost};
    use std::cell::{Cell, RefCell};

    // ---- the test runner: behaviour chosen by the item title ---------------------------------------------

    fn saved() -> Outcome {
        Outcome::Saved {
            route: Route::Oa,
            source: "openalex".into(),
            version: Version::Published,
            url: "https://x/y.pdf".into(),
            attachment_key: "1/K".into(),
        }
    }

    /// `title` picks what the item task does:
    /// `ok` finishes at once; `sleep:N` sleeps N ms; `op` waits for a Zotero OA op; `save` marks itself saving
    /// then sleeps 500 ms; `admit` waits for admission; `claim` waits for admission and claims a slot;
    /// `group` waits for the group answer; `silent` returns without an outcome; `button` shows a sign-in row.
    fn test_runner(cx: ItemCx) -> LocalFuture {
        Box::pin(async move {
            let title = cx.snap.title.clone();
            let (verb, arg) = title.split_once(':').unwrap_or((title.as_str(), ""));
            let millis: u64 = arg.parse().unwrap_or(0);
            let outcome = match verb {
                "ok" => saved(),
                "sleep" => {
                    cx.sleep(millis).await;
                    saved()
                }
                "op" => {
                    let _ = cx.zotero_oa("10.1234/x").await;
                    Outcome::NotFound {
                        reason: NotFoundReason::NoOaCopy,
                    }
                }
                "save" => {
                    cx.mark_saving();
                    cx.sleep(500).await;
                    saved()
                }
                "admit" => match cx.wait_admitted().await {
                    RunAdmission::Go { inst_allowed } => Outcome::Failed {
                        code: format!("go:{inst_allowed}"),
                    },
                    RunAdmission::Cancelled => Outcome::Cancelled {
                        by: CancelledBy::User,
                    },
                },
                "claim" => match cx.wait_admitted().await {
                    RunAdmission::Go { .. } => Outcome::Failed {
                        code: format!("{:?}", cx.claim_inst_slot()),
                    },
                    RunAdmission::Cancelled => Outcome::Cancelled {
                        by: CancelledBy::User,
                    },
                },
                "group" => {
                    let _ = cx.wait_admitted().await;
                    Outcome::Failed {
                        code: format!("consent:{}", cx.group_consent().await),
                    }
                }
                "button" => {
                    cx.progress(
                        l10n::row(RowState::Waiting, id::WAITING_LOGIN)
                            .with_action(RowAction::SignIn),
                    );
                    cx.sleep(1_000_000).await;
                    saved()
                }
                "silent" => return,
                _ => Outcome::Failed {
                    code: "bad_test_title".into(),
                },
            };
            cx.finish(outcome);
        })
    }

    fn titled(id: ItemId, title: &str) -> ItemSnapshot {
        let mut it = item(id, "10.1234/x");
        it.title = title.to_owned();
        it
    }

    fn group_item(id: ItemId, title: &str) -> ItemSnapshot {
        let mut it = titled(id, title);
        it.library = LibraryInfo {
            id: 7,
            kind: LibraryKind::Group,
            files_editable: true,
            name: "Lab group".into(),
        };
        it
    }

    fn world(responder: ScriptResponder) -> SimHost {
        host_with(test_config(), responder)
    }

    fn host_with(cfg: EngineConfig, responder: ScriptResponder) -> SimHost {
        let mut host = SimHost::with_engine(Engine::with_runner(test_runner), Box::new(responder));
        host.configure(cfg, Persisted::default(), 7)
            .expect("configure");
        host
    }

    fn start(
        host: &mut SimHost,
        run: RunId,
        mode: Mode,
        interactive: bool,
        items: Vec<ItemSnapshot>,
        resume: Vec<ResumeHint>,
    ) -> StepOut {
        host.step(vec![Command::StartRun {
            run,
            mode,
            interactive,
            items,
            resume,
        }])
        .expect("start run")
    }

    fn quiet() -> ScriptResponder {
        ScriptResponder::new().fallback(Reply::Never)
    }

    fn confirm_with(choice: ConfirmChoice) -> ScriptResponder {
        quiet().on(
            is_type("confirm"),
            Reply::ok(100, json!({ "choice": choice })),
        )
    }

    fn finished(out: &StepOut) -> Vec<&RunReport> {
        out.effects
            .iter()
            .filter_map(|e| match e {
                Effect::RunFinished { report, .. } => Some(report),
                _ => None,
            })
            .collect()
    }

    fn rows(effects: &[Effect], item: ItemId) -> Vec<(RowState, String)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Progress { item: i, row, .. } if *i == item => {
                    Some((row.state, row.l10n.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn outcome_of(report: &RunReport, item: ItemId) -> Outcome {
        report
            .items
            .iter()
            .find(|i| i.item == item)
            .map(|i| i.outcome.clone())
            .unwrap_or(Outcome::Failed {
                code: "missing".into(),
            })
    }

    fn claims(report: &RunReport) -> Vec<(ItemId, String)> {
        report
            .items
            .iter()
            .map(|i| match &i.outcome {
                Outcome::Failed { code } => (i.item, code.clone()),
                other => (i.item, format!("{other:?}")),
            })
            .collect()
    }

    // ---- start, report, order ---------------------------------------------------------------------------

    #[test]
    fn counts_and_report_order_follow_the_snapshot_not_the_completion_order() {
        let mut host = world(quiet());
        // finishing order is 2, 3, 1; item 4 never reports
        let items = vec![
            titled(1, "sleep:3000"),
            titled(2, "ok"),
            titled(3, "sleep:1000"),
            titled(4, "silent"),
        ];
        let out = start(&mut host, 1, Mode::Full, true, items, Vec::new());
        assert_eq!(
            rows(&out.effects, 1),
            vec![(RowState::Queued, id::QUEUED.to_owned())]
        );
        assert!(finished(&out).is_empty());
        host.run_until_idle(60_000).expect("run");
        assert_eq!(host.reports.len(), 1, "exactly one RunFinished");
        let report = &host.reports[0];
        assert_eq!(
            report.items.iter().map(|i| i.item).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!((report.counts.saved, report.counts.failed), (3, 1));
        assert_eq!(
            outcome_of(report, 4),
            Outcome::Failed {
                code: "task_lost".into()
            },
            "a task that forgot to report never hangs the run"
        );
        assert_eq!(report.engine_restarts, 0);
        assert_eq!(report.ledger.hour_cap, 30);
        assert!(
            host.engine
                .shared()
                .with(|s| s.runs.runs.is_empty() && s.runs.claimed.is_empty())
                .unwrap_or(false),
            "the run is forgotten"
        );
        let terminal: Vec<_> = host.effects().filter(|e| matches!(e, Effect::Progress { item: 2, row, .. } if row.state == RowState::Done)).collect();
        assert_eq!(terminal.len(), 1);
    }

    #[test]
    fn an_empty_run_finishes_in_the_step_that_starts_it_and_never_again() {
        let mut host = world(quiet());
        let out = start(&mut host, 5, Mode::OaOnly, true, Vec::new(), Vec::new());
        let reports = finished(&out);
        assert_eq!(reports.len(), 1);
        assert_eq!(
            (
                reports[0].run,
                reports[0].counts.clone(),
                reports[0].items.len()
            ),
            (5, ReportCounts::default(), 0)
        );
        assert_eq!(host.run_until_idle(1_000), Ok(SimEnd::Idle));
        assert_eq!(host.reports.len(), 1);
        // The id can be used again once the run is forgotten.
        assert_eq!(
            finished(&start(
                &mut host,
                5,
                Mode::OaOnly,
                true,
                Vec::new(),
                Vec::new()
            ))
            .len(),
            1
        );
    }

    #[test]
    fn a_live_run_id_cannot_be_started_twice() {
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(1, "sleep:5000")],
            Vec::new(),
        );
        let err = host
            .step(vec![Command::StartRun {
                run: 1,
                mode: Mode::Full,
                interactive: true,
                items: vec![titled(2, "ok")],
                resume: Vec::new(),
            }])
            .expect_err("duplicate run id");
        assert_eq!(err.code, code::BAD_COMMAND);
    }

    #[test]
    fn an_item_listed_twice_in_one_run_runs_once() {
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(1, "ok"), titled(1, "ok"), titled(2, "ok")],
            Vec::new(),
        );
        host.run_until_idle(1_000).expect("run");
        assert_eq!(
            host.reports[0]
                .items
                .iter()
                .map(|i| i.item)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    // ---- F21 duplicates -----------------------------------------------------------------------------------

    #[test]
    fn an_item_that_another_live_run_owns_is_skipped_at_once_and_claimable_afterwards() {
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(5, "sleep:5000")],
            Vec::new(),
        );
        let out = start(
            &mut host,
            2,
            Mode::Full,
            true,
            vec![titled(5, "ok"), titled(6, "sleep:100")],
            Vec::new(),
        );
        assert_eq!(
            rows(&out.effects, 5),
            vec![(RowState::Skipped, id::SKIPPED_IN_PROGRESS.to_owned())],
            "terminal row in the step that notices the duplicate; no Queued row"
        );
        host.run_until_idle(60_000).expect("run");
        assert_eq!(host.reports.len(), 2);
        let second = host.reports.iter().find(|r| r.run == 2).expect("run 2");
        assert_eq!(
            outcome_of(second, 5),
            Outcome::Skipped {
                reason: SkipReason::InProgressElsewhere
            }
        );
        assert_eq!(outcome_of(second, 6), saved());
        let first = host.reports.iter().find(|r| r.run == 1).expect("run 1");
        assert_eq!(outcome_of(first, 5), saved(), "the owner is not disturbed");
        // The claim went away with the owner.
        start(
            &mut host,
            3,
            Mode::Full,
            true,
            vec![titled(5, "ok")],
            Vec::new(),
        );
        host.run_until_idle(1_000).expect("run");
        assert_eq!(outcome_of(host.reports.last().expect("run 3"), 5), saved());
    }

    // ---- F22 cancel -----------------------------------------------------------------------------------------

    #[test]
    fn cancel_run_cancels_every_op_in_the_same_step_and_finishes_the_run() {
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(1, "op"), titled(2, "op"), titled(3, "sleep:60000")],
            Vec::new(),
        );
        host.run_until_idle(1_000).expect("run");
        let ops = host.pending_where(is_type("zotero_oa"));
        assert_eq!(ops.len(), 2);
        let out = host
            .step(vec![Command::CancelRun { run: 1 }])
            .expect("cancel");
        let cancelled: Vec<OpId> = out
            .effects
            .iter()
            .filter_map(|e| {
                if let Effect::Cancel { op } = e {
                    Some(*op)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            cancelled, ops,
            "every in-flight op is cancelled in the step of the command"
        );
        let reports = finished(&out);
        assert_eq!(reports.len(), 1, "and the run is over in that step");
        assert_eq!(reports[0].counts.cancelled, 3);
        assert!(reports[0].items.iter().all(|i| i.outcome
            == Outcome::Cancelled {
                by: CancelledBy::User
            }));
        assert_eq!(
            rows(&out.effects, 1),
            vec![(RowState::Cancelled, id::CANCELLED.to_owned())]
        );
        assert!(host.pending.is_empty());
        assert_eq!(host.run_until_idle(120_000), Ok(SimEnd::Idle));
        assert_eq!(host.reports.len(), 1, "no second report");
        assert!(
            host.engine
                .shared()
                .with(|s| s.runs.claimed.is_empty())
                .unwrap_or(false),
            "claims released"
        );
    }

    #[test]
    fn an_item_that_is_saving_survives_cancel_run_and_the_run_waits_for_it() {
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(1, "save"), titled(2, "sleep:60000")],
            Vec::new(),
        );
        host.run_until_idle(100).expect("run");
        let out = host
            .step(vec![Command::CancelRun { run: 1 }])
            .expect("cancel");
        assert!(
            finished(&out).is_empty(),
            "the saving item is still running"
        );
        assert_eq!(
            rows(&out.effects, 2),
            vec![(RowState::Cancelled, id::CANCELLED.to_owned())]
        );
        assert!(
            rows(&out.effects, 1).is_empty(),
            "no terminal row for the saving item yet"
        );
        host.run_until_idle(60_000).expect("run");
        let report = &host.reports[0];
        assert_eq!(outcome_of(report, 1), saved());
        assert_eq!(
            outcome_of(report, 2),
            Outcome::Cancelled {
                by: CancelledBy::User
            }
        );
        assert_eq!((report.counts.saved, report.counts.cancelled), (1, 1));
    }

    #[test]
    fn cancel_item_touches_only_that_item_and_ignores_unknown_ones() {
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(1, "op"), titled(2, "sleep:1000")],
            Vec::new(),
        );
        host.run_until_idle(100).expect("run");
        let ops = host.pending_where(is_type("zotero_oa"));
        let out = host
            .step(vec![Command::CancelItem { run: 1, item: 1 }])
            .expect("cancel item");
        assert_eq!(
            out.effects
                .iter()
                .filter(|e| matches!(e, Effect::Cancel { op } if *op == ops[0]))
                .count(),
            1
        );
        assert!(finished(&out).is_empty());
        // Unknown run, unknown item, already finished item: logged, never an error.
        let out = host
            .step(vec![
                Command::CancelItem { run: 1, item: 99 },
                Command::CancelItem { run: 1, item: 1 },
                Command::CancelRun { run: 42 },
            ])
            .expect("ignored");
        assert_eq!(out.effects.iter().filter(|e| matches!(e, Effect::Log { level: LogLevel::Info, msg } if msg.contains("cancel ignored"))).count(), 3);
        host.run_until_idle(10_000).expect("run");
        let report = &host.reports[0];
        assert_eq!(
            outcome_of(report, 1),
            Outcome::Cancelled {
                by: CancelledBy::User
            }
        );
        assert_eq!(outcome_of(report, 2), saved());
    }

    // ---- F17 shutdown ---------------------------------------------------------------------------------------

    #[test]
    fn shutdown_cancels_everything_flushes_the_persisted_state_and_finishes_the_runs() {
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(1, "op"), titled(2, "sleep:60000")],
            Vec::new(),
        );
        start(
            &mut host,
            2,
            Mode::OaOnly,
            true,
            vec![titled(3, "op")],
            Vec::new(),
        );
        host.run_until_idle(100).expect("run");
        let out = host.step(vec![Command::Shutdown]).expect("shutdown");
        assert_eq!(
            out.effects
                .iter()
                .filter(|e| matches!(e, Effect::Cancel { .. }))
                .count(),
            2
        );
        let keys: Vec<PersistKey> = out
            .effects
            .iter()
            .filter_map(|e| match e {
                Effect::Persist {
                    key, flush: true, ..
                } => Some(*key),
                _ => None,
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                PersistKey::Ledger,
                PersistKey::Halts,
                PersistKey::Ezproxy,
                PersistKey::Alma
            ]
        );
        let reports = finished(&out);
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().flat_map(|r| &r.items).all(|i| i.outcome
            == Outcome::Cancelled {
                by: CancelledBy::Shutdown
            }));
        let last_cancel = out
            .effects
            .iter()
            .rposition(|e| matches!(e, Effect::Cancel { .. }));
        let first_persist = out
            .effects
            .iter()
            .position(|e| matches!(e, Effect::Persist { .. }));
        assert!(
            last_cancel < first_persist,
            "the ledger is flushed after the cancelled work settled"
        );
        assert_eq!(host.run_until_idle(10_000), Ok(SimEnd::Idle));
    }

    // ---- table capacity -------------------------------------------------------------------------------------

    #[test]
    fn items_beyond_the_task_table_wait_and_start_as_slots_free_up() {
        let mut host = world(quiet());
        *host.engine.exec_mut() = Executor::new(u32::try_from(HELPER_HEADROOM).unwrap_or(8) + 2);
        let items: Vec<ItemSnapshot> = (1..=5).map(|i| titled(i, "sleep:1000")).collect();
        start(&mut host, 1, Mode::Full, true, items, Vec::new());
        let phases = |host: &SimHost| {
            host.engine
                .shared()
                .with(|s| {
                    s.runs
                        .runs
                        .get(&1)
                        .map(|r| {
                            r.order
                                .iter()
                                .filter_map(|i| r.items.get(i))
                                .map(|rec| rec.phase)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            phases(&host)
                .iter()
                .filter(|p| **p == ItemPhase::Active)
                .count(),
            2,
            "two item slots"
        );
        assert_eq!(
            phases(&host)
                .iter()
                .filter(|p| **p == ItemPhase::Queued)
                .count(),
            3
        );
        let mut peak = 0;
        while host
            .engine
            .shared()
            .with(|s| s.runs.runs.contains_key(&1))
            .unwrap_or(false)
        {
            peak = peak.max(host.engine.exec_mut().live());
            host.advance(500);
            host.step(Vec::new()).expect("step");
        }
        assert!(peak <= 2, "never more tasks than item slots: {peak}");
        let report = &host.reports[0];
        assert_eq!(report.counts.saved, 5);
        assert!(
            host.now >= 1_000 + 3_000,
            "three waves of 1 s: {}",
            host.now
        );
    }

    #[test]
    fn cancelling_a_run_also_cancels_items_that_never_got_a_task() {
        let mut host = world(quiet());
        *host.engine.exec_mut() = Executor::new(u32::try_from(HELPER_HEADROOM).unwrap_or(8) + 1);
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            (1..=3).map(|i| titled(i, "sleep:60000")).collect(),
            Vec::new(),
        );
        let out = host
            .step(vec![Command::CancelRun { run: 1 }])
            .expect("cancel");
        let reports = finished(&out);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].counts.cancelled, 3);
    }

    // ---- confirm flows --------------------------------------------------------------------------------------

    fn big_batch(n: u32, title: &str) -> Vec<ItemSnapshot> {
        (1..=n).map(|i| titled(i, title)).collect()
    }

    #[test]
    fn a_large_interactive_batch_asks_once_and_holds_every_item_until_the_answer() {
        let mut host = world(confirm_with(ConfirmChoice::Yes));
        let out = start(
            &mut host,
            1,
            Mode::Full,
            true,
            big_batch(12, "claim"),
            Vec::new(),
        );
        host.run_until_idle(50).expect("run");
        let confirms: Vec<_> = host
            .effects()
            .filter_map(|e| {
                if let Effect::Confirm { req, run, .. } = e {
                    Some((req.clone(), *run))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            confirms,
            vec![(
                ConfirmReq {
                    kind: ConfirmKind::LargeBatch,
                    n: 12,
                    cap: 20,
                    library: String::new()
                },
                1
            )]
        );
        assert_eq!(
            rows(&out.effects, 3),
            vec![(RowState::Waiting, id::WAITING_CONFIRM.to_owned())]
        );
        assert!(
            host.effects()
                .all(|e| !matches!(e, Effect::Http { .. } | Effect::ZoteroOa { .. })),
            "nothing started before the answer"
        );
        assert!(host.reports.is_empty());
        host.run_until_idle(10_000).expect("run");
        let report = &host.reports[0];
        assert!(
            report.items.iter().all(|i| i.outcome
                == Outcome::Failed {
                    code: "Granted".into()
                }),
            "{:?}",
            claims(report)
        );
        assert_eq!(
            host.effects()
                .filter(|e| matches!(e, Effect::Confirm { .. }))
                .count(),
            1
        );
        assert_eq!(
            rows(
                &host
                    .log
                    .iter()
                    .map(|l| l.effect.clone())
                    .collect::<Vec<_>>(),
                3
            )
            .first(),
            Some(&(RowState::Waiting, id::WAITING_CONFIRM.to_owned()))
        );
    }

    #[test]
    fn yes_gives_the_first_cap_items_the_institutional_route_and_defers_the_rest() {
        let mut cfg = test_config();
        cfg.inst.batch_max = 5;
        let mut host = host_with(cfg, confirm_with(ConfirmChoice::Yes));
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            big_batch(12, "claim"),
            Vec::new(),
        );
        let confirm = host.log.iter().find_map(|l| {
            if let Effect::Confirm { req, .. } = &l.effect {
                Some(req.clone())
            } else {
                None
            }
        });
        host.run_until_idle(10_000).expect("run");
        assert_eq!(confirm.map(|r| (r.n, r.cap)), Some((12, 5)));
        let granted = host.reports[0]
            .items
            .iter()
            .filter(|i| {
                i.outcome
                    == Outcome::Failed {
                        code: "Granted".into(),
                    }
            })
            .count();
        let capped = host.reports[0]
            .items
            .iter()
            .filter(|i| {
                i.outcome
                    == Outcome::Failed {
                        code: "BatchCap".into(),
                    }
            })
            .count();
        assert_eq!((granted, capped), (5, 7));
    }

    #[test]
    fn oa_only_answer_turns_the_institutional_route_off_for_the_run() {
        let mut host = world(confirm_with(ConfirmChoice::OaOnly));
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            big_batch(12, "claim"),
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        assert!(host.reports[0].items.iter().all(|i| i.outcome
            == Outcome::Failed {
                code: "NotAllowed".into()
            }));
    }

    #[test]
    fn cancel_answer_cancels_the_run() {
        let mut host = world(confirm_with(ConfirmChoice::Cancel));
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            big_batch(12, "admit"),
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        assert_eq!(host.reports.len(), 1);
        assert_eq!(host.reports[0].counts.cancelled, 12);
        assert!(host.reports[0].items.iter().all(|i| i.outcome
            == Outcome::Cancelled {
                by: CancelledBy::User
            }));
        assert!(host.pending.is_empty());
    }

    #[test]
    fn a_failed_prompt_counts_as_cancel() {
        let mut host = world(quiet().on(is_type("confirm"), Reply::err(10, OpErrKind::Internal)));
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            big_batch(12, "admit"),
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        assert_eq!(host.reports[0].counts.cancelled, 12);
    }

    #[test]
    fn cancelling_while_the_prompt_is_open_cancels_the_prompt_too() {
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            big_batch(12, "admit"),
            Vec::new(),
        );
        host.run_until_idle(10).expect("run");
        let prompt = host.pending_where(is_type("confirm"));
        assert_eq!(prompt.len(), 1);
        let out = host
            .step(vec![Command::CancelRun { run: 1 }])
            .expect("cancel");
        assert!(
            out.effects
                .iter()
                .any(|e| matches!(e, Effect::Cancel { op } if *op == prompt[0])),
            "the open dialog is closed"
        );
        assert_eq!(finished(&out).len(), 1);
    }

    #[test]
    fn no_prompt_for_small_batches_oa_only_runs_or_a_disabled_route() {
        // exactly confirm_above items: no prompt
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            big_batch(10, "claim"),
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        assert_eq!(
            host.effects()
                .filter(|e| matches!(e, Effect::Confirm { .. }))
                .count(),
            0
        );
        assert!(host.reports[0].items.iter().all(|i| i.outcome
            == Outcome::Failed {
                code: "Granted".into()
            }));
        // OA-only run of many items
        start(
            &mut host,
            2,
            Mode::OaOnly,
            true,
            (101..=120).map(|i| titled(i, "claim")).collect(),
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        assert_eq!(
            host.effects()
                .filter(|e| matches!(e, Effect::Confirm { .. }))
                .count(),
            0
        );
        assert!(host.reports[1].items.iter().all(|i| i.outcome
            == Outcome::Failed {
                code: "NotAllowed".into()
            }));
        // route disabled in prefs
        let mut cfg = test_config();
        cfg.inst.enabled = false;
        let mut host = host_with(cfg, quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            big_batch(15, "claim"),
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        assert_eq!(
            host.effects()
                .filter(|e| matches!(e, Effect::Confirm { .. }))
                .count(),
            0
        );
        assert!(host.reports[0].items.iter().all(|i| i.outcome
            == Outcome::Failed {
                code: "NotAllowed".into()
            }));
    }

    #[test]
    fn items_that_cannot_use_the_route_do_not_count_towards_the_prompt() {
        let mut host = world(quiet());
        let mut items: Vec<ItemSnapshot> = (1..=8).map(|i| titled(i, "ok")).collect();
        items.extend((9..=20).map(|i| {
            let mut it = titled(i, "ok");
            it.pdf_attachments = 1; // will be skipped by the real pipeline
            it
        }));
        start(&mut host, 1, Mode::Full, true, items, Vec::new());
        assert_eq!(
            host.effects()
                .filter(|e| matches!(e, Effect::Confirm { .. }))
                .count(),
            0,
            "8 eligible items are below the threshold"
        );
    }

    #[test]
    fn a_non_interactive_large_batch_never_prompts_and_is_capped() {
        let mut host = world(quiet());
        let out = start(
            &mut host,
            1,
            Mode::Full,
            false,
            big_batch(30, "claim"),
            Vec::new(),
        );
        assert_eq!(
            rows(&out.effects, 1).first(),
            Some(&(RowState::Queued, id::QUEUED.to_owned()))
        );
        host.run_until_idle(10_000).expect("run");
        assert_eq!(
            host.effects()
                .filter(|e| matches!(e, Effect::Confirm { .. }))
                .count(),
            0
        );
        let granted = host.reports[0]
            .items
            .iter()
            .filter(|i| {
                i.outcome
                    == Outcome::Failed {
                        code: "Granted".into(),
                    }
            })
            .count();
        let capped = host.reports[0]
            .items
            .iter()
            .filter(|i| {
                i.outcome
                    == Outcome::Failed {
                        code: "BatchCap".into(),
                    }
            })
            .count();
        assert_eq!(
            (granted, capped),
            (20, 10),
            "the first 20 asked get the route"
        );
    }

    // ---- group libraries --------------------------------------------------------------------------------------

    #[test]
    fn the_group_prompt_is_asked_once_for_all_items_and_cached() {
        let mut host = world(confirm_with(ConfirmChoice::Yes));
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![
                group_item(1, "group"),
                group_item(2, "group"),
                group_item(3, "group"),
            ],
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        let prompts: Vec<ConfirmReq> = host
            .effects()
            .filter_map(|e| {
                if let Effect::Confirm { req, .. } = e {
                    Some(req.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            prompts,
            vec![ConfirmReq {
                kind: ConfirmKind::GroupLibrary,
                n: 3,
                cap: 0,
                library: "Lab group".into()
            }]
        );
        assert!(host.reports[0].items.iter().all(|i| i.outcome
            == Outcome::Failed {
                code: "consent:true".into()
            }));
        assert!(host.pending.is_empty());
    }

    #[test]
    fn a_declined_group_prompt_denies_this_item_and_every_later_claim() {
        let mut host = world(confirm_with(ConfirmChoice::OaOnly));
        let mut late = group_item(2, "claim");
        late.title = "claim".into();
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![group_item(1, "group"), late],
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        let report = &host.reports[0];
        assert_eq!(
            outcome_of(report, 1),
            Outcome::Failed {
                code: "consent:false".into()
            }
        );
        // The second item claims either before or after the answer; after it, the claim is denied.
        let second = outcome_of(report, 2);
        assert!(
            second
                == Outcome::Failed {
                    code: "GroupDenied".into()
                }
                || second
                    == Outcome::Failed {
                        code: "Granted".into()
                    },
            "{second:?}"
        );
        assert_eq!(
            host.effects()
                .filter(|e| matches!(e, Effect::Confirm { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn group_policy_never_and_non_interactive_ask_deny_without_asking() {
        let mut cfg = test_config();
        cfg.inst.group_libraries = GroupPolicy::Never;
        let mut host = host_with(cfg, quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![group_item(1, "claim"), titled(2, "claim")],
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        assert_eq!(
            outcome_of(&host.reports[0], 1),
            Outcome::Failed {
                code: "GroupDenied".into()
            }
        );
        assert_eq!(
            outcome_of(&host.reports[0], 2),
            Outcome::Failed {
                code: "Granted".into()
            },
            "user libraries are unaffected"
        );

        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            false,
            vec![group_item(1, "claim"), group_item(2, "group")],
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        assert_eq!(
            outcome_of(&host.reports[0], 1),
            Outcome::Failed {
                code: "GroupDenied".into()
            }
        );
        assert_eq!(
            outcome_of(&host.reports[0], 2),
            Outcome::Failed {
                code: "consent:false".into()
            }
        );
        assert_eq!(
            host.effects()
                .filter(|e| matches!(e, Effect::Confirm { .. }))
                .count(),
            0
        );

        let mut cfg = test_config();
        cfg.inst.group_libraries = GroupPolicy::Allow;
        let mut host = host_with(cfg, quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            false,
            vec![group_item(1, "claim")],
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        assert_eq!(
            outcome_of(&host.reports[0], 1),
            Outcome::Failed {
                code: "Granted".into()
            }
        );
    }

    #[test]
    fn a_group_prompt_nobody_needs_any_more_is_closed_with_the_run() {
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![group_item(1, "group")],
            Vec::new(),
        );
        host.run_until_idle(100).expect("run");
        let prompt = host.pending_where(is_type("confirm"));
        assert_eq!(prompt.len(), 1);
        let out = host
            .step(vec![Command::CancelItem { run: 1, item: 1 }])
            .expect("cancel item");
        assert!(
            out.effects
                .iter()
                .any(|e| matches!(e, Effect::Cancel { op } if *op == prompt[0])),
            "the last item is gone: the dialog is closed"
        );
        assert_eq!(finished(&out).len(), 1);
        assert!(host.pending.is_empty());
    }

    // ---- resume hints, halts ---------------------------------------------------------------------------------

    #[test]
    fn resume_hints_block_the_route_for_those_items_and_count_one_restart() {
        let mut host = world(quiet());
        let resume = vec![
            ResumeHint {
                item: 2,
                inst_blocked: true,
            },
            ResumeHint {
                item: 3,
                inst_blocked: false,
            },
        ];
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(1, "claim"), titled(2, "claim"), titled(3, "claim")],
            resume,
        );
        host.run_until_idle(10_000).expect("run");
        let report = &host.reports[0];
        assert_eq!(
            claims(report),
            vec![
                (1, "Granted".into()),
                (2, "ResumeBlocked".into()),
                (3, "Granted".into())
            ]
        );
        assert_eq!(report.engine_restarts, 1);
    }

    #[test]
    fn a_halted_route_refuses_the_claim_and_the_report_names_the_reason() {
        let mut host = world(quiet());
        host.engine.shared().with(|s| {
            s.sched.halts.halt_inst(
                crate::sched::hosts::HaltReason::RateLimited,
                None,
                s.clock.wall,
            )
        });
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(1, "sleep:100"), titled(2, "claim")],
            Vec::new(),
        );
        host.run_until_idle(10_000).expect("run");
        let report = &host.reports[0];
        assert_eq!(
            outcome_of(report, 2),
            Outcome::Failed {
                code: "Halted { reason: \"rate_limited\" }".into()
            }
        );
        assert_eq!(report.halted.as_deref(), Some("rate_limited"));
    }

    #[test]
    fn a_run_that_never_used_the_route_does_not_report_a_halt() {
        let mut host = world(quiet());
        host.engine.shared().with(|s| {
            s.sched.halts.halt_inst(
                crate::sched::hosts::HaltReason::RateLimited,
                None,
                s.clock.wall,
            )
        });
        start(
            &mut host,
            1,
            Mode::OaOnly,
            true,
            vec![titled(1, "ok")],
            Vec::new(),
        );
        host.run_until_idle(1_000).expect("run");
        assert_eq!(host.reports[0].halted, None);
        start(
            &mut host,
            2,
            Mode::Full,
            true,
            vec![titled(2, "ok")],
            Vec::new(),
        );
        host.run_until_idle(1_000).expect("run");
        assert_eq!(
            host.reports[1].halted.as_deref(),
            Some("rate_limited"),
            "a Full run ends with the route halted: say so"
        );
    }

    // ---- user actions ----------------------------------------------------------------------------------------

    #[test]
    fn skip_and_retry_sign_in_take_the_button_off_the_waiting_rows() {
        let mut host = world(quiet());
        let out = start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(1, "button"), titled(2, "sleep:5000")],
            Vec::new(),
        );
        assert_eq!(out.effects.iter().filter(|e| matches!(e, Effect::Progress { row, .. } if row.actions.contains(&RowAction::SignIn))).count(), 1);
        for kind in [UserActionKind::KeepWaiting, UserActionKind::SignInNow] {
            let out = host
                .step(vec![Command::UserAction { run: 1, kind }])
                .expect("action");
            assert!(
                rows(&out.effects, 1).is_empty(),
                "{kind:?} leaves the row alone"
            );
        }
        let out = host
            .step(vec![Command::UserAction {
                run: 1,
                kind: UserActionKind::SkipSignIn,
            }])
            .expect("skip");
        let after: Vec<&Effect> = out
            .effects
            .iter()
            .filter(|e| matches!(e, Effect::Progress { item: 1, .. }))
            .collect();
        assert_eq!(after.len(), 1);
        assert!(
            matches!(after[0], Effect::Progress { row, .. } if row.l10n == id::WAITING_LOGIN && row.actions.is_empty() && row.route == Some(Route::Inst))
        );
        assert!(
            rows(&out.effects, 2).is_empty(),
            "other items keep their rows"
        );
        // once is enough
        let out = host
            .step(vec![Command::UserAction {
                run: 1,
                kind: UserActionKind::RetrySignIn,
            }])
            .expect("retry");
        assert!(rows(&out.effects, 1).is_empty());
    }

    // ---- dump -------------------------------------------------------------------------------------------------

    #[test]
    fn dump_state_is_one_deterministic_log_with_the_whole_picture() {
        let dump = || {
            let mut host = world(quiet());
            start(
                &mut host,
                1,
                Mode::Full,
                true,
                vec![titled(1, "op"), titled(2, "sleep:5000")],
                Vec::new(),
            );
            host.run_until_idle(100).expect("run");
            let out = host.step(vec![Command::DumpState]).expect("dump");
            let logs: Vec<String> = out
                .effects
                .iter()
                .filter_map(|e| {
                    if let Effect::Log {
                        level: LogLevel::Info,
                        msg,
                    } = e
                    {
                        Some(msg.clone())
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(logs.len(), 1);
            logs.into_iter().next().unwrap_or_default()
        };
        let (a, b) = (dump(), dump());
        assert_eq!(a, b);
        let v: Value = serde_json::from_str(&a).expect("json");
        assert_eq!(v["tasks"]["live"], 2);
        assert_eq!(v["tasks"]["owners"], json!(["item:1:1", "item:1:2"]));
        assert_eq!(v["ops"][0]["kind"], "ZoteroOa");
        assert_eq!(v["ops"].as_array().map(Vec::len), Some(1));
        assert_eq!(v["timers"]["count"], 1);
        assert_eq!(
            v["runs"]["1"]["items"],
            json!({ "1": "active", "2": "active" })
        );
        assert_eq!(v["runs"]["1"]["admission"], "Go { inst_allowed: true }");
        assert_eq!(v["claimed"], json!({ "1": 1, "2": 1 }));
        assert!(v["sched"]["gates"].is_array() && v["login"]["realms"].is_object());
        assert!(
            a.find("\"claimed\"") < a.find("\"clock\"")
                || a.find("\"clock\"") < a.find("\"claimed\""),
            "stable text"
        );
    }

    // ---- items finishing on their own ---------------------------------------------------------------------------

    #[test]
    fn an_item_that_finishes_after_its_run_was_cancelled_cannot_resurrect_it() {
        let mut host = world(quiet());
        start(
            &mut host,
            1,
            Mode::Full,
            true,
            vec![titled(1, "save")],
            Vec::new(),
        );
        host.run_until_idle(100).expect("run");
        host.step(vec![Command::CancelRun { run: 1 }])
            .expect("cancel");
        host.run_until_idle(10_000).expect("run");
        assert_eq!(
            host.reports.len(),
            1,
            "one report, with the real outcome of the save"
        );
        assert_eq!(outcome_of(&host.reports[0], 1), saved());
    }

    #[test]
    fn ledger_usage_and_trace_travel_in_the_report() {
        fn tracing_runner(cx: ItemCx) -> LocalFuture {
            Box::pin(async move {
                cx.trace_step("openalex", "api", "found", 12);
                cx.finish(saved());
            })
        }
        let mut host = SimHost::with_engine(Engine::with_runner(tracing_runner), Box::new(quiet()));
        host.configure(test_config(), Persisted::default(), 1)
            .expect("configure");
        start(
            &mut host,
            1,
            Mode::OaOnly,
            true,
            vec![titled(1, "x")],
            Vec::new(),
        );
        host.run_until_idle(100).expect("run");
        let report = &host.reports[0];
        assert_eq!(
            report.items[0].trace,
            vec![TraceStep {
                src: "openalex".into(),
                tier: "api".into(),
                class: "found".into(),
                ms: 12
            }]
        );
        assert_eq!(report.items[0].key, "1/ITEM0001");
        assert_eq!(
            (
                report.ledger.hour,
                report.ledger.day_cap,
                report.ledger.mb_hour_cap
            ),
            (0, 80, 300)
        );
    }

    #[test]
    fn wait_admitted_without_a_run_record_means_open_access_only() {
        let mut host = world(quiet());
        let seen: Rc<RefCell<Option<RunAdmission>>> = Rc::default();
        let slot = Rc::clone(&seen);
        let shared = Rc::clone(host.engine.shared());
        let cx = ItemCx::new(
            shared,
            9,
            Mode::Full,
            true,
            Rc::new(test_config()),
            Rc::new(fixtures::item(1, "10.1234/x")),
        );
        host.engine
            .spawn_test(Owner::Item { run: 9, item: 1 }, move |_| {
                Box::pin(async move { *slot.borrow_mut() = Some(cx.wait_admitted().await) })
            })
            .expect("spawn");
        host.run_until_idle(10).expect("run");
        assert_eq!(
            *seen.borrow(),
            Some(RunAdmission::Go {
                inst_allowed: false
            })
        );
        let claim_seen = Rc::new(Cell::new(false));
        let flag = Rc::clone(&claim_seen);
        let cx = ItemCx::new(
            Rc::clone(host.engine.shared()),
            9,
            Mode::Full,
            true,
            Rc::new(test_config()),
            Rc::new(fixtures::item(1, "10.1234/x")),
        );
        flag.set(cx.claim_inst_slot() == InstClaim::NotAllowed);
        assert!(claim_seen.get());
    }
}
