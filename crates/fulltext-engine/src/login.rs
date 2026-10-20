//! Single-flight institutional session gate with epochs, and the one-at-a-time interactive window gate.
//!
//! # Why
//! Twenty items hitting the OpenAthens redirector must produce ONE sign-in window, not twenty, and no item may
//! re-prompt after a successful sign-in. All waiting items are served by one `LoginWindow` effect.
//!
//! # State machine (per `Realm`; sessions are engine-wide because the cookie jar is)
//! ```text
//! SignedOut --session_admit (first caller)--> Probing{since}          the first item probes the redirector alone
//! Probing   --other callers--> wait                                    (no parallel hidden loads that all bounce)
//! Probing   --session_report(LoggedIn)--> SignedIn{epoch, verified:true}   wake all
//! Probing   --session_report(NeedsLogin)--> (stays) the reporter calls session_login
//! SignedOut|Probing|SignedIn --session_login--> LoginOpen{window op, waiting}   spawns ONE driver task (Owner::System)
//! LoginOpen --more session_login callers--> waiting += 1, Effect::LoginUpdate { op, waiting }
//! LoginOpen --window Landed|Pdf--> SignedIn{epoch += 1, verified:false}    wake all, verdict Retry{epoch}
//! SignedIn{verified:false} --first resumed attempt ok--> verified:true
//! SignedIn{verified:false} --first resumed attempt bounces (session_report NeedsLogin)--> Halted(Ineffective)
//! SignedIn{verified:true}  --bounce--> Expired: back to LoginOpen, at most MAX_PROMPTS_PER_REALM prompts/run
//! LoginOpen --ClosedByUser|Handoff--> Halted(Declined) for the RUN that closed it; --Timeout--> Halted(TimedOut)
//! LoginOpen --CaBlocked / ca_error--> Refused{code}                    all waiters, all runs until a new sign-in
//! any --OpenathensBadOrg--> Halted(Misconfigured); --EzproxyError{suspend}--> Halted(Suspended) (+ sched halt)
//! Halted/Refused --UserAction RetrySignIn--> SignedOut for that run
//! ```
//! * Epoch: bumps on each successful sign-in. An item that failed under epoch `e` and sees epoch `> e` retries
//!   silently once (`SessionVerdict::Retry`) instead of prompting.
//! * Declined / TimedOut are tagged with the run; a declined run never reopens a window, a NEW run may prompt.
//! * Window driver (Owner::System task, so cancelling a run never closes a window other runs wait on): waits for
//!   the interactive gate, emits `LoginWindow` (first waiter's run/item/title/entry URL, `waiting` count), races
//!   it against the engine-side timeout (`InstPrefs.login_timeout_sec`; `UserAction::KeepWaiting` re-arms it),
//!   closes the window (drops the op -> `Cancel`) when `waiting` falls to 0 (waiters cancelled), and resolves all
//!   waiters. `autoOpenLogin = false`: the driver first waits for `UserAction::SignInNow`.
//! * Interactive gate: capacity 1; queue priority login > challenge, FIFO within a class.
//! * `Effect::LoginUpdate { op, waiting }`: `op` is a FRESH id from `Reactor::alloc_id`, NOT the window's op
//!   (JS completes a `LoginUpdate` immediately with `ok: {}`; reusing the window's id would complete the window).
//!   The completion lands in `StepOut.ignored`; JS finds the one open window itself.
//! * Row feedback: waiting items show `l10n::id::WAITING_LOGIN`; the item rows get `RowAction::SignIn` when
//!   `autoOpenLogin` is off.
//!
//! # How the implementation works (read before changing it)
//! * Everything is plain data in [`LoginGate`] (inside `State`); the futures below poll it under one
//!   `Shared::with` and register their waker in `RealmState::waiters`. Every state change calls
//!   `RealmState::wake_all`, and every waiter re-derives its answer from the state (no per-waiter result slots):
//!   a waiter whose `seen_epoch` is below the epoch gets `Retry`, a waiter whose run is in `blocked_runs` gets the
//!   run's verdict, `Refused` / `Halted` states answer everybody.
//! * `parties` records who is parked (`session_admit` callers and `session_login` callers, with their run). The
//!   login parties are the `waiting` count shown to the user; when the window ends, their runs (and the runs of
//!   parked admit callers) are the ones tagged `Declined` / `TimedOut`, so a parked item of the same run cannot
//!   re-prompt afterwards.
//! * Dropping a future (item cancelled) removes its party; the driver is woken, notices `waiting == 0` and drops
//!   the window op (`Cancel`), so a window never outlives its last waiter. A window that was never shown refunds the
//!   opener's prompt.
//! * Probing: the prober is `(run, item)` of the `session_admit` caller that moved `SignedOut -> Probing`. If it
//!   never reports (item cancelled, load hung) another caller takes the probe over after `PROBE_STALE_MS`.
//! * The prompt cap (`MAX_PROMPTS_PER_REALM`) limits OPENING windows per run; joining a window that is already open
//!   is always allowed.
//! * Shutdown: `on_shutdown` sets `LoginGate::shutdown`; every future then resolves with the `Cancelled`
//!   verdict / a cancelled op error.
use crate::cx::{Ctl, Cx, Owner, SpawnReq, State};
use crate::l10n;
use crate::ops::{LoginResp, OpKind, OpResult, TimerKey};
use crate::proto::*;
use crate::sched::acquire::arm_timer;
use crate::sched::hosts::HaltReason;
use crate::sched::policy;
use fulltext_core::model::*;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Why a realm refuses to prompt (for a run or globally).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HaltWhy {
    Declined,
    TimedOut,
    Ineffective,
    Misconfigured,
    Suspended,
}

/// State of one realm's session. See the module diagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionState {
    SignedOut,
    Probing {
        since: u64,
    },
    LoginOpen {
        window: Option<OpId>,
        opened_at: u64,
        waiting: u32,
    },
    SignedIn {
        verified: bool,
    },
    Refused {
        code: Option<String>,
    },
    Halted {
        why: HaltWhy,
    },
}

/// What a caller learns from `session_admit` / `session_login`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionVerdict {
    /// Someone signed in since the caller's attempt: retry the hidden load (once) under this epoch.
    Retry {
        epoch: u32,
    },
    Declined,
    TimedOut,
    Refused {
        code: Option<String>,
    },
    /// Signed in, but the first resumed attempt bounced to the IdP: stop prompting this run.
    Ineffective,
    Misconfigured,
    Suspended,
    /// The caller's item was cancelled (never observed by the caller itself; used for waiter bookkeeping).
    Cancelled,
}

/// Result of `session_admit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Go ahead and load the redirector; report the result with `session_report` under this epoch.
    Proceed { epoch: u32 },
    /// Do not load; hand off with this verdict.
    Blocked(SessionVerdict),
}

/// What an item saw when it used the session (`session_report`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeResult {
    /// Reached the publisher: the session works.
    LoggedIn,
    /// Landed on the IdP / EZproxy login: sign-in needed.
    NeedsLogin,
    /// `OpenathensBadOrg` / EZproxy "not configured for this realm".
    Misconfigured,
    /// EZproxy suspend page.
    Suspended,
    /// Network or unrelated failure: leave the state alone, release a probe lock if held.
    Failed,
}

/// Everything the window needs to open for the first waiter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoginRequest {
    pub entry_url: String,
    pub item_title: String,
    /// The epoch the caller's failed attempt ran under.
    pub seen_epoch: u32,
    pub capture_pdf: bool,
}

/// A caller parked on a realm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PartyKind {
    /// Inside `session_admit`, waiting for a probe or a window.
    Admit,
    /// Inside `session_login`: counted in the window's `waiting`.
    Login,
}

#[derive(Clone, Copy, Debug)]
struct Party {
    run: RunId,
    kind: PartyKind,
}

/// Bookkeeping of the window driver task of a realm.
#[derive(Clone, Debug)]
struct DriverInfo {
    /// The run whose prompt counter was charged for this window.
    opener_run: RunId,
    /// The `LoginWindow` effect has been emitted.
    shown: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct RealmState {
    pub(crate) state: SessionState,
    /// Bumps on every successful sign-in.
    pub(crate) epoch: u32,
    /// Prompts shown per run (cap `MAX_PROMPTS_PER_REALM`).
    pub(crate) prompts: BTreeMap<RunId, u32>,
    /// Runs that declined / timed out / saw an ineffective sign-in.
    pub(crate) blocked_runs: BTreeMap<RunId, HaltWhy>,
    /// Wakers of callers waiting for a state change.
    pub(crate) waiters: Vec<Waker>,
    /// Parked callers by id.
    parties: BTreeMap<u64, Party>,
    next_party: u64,
    /// The `(run, item)` that moved the realm to `Probing`.
    prober: Option<(RunId, ItemId)>,
    /// Present while a window driver task exists.
    driver: Option<DriverInfo>,
    /// `UserAction::SignInNow` was received (only matters with `auto_open_login = false`).
    sign_in_now: bool,
    /// Engine-side deadline (monotonic ms) of the open window; moved by `KeepWaiting`.
    deadline: Option<u64>,
    /// How far `KeepWaiting` moves the deadline.
    extend_ms: u64,
    /// The `waiting` value JS was last told about (`LoginWindow.waiting` or `LoginUpdate.waiting`).
    sent_waiting: u32,
}

impl RealmState {
    fn new() -> Self {
        Self {
            state: SessionState::SignedOut,
            epoch: 0,
            prompts: BTreeMap::new(),
            blocked_runs: BTreeMap::new(),
            waiters: Vec::new(),
            parties: BTreeMap::new(),
            next_party: 0,
            prober: None,
            driver: None,
            sign_in_now: false,
            deadline: None,
            extend_ms: 0,
            sent_waiting: 0,
        }
    }

    /// Callers waiting inside `session_login` (what the user sees as "N items waiting").
    fn login_waiting(&self) -> u32 {
        let n = self
            .parties
            .values()
            .filter(|p| p.kind == PartyKind::Login)
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Keep `SessionState::LoginOpen::waiting` equal to the real count.
    fn sync(&mut self) {
        let n = self.login_waiting();
        if let SessionState::LoginOpen { waiting, .. } = &mut self.state {
            *waiting = n;
        }
    }

    fn add_party(&mut self, run: RunId, kind: PartyKind) -> u64 {
        self.next_party = self.next_party.saturating_add(1);
        self.parties.insert(self.next_party, Party { run, kind });
        self.next_party
    }

    /// Remember `waker` for the next state change (deduplicated).
    fn park(&mut self, waker: &Waker) {
        if !self.waiters.iter().any(|w| w.will_wake(waker)) {
            self.waiters.push(waker.clone());
        }
    }

    /// Wake every parked future and driver (waking only queues them, so this is fine inside `with`).
    fn wake_all(&mut self) {
        std::mem::take(&mut self.waiters)
            .into_iter()
            .for_each(Waker::wake);
    }

    fn is_open(&self) -> bool {
        matches!(self.state, SessionState::LoginOpen { .. })
    }

    /// Runs of every parked caller (login and admit), for run-scoped verdicts.
    fn party_runs(&self) -> BTreeSet<RunId> {
        self.parties.values().map(|p| p.run).collect()
    }
}

fn realm_mut(login: &mut LoginGate, realm: Realm) -> &mut RealmState {
    login.realms.entry(realm).or_insert_with(RealmState::new)
}

fn verdict_of(why: HaltWhy) -> SessionVerdict {
    match why {
        HaltWhy::Declined => SessionVerdict::Declined,
        HaltWhy::TimedOut => SessionVerdict::TimedOut,
        HaltWhy::Ineffective => SessionVerdict::Ineffective,
        HaltWhy::Misconfigured => SessionVerdict::Misconfigured,
        HaltWhy::Suspended => SessionVerdict::Suspended,
    }
}

/// The answer that needs no waiting, if there is one: a sign-in newer than the caller's attempt (silent retry),
/// a block recorded for the caller's run, or a realm-wide refusal / halt.
fn settled_verdict(r: &RealmState, run: RunId, seen_epoch: u32) -> Option<SessionVerdict> {
    if r.epoch > seen_epoch {
        return Some(SessionVerdict::Retry { epoch: r.epoch });
    }
    if let Some(why) = r.blocked_runs.get(&run) {
        return Some(verdict_of(*why));
    }
    match &r.state {
        SessionState::Refused { code } => Some(SessionVerdict::Refused { code: code.clone() }),
        SessionState::Halted { why } => Some(verdict_of(*why)),
        _ => None,
    }
}

/// The interactive-window gate: at most one visible window (login or challenge) at a time. Priority login >
/// challenge (the queue is sorted by kind), FIFO within a kind. `held_by` and the queue entries use the same
/// ticket numbers.
#[derive(Clone, Debug, Default)]
pub(crate) struct InteractiveGate {
    pub(crate) held_by: Option<OpId>,
    pub(crate) queue: Vec<(LoginKind, u64, Option<Waker>)>,
    next_ticket: u64,
}

impl InteractiveGate {
    /// Join the queue; returns the ticket.
    pub(crate) fn enqueue(&mut self, kind: LoginKind) -> u64 {
        self.next_ticket = self.next_ticket.saturating_add(1);
        let ticket = self.next_ticket;
        let at = self.queue.partition_point(|(k, _, _)| *k <= kind);
        self.queue.insert(at, (kind, ticket, None));
        ticket
    }

    /// Take the gate if it is free and `ticket` is first in line; otherwise remember `waker`.
    pub(crate) fn try_take(&mut self, ticket: u64, waker: &Waker) -> bool {
        if self.held_by.is_none() && self.queue.first().is_some_and(|e| e.1 == ticket) {
            self.queue.remove(0);
            self.held_by = Some(ticket);
            return true;
        }
        if let Some(e) = self.queue.iter_mut().find(|e| e.1 == ticket) {
            match &e.2 {
                Some(old) if old.will_wake(waker) => {}
                _ => e.2 = Some(waker.clone()),
            }
        }
        false
    }

    /// Give the gate back (if `ticket` holds it) or leave the queue (if it still waits). Returns the waker of
    /// the new head to wake.
    pub(crate) fn release(&mut self, ticket: u64) -> Option<Waker> {
        let mut changed = false;
        if self.held_by == Some(ticket) {
            self.held_by = None;
            changed = true;
        }
        let before = self.queue.len();
        self.queue.retain(|e| e.1 != ticket);
        changed |= self.queue.len() != before;
        if changed {
            self.queue.first().and_then(|e| e.2.clone())
        } else {
            None
        }
    }

    /// True while a window holds the gate.
    #[cfg(test)]
    pub(crate) fn busy(&self) -> bool {
        self.held_by.is_some()
    }
}

/// RAII claim on the interactive gate: dropping it frees the gate (or leaves the queue) and wakes the next.
struct GateGuard {
    shared: Rc<crate::cx::Shared>,
    ticket: u64,
}

impl GateGuard {
    fn enqueue(cx: &Cx, kind: LoginKind) -> GateGuard {
        let ticket = cx
            .shared
            .with(|s| s.login.interactive.enqueue(kind))
            .unwrap_or(0);
        GateGuard {
            shared: Rc::clone(&cx.shared),
            ticket,
        }
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        let ticket = self.ticket;
        if let Some(Some(w)) = self.shared.with(|s| s.login.interactive.release(ticket)) {
            w.wake();
        }
    }
}

/// Login state of the whole engine. Lives in `State`.
#[derive(Debug)]
pub(crate) struct LoginGate {
    pub(crate) realms: BTreeMap<Realm, RealmState>,
    pub(crate) interactive: InteractiveGate,
    /// `Shutdown` was received: every waiter resolves as cancelled.
    pub(crate) shutdown: bool,
}

impl LoginGate {
    pub(crate) fn new() -> Self {
        let mut realms = BTreeMap::new();
        realms.insert(Realm::Openathens, RealmState::new());
        realms.insert(Realm::Ezproxy, RealmState::new());
        Self {
            realms,
            interactive: InteractiveGate::default(),
            shutdown: false,
        }
    }

    /// Current epoch of a realm (0 before the first sign-in).
    #[cfg(test)]
    pub(crate) fn epoch(&self, realm: Realm) -> u32 {
        self.realms.get(&realm).map(|r| r.epoch).unwrap_or(0)
    }

    /// Items waiting on a window, a probe or a `SignInNow`, for `Stats.waiting_user`. This keeps the engine's
    /// stall detector quiet while nothing but the user can make progress.
    pub(crate) fn waiting_user(&self) -> u32 {
        let n: usize = self.realms.values().map(|r| r.parties.len()).sum();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Compact JSON of both realms and the interactive gate, for `DumpState`.
    pub(crate) fn dump(&self) -> serde_json::Value {
        let realms: serde_json::Map<String, serde_json::Value> = self
            .realms
            .iter()
            .map(|(realm, r)| {
                let blocked: BTreeMap<String, String> = r
                    .blocked_runs
                    .iter()
                    .map(|(run, why)| (run.to_string(), format!("{why:?}")))
                    .collect();
                let value = serde_json::json!({
                    "state": format!("{:?}", r.state),
                    "epoch": r.epoch,
                    "waiting": r.login_waiting(),
                    "parked": r.parties.len(),
                    "prober": r.prober,
                    "prompts": r.prompts,
                    "blocked_runs": blocked,
                });
                (format!("{realm:?}").to_ascii_lowercase(), value)
            })
            .collect();
        serde_json::json!({
            "shutdown": self.shutdown,
            "interactive": { "held_by": self.interactive.held_by, "queued": self.interactive.queue.len() },
            "realms": realms,
        })
    }

    /// A run was cancelled or finished: drop its waiter counts, prompt counters and run-scoped blocks; if a
    /// window has no waiters left the driver closes it.
    pub(crate) fn on_run_gone(&mut self, run: RunId) {
        for r in self.realms.values_mut() {
            r.prompts.remove(&run);
            r.blocked_runs.remove(&run);
            let parties = r.parties.len();
            r.parties.retain(|_, p| p.run != run);
            let mut changed = r.parties.len() != parties;
            if r.prober.is_some_and(|(pr, _)| pr == run) {
                r.prober = None;
                if matches!(r.state, SessionState::Probing { .. }) {
                    r.state = SessionState::SignedOut;
                }
                changed = true;
            }
            if changed {
                r.sync();
                r.wake_all();
            }
        }
    }
}

/// Tell JS how many items wait now (`LoginUpdate`), when a window is open and the count changed. The op id is
/// fresh and never the window's (see the module docs).
fn push_update(s: &mut State, realm: Realm) {
    let waiting = {
        let r = realm_mut(&mut s.login, realm);
        if !matches!(
            r.state,
            SessionState::LoginOpen {
                window: Some(_),
                ..
            }
        ) {
            return;
        }
        r.sync();
        let n = r.login_waiting();
        if n == r.sent_waiting {
            return;
        }
        r.sent_waiting = n;
        n
    };
    let op = s.reactor.alloc_id();
    s.outbox.push(Effect::LoginUpdate { op, waiting });
}

/// Row for an item that waits for the sign-in (with a "Sign in" button when the window is not opened by itself).
fn emit_waiting_row(s: &mut State, run: RunId, item: ItemId) {
    let auto_open = s
        .cfg
        .as_ref()
        .map(|c| c.inst.auto_open_login)
        .unwrap_or(true);
    let mut row = l10n::row(RowState::Waiting, l10n::id::WAITING_LOGIN).on_route(Route::Inst);
    if !auto_open {
        row = row.with_action(RowAction::SignIn);
    }
    s.outbox.push(Effect::Progress { run, item, row });
}

/// `UserAction` for the login gate: `SignInNow` (open the window now), `SkipSignIn` (decline: close the window,
/// `Halted(Declined)` for that run), `KeepWaiting` (re-arm the window timeout), `RetrySignIn` (clear the run's
/// block so the next attempt may prompt).
pub(crate) fn on_user_action(ctl: &mut Ctl<'_>, run: RunId, kind: UserActionKind) {
    let _ = ctl.shared.with(|s| user_action(s, run, kind));
}

fn user_action(s: &mut State, run: RunId, kind: UserActionKind) {
    let now = s.clock.now;
    let realms: Vec<Realm> = s.login.realms.keys().copied().collect();
    for realm in realms {
        let r = realm_mut(&mut s.login, realm);
        match kind {
            UserActionKind::SignInNow => {
                if r.is_open() {
                    r.sign_in_now = true;
                    r.wake_all();
                }
            }
            UserActionKind::SkipSignIn => {
                r.blocked_runs.insert(run, HaltWhy::Declined);
                r.parties.retain(|_, p| p.run != run);
                r.sync();
                r.wake_all();
            }
            UserActionKind::KeepWaiting => {
                if r.is_open()
                    && let Some(d) = r.deadline
                {
                    r.deadline = Some(d.max(now).saturating_add(r.extend_ms));
                    r.wake_all();
                }
            }
            UserActionKind::RetrySignIn => {
                r.blocked_runs.remove(&run);
                r.prompts.remove(&run);
                if matches!(
                    r.state,
                    SessionState::Refused { .. } | SessionState::Halted { .. }
                ) {
                    r.state = SessionState::SignedOut;
                }
                r.wake_all();
            }
        }
        push_update(s, realm);
    }
}

/// Shutdown: wake every waiter with `Cancelled`; the driver tasks are dropped by the executor.
pub(crate) fn on_shutdown(ctl: &mut Ctl<'_>) {
    let _ = ctl.shared.with(|s| {
        s.login.shutdown = true;
        s.sched.begin_shutdown();
        for r in s.login.realms.values_mut() {
            r.wake_all();
        }
        // challenge windows queued on the interactive gate refuse at their next poll
        for (_, _, waker) in &s.login.interactive.queue {
            if let Some(w) = waker {
                w.wake_by_ref();
            }
        }
    });
}

// ------------------------------------------------------------------------------------------ session_admit

enum Decision {
    Done(Admission),
    Park { wake_at: Option<u64> },
}

/// The pure part of `session_admit` (mutates only the probe lock).
fn admit_decision(
    r: &mut RealmState,
    run: RunId,
    item: ItemId,
    now: u64,
    shutdown: bool,
) -> Decision {
    if shutdown {
        return Decision::Done(Admission::Blocked(SessionVerdict::Cancelled));
    }
    match &r.state {
        SessionState::Refused { code } => {
            return Decision::Done(Admission::Blocked(SessionVerdict::Refused {
                code: code.clone(),
            }));
        }
        SessionState::Halted { why } => {
            return Decision::Done(Admission::Blocked(verdict_of(*why)));
        }
        // A working session serves every run, even one that declined earlier.
        SessionState::SignedIn { .. } => {
            return Decision::Done(Admission::Proceed { epoch: r.epoch });
        }
        SessionState::SignedOut | SessionState::Probing { .. } | SessionState::LoginOpen { .. } => {
        }
    }
    if let Some(why) = r.blocked_runs.get(&run) {
        return Decision::Done(Admission::Blocked(verdict_of(*why)));
    }
    match r.state {
        SessionState::SignedOut => {
            r.state = SessionState::Probing { since: now };
            r.prober = Some((run, item));
            Decision::Done(Admission::Proceed { epoch: r.epoch })
        }
        SessionState::Probing { since } => {
            let stale_at = since.saturating_add(policy::PROBE_STALE_MS);
            if r.prober == Some((run, item)) {
                Decision::Done(Admission::Proceed { epoch: r.epoch })
            } else if now >= stale_at {
                // The prober never reported: take the probe over.
                r.state = SessionState::Probing { since: now };
                r.prober = Some((run, item));
                Decision::Done(Admission::Proceed { epoch: r.epoch })
            } else {
                Decision::Park {
                    wake_at: Some(stale_at),
                }
            }
        }
        _ => Decision::Park { wake_at: None },
    }
}

/// Future of `Cx::session_admit`.
pub(crate) struct SessionAdmit {
    cx: Cx,
    realm: Realm,
    party: Option<u64>,
    timer: Option<TimerKey>,
}

fn admit_step(
    s: &mut State,
    realm: Realm,
    run: RunId,
    item: ItemId,
    party: &mut Option<u64>,
    timer: &mut Option<TimerKey>,
    waker: &Waker,
) -> Option<Admission> {
    let (now, shutdown) = (s.clock.now, s.login.shutdown);
    let r = realm_mut(&mut s.login, realm);
    match admit_decision(r, run, item, now, shutdown) {
        Decision::Done(admission) => {
            if let Some(id) = party.take() {
                r.parties.remove(&id);
            }
            arm_timer(&mut s.reactor, timer, None, waker);
            Some(admission)
        }
        Decision::Park { wake_at } => {
            if party.is_none() {
                *party = Some(r.add_party(run, PartyKind::Admit));
            }
            r.park(waker);
            arm_timer(&mut s.reactor, timer, wake_at, waker);
            None
        }
    }
}

impl Future for SessionAdmit {
    type Output = Admission;

    fn poll(mut self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Admission> {
        let this = &mut *self;
        let shared = Rc::clone(&this.cx.shared);
        let (realm, run, item) = (this.realm, this.cx.run, this.cx.item);
        let out = shared.with(|s| {
            admit_step(
                s,
                realm,
                run,
                item,
                &mut this.party,
                &mut this.timer,
                task.waker(),
            )
        });
        match out {
            Some(Some(admission)) => Poll::Ready(admission),
            Some(None) => Poll::Pending,
            None => Poll::Ready(Admission::Blocked(SessionVerdict::Cancelled)),
        }
    }
}

impl Drop for SessionAdmit {
    fn drop(&mut self) {
        if self.party.is_none() && self.timer.is_none() {
            return;
        }
        let (realm, party, timer) = (self.realm, self.party.take(), self.timer.take());
        let _ = self.cx.shared.with(|s| {
            if let Some(k) = timer {
                s.reactor.remove_timer(k);
            }
            if let Some(id) = party {
                realm_mut(&mut s.login, realm).parties.remove(&id);
            }
        });
    }
}

// ------------------------------------------------------------------------------------------- session_report

/// Apply what an attempt saw to the realm's state machine (see the module diagram).
fn report_step(
    s: &mut State,
    realm: Realm,
    run: RunId,
    item: ItemId,
    epoch: u32,
    result: ProbeResult,
) {
    let wall = s.clock.wall;
    let mut suspend = false;
    {
        let r = realm_mut(&mut s.login, realm);
        match result {
            ProbeResult::LoggedIn => {
                let promote = match &r.state {
                    SessionState::SignedOut
                    | SessionState::Probing { .. }
                    | SessionState::SignedIn { verified: false } => true,
                    SessionState::SignedIn { verified: true }
                    | SessionState::LoginOpen { .. }
                    | SessionState::Refused { .. }
                    | SessionState::Halted { .. } => false,
                };
                if promote {
                    r.state = SessionState::SignedIn { verified: true };
                    r.prober = None;
                    r.wake_all();
                }
            }
            ProbeResult::NeedsLogin => {
                // The first attempt after a sign-in bounced: the sign-in did not take effect for this run.
                if matches!(r.state, SessionState::SignedIn { verified: false }) && epoch >= r.epoch
                {
                    r.blocked_runs.insert(run, HaltWhy::Ineffective);
                    // A new run starts from scratch (probe, then prompt) instead of inheriting a dead session.
                    r.state = SessionState::SignedOut;
                    r.prober = None;
                    r.wake_all();
                }
            }
            ProbeResult::Misconfigured => {
                r.state = SessionState::Halted {
                    why: HaltWhy::Misconfigured,
                };
                r.prober = None;
                r.wake_all();
            }
            ProbeResult::Suspended => {
                r.state = SessionState::Halted {
                    why: HaltWhy::Suspended,
                };
                r.prober = None;
                r.wake_all();
                suspend = true;
            }
            ProbeResult::Failed => {
                if r.prober == Some((run, item)) && matches!(r.state, SessionState::Probing { .. })
                {
                    r.state = SessionState::SignedOut;
                    r.prober = None;
                    r.wake_all();
                }
            }
        }
    }
    if suspend
        && s.sched
            .halts
            .halt_inst(HaltReason::EzproxySuspended, None, wall)
    {
        let eff = s.sched.halts_effect(true);
        s.outbox.push(eff);
        s.sched.inst_wakers().into_iter().for_each(Waker::wake);
    }
}

// -------------------------------------------------------------------------------------------- session_login

/// The pure part of the first poll of `session_login`: an answer at once, or "join / open the window".
fn login_step(
    s: &mut State,
    cx: &Cx,
    realm: Realm,
    req: &LoginRequest,
    party: &mut Option<u64>,
    waker: &Waker,
) -> Option<SessionVerdict> {
    let (now, run, item) = (s.clock.now, cx.run, cx.item);
    if s.login.shutdown {
        if let Some(id) = party.take() {
            realm_mut(&mut s.login, realm).parties.remove(&id);
        }
        return Some(SessionVerdict::Cancelled);
    }
    let r = realm_mut(&mut s.login, realm);
    if let Some(id) = *party {
        // Already waiting. Still waiting only while our party exists and the window is open.
        if r.parties.contains_key(&id) && r.is_open() {
            r.park(waker);
            return None;
        }
        r.parties.remove(&id);
        *party = None;
        r.sync();
        return Some(settled_verdict(r, run, req.seen_epoch).unwrap_or(SessionVerdict::Declined));
    }
    if let Some(v) = settled_verdict(r, run, req.seen_epoch) {
        return Some(v);
    }
    if r.is_open() {
        // Join the window that is already open (no prompt is consumed).
        *party = Some(r.add_party(run, PartyKind::Login));
        r.sync();
        r.park(waker);
        emit_waiting_row(s, run, item);
        push_update(s, realm);
        return None;
    }
    if r.prompts.get(&run).copied().unwrap_or(0) >= policy::MAX_PROMPTS_PER_REALM {
        return Some(SessionVerdict::Ineffective);
    }
    // Open the one window for this realm.
    *r.prompts.entry(run).or_insert(0) += 1;
    r.state = SessionState::LoginOpen {
        window: None,
        opened_at: now,
        waiting: 0,
    };
    r.prober = None;
    r.sign_in_now = false;
    r.deadline = None;
    r.sent_waiting = 0;
    r.driver = Some(DriverInfo {
        opener_run: run,
        shown: false,
    });
    *party = Some(r.add_party(run, PartyKind::Login));
    r.sync();
    r.park(waker);
    // The driver is an engine-wide task: cancelling the opener's run must not close a window others wait on.
    let driver_cx = Cx {
        owner: Owner::System,
        ..cx.clone()
    };
    s.spawns.push(SpawnReq {
        owner: Owner::System,
        fut: Box::pin(window_driver(driver_cx, realm, req.clone())),
    });
    emit_waiting_row(s, run, item);
    None
}

/// Future of `Cx::session_login`.
pub(crate) struct SessionLogin {
    cx: Cx,
    realm: Realm,
    req: LoginRequest,
    party: Option<u64>,
    done: bool,
}

impl Future for SessionLogin {
    type Output = SessionVerdict;

    fn poll(mut self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<SessionVerdict> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(SessionVerdict::Cancelled);
        }
        let shared = Rc::clone(&this.cx.shared);
        let out = shared.with(|s| {
            login_step(
                s,
                &this.cx,
                this.realm,
                &this.req,
                &mut this.party,
                task.waker(),
            )
        });
        match out {
            Some(Some(verdict)) => {
                this.done = true;
                Poll::Ready(verdict)
            }
            Some(None) => Poll::Pending,
            None => {
                this.done = true;
                Poll::Ready(SessionVerdict::Cancelled)
            }
        }
    }
}

impl Drop for SessionLogin {
    fn drop(&mut self) {
        let Some(id) = self.party.take() else { return };
        let realm = self.realm;
        let _ = self.cx.shared.with(|s| {
            let removed = realm_mut(&mut s.login, realm).parties.remove(&id).is_some();
            if removed {
                let r = realm_mut(&mut s.login, realm);
                r.sync();
                // The driver re-checks `waiting` (and closes the window at 0).
                r.wake_all();
                push_update(s, realm);
            }
        });
    }
}

// ------------------------------------------------------------------------------------------- window driver

/// How a driver wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Go {
    Open,
    Abandon,
    Shutdown,
}

/// What the driver ended with.
enum End {
    /// Nobody is waiting any more (all cancelled / skipped): nothing to resolve.
    Abandoned,
    /// The engine is shutting down.
    Shutdown,
    /// The window answered.
    Done {
        result: LoginResult,
        ca_error: Option<String>,
    },
    /// The window op failed (JS error or undecodable answer).
    WindowFailed,
    /// The engine-side timer ran out before the window answered.
    TimedOut,
}

/// `(auto_open_login, window timeout in ms)` for the run that opened the window (its config snapshot), else the
/// latest config, else the documented defaults.
fn login_prefs(s: &State, run: RunId) -> (bool, u32) {
    let (auto_open, secs) = match s.runs.runs.get(&run) {
        Some(rec) => (rec.cfg.inst.auto_open_login, rec.cfg.inst.login_timeout_sec),
        None => match &s.cfg {
            Some(c) => (c.inst.auto_open_login, c.inst.login_timeout_sec),
            None => (true, 300),
        },
    };
    (auto_open, secs.saturating_mul(1000))
}

/// Poll `pred` under one borrow until it answers; while it does not, the caller is parked on the realm (so every
/// state change re-runs it). `None` = the cell was borrowed (engine bug), treated like a shutdown by callers.
async fn wait_for<T>(
    cx: &Cx,
    realm: Realm,
    mut pred: impl FnMut(&mut State, &Waker) -> Option<T>,
) -> Option<T> {
    let shared = Rc::clone(&cx.shared);
    std::future::poll_fn(move |task| {
        let waker = task.waker();
        let polled = shared.with(|s| {
            let answer = pred(s, waker);
            if answer.is_none() {
                realm_mut(&mut s.login, realm).park(waker);
            }
            answer
        });
        match polled {
            Some(Some(v)) => Poll::Ready(Some(v)),
            Some(None) => Poll::Pending,
            None => Poll::Ready(None),
        }
    })
    .await
}

/// Cleans up when the driver future is dropped before it finished (the executor refused or cancelled the task):
/// without it the realm would stay `LoginOpen` for ever with parked waiters. The waiters' runs are told the
/// window did not complete (`TimedOut`).
struct DriverGuard {
    cx: Cx,
    realm: Realm,
    finished: bool,
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        if !self.finished {
            finish_window(&self.cx, self.realm, End::WindowFailed);
        }
    }
}

/// The window driver task body (spawned once per opened window with `Owner::System`).
pub(crate) async fn window_driver(cx: Cx, realm: Realm, req: LoginRequest) {
    let mut guard = DriverGuard {
        cx: cx.clone(),
        realm,
        finished: false,
    };
    let end = drive(&cx, realm, &req).await;
    finish_window(&cx, realm, end);
    guard.finished = true;
}

async fn drive(cx: &Cx, realm: Realm, req: &LoginRequest) -> End {
    let run = cx.run;
    let Some((auto_open, timeout_ms)) = cx.shared.with(|s| login_prefs(s, run)) else {
        return End::Shutdown;
    };

    // A. With autoOpenLogin off the user has to press "Sign in" first (rows carry that button).
    if !auto_open {
        let go = wait_for(cx, realm, |s, _| {
            if s.login.shutdown {
                return Some(Go::Shutdown);
            }
            let r = realm_mut(&mut s.login, realm);
            if r.sign_in_now {
                Some(Go::Open)
            } else if r.login_waiting() == 0 {
                Some(Go::Abandon)
            } else {
                None
            }
        })
        .await;
        match go {
            Some(Go::Open) => {}
            Some(Go::Abandon) => return End::Abandoned,
            Some(Go::Shutdown) | None => return End::Shutdown,
        }
    }

    // B. One visible window at a time; a login queues ahead of any challenge window.
    let gate = GateGuard::enqueue(cx, LoginKind::Login);
    let ticket = gate.ticket;
    let go = wait_for(cx, realm, |s, waker| {
        if s.login.shutdown {
            return Some(Go::Shutdown);
        }
        if realm_mut(&mut s.login, realm).login_waiting() == 0 {
            return Some(Go::Abandon);
        }
        s.login
            .interactive
            .try_take(ticket, waker)
            .then_some(Go::Open)
    })
    .await;
    match go {
        Some(Go::Open) => {}
        Some(Go::Abandon) => return End::Abandoned,
        Some(Go::Shutdown) | None => return End::Shutdown,
    }

    // C. The window itself, raced against the engine-side timer and against "nobody is waiting any more".
    let Some(waiting) = cx
        .shared
        .with(|s| realm_mut(&mut s.login, realm).login_waiting())
    else {
        return End::Shutdown;
    };
    let login_req = LoginReq {
        kind: LoginKind::Login,
        realm,
        entry_url: req.entry_url.clone(),
        item_title: req.item_title.clone(),
        waiting,
        timeout_ms,
        capture_pdf: req.capture_pdf,
    };
    let shared = Rc::clone(&cx.shared);
    let mut window = Box::pin(cx.login_window(login_req));
    let mut timer: Option<TimerKey> = None;
    let mut opened = false;
    let end = std::future::poll_fn(|task| {
        // 1. The window op (its first poll emits the `LoginWindow` effect). Bodies in the answer are released
        //    here, outside the state borrow.
        if let Poll::Ready(answer) = window.as_mut().poll(task) {
            return Poll::Ready(match answer {
                Ok(resp) => {
                    let end = End::Done {
                        result: resp.result,
                        ca_error: resp.ca_error.clone(),
                    };
                    drop(resp);
                    end
                }
                Err(_) => End::WindowFailed,
            });
        }
        // 2. Bookkeeping under one borrow.
        let waker = task.waker();
        let was_open = opened;
        let step = shared.with(|s| {
            if s.login.shutdown {
                return Some(End::Shutdown);
            }
            let now = s.clock.now;
            // The window op now exists: find its id for the state / dumps.
            let window_op = s
                .reactor
                .waiting()
                .into_iter()
                .filter(|(_, owner, kind)| *owner == Owner::System && *kind == OpKind::LoginWindow)
                .map(|(op, _, _)| op)
                .max();
            let r = realm_mut(&mut s.login, realm);
            if !was_open {
                r.deadline = Some(
                    now.saturating_add(u64::from(timeout_ms))
                        .saturating_add(policy::LOGIN_ENGINE_GRACE_MS),
                );
                r.extend_ms = u64::from(timeout_ms);
                r.sent_waiting = waiting;
                if let Some(d) = r.driver.as_mut() {
                    d.shown = true;
                }
                if let SessionState::LoginOpen { window, .. } = &mut r.state {
                    *window = window_op;
                }
            }
            if r.login_waiting() == 0 {
                return Some(End::Abandoned);
            }
            let deadline = r.deadline.unwrap_or(0);
            if now >= deadline {
                return Some(End::TimedOut);
            }
            r.park(waker);
            arm_timer(&mut s.reactor, &mut timer, Some(deadline), waker);
            push_update(s, realm);
            None
        });
        opened = true;
        match step {
            Some(Some(end)) => Poll::Ready(end),
            Some(None) => Poll::Pending,
            None => Poll::Ready(End::Shutdown),
        }
    })
    .await;
    let _ = cx.shared.with(|s| {
        if let Some(k) = timer {
            s.reactor.remove_timer(k);
        }
    });
    // `window` (still waiting unless it answered) is dropped here -> `Effect::Cancel` closes the window; then
    // `gate` is dropped, which lets the next queued window open.
    drop(window);
    drop(gate);
    end
}

/// Apply the driver's result to the realm and wake everybody (see the module diagram).
fn finish_window(cx: &Cx, realm: Realm, end: End) {
    let _ = cx.shared.with(|s| {
        let r = realm_mut(&mut s.login, realm);
        if r.is_open() {
            let who: BTreeSet<RunId> = r.party_runs();
            let tag = |r: &mut RealmState, why: HaltWhy| {
                for run in &who {
                    r.blocked_runs.insert(*run, why);
                }
            };
            match end {
                End::Done { result, ca_error } => {
                    if ca_error.is_some() || result == LoginResult::CaBlocked {
                        r.state = SessionState::Refused { code: ca_error };
                    } else {
                        match result {
                            LoginResult::Landed | LoginResult::Pdf => {
                                r.epoch = r.epoch.saturating_add(1);
                                r.state = SessionState::SignedIn { verified: false };
                                r.prober = None;
                            }
                            LoginResult::ClosedByUser | LoginResult::Handoff => {
                                tag(r, HaltWhy::Declined);
                                r.state = SessionState::SignedOut;
                            }
                            LoginResult::Timeout | LoginResult::CaBlocked => {
                                tag(r, HaltWhy::TimedOut);
                                r.state = SessionState::SignedOut;
                            }
                        }
                    }
                }
                End::WindowFailed | End::TimedOut => {
                    tag(r, HaltWhy::TimedOut);
                    r.state = SessionState::SignedOut;
                }
                End::Abandoned => {
                    // Everyone left. A window that was never shown did not cost the opener a prompt.
                    if let Some(d) = &r.driver
                        && !d.shown
                        && let Some(n) = r.prompts.get_mut(&d.opener_run)
                    {
                        *n = n.saturating_sub(1);
                    }
                    r.state = SessionState::SignedOut;
                }
                End::Shutdown => {}
            }
        }
        r.parties.retain(|_, p| p.kind != PartyKind::Login);
        r.driver = None;
        r.deadline = None;
        r.sign_in_now = false;
        r.sent_waiting = 0;
        r.wake_all();
    });
}

// ----------------------------------------------------------------------------------------- challenge_window

fn cancelled(message: &str) -> OpErr {
    OpErr {
        kind: OpErrKind::Cancelled,
        message: message.to_owned(),
        after_ms: None,
        status: None,
    }
}

type WindowFuture = Pin<Box<dyn Future<Output = OpResult<LoginResp>>>>;

/// Future of `Cx::challenge_window`.
pub(crate) struct ChallengeWindow {
    cx: Cx,
    req: LoginReq,
    // Field order matters on drop: the window op is cancelled (its `Cancel` is queued) before the gate is
    // released and the next window can be emitted.
    window: Option<WindowFuture>,
    guard: Option<GateGuard>,
    done: bool,
}

impl Future for ChallengeWindow {
    type Output = OpResult<LoginResp>;

    fn poll(mut self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(Err(cancelled("challenge window polled after completion")));
        }
        let shared = Rc::clone(&this.cx.shared);
        let run = this.cx.run;
        // Shutdown, or the run's user said "skip sign-in": no window.
        let refused = shared
            .with(|s| {
                s.login.shutdown
                    || s.login
                        .realms
                        .values()
                        .any(|r| r.blocked_runs.get(&run) == Some(&HaltWhy::Declined))
            })
            .unwrap_or(true);
        if refused {
            this.done = true;
            this.window = None;
            this.guard = None;
            return Poll::Ready(Err(cancelled(
                "challenge window refused (declined or shutting down)",
            )));
        }
        if this.window.is_none() {
            let guard = this
                .guard
                .get_or_insert_with(|| GateGuard::enqueue(&this.cx, LoginKind::Challenge));
            let ticket = guard.ticket;
            if !shared
                .with(|s| s.login.interactive.try_take(ticket, task.waker()))
                .unwrap_or(false)
            {
                return Poll::Pending;
            }
            let (cx, req) = (this.cx.clone(), this.req.clone());
            this.window = Some(Box::pin(async move { cx.login_window(req).await }));
        }
        let Some(window) = this.window.as_mut() else {
            return Poll::Pending;
        };
        let polled = window.as_mut().poll(task);
        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(answer) => {
                this.done = true;
                this.window = None;
                this.guard = None; // frees the gate for the next window
                Poll::Ready(match answer {
                    // The user closed the window or chose the hand-off: that is a decline, not a result.
                    Ok(resp)
                        if matches!(
                            resp.result,
                            LoginResult::ClosedByUser | LoginResult::Handoff
                        ) =>
                    {
                        drop(resp);
                        Err(cancelled("challenge window declined by the user"))
                    }
                    other => other,
                })
            }
        }
    }
}

impl Cx {
    /// Call before every hidden-browser redirector / EZproxy load. Waits while another item is probing or a
    /// window is open, then returns `Proceed { epoch }` (the caller may be the prober) or `Blocked(verdict)`
    /// (declined / refused / ineffective / misconfigured / suspended for this run: go straight to hand-off).
    pub(crate) fn session_admit(&self, realm: Realm) -> SessionAdmit {
        SessionAdmit {
            cx: self.clone(),
            realm,
            party: None,
            timer: None,
        }
    }

    /// Report what the attempt saw (see [`ProbeResult`]); updates the state machine and wakes waiters.
    pub(crate) fn session_report(&self, realm: Realm, epoch: u32, result: ProbeResult) {
        let (run, item) = (self.run, self.item);
        let _ = self
            .shared
            .with(|s| report_step(s, realm, run, item, epoch, result));
    }

    /// The attempt landed on a login page: join (or open) the single sign-in. Resolves when the sign-in is
    /// over for this caller. The caller must have released its item slot first (parking) and re-acquires with
    /// `Prio::Resumed` afterwards.
    pub(crate) fn session_login(&self, realm: Realm, req: LoginRequest) -> SessionLogin {
        SessionLogin {
            cx: self.clone(),
            realm,
            req,
            party: None,
            done: false,
        }
    }

    /// Visible challenge window (user acts; we never solve CAPTCHAs) through the interactive gate. Resolves
    /// with an `OpErr { kind: Cancelled }` when the user declined or the gate was shut.
    pub(crate) fn challenge_window(&self, req: LoginReq) -> ChallengeWindow {
        let req = LoginReq {
            kind: LoginKind::Challenge,
            ..req
        };
        ChallengeWindow {
            cx: self.clone(),
            req,
            guard: None,
            window: None,
            done: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::Executor;
    use crate::sched::testkit::{World, flag_waker};
    use serde_json::{Value, json};
    use std::cell::{Cell, RefCell};

    type Log = Rc<RefCell<Vec<String>>>;

    fn log() -> Log {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn lines(l: &Log) -> Vec<String> {
        l.borrow().clone()
    }

    /// Same lines, sorted: the order in which woken items resume depends on the test executor, not on the engine.
    fn sorted(l: &Log) -> Vec<String> {
        let mut v = lines(l);
        v.sort();
        v
    }

    fn login_req(seen_epoch: u32) -> LoginRequest {
        LoginRequest {
            entry_url: "https://go.openathens.net/redirector/kcl.ac.uk?url=x".into(),
            item_title: "A very long article title".into(),
            seen_epoch,
            capture_pdf: true,
        }
    }

    /// Spawn an item that calls `session_login` and logs `"<item>:<verdict>"`.
    fn spawn_login(w: &mut World, run: RunId, item: ItemId, seen: u32, out: &Log) -> usize {
        let (cx, out) = (w.item_cx(run, item), Rc::clone(out));
        w.spawn(async move {
            let v = cx.session_login(Realm::Openathens, login_req(seen)).await;
            out.borrow_mut().push(format!("{item}:{v:?}"));
        })
    }

    fn login_ok(result: &str) -> Value {
        json!({ "result": result, "final_url": "https://journals.lww.com/x", "inspect": { "expect": "none" } })
    }

    fn windows(fx: &[Effect]) -> Vec<(OpId, RunId, ItemId, LoginReq)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::LoginWindow { op, run, item, req } => Some((*op, *run, *item, req.clone())),
                _ => None,
            })
            .collect()
    }

    fn updates(fx: &[Effect]) -> Vec<(OpId, u32)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::LoginUpdate { op, waiting } => Some((*op, *waiting)),
                _ => None,
            })
            .collect()
    }

    fn cancels(fx: &[Effect]) -> Vec<OpId> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Cancel { op } => Some(*op),
                _ => None,
            })
            .collect()
    }

    fn rows(fx: &[Effect]) -> Vec<(RunId, ItemId, ProgressRow)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Progress { run, item, row } => Some((*run, *item, row.clone())),
                _ => None,
            })
            .collect()
    }

    fn state_of(w: &World, realm: Realm) -> SessionState {
        w.shared
            .with(|s| s.login.realms.get(&realm).map(|r| r.state.clone()))
            .flatten()
            .expect("realm state")
    }

    fn user_action_now(w: &World, run: RunId, kind: UserActionKind) {
        let mut exec = Executor::new(8);
        let mut ctl = Ctl {
            shared: &w.shared,
            exec: &mut exec,
        };
        on_user_action(&mut ctl, run, kind);
    }

    /// One window with `n` waiters from run 1, items `1..=n`, started one by one. Returns the effects so far.
    fn open_window(w: &mut World, n: u32, out: &Log) -> Vec<Effect> {
        let mut fx = Vec::new();
        for i in 1..=n {
            spawn_login(w, 1, i, 0, out);
            w.run();
            fx.extend(w.effects());
        }
        fx
    }

    // ------------------------------------------------------------------------------- one window for all

    #[test]
    fn twenty_callers_produce_one_window_and_growing_updates() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 20, &out);
        let wins = windows(&fx);
        assert_eq!(wins.len(), 1, "exactly ONE LoginWindow for 20 callers");
        let (window_op, run, item, req) = wins[0].clone();
        assert_eq!((run, item), (1, 1), "labelled with the first waiter");
        assert_eq!(
            (
                req.kind,
                req.realm,
                req.waiting,
                req.timeout_ms,
                req.capture_pdf
            ),
            (LoginKind::Login, Realm::Openathens, 1, 300_000, true)
        );
        assert_eq!(
            req.entry_url,
            "https://go.openathens.net/redirector/kcl.ac.uk?url=x"
        );
        assert_eq!(req.item_title, "A very long article title");
        // 19 joiners -> 19 updates with the count growing 2..=20, each with a fresh op id (never the window's)
        let ups = updates(&fx);
        assert_eq!(
            ups.iter().map(|u| u.1).collect::<Vec<_>>(),
            (2..=20).collect::<Vec<u32>>()
        );
        let ids: BTreeSet<OpId> = ups.iter().map(|u| u.0).collect();
        assert_eq!(ids.len(), 19);
        assert!(!ids.contains(&window_op));
        assert_eq!(
            state_of(&w, Realm::Openathens),
            SessionState::LoginOpen {
                window: Some(window_op),
                opened_at: 1_000,
                waiting: 20
            }
        );
        assert_eq!(w.shared.with(|s| s.login.waiting_user()), Some(20));
        // every waiting item got a row
        let waiting_rows = rows(&fx);
        assert_eq!(waiting_rows.len(), 20);
        assert!(waiting_rows.iter().all(|(r, _, row)| *r == 1
            && row.l10n == l10n::id::WAITING_LOGIN
            && row.state == RowState::Waiting
            && row.actions.is_empty()));
        assert!(out.borrow().is_empty());

        // the sign-in succeeds: everybody retries under epoch 1, nobody is prompted again
        w.complete_ok(window_op, login_ok("landed"));
        w.run();
        let got = lines(&out);
        assert_eq!(got.len(), 20);
        assert!(
            got.iter().all(|l| l.ends_with(":Retry { epoch: 1 }")),
            "{got:?}"
        );
        assert_eq!(
            state_of(&w, Realm::Openathens),
            SessionState::SignedIn { verified: false }
        );
        assert_eq!(w.shared.with(|s| s.login.epoch(Realm::Openathens)), Some(1));
        assert_eq!(w.shared.with(|s| s.login.waiting_user()), Some(0));
        let after = w.effects();
        assert!(windows(&after).is_empty());
        assert!(
            cancels(&after).is_empty(),
            "a window that answered is not cancelled"
        );
        assert!(
            !w.shared
                .with(|s| s.login.interactive.busy())
                .expect("state"),
            "the interactive gate was released"
        );
    }

    #[test]
    fn a_later_caller_with_an_old_epoch_retries_silently_without_a_prompt() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 2, &out);
        let window_op = windows(&fx)[0].0;
        w.complete_ok(window_op, login_ok("landed"));
        w.run();
        w.effects();
        out.borrow_mut().clear();
        // a different run's item whose attempt ran under epoch 0 and bounced before the sign-in finished
        spawn_login(&mut w, 7, 99, 0, &out);
        w.run();
        assert_eq!(lines(&out), vec!["99:Retry { epoch: 1 }"]);
        assert!(windows(&w.effects()).is_empty());
        // ... but a caller that bounced under epoch 1 is a new expiry and does prompt
        spawn_login(&mut w, 7, 100, 1, &out);
        w.run();
        assert_eq!(windows(&w.effects()).len(), 1);
    }

    #[test]
    fn a_pdf_result_signs_in_just_like_a_landing() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 3, &out);
        w.complete_ok(windows(&fx)[0].0, login_ok("pdf"));
        w.run();
        assert_eq!(lines(&out).len(), 3);
        assert!(
            lines(&out)
                .iter()
                .all(|l| l.ends_with("Retry { epoch: 1 }"))
        );
    }

    #[test]
    fn the_window_body_handles_are_released() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        let ok = json!({
            "result": "landed", "final_url": "https://x.org/", "inspect": { "expect": "none" },
            "page": { "handle": 5, "len": 100 }, "pdf": { "handle": 6, "len": 200 }
        });
        w.complete_ok(windows(&fx)[0].0, ok);
        w.run();
        let released: BTreeSet<u32> = w
            .effects()
            .iter()
            .filter_map(|e| match e {
                Effect::Release { handle } => Some(*handle),
                _ => None,
            })
            .collect();
        assert_eq!(released, BTreeSet::from([5, 6]));
    }

    // ------------------------------------------------------------------------------------- declines etc.

    #[test]
    fn decline_is_scoped_to_the_run() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 3, &out);
        w.complete_ok(windows(&fx)[0].0, login_ok("closed_by_user"));
        w.run();
        assert_eq!(sorted(&out), vec!["1:Declined", "2:Declined", "3:Declined"]);
        assert_eq!(state_of(&w, Realm::Openathens), SessionState::SignedOut);
        assert_eq!(
            w.shared.with(|s| s.login.epoch(Realm::Openathens)),
            Some(0),
            "no sign-in happened"
        );
        w.effects();
        // a later item of the SAME run is declined at once, no new window
        spawn_login(&mut w, 1, 4, 0, &out);
        w.run();
        assert_eq!(lines(&out).last().map(String::as_str), Some("4:Declined"));
        assert!(windows(&w.effects()).is_empty());
        // a NEW run is prompted again
        spawn_login(&mut w, 2, 5, 0, &out);
        w.run();
        assert_eq!(windows(&w.effects()).len(), 1);
        // the declined run's block disappears when the run is gone
        let _ = w.shared.with(|s| s.login.on_run_gone(1));
        let blocked = w.shared.with(|s| {
            s.login
                .realms
                .values()
                .any(|r| r.blocked_runs.contains_key(&1))
        });
        assert_eq!(blocked, Some(false));
    }

    #[test]
    fn handoff_counts_as_a_decline_too() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        w.complete_ok(windows(&fx)[0].0, login_ok("handoff"));
        w.run();
        assert_eq!(lines(&out), vec!["1:Declined"]);
    }

    #[test]
    fn a_decline_also_blocks_parked_admit_callers_of_the_same_run() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        // item 50 of run 1 asks for admission while the window is open and parks
        let cx = w.item_cx(1, 50);
        let adm = Rc::clone(&out);
        w.spawn(async move {
            let a = cx.session_admit(Realm::Openathens).await;
            adm.borrow_mut().push(format!("50:{a:?}"));
        });
        w.run();
        assert_eq!(w.shared.with(|s| s.login.waiting_user()), Some(2));
        w.complete_ok(windows(&fx)[0].0, login_ok("closed_by_user"));
        w.run();
        let got = lines(&out);
        assert!(got.contains(&"50:Blocked(Declined)".to_owned()), "{got:?}");
        assert!(got.contains(&"1:Declined".to_owned()));
    }

    #[test]
    fn a_js_timeout_blocks_the_run_and_a_new_run_may_prompt() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 2, &out);
        w.complete_ok(windows(&fx)[0].0, login_ok("timeout"));
        w.run();
        assert_eq!(sorted(&out), vec!["1:TimedOut", "2:TimedOut"]);
        spawn_login(&mut w, 1, 3, 0, &out);
        w.run();
        assert_eq!(lines(&out).last().map(String::as_str), Some("3:TimedOut"));
        spawn_login(&mut w, 2, 4, 0, &out);
        w.run();
        assert_eq!(windows(&w.effects()).len(), 1);
    }

    #[test]
    fn a_failed_window_op_is_a_timeout() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        w.complete_err(windows(&fx)[0].0, OpErrKind::Internal);
        w.run();
        assert_eq!(lines(&out), vec!["1:TimedOut"]);
        assert_eq!(state_of(&w, Realm::Openathens), SessionState::SignedOut);
    }

    #[test]
    fn the_engine_timer_closes_a_window_that_nobody_answers() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 2, &out);
        let window_op = windows(&fx)[0].0;
        // JS's window timeout is 300 s; the engine backstop adds a 5 s grace
        w.advance(300_000 + policy::LOGIN_ENGINE_GRACE_MS - 1);
        w.run();
        assert!(out.borrow().is_empty());
        assert!(cancels(&w.effects()).is_empty());
        w.advance(1);
        w.run();
        assert_eq!(sorted(&out), vec!["1:TimedOut", "2:TimedOut"]);
        let fx = w.effects();
        assert_eq!(
            cancels(&fx),
            vec![window_op],
            "dropping the op closes the window"
        );
        assert!(
            !w.shared
                .with(|s| s.login.interactive.busy())
                .expect("state")
        );
        assert_eq!(w.timers(), 0, "no timer is left armed");
    }

    #[test]
    fn keep_waiting_moves_the_engine_deadline() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        let window_op = windows(&fx)[0].0;
        // 295 s in, the user asks for more time: the deadline (305 s after opening) moves by one timeout (300 s)
        w.advance(295_000);
        w.run();
        user_action_now(&w, 1, UserActionKind::KeepWaiting);
        w.run();
        w.advance(10_000); // 305 s: the original deadline passes
        w.run();
        assert!(
            out.borrow().is_empty(),
            "still waiting after the original deadline"
        );
        assert!(cancels(&w.effects()).is_empty());
        w.advance(299_999); // 1 ms before the new deadline (606 s)
        w.run();
        assert!(out.borrow().is_empty());
        w.advance(1);
        w.run();
        assert_eq!(lines(&out), vec!["1:TimedOut"]);
        assert_eq!(cancels(&w.effects()), vec![window_op]);
    }

    #[test]
    fn keep_waiting_without_an_open_window_does_nothing() {
        let w = World::new();
        user_action_now(&w, 1, UserActionKind::KeepWaiting);
        assert_eq!(state_of(&w, Realm::Openathens), SessionState::SignedOut);
    }

    // ----------------------------------------------------------------------------------------- refusals

    #[test]
    fn conditional_access_refuses_every_run_until_retry() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 2, &out);
        let ok = json!({ "result": "ca_blocked", "final_url": "https://login.microsoftonline.com/x", "ca_error": "AADSTS53000", "inspect": { "expect": "none" } });
        w.complete_ok(windows(&fx)[0].0, ok);
        w.run();
        let refused = "Refused { code: Some(\"AADSTS53000\") }";
        assert_eq!(
            sorted(&out),
            vec![format!("1:{refused}"), format!("2:{refused}")]
        );
        assert_eq!(
            state_of(&w, Realm::Openathens),
            SessionState::Refused {
                code: Some("AADSTS53000".into())
            }
        );
        w.effects();
        // any run, no window, no admission
        spawn_login(&mut w, 9, 30, 0, &out);
        let cx = w.item_cx(8, 31);
        let o2 = Rc::clone(&out);
        w.spawn(async move {
            let a = cx.session_admit(Realm::Openathens).await;
            o2.borrow_mut().push(format!("31:{a:?}"));
        });
        w.run();
        let got = lines(&out);
        assert!(got.contains(&format!("30:{refused}")), "{got:?}");
        assert!(got.contains(&format!("31:Blocked({refused})")), "{got:?}");
        assert!(windows(&w.effects()).is_empty());
        // the user's "retry sign-in" lifts the refusal for the next attempt
        user_action_now(&w, 9, UserActionKind::RetrySignIn);
        assert_eq!(state_of(&w, Realm::Openathens), SessionState::SignedOut);
        spawn_login(&mut w, 9, 32, 0, &out);
        w.run();
        assert_eq!(windows(&w.effects()).len(), 1);
    }

    #[test]
    fn an_ca_error_on_a_landed_result_is_still_a_refusal() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        let ok = json!({ "result": "landed", "final_url": "https://x.org/", "ca_error": "AADSTS53003", "inspect": { "expect": "none" } });
        w.complete_ok(windows(&fx)[0].0, ok);
        w.run();
        assert_eq!(
            lines(&out),
            vec!["1:Refused { code: Some(\"AADSTS53003\") }"]
        );
        assert_eq!(
            w.shared.with(|s| s.login.epoch(Realm::Openathens)),
            Some(0),
            "no sign-in is recorded"
        );
    }

    #[test]
    fn ca_blocked_without_a_code_is_refused_with_none() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        w.complete_ok(windows(&fx)[0].0, login_ok("ca_blocked"));
        w.run();
        assert_eq!(lines(&out), vec!["1:Refused { code: None }"]);
    }

    #[test]
    fn a_sign_in_that_does_not_stick_is_ineffective_for_the_run_only() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        w.complete_ok(windows(&fx)[0].0, login_ok("landed"));
        w.run();
        w.effects();
        out.borrow_mut().clear();
        // item 1 of run 1 retries under epoch 1 and bounces to the IdP again
        let cx = w.item_cx(1, 1);
        let o = Rc::clone(&out);
        w.spawn(async move {
            let a = cx.session_admit(Realm::Openathens).await;
            o.borrow_mut().push(format!("admit:{a:?}"));
            cx.session_report(Realm::Openathens, 1, ProbeResult::NeedsLogin);
            let v = cx.session_login(Realm::Openathens, login_req(1)).await;
            o.borrow_mut().push(format!("login:{v:?}"));
        });
        w.run();
        assert_eq!(
            lines(&out),
            vec!["admit:Proceed { epoch: 1 }", "login:Ineffective"]
        );
        assert!(windows(&w.effects()).is_empty(), "no second prompt");
        // the rest of run 1 is blocked without waiting ...
        let cx = w.item_cx(1, 2);
        let o = Rc::clone(&out);
        w.spawn(async move {
            let a = cx.session_admit(Realm::Openathens).await;
            o.borrow_mut().push(format!("again:{a:?}"));
        });
        w.run();
        assert_eq!(
            lines(&out).last().map(String::as_str),
            Some("again:Blocked(Ineffective)")
        );
        // ... but a new run starts from scratch: it probes, and may prompt
        let cx = w.item_cx(2, 3);
        let o = Rc::clone(&out);
        w.spawn(async move {
            let a = cx.session_admit(Realm::Openathens).await;
            o.borrow_mut().push(format!("new run:{a:?}"));
        });
        w.run();
        assert_eq!(
            lines(&out).last().map(String::as_str),
            Some("new run:Proceed { epoch: 1 }")
        );
        assert!(matches!(
            state_of(&w, Realm::Openathens),
            SessionState::Probing { .. }
        ));
    }

    #[test]
    fn a_bounce_under_an_older_epoch_is_not_ineffective() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        w.complete_ok(windows(&fx)[0].0, login_ok("landed"));
        w.run();
        // an attempt that started under epoch 0 reports its bounce only now: it must get a silent retry
        let cx = w.item_cx(1, 40);
        let o = Rc::clone(&out);
        w.spawn(async move {
            cx.session_report(Realm::Openathens, 0, ProbeResult::NeedsLogin);
            let v = cx.session_login(Realm::Openathens, login_req(0)).await;
            o.borrow_mut().push(format!("40:{v:?}"));
        });
        w.run();
        assert_eq!(
            lines(&out).last().map(String::as_str),
            Some("40:Retry { epoch: 1 }")
        );
        assert_eq!(
            state_of(&w, Realm::Openathens),
            SessionState::SignedIn { verified: false }
        );
    }

    #[test]
    fn the_prompt_cap_limits_windows_per_run_but_joining_is_always_allowed() {
        let mut w = World::new();
        let out = log();
        // window 1 (run 1)
        spawn_login(&mut w, 1, 1, 0, &out);
        w.run();
        let first = windows(&w.effects());
        w.complete_ok(first[0].0, login_ok("landed"));
        w.run();
        // window 2 (run 1 again: its resumed attempt bounced under epoch 1); two items of run 1 share it
        spawn_login(&mut w, 1, 2, 1, &out);
        w.run();
        spawn_login(&mut w, 1, 3, 1, &out);
        w.run();
        let fx = w.effects();
        let second = windows(&fx);
        assert_eq!(
            second.len(),
            1,
            "item 3 joined window 2 although run 1 has used its prompts"
        );
        assert_eq!(updates(&fx).last().map(|u| u.1), Some(2));
        w.complete_ok(second[0].0, login_ok("landed"));
        w.run();
        out.borrow_mut().clear();
        // a third bounce: no more windows for run 1
        spawn_login(&mut w, 1, 4, 2, &out);
        w.run();
        assert_eq!(lines(&out), vec!["4:Ineffective"]);
        assert!(windows(&w.effects()).is_empty());
        // another run still gets its own prompts
        spawn_login(&mut w, 2, 5, 2, &out);
        w.run();
        assert_eq!(windows(&w.effects()).len(), 1);
    }

    // -------------------------------------------------------------------------------------- cancellation

    #[test]
    fn when_every_waiter_is_cancelled_the_window_is_closed() {
        let mut w = World::new();
        let out = log();
        let mut ids = Vec::new();
        let mut fx = Vec::new();
        for i in 1..=3 {
            ids.push(spawn_login(&mut w, 1, i, 0, &out));
            w.run();
            fx.extend(w.effects());
        }
        let window_op = windows(&fx)[0].0;
        w.kill(ids[0]);
        w.kill(ids[1]);
        w.run();
        let fx = w.effects();
        assert_eq!(
            updates(&fx).iter().map(|u| u.1).collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert!(
            cancels(&fx).is_empty(),
            "one waiter is left: the window stays"
        );
        w.kill(ids[2]);
        w.run();
        let fx = w.effects();
        assert_eq!(
            cancels(&fx),
            vec![window_op],
            "last waiter gone: dropping the op cancels (closes) the window"
        );
        assert_eq!(updates(&fx).last().map(|u| u.1), Some(0));
        assert_eq!(state_of(&w, Realm::Openathens), SessionState::SignedOut);
        assert!(
            !w.shared
                .with(|s| s.login.interactive.busy())
                .expect("state")
        );
        assert!(out.borrow().is_empty(), "cancelled items observe nothing");
        // the prompt of the shown window stays counted
        assert_eq!(
            w.shared
                .with(|s| s
                    .login
                    .realms
                    .get(&Realm::Openathens)
                    .and_then(|r| r.prompts.get(&1).copied()))
                .flatten(),
            Some(1)
        );
    }

    #[test]
    fn cancelling_the_opener_does_not_close_the_window_for_the_others() {
        let mut w = World::new();
        let out = log();
        let first = spawn_login(&mut w, 1, 1, 0, &out);
        w.run();
        spawn_login(&mut w, 2, 2, 0, &out);
        w.run();
        let window_op = windows(&w.effects())[0].0;
        w.kill(first);
        w.run();
        assert!(cancels(&w.effects()).is_empty());
        w.complete_ok(window_op, login_ok("landed"));
        w.run();
        assert_eq!(lines(&out), vec!["2:Retry { epoch: 1 }"]);
    }

    #[test]
    fn a_driver_that_is_dropped_does_not_leave_the_realm_open() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 2, &out);
        let window_op = windows(&fx)[0].0;
        // tasks so far: item 1 (0), the spawned window driver (1), item 2 (2)
        w.kill(1);
        w.run();
        assert_eq!(
            cancels(&w.effects()),
            vec![window_op],
            "the window is closed with the driver"
        );
        assert_eq!(sorted(&out), vec!["1:TimedOut", "2:TimedOut"]);
        assert_eq!(state_of(&w, Realm::Openathens), SessionState::SignedOut);
        assert!(
            !w.shared
                .with(|s| s.login.interactive.busy())
                .expect("state")
        );
        assert_eq!(w.shared.with(|s| s.login.waiting_user()), Some(0));
    }

    #[test]
    fn a_run_that_ends_takes_its_waiters_with_it_and_the_driver_tells_js() {
        let mut w = World::new();
        let out = log();
        spawn_login(&mut w, 1, 1, 0, &out);
        w.run();
        spawn_login(&mut w, 2, 2, 0, &out);
        w.run();
        let fx = w.effects();
        let window_op = windows(&fx)[0].0;
        let _ = w.shared.with(|s| s.login.on_run_gone(1));
        w.run();
        let fx = w.effects();
        assert_eq!(
            updates(&fx).iter().map(|u| u.1).collect::<Vec<_>>(),
            vec![1]
        );
        assert!(cancels(&fx).is_empty());
        let _ = w.shared.with(|s| s.login.on_run_gone(2));
        w.run();
        assert_eq!(cancels(&w.effects()), vec![window_op]);
    }

    #[test]
    fn a_window_never_shown_does_not_cost_a_prompt() {
        let mut w = World::new();
        w.edit_config(|c| c.inst.auto_open_login = false);
        let out = log();
        let id = spawn_login(&mut w, 1, 1, 0, &out);
        w.run();
        assert_eq!(
            w.shared
                .with(|s| s
                    .login
                    .realms
                    .get(&Realm::Openathens)
                    .and_then(|r| r.prompts.get(&1).copied()))
                .flatten(),
            Some(1)
        );
        w.kill(id);
        w.run();
        assert!(windows(&w.effects()).is_empty());
        assert_eq!(
            w.shared
                .with(|s| s
                    .login
                    .realms
                    .get(&Realm::Openathens)
                    .and_then(|r| r.prompts.get(&1).copied()))
                .flatten(),
            Some(0)
        );
        assert_eq!(state_of(&w, Realm::Openathens), SessionState::SignedOut);
    }

    // -------------------------------------------------------------------------------- user actions / rows

    #[test]
    fn without_auto_open_the_window_waits_for_sign_in_now() {
        let mut w = World::new();
        w.edit_config(|c| c.inst.auto_open_login = false);
        let out = log();
        spawn_login(&mut w, 1, 1, 0, &out);
        w.run();
        spawn_login(&mut w, 1, 2, 0, &out);
        w.run();
        let fx = w.effects();
        assert!(windows(&fx).is_empty(), "nothing opens by itself");
        // the rows carry the Sign in button
        let r = rows(&fx);
        assert_eq!(r.len(), 2);
        assert!(
            r.iter()
                .all(|(_, _, row)| row.actions == vec![RowAction::SignIn]
                    && row.l10n == l10n::id::WAITING_LOGIN)
        );
        // waiting for the user counts as progress for the stall detector
        assert_eq!(w.shared.with(|s| s.login.waiting_user()), Some(2));
        user_action_now(&w, 1, UserActionKind::SignInNow);
        w.run();
        let fx = w.effects();
        let wins = windows(&fx);
        assert_eq!(wins.len(), 1);
        assert_eq!(wins[0].3.waiting, 2, "the window shows both waiting items");
        w.complete_ok(wins[0].0, login_ok("landed"));
        w.run();
        assert_eq!(lines(&out).len(), 2);
    }

    #[test]
    fn skip_sign_in_declines_one_run_and_closes_the_window_when_nobody_is_left() {
        let mut w = World::new();
        let out = log();
        spawn_login(&mut w, 1, 1, 0, &out);
        w.run();
        spawn_login(&mut w, 2, 2, 0, &out);
        w.run();
        let fx = w.effects();
        let window_op = windows(&fx)[0].0;
        // run 1 skips: its item is declined, the window stays for run 2 and JS is told 1 item waits
        user_action_now(&w, 1, UserActionKind::SkipSignIn);
        w.run();
        let fx = w.effects();
        assert_eq!(lines(&out), vec!["1:Declined"]);
        assert_eq!(
            updates(&fx).iter().map(|u| u.1).collect::<Vec<_>>(),
            vec![1]
        );
        assert!(cancels(&fx).is_empty());
        // run 2 skips too: no waiter left, the window is closed
        user_action_now(&w, 2, UserActionKind::SkipSignIn);
        w.run();
        assert_eq!(lines(&out), vec!["1:Declined", "2:Declined"]);
        assert_eq!(cancels(&w.effects()), vec![window_op]);
        assert_eq!(state_of(&w, Realm::Openathens), SessionState::SignedOut);
    }

    #[test]
    fn retry_sign_in_clears_the_block_and_the_prompt_counter() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        w.complete_ok(windows(&fx)[0].0, login_ok("closed_by_user"));
        w.run();
        spawn_login(&mut w, 1, 2, 0, &out);
        w.run();
        assert_eq!(lines(&out).last().map(String::as_str), Some("2:Declined"));
        assert!(windows(&w.effects()).is_empty());
        user_action_now(&w, 1, UserActionKind::RetrySignIn);
        spawn_login(&mut w, 1, 3, 0, &out);
        w.run();
        assert_eq!(
            windows(&w.effects()).len(),
            1,
            "after RetrySignIn the run may be prompted again"
        );
    }

    #[test]
    fn retry_sign_in_also_resets_the_prompt_cap() {
        let mut w = World::new();
        let out = log();
        for (item, seen) in [(1, 0), (2, 1)] {
            spawn_login(&mut w, 1, item, seen, &out);
            w.run();
            let fx = w.effects();
            w.complete_ok(windows(&fx)[0].0, login_ok("landed"));
            w.run();
        }
        spawn_login(&mut w, 1, 3, 2, &out);
        w.run();
        assert_eq!(
            lines(&out).last().map(String::as_str),
            Some("3:Ineffective")
        );
        user_action_now(&w, 1, UserActionKind::RetrySignIn);
        spawn_login(&mut w, 1, 4, 2, &out);
        w.run();
        assert_eq!(windows(&w.effects()).len(), 1);
    }

    // ------------------------------------------------------------------------------------ admission

    fn admit_task(w: &mut World, run: RunId, item: ItemId, name: &'static str, out: &Log) -> usize {
        let (cx, out) = (w.item_cx(run, item), Rc::clone(out));
        w.spawn(async move {
            let a = cx.session_admit(Realm::Openathens).await;
            out.borrow_mut().push(format!("{name}:{a:?}"));
        })
    }

    #[test]
    fn the_first_caller_probes_alone_and_the_rest_wait_for_its_report() {
        let mut w = World::new();
        let out = log();
        admit_task(&mut w, 1, 1, "a", &out);
        admit_task(&mut w, 1, 2, "b", &out);
        admit_task(&mut w, 1, 3, "c", &out);
        w.run();
        assert_eq!(lines(&out), vec!["a:Proceed { epoch: 0 }"]);
        assert!(matches!(
            state_of(&w, Realm::Openathens),
            SessionState::Probing { since: 1_000 }
        ));
        assert_eq!(w.shared.with(|s| s.login.waiting_user()), Some(2));
        // a reaches the publisher: b and c go ahead together
        w.item_cx(1, 1)
            .session_report(Realm::Openathens, 0, ProbeResult::LoggedIn);
        w.run();
        assert_eq!(
            lines(&out),
            vec![
                "a:Proceed { epoch: 0 }",
                "b:Proceed { epoch: 0 }",
                "c:Proceed { epoch: 0 }"
            ]
        );
        assert_eq!(
            state_of(&w, Realm::Openathens),
            SessionState::SignedIn { verified: true }
        );
        assert_eq!(w.shared.with(|s| s.login.waiting_user()), Some(0));
        // later callers do not wait at all
        admit_task(&mut w, 2, 9, "d", &out);
        w.run();
        assert_eq!(
            lines(&out).last().map(String::as_str),
            Some("d:Proceed { epoch: 0 }")
        );
    }

    #[test]
    fn a_bounced_probe_opens_the_window_and_waiting_callers_proceed_after_the_sign_in() {
        let mut w = World::new();
        let out = log();
        // the prober bounces and asks for the window
        let cx = w.item_cx(1, 1);
        let o = Rc::clone(&out);
        w.spawn(async move {
            let a = cx.session_admit(Realm::Openathens).await;
            o.borrow_mut().push(format!("a:{a:?}"));
            cx.session_report(Realm::Openathens, 0, ProbeResult::NeedsLogin);
            let v = cx.session_login(Realm::Openathens, login_req(0)).await;
            o.borrow_mut().push(format!("a login:{v:?}"));
        });
        w.run();
        admit_task(&mut w, 1, 2, "b", &out);
        w.run();
        let fx = w.effects();
        assert_eq!(windows(&fx).len(), 1);
        assert_eq!(
            lines(&out),
            vec!["a:Proceed { epoch: 0 }"],
            "b waits behind the window"
        );
        w.complete_ok(windows(&fx)[0].0, login_ok("landed"));
        w.run();
        let got = lines(&out);
        assert!(
            got.contains(&"a login:Retry { epoch: 1 }".to_owned()),
            "{got:?}"
        );
        assert!(
            got.contains(&"b:Proceed { epoch: 1 }".to_owned()),
            "{got:?}"
        );
    }

    #[test]
    fn a_failed_probe_hands_the_probe_to_the_next_caller() {
        let mut w = World::new();
        let out = log();
        admit_task(&mut w, 1, 1, "a", &out);
        admit_task(&mut w, 1, 2, "b", &out);
        w.run();
        assert_eq!(lines(&out).len(), 1);
        // a different item cannot release a's lock
        w.item_cx(1, 2)
            .session_report(Realm::Openathens, 0, ProbeResult::Failed);
        w.run();
        assert_eq!(lines(&out).len(), 1);
        w.item_cx(1, 1)
            .session_report(Realm::Openathens, 0, ProbeResult::Failed);
        w.run();
        assert_eq!(
            lines(&out),
            vec!["a:Proceed { epoch: 0 }", "b:Proceed { epoch: 0 }"]
        );
        assert!(matches!(
            state_of(&w, Realm::Openathens),
            SessionState::Probing { .. }
        ));
    }

    #[test]
    fn a_probe_that_is_never_reported_is_taken_over_after_the_stale_limit() {
        let mut w = World::new();
        let out = log();
        let a = admit_task(&mut w, 1, 1, "a", &out);
        admit_task(&mut w, 1, 2, "b", &out);
        w.run();
        w.kill(a); // item a was cancelled after it was admitted: it never reports
        w.advance(policy::PROBE_STALE_MS - 1);
        w.run();
        assert_eq!(lines(&out).len(), 1);
        assert_eq!(
            w.timers(),
            1,
            "the parked caller armed a timer for the take-over"
        );
        w.advance(1);
        w.run();
        assert_eq!(
            lines(&out),
            vec!["a:Proceed { epoch: 0 }", "b:Proceed { epoch: 0 }"]
        );
        assert_eq!(w.timers(), 0);
    }

    #[test]
    fn dropping_a_parked_admit_leaves_nothing_behind() {
        let mut w = World::new();
        let out = log();
        admit_task(&mut w, 1, 1, "a", &out);
        let b = admit_task(&mut w, 1, 2, "b", &out);
        w.run();
        assert_eq!(w.shared.with(|s| s.login.waiting_user()), Some(1));
        assert_eq!(w.timers(), 1);
        w.kill(b);
        assert_eq!(w.shared.with(|s| s.login.waiting_user()), Some(0));
        assert_eq!(w.timers(), 0);
    }

    #[test]
    fn the_end_of_the_probers_run_releases_the_probe() {
        let mut w = World::new();
        let out = log();
        admit_task(&mut w, 1, 1, "a", &out);
        admit_task(&mut w, 2, 2, "b", &out);
        w.run();
        let _ = w.shared.with(|s| s.login.on_run_gone(1));
        w.run();
        assert_eq!(
            lines(&out),
            vec!["a:Proceed { epoch: 0 }", "b:Proceed { epoch: 0 }"]
        );
    }

    #[test]
    fn a_misconfigured_realm_refuses_admission_and_logins_for_everyone() {
        let mut w = World::new();
        let out = log();
        admit_task(&mut w, 1, 1, "a", &out);
        w.run();
        w.item_cx(1, 1)
            .session_report(Realm::Openathens, 0, ProbeResult::Misconfigured);
        admit_task(&mut w, 2, 2, "b", &out);
        spawn_login(&mut w, 3, 3, 0, &out);
        w.run();
        let got = lines(&out);
        assert!(
            got.contains(&"b:Blocked(Misconfigured)".to_owned()),
            "{got:?}"
        );
        assert!(got.contains(&"3:Misconfigured".to_owned()), "{got:?}");
        assert!(windows(&w.effects()).is_empty());
        // the other realm is unaffected
        let cx = w.item_cx(4, 4);
        let o = Rc::clone(&out);
        w.spawn(async move {
            let a = cx.session_admit(Realm::Ezproxy).await;
            o.borrow_mut().push(format!("ez:{a:?}"));
        });
        w.run();
        assert_eq!(
            lines(&out).last().map(String::as_str),
            Some("ez:Proceed { epoch: 0 }")
        );
    }

    #[test]
    fn a_suspend_page_halts_the_realm_and_the_whole_institutional_route() {
        let mut w = World::new();
        let out = log();
        admit_task(&mut w, 1, 1, "a", &out);
        w.run();
        w.effects();
        w.item_cx(1, 1)
            .session_report(Realm::Openathens, 0, ProbeResult::Suspended);
        let fx = w.effects();
        assert_eq!(
            fx.iter()
                .filter(|e| matches!(
                    e,
                    Effect::Persist {
                        key: PersistKey::Halts,
                        flush: true,
                        ..
                    }
                ))
                .count(),
            1,
            "the halt is persisted"
        );
        assert_eq!(
            w.item_cx(1, 1).inst_halted().map(|(r, _)| r),
            Some("ezproxy_suspended".to_owned())
        );
        admit_task(&mut w, 2, 2, "b", &out);
        w.run();
        assert_eq!(
            lines(&out).last().map(String::as_str),
            Some("b:Blocked(Suspended)")
        );
    }

    #[test]
    fn shutdown_cancels_every_waiter() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 2, &out);
        assert_eq!(windows(&fx).len(), 1);
        admit_task(&mut w, 3, 9, "p", &out);
        w.run();
        let mut exec = Executor::new(8);
        let mut ctl = Ctl {
            shared: &w.shared,
            exec: &mut exec,
        };
        on_shutdown(&mut ctl);
        w.run();
        let got = lines(&out);
        assert_eq!(
            got.iter().filter(|l| l.ends_with("Cancelled")).count(),
            2,
            "{got:?}"
        );
        assert!(got.contains(&"p:Blocked(Cancelled)".to_owned()), "{got:?}");
        // later calls resolve the same way and open nothing
        spawn_login(&mut w, 5, 5, 0, &out);
        w.run();
        assert_eq!(lines(&out).last().map(String::as_str), Some("5:Cancelled"));
        assert!(windows(&w.effects()).is_empty());
    }

    // --------------------------------------------------------------------------- the interactive gate

    fn challenge_req(url: &str) -> LoginReq {
        LoginReq {
            kind: LoginKind::Challenge,
            realm: Realm::Openathens,
            entry_url: url.into(),
            item_title: "t".into(),
            waiting: 0,
            timeout_ms: 120_000,
            capture_pdf: true,
        }
    }

    fn spawn_challenge(
        w: &mut World,
        run: RunId,
        item: ItemId,
        url: &'static str,
        out: &Log,
    ) -> usize {
        let (cx, out) = (w.item_cx(run, item), Rc::clone(out));
        w.spawn(async move {
            let r = cx.challenge_window(challenge_req(url)).await;
            out.borrow_mut().push(format!(
                "{item}:{}",
                match r {
                    Ok(resp) => format!("ok {:?}", resp.result),
                    Err(e) => format!("err {:?}", e.kind),
                }
            ));
        })
    }

    #[test]
    fn a_challenge_window_opens_alone_and_returns_the_answer() {
        let mut w = World::new();
        let out = log();
        spawn_challenge(&mut w, 1, 7, "https://www.sciencedirect.com/x", &out);
        w.run();
        let fx = w.effects();
        let wins = windows(&fx);
        assert_eq!(wins.len(), 1);
        assert_eq!(
            (wins[0].1, wins[0].2, wins[0].3.kind),
            (1, 7, LoginKind::Challenge)
        );
        assert!(
            w.shared
                .with(|s| s.login.interactive.busy())
                .expect("state")
        );
        w.complete_ok(wins[0].0, login_ok("landed"));
        w.run();
        assert_eq!(lines(&out), vec!["7:ok Landed"]);
        assert!(
            !w.shared
                .with(|s| s.login.interactive.busy())
                .expect("state")
        );
    }

    #[test]
    fn a_challenge_window_waits_behind_a_login_window() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 1, &out);
        let login_op = windows(&fx)[0].0;
        spawn_challenge(&mut w, 1, 7, "https://www.sciencedirect.com/x", &out);
        w.run();
        assert!(
            windows(&w.effects()).is_empty(),
            "only one visible window at a time"
        );
        w.complete_ok(login_op, login_ok("landed"));
        w.run();
        let fx = w.effects();
        let wins = windows(&fx);
        assert_eq!(
            wins.len(),
            1,
            "the challenge window opens once the login window is done"
        );
        assert_eq!(wins[0].3.kind, LoginKind::Challenge);
        assert_eq!(wins[0].3.entry_url, "https://www.sciencedirect.com/x");
        w.complete_ok(wins[0].0, login_ok("landed"));
        w.run();
        assert!(lines(&out).contains(&"7:ok Landed".to_owned()));
    }

    #[test]
    fn a_login_overtakes_queued_challenges() {
        let mut w = World::new();
        let out = log();
        // challenge A holds the gate, challenge B queues, then a login arrives
        spawn_challenge(&mut w, 1, 1, "https://a.example/", &out);
        w.run();
        let a = windows(&w.effects())[0].0;
        spawn_challenge(&mut w, 1, 2, "https://b.example/", &out);
        w.run();
        spawn_login(&mut w, 1, 3, 0, &out);
        w.run();
        assert!(windows(&w.effects()).is_empty());
        w.complete_ok(a, login_ok("landed"));
        w.run();
        let fx = w.effects();
        let wins = windows(&fx);
        assert_eq!(wins.len(), 1);
        assert_eq!(wins[0].3.kind, LoginKind::Login, "login before challenge");
        w.complete_ok(wins[0].0, login_ok("landed"));
        w.run();
        let fx = w.effects();
        let wins = windows(&fx);
        assert_eq!(wins.len(), 1);
        assert_eq!(wins[0].3.entry_url, "https://b.example/");
    }

    #[test]
    fn challenge_windows_are_fifo_and_a_declined_one_is_cancelled() {
        let mut w = World::new();
        let out = log();
        spawn_challenge(&mut w, 1, 1, "https://a.example/", &out);
        w.run();
        let a = windows(&w.effects())[0].0;
        spawn_challenge(&mut w, 1, 2, "https://b.example/", &out);
        w.run();
        w.complete_ok(a, login_ok("closed_by_user"));
        w.run();
        assert_eq!(
            lines(&out),
            vec!["1:err Cancelled"],
            "the user declined: Cancelled"
        );
        let wins = windows(&w.effects());
        assert_eq!(wins.len(), 1, "the next challenge opens");
        assert_eq!(wins[0].3.entry_url, "https://b.example/");
    }

    #[test]
    fn dropping_a_challenge_closes_its_window_and_frees_the_gate() {
        let mut w = World::new();
        let out = log();
        let id = spawn_challenge(&mut w, 1, 1, "https://a.example/", &out);
        w.run();
        let op = windows(&w.effects())[0].0;
        spawn_challenge(&mut w, 1, 2, "https://b.example/", &out);
        w.run();
        w.kill(id);
        w.run();
        let fx = w.effects();
        assert_eq!(cancels(&fx), vec![op]);
        assert_eq!(windows(&fx).len(), 1, "the queued challenge got the gate");
    }

    #[test]
    fn a_run_that_skipped_sign_in_gets_no_challenge_windows() {
        let mut w = World::new();
        let out = log();
        user_action_now(&w, 1, UserActionKind::SkipSignIn);
        spawn_challenge(&mut w, 1, 1, "https://a.example/", &out);
        w.run();
        assert_eq!(lines(&out), vec!["1:err Cancelled"]);
        assert!(windows(&w.effects()).is_empty());
        // another run is unaffected
        spawn_challenge(&mut w, 2, 2, "https://b.example/", &out);
        w.run();
        assert_eq!(windows(&w.effects()).len(), 1);
    }

    #[test]
    fn challenge_windows_are_cancelled_at_shutdown() {
        let mut w = World::new();
        let out = log();
        spawn_challenge(&mut w, 1, 1, "https://a.example/", &out);
        w.run();
        spawn_challenge(&mut w, 1, 2, "https://b.example/", &out);
        w.run();
        let mut exec = Executor::new(8);
        let mut ctl = Ctl {
            shared: &w.shared,
            exec: &mut exec,
        };
        on_shutdown(&mut ctl);
        // the queued challenge is woken and refuses; the holder's window still waits for its own answer
        w.run();
        assert!(
            lines(&out).contains(&"2:err Cancelled".to_owned()),
            "{:?}",
            lines(&out)
        );
    }

    #[test]
    fn interactive_gate_orders_by_kind_then_arrival() {
        let (_, waker) = flag_waker();
        let mut g = InteractiveGate::default();
        let c1 = g.enqueue(LoginKind::Challenge);
        let c2 = g.enqueue(LoginKind::Challenge);
        let l1 = g.enqueue(LoginKind::Login);
        let l2 = g.enqueue(LoginKind::Login);
        assert!(!g.try_take(c1, &waker), "logins are ahead");
        assert!(g.try_take(l1, &waker));
        assert!(g.busy());
        assert!(!g.try_take(l2, &waker), "busy");
        assert!(
            g.release(l1).is_some(),
            "l2 waited with a waker: it is told"
        );
        assert!(g.try_take(l2, &waker));
        g.release(l2);
        assert!(g.try_take(c1, &waker));
        // leaving the queue (never held) frees the way for the next waiter and returns its waker
        let (flag, w2) = flag_waker();
        assert!(!g.try_take(c2, &w2));
        let woken = g.release(c1).expect("c2 is next");
        woken.wake();
        assert_eq!(flag.count(), 1);
        assert!(g.try_take(c2, &w2));
        // an unknown ticket changes nothing
        assert!(g.release(12345).is_none());
        assert!(g.busy());
    }

    // ------------------------------------------------------------------------------------------ stress

    /// An item that keeps trying the redirector like `inst.rs` does: admit, load (50 ms), report, and on a bounce
    /// join the sign-in and retry silently. `valid` is the "server side truth" the test flips.
    fn traffic_item(
        w: &mut World,
        run: RunId,
        item: ItemId,
        start: u64,
        valid: &Rc<Cell<bool>>,
        out: &Log,
    ) -> usize {
        let (cx, valid, out) = (w.item_cx(run, item), Rc::clone(valid), Rc::clone(out));
        w.spawn(async move {
            cx.sleep(start).await;
            for _ in 0..4 {
                match cx.session_admit(Realm::Openathens).await {
                    Admission::Blocked(v) => {
                        out.borrow_mut().push(format!("{item}:blocked {v:?}"));
                        return;
                    }
                    Admission::Proceed { epoch } => {
                        cx.sleep(50).await;
                        if valid.get() {
                            cx.session_report(Realm::Openathens, epoch, ProbeResult::LoggedIn);
                            out.borrow_mut().push(format!("{item}:ok"));
                            return;
                        }
                        cx.session_report(Realm::Openathens, epoch, ProbeResult::NeedsLogin);
                        match cx.session_login(Realm::Openathens, login_req(epoch)).await {
                            SessionVerdict::Retry { .. } => {}
                            other => {
                                out.borrow_mut().push(format!("{item}:{other:?}"));
                                return;
                            }
                        }
                    }
                }
            }
            out.borrow_mut().push(format!("{item}:gave up"));
        })
    }

    fn track_windows(pending: &mut BTreeSet<OpId>, fx: &[Effect]) {
        for e in fx {
            match e {
                Effect::LoginWindow { op, .. } => {
                    pending.insert(*op);
                }
                Effect::Cancel { op } => {
                    pending.remove(op);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn random_traffic_keeps_one_window_and_strands_nobody() {
        use crate::sched::testkit::Lcg;
        let mut windows_seen = 0usize;
        for seed in 1..=30u64 {
            let mut w = World::new();
            let mut rng = Lcg(seed.wrapping_mul(104_729));
            let out = log();
            let valid = Rc::new(Cell::new(false));
            let mut ids = Vec::new();
            for item in 1..=24u32 {
                let run = 1 + item % 3;
                let start = rng.below(60_000);
                ids.push(traffic_item(&mut w, run, item, start, &valid, &out));
            }
            let mut pending = BTreeSet::new();
            let mut killed = BTreeSet::new();
            for _ in 0..150 {
                w.run();
                let fx = w.effects();
                windows_seen += windows(&fx).len();
                track_windows(&mut pending, &fx);
                assert!(
                    pending.len() <= 1,
                    "seed {seed}: {} windows open at once",
                    pending.len()
                );
                if let Some(op) = pending.iter().next().copied()
                    && rng.below(3) == 0
                {
                    let answer = [
                        "landed",
                        "landed",
                        "landed",
                        "pdf",
                        "closed_by_user",
                        "timeout",
                        "ca_blocked",
                        "handoff",
                    ][rng.below(8) as usize];
                    w.complete_ok(op, login_ok(answer));
                    pending.remove(&op);
                    if answer == "landed" || answer == "pdf" {
                        valid.set(true);
                    }
                }
                match rng.below(14) {
                    0 => valid.set(false), // the session expires
                    1 => user_action_now(&w, 1 + rng.below(3) as u32, UserActionKind::SkipSignIn),
                    2 => user_action_now(&w, 1 + rng.below(3) as u32, UserActionKind::KeepWaiting),
                    3 => user_action_now(&w, 1 + rng.below(3) as u32, UserActionKind::RetrySignIn),
                    4 => {
                        let victim = ids[rng.below(ids.len() as u64) as usize];
                        if !w.finished(victim) {
                            w.kill(victim);
                            killed.insert(victim);
                        }
                    }
                    _ => {}
                }
                w.advance(rng.below(40_000) + 1);
            }
            // drain: answer what is open, let every timer run out
            valid.set(true);
            for _ in 0..80 {
                w.run();
                let fx = w.effects();
                windows_seen += windows(&fx).len();
                track_windows(&mut pending, &fx);
                assert!(
                    pending.len() <= 1,
                    "seed {seed}: {} windows open at once",
                    pending.len()
                );
                if let Some(op) = pending.iter().next().copied() {
                    w.complete_ok(op, login_ok("landed"));
                    pending.remove(&op);
                }
                w.advance(200_000);
            }
            w.run();
            for id in &ids {
                assert!(
                    killed.contains(id) || w.finished(*id),
                    "seed {seed}: item task {id} is stranded; log {:?}",
                    lines(&out)
                );
            }
            let (waiting, busy, open, parties) = w
                .shared
                .with(|s| {
                    (
                        s.login.waiting_user(),
                        s.login.interactive.busy(),
                        s.login.realms.values().any(|r| r.is_open()),
                        s.login
                            .realms
                            .values()
                            .map(|r| r.parties.len())
                            .sum::<usize>(),
                    )
                })
                .expect("state");
            assert_eq!(
                (waiting, busy, open, parties),
                (0, false, false, 0),
                "seed {seed}: login gate not idle"
            );
            assert_eq!(w.timers(), 0, "seed {seed}: timers left");
        }
        assert!(
            windows_seen > 20,
            "the traffic opened only {windows_seen} windows"
        );
    }

    // ------------------------------------------------------------------------------------- misc state

    #[test]
    fn settled_verdict_precedence() {
        let mut r = RealmState::new();
        assert_eq!(settled_verdict(&r, 1, 0), None);
        r.blocked_runs.insert(1, HaltWhy::Declined);
        assert_eq!(settled_verdict(&r, 1, 0), Some(SessionVerdict::Declined));
        assert_eq!(settled_verdict(&r, 2, 0), None);
        r.epoch = 3;
        assert_eq!(
            settled_verdict(&r, 1, 2),
            Some(SessionVerdict::Retry { epoch: 3 }),
            "a newer sign-in beats a decline"
        );
        assert_eq!(settled_verdict(&r, 1, 3), Some(SessionVerdict::Declined));
        r.state = SessionState::Refused { code: None };
        assert_eq!(
            settled_verdict(&r, 2, 3),
            Some(SessionVerdict::Refused { code: None })
        );
        r.state = SessionState::Halted {
            why: HaltWhy::Suspended,
        };
        assert_eq!(settled_verdict(&r, 2, 3), Some(SessionVerdict::Suspended));
    }

    #[test]
    fn dump_state_shows_realms_prompts_and_blocks() {
        let mut w = World::new();
        let out = log();
        let fx = open_window(&mut w, 2, &out);
        let dump = w.shared.with(|s| s.login.dump()).expect("state");
        assert_eq!(dump.pointer("/realms/openathens/waiting"), Some(&json!(2)));
        assert_eq!(
            dump.pointer("/realms/openathens/prompts/1"),
            Some(&json!(1))
        );
        assert_eq!(
            dump.pointer("/realms/ezproxy/state"),
            Some(&json!("SignedOut"))
        );
        assert_eq!(dump.pointer("/interactive/queued"), Some(&json!(0)));
        assert!(
            dump.pointer("/interactive/held_by")
                .is_some_and(|v| v.is_number())
        );
        assert!(
            dump.pointer("/realms/openathens/state")
                .and_then(Value::as_str)
                .is_some_and(|s| s.starts_with("LoginOpen"))
        );
        w.complete_ok(windows(&fx)[0].0, login_ok("closed_by_user"));
        w.run();
        let dump = w.shared.with(|s| s.login.dump()).expect("state");
        assert_eq!(
            dump.pointer("/realms/openathens/blocked_runs/1"),
            Some(&json!("Declined"))
        );
        assert_eq!(dump.pointer("/interactive/held_by"), Some(&Value::Null));
    }

    #[test]
    fn waiting_user_counts_login_and_admit_parties_of_both_realms() {
        let mut g = LoginGate::new();
        assert_eq!(g.waiting_user(), 0);
        let oa = realm_mut(&mut g, Realm::Openathens);
        oa.add_party(1, PartyKind::Login);
        oa.add_party(2, PartyKind::Admit);
        realm_mut(&mut g, Realm::Ezproxy).add_party(1, PartyKind::Login);
        assert_eq!(g.waiting_user(), 3);
        g.on_run_gone(1);
        assert_eq!(g.waiting_user(), 1);
        g.on_run_gone(2);
        assert_eq!(g.waiting_user(), 0);
    }

    #[test]
    fn both_realms_have_independent_windows() {
        let mut w = World::new();
        let out = log();
        spawn_login(&mut w, 1, 1, 0, &out);
        w.run();
        let ez = w.item_cx(1, 2);
        let o = Rc::clone(&out);
        w.spawn(async move {
            let v = ez.session_login(Realm::Ezproxy, login_req(0)).await;
            o.borrow_mut().push(format!("ez:{v:?}"));
        });
        w.run();
        let fx = w.effects();
        let wins = windows(&fx);
        assert_eq!(
            wins.len(),
            1,
            "the second realm's window queues behind the first (one visible window at a time)"
        );
        w.complete_ok(wins[0].0, login_ok("landed"));
        w.run();
        let fx = w.effects();
        let wins = windows(&fx);
        assert_eq!(wins.len(), 1, "now the EZproxy window opens");
        assert_eq!(wins[0].3.realm, Realm::Ezproxy);
        assert_eq!(w.shared.with(|s| s.login.epoch(Realm::Openathens)), Some(1));
        assert_eq!(w.shared.with(|s| s.login.epoch(Realm::Ezproxy)), Some(0));
    }
}
