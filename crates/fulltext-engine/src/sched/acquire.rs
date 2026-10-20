//! The async face of the scheduler: acquire futures, RAII permits, the ledger ticket, and the
//! pipeline-facing methods on `Cx`. Everything the pipeline needs from `sched` is in this file's `impl Cx`.
//!
//! # Contract (all of it testable with a manual-poll helper; no executor needed)
//! * An `Acquire*` future enqueues a ticket on its first poll and keeps it for its whole life. Dropping the future
//!   before it resolves calls `Limiter::forget` (passing the baton to the next waiter). Dropping a permit
//!   releases it (`Limiter::release` with the gap for its kind) and wakes the waiters that release returns.
//! * When the only obstacle is time (gap, token) the future arms a virtual timer (`Reactor::add_timer`) for
//!   `retry_at` and removes it on drop; when the obstacle is capacity it relies on the release wake-up.
//! * `Refused` is returned immediately (no waiting) when the route is halted, the host is skipped, or a cap
//!   would be exceeded; it is re-checked after every wake-up (a halt can start while an item waits).
//! * Permits and tickets are never stored inside `State` (cx.rs rule 2).
//!
//! # Institutional grants
//! `AcquireInst` needs the publisher-family gate AND the global gate (2 in flight overall). It waits on the
//! family queue first; only the family's head asks for a global place, and it takes both in one `with`
//! (dry run of both heads, then take both). An item that waits for a busy or recently used family therefore
//! never sits in the global queue, so it cannot block other families (no head-of-line blocking), and a holder
//! of one gate never waits for the other (no deadlock). The family gap is stamped when the `InstPermit` drops:
//! page 2 s, PDF `max(5 s, min_gap_sec)` + 0-3 s of jitter; the global gate gets no gap.
//!
//! # Order of acquisition (callers; this is what keeps the gates deadlock-free)
//! An item takes `Items` first, then (institutional route) `acquire_inst`, and only then `acquire_browser`; it
//! never waits for an `InstPermit` while holding a browser slot (item A holding the browser and wanting the
//! family while item B holds the family and wants the browser would wait for each other). Nothing is held while
//! waiting on the login gate: the item slot is released first (`pipeline::parked`). `Save` and `Bytes` are
//! leaf gates. OA host / API permits are short and taken alone.
//!
//! # Failure feedback
//! `Permit::report(ok)` feeds two circuit breakers: 5 failures in a row (or 2 challenges, `Cx::note_challenge`)
//! skip an OA host for 30 minutes (`Cx::wall_host` does it at once, for a hard block), 3 in a row switch an OA API
//! off for 5 minutes (`Cx::disable_api[_for]` does the same on demand, e.g. after a 429 with a `Retry-After`).
//! `Cx::block_host` is the persisted variant for a publisher family. Queued waiters are woken when a breaker trips
//! so they see `Refused::HostSkipped` immediately. Host keys ignore case and a leading `www.`.
//!
//! # Licence tickets
//! `reserve_licence` counts the reservation and pushes `Persist { Ledger, flush: true }` BEFORE it returns, so
//! the caller's `Http { barrier: true }` effect (pushed later) always follows the persist in the outbox. A ticket
//! dropped unresolved commits its estimate when `mark_sent` was called and rolls back otherwise.
use crate::cx::{Cx, Shared, State};
use crate::ops::{Reactor, TimerKey};
use crate::proto::*;
use crate::sched::gate::{GatePoll, Prio, Ticket};
use crate::sched::hosts::{HaltReason, host_key_norm};
use crate::sched::policy::{self, FetchClass};
use crate::sched::{FetchKind, GateKey, InstKey, Refused};
use fulltext_core::model::*;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

fn wake_all(wakers: Vec<Waker>) {
    wakers.into_iter().for_each(Waker::wake);
}

/// A held slot on one limiter. `report(ok)` records the outcome for hosts that track consecutive failures.
/// Dropping releases it.
pub(crate) struct Permit {
    shared: Rc<Shared>,
    key: GateKey,
    weight: u64,
    /// Gap stamped on the limiter at release (ms).
    gap_ms: u32,
    outcome: Option<bool>,
}

impl Permit {
    /// Record success (`true`) or failure (`false`) of the work done under this permit. OA hosts: 5 consecutive
    /// failures skip the host for 30 minutes. OA APIs: 3 consecutive failures switch the API off for 5 minutes.
    /// Other gates ignore it. The last call wins; no call counts as neither.
    pub(crate) fn report(&mut self, ok: bool) {
        self.outcome = Some(ok);
    }

    #[cfg(test)]
    pub(crate) fn weight(&self) -> u64 {
        self.weight
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let (key, weight, gap, outcome) =
            (self.key.clone(), self.weight, self.gap_ms, self.outcome);
        let wakers = self.shared.with(|s| {
            let mut wakers = Vec::new();
            let now = s.clock.now;
            if let Some(ok) = outcome {
                match &key {
                    GateKey::OaHost(host) => {
                        s.sched.hosts.note_outcome_at(host, ok, now);
                        if s.sched.hosts.is_skipped_at(host, now) {
                            // The host just got skipped: everyone queued behind this permit must see the refusal now.
                            wakers.extend(s.sched.gate_wakers(&key));
                        }
                    }
                    GateKey::Api(src) => {
                        let tripped = s.sched.hosts.note_api_outcome(*src, ok, now);
                        if tripped {
                            wakers.extend(s.sched.gate_wakers(&key));
                        }
                    }
                    GateKey::Items
                    | GateKey::InstHost(_)
                    | GateKey::InstGlobal
                    | GateKey::Browser
                    | GateKey::Save
                    | GateKey::Bytes => {}
                }
            }
            wakers.extend(s.sched.limiter_mut(&key).release(weight, now, gap));
            wakers
        });
        wake_all(wakers.unwrap_or_default());
    }
}

/// An item slot; same type, different key.
pub(crate) type ItemPermit = Permit;

/// A held institutional slot: the publisher-family gate AND the global cap, granted together.
/// Dropping releases both and stamps the gap for its `FetchKind` (page 2 s; PDF 5 s + jitter).
pub(crate) struct InstPermit {
    shared: Rc<Shared>,
    host: InstKey,
    kind: FetchKind,
}

impl InstPermit {
    #[cfg(test)]
    pub(crate) fn kind(&self) -> FetchKind {
        self.kind
    }

    #[cfg(test)]
    pub(crate) fn host(&self) -> &InstKey {
        &self.host
    }
}

impl Drop for InstPermit {
    fn drop(&mut self) {
        let (host, kind) = (self.host.clone(), self.kind);
        let wakers = self.shared.with(|s| {
            let now = s.clock.now;
            let gap = s.sched.inst_gap_ms(kind);
            let mut wakers = s
                .sched
                .limiter_mut(&GateKey::InstHost(host))
                .release(1, now, gap);
            wakers.extend(s.sched.limiter_mut(&GateKey::InstGlobal).release(1, now, 0));
            wakers
        });
        wake_all(wakers.unwrap_or_default());
    }
}

/// A licence reservation. See ledger.rs. Lifecycle:
/// `reserve_licence` -> (`mark_sent` when the request effect is about to be emitted) -> `commit(bytes)` on a
/// verified PDF | `rollback()` on a non-PDF response or when the item gives up before sending.
/// Dropping an unresolved ticket: if `mark_sent` was called it COMMITS the estimate (the request may have
/// reached the publisher; never under-count), otherwise it rolls back.
pub(crate) struct LedgerTicket {
    shared: Rc<Shared>,
    id: u64,
    est_bytes: u64,
    sent: bool,
    resolved: bool,
}

impl LedgerTicket {
    pub(crate) fn mark_sent(&mut self) {
        self.sent = true;
    }

    /// Count the fetch with its real size and persist.
    pub(crate) fn commit(mut self, bytes: u64) {
        self.resolved = true;
        let (id, shared) = (self.id, Rc::clone(&self.shared));
        let _ = shared.with(|s| {
            let wall = s.clock.wall;
            if s.sched.ledger.commit(id, wall, bytes) {
                let eff = s.sched.ledger.persist_effect(true);
                s.outbox.push(eff);
            }
        });
    }

    /// Release the reservation and persist.
    pub(crate) fn rollback(mut self) {
        self.resolved = true;
        let (id, shared) = (self.id, Rc::clone(&self.shared));
        let _ = shared.with(|s| {
            if s.sched.ledger.rollback(id) {
                let eff = s.sched.ledger.persist_effect(true);
                s.outbox.push(eff);
            }
        });
    }
}

impl Drop for LedgerTicket {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        let (id, est, sent, shared) = (self.id, self.est_bytes, self.sent, Rc::clone(&self.shared));
        let _ = shared.with(|s| {
            let wall = s.clock.wall;
            let changed = if sent {
                s.sched.ledger.commit(id, wall, est)
            } else {
                s.sched.ledger.rollback(id)
            };
            if changed {
                let eff = s.sched.ledger.persist_effect(true);
                s.outbox.push(eff);
            }
        });
    }
}

// ------------------------------------------------------------------------------------------ shared helpers

/// Outcome of one poll step of an acquire future.
enum Step {
    Granted,
    Pending,
    Refused(Refused),
}

/// Arm (or move, or clear) the virtual timer that wakes a waiter whose only obstacle is time. The timer is
/// re-armed when the instant changed (a later release moved `next_at`), and removed when the obstacle is capacity.
/// Shared with the login gate (probe take-over, window deadline).
pub(crate) fn arm_timer(
    reactor: &mut Reactor,
    timer: &mut Option<TimerKey>,
    retry_at: Option<u64>,
    waker: &Waker,
) {
    match (retry_at, *timer) {
        (Some(t), Some(k)) if k.0 == t && reactor.timer_pending(k) => reactor.park_timer(k, waker),
        (Some(t), old) => {
            if let Some(k) = old {
                reactor.remove_timer(k);
            }
            *timer = Some(reactor.add_timer(t, waker));
        }
        (None, old) => {
            if let Some(k) = old {
                reactor.remove_timer(k);
            }
            *timer = None;
        }
    }
}

/// One poll of a single-limiter acquire.
fn step_single(
    s: &mut State,
    key: &GateKey,
    weight: u64,
    prio: Prio,
    ticket: &mut Option<Ticket>,
    timer: &mut Option<TimerKey>,
    waker: &Waker,
) -> Step {
    let (now, wall) = (s.clock.now, s.clock.wall);
    if let Some(refused) = s.sched.refusal(key, now, wall) {
        if let Some(t) = ticket.take() {
            wake_all(s.sched.limiter_mut(key).forget(t));
        }
        arm_timer(&mut s.reactor, timer, None, waker);
        return Step::Refused(refused);
    }
    let limiter = s.sched.limiter_mut(key);
    let t = *ticket.get_or_insert_with(|| limiter.enqueue(prio));
    match limiter.poll(t, now, weight, waker) {
        GatePoll::Granted => {
            *ticket = None;
            arm_timer(&mut s.reactor, timer, None, waker);
            Step::Granted
        }
        GatePoll::Wait { retry_at } => {
            arm_timer(&mut s.reactor, timer, retry_at, waker);
            Step::Pending
        }
    }
}

/// Waits for one weighted slot on a single limiter.
pub(crate) struct Acquire {
    shared: Rc<Shared>,
    key: GateKey,
    weight: u64,
    prio: Prio,
    gap_ms: u32,
    ticket: Option<Ticket>,
    timer: Option<TimerKey>,
    done: bool,
}

impl Future for Acquire {
    type Output = Result<Permit, Refused>;

    fn poll(mut self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(Err(Refused::ShuttingDown));
        }
        let (weight, prio) = (this.weight, this.prio);
        let (shared, key, ticket, timer) =
            (&this.shared, &this.key, &mut this.ticket, &mut this.timer);
        let step = shared.with(|s| step_single(s, key, weight, prio, ticket, timer, task.waker()));
        match step {
            Some(Step::Granted) => {
                this.done = true;
                Poll::Ready(Ok(Permit {
                    shared: Rc::clone(&this.shared),
                    key: this.key.clone(),
                    weight,
                    gap_ms: this.gap_ms,
                    outcome: None,
                }))
            }
            Some(Step::Pending) => Poll::Pending,
            Some(Step::Refused(r)) => {
                this.done = true;
                Poll::Ready(Err(r))
            }
            None => {
                this.done = true;
                Poll::Ready(Err(Refused::ShuttingDown))
            }
        }
    }
}

impl Drop for Acquire {
    fn drop(&mut self) {
        if self.ticket.is_none() && self.timer.is_none() {
            return;
        }
        let (key, ticket, timer) = (self.key.clone(), self.ticket.take(), self.timer.take());
        let wakers = self.shared.with(|s| {
            if let Some(k) = timer {
                s.reactor.remove_timer(k);
            }
            ticket
                .map(|t| s.sched.limiter_mut(&key).forget(t))
                .unwrap_or_default()
        });
        wake_all(wakers.unwrap_or_default());
    }
}

/// The two tickets of an institutional acquire. The global ticket exists only while the family gate would
/// grant (see the module docs).
#[derive(Default)]
struct InstTickets {
    host: Option<Ticket>,
    global: Option<Ticket>,
}

/// One poll of an institutional acquire: family gate dry run, then (only if it would grant) the global gate
/// dry run, then take both.
fn step_inst(
    s: &mut State,
    host: &InstKey,
    tickets: &mut InstTickets,
    timer: &mut Option<TimerKey>,
    waker: &Waker,
) -> Step {
    let (now, wall) = (s.clock.now, s.clock.wall);
    let (hk, gk) = (GateKey::InstHost(host.clone()), GateKey::InstGlobal);
    if let Some(refused) = s
        .sched
        .refusal(&hk, now, wall)
        .or_else(|| s.sched.refusal(&gk, now, wall))
    {
        if let Some(t) = tickets.host.take() {
            wake_all(s.sched.limiter_mut(&hk).forget(t));
        }
        if let Some(t) = tickets.global.take() {
            wake_all(s.sched.limiter_mut(&gk).forget(t));
        }
        arm_timer(&mut s.reactor, timer, None, waker);
        return Step::Refused(refused);
    }
    let ht = match tickets.host {
        Some(t) => t,
        None => {
            let t = s.sched.limiter_mut(&hk).enqueue(Prio::New);
            tickets.host = Some(t);
            t
        }
    };
    match s.sched.limiter_mut(&hk).check(ht, now, 1) {
        GatePoll::Wait { retry_at } => {
            // The family is busy, cooling down or not our turn: hold NO place in the global queue meanwhile.
            if let Some(gt) = tickets.global.take() {
                wake_all(s.sched.limiter_mut(&gk).forget(gt));
            }
            s.sched.limiter_mut(&hk).park(ht, waker);
            arm_timer(&mut s.reactor, timer, retry_at, waker);
            Step::Pending
        }
        GatePoll::Granted => {
            let gt = match tickets.global {
                Some(t) => t,
                None => {
                    let t = s.sched.limiter_mut(&gk).enqueue(Prio::New);
                    tickets.global = Some(t);
                    t
                }
            };
            match s.sched.limiter_mut(&gk).check(gt, now, 1) {
                GatePoll::Granted => {
                    wake_all(s.sched.limiter_mut(&hk).take(ht, now, 1));
                    wake_all(s.sched.limiter_mut(&gk).take(gt, now, 1));
                    *tickets = InstTickets::default();
                    arm_timer(&mut s.reactor, timer, None, waker);
                    Step::Granted
                }
                GatePoll::Wait { retry_at } => {
                    s.sched.limiter_mut(&gk).park(gt, waker);
                    s.sched.limiter_mut(&hk).park(ht, waker);
                    arm_timer(&mut s.reactor, timer, retry_at, waker);
                    Step::Pending
                }
            }
        }
    }
}

/// Waits for the institutional host gate + global cap together.
pub(crate) struct AcquireInst {
    shared: Rc<Shared>,
    host: InstKey,
    kind: FetchKind,
    tickets: InstTickets,
    timer: Option<TimerKey>,
    done: bool,
}

impl Future for AcquireInst {
    type Output = Result<InstPermit, Refused>;

    fn poll(mut self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(Err(Refused::ShuttingDown));
        }
        let (shared, host, tickets, timer) =
            (&this.shared, &this.host, &mut this.tickets, &mut this.timer);
        let step = shared.with(|s| step_inst(s, host, tickets, timer, task.waker()));
        match step {
            Some(Step::Granted) => {
                this.done = true;
                Poll::Ready(Ok(InstPermit {
                    shared: Rc::clone(&this.shared),
                    host: this.host.clone(),
                    kind: this.kind,
                }))
            }
            Some(Step::Pending) => Poll::Pending,
            Some(Step::Refused(r)) => {
                this.done = true;
                Poll::Ready(Err(r))
            }
            None => {
                this.done = true;
                Poll::Ready(Err(Refused::ShuttingDown))
            }
        }
    }
}

impl Drop for AcquireInst {
    fn drop(&mut self) {
        if self.tickets.host.is_none() && self.tickets.global.is_none() && self.timer.is_none() {
            return;
        }
        let (host, tickets, timer) = (
            self.host.clone(),
            std::mem::take(&mut self.tickets),
            self.timer.take(),
        );
        let wakers = self.shared.with(|s| {
            if let Some(k) = timer {
                s.reactor.remove_timer(k);
            }
            let mut wakers = Vec::new();
            if let Some(t) = tickets.host {
                wakers.extend(s.sched.limiter_mut(&GateKey::InstHost(host)).forget(t));
            }
            if let Some(t) = tickets.global {
                wakers.extend(s.sched.limiter_mut(&GateKey::InstGlobal).forget(t));
            }
            wakers
        });
        wake_all(wakers.unwrap_or_default());
    }
}

impl Cx {
    fn acquire_on(&self, key: GateKey, weight: u64, prio: Prio, gap_ms: u32) -> Acquire {
        Acquire {
            shared: Rc::clone(&self.shared),
            key,
            weight,
            prio,
            gap_ms,
            ticket: None,
            timer: None,
            done: false,
        }
    }

    /// One of the run-wide item slots (`RunPrefs.item_concurrency`). Held for the duration of the item's
    /// *active* work: callers drop it before parking on the login gate and re-acquire with `Prio::Resumed`.
    pub(crate) fn acquire_item_slot(&self, prio: Prio) -> Acquire {
        self.acquire_on(GateKey::Items, 1, prio, 0)
    }

    /// Politeness gate of one OA API (token bucket + concurrency). `Refused::HostSkipped` when the API was
    /// disabled for the run (repeated 429s, see [`Cx::disable_api`]).
    pub(crate) fn acquire_api(&self, src: Source) -> Acquire {
        self.acquire_on(GateKey::Api(src), 1, Prio::New, 0)
    }

    /// OA-phase gate of one publisher / repository host (`hosts::oa_host_key`): 1 in flight, 1 s gap.
    /// `Refused::HostSkipped` once the host failed `OA_HOST_FAIL_LIMIT` times in a row or was walled.
    pub(crate) fn acquire_oa_host(&self, host_key: &str) -> Acquire {
        self.acquire_on(
            GateKey::OaHost(host_key_norm(host_key)),
            1,
            Prio::New,
            policy::OA_HOST_GAP_MS,
        )
    }

    /// One of the 2 hidden-browser slots.
    pub(crate) fn acquire_browser(&self) -> Acquire {
        self.acquire_on(GateKey::Browser, 1, Prio::New, 0)
    }

    /// The single save slot (Zotero DB writes are serialised anyway).
    pub(crate) fn acquire_save(&self) -> Acquire {
        self.acquire_on(GateKey::Save, 1, Prio::New, 0)
    }

    /// Budget for retained bodies: weight = `max(est_bytes, BYTES_MIN_RESERVE)`.
    #[cfg(test)]
    pub(crate) fn acquire_bytes(&self, est_bytes: u64) -> Acquire {
        self.acquire_on(
            GateKey::Bytes,
            est_bytes.max(policy::BYTES_MIN_RESERVE),
            Prio::New,
            0,
        )
    }

    /// Institutional slot for a fetch of `kind` at publisher `key`. Resolves `Err(InstHalted)` immediately when
    /// the route is halted (checked again after every wake-up). The page/PDF gap is stamped on release.
    pub(crate) fn acquire_inst(&self, key: &InstKey, kind: FetchKind) -> AcquireInst {
        AcquireInst {
            shared: Rc::clone(&self.shared),
            host: key.clone(),
            kind,
            tickets: InstTickets::default(),
            timer: None,
            done: false,
        }
    }

    /// Reserve one institutional PDF against the licence ledger and push `Persist { Ledger, flush: true }` to
    /// the outbox. `Err(Refused::Cap)` (nothing reserved) when any cap would be exceeded; `Err(InstHalted)` when
    /// halted. Call BEFORE emitting the `barrier: true` fetch effect.
    pub(crate) fn reserve_licence(&self, est_bytes: u64) -> Result<LedgerTicket, Refused> {
        let reserved = self.shared.with(|s| {
            let wall = s.clock.wall;
            if s.sched.is_shutting_down() {
                return Err(Refused::ShuttingDown);
            }
            if let Some((reason, until_wall)) = s.sched.halts.inst_halted(wall) {
                return Err(Refused::InstHalted { reason, until_wall });
            }
            let r = s
                .sched
                .ledger
                .reserve(wall, est_bytes)
                .map_err(Refused::Cap)?;
            let eff = s.sched.ledger.persist_effect(true);
            s.outbox.push(eff);
            Ok(r.id)
        });
        match reserved {
            Some(Ok(id)) => Ok(LedgerTicket {
                shared: Rc::clone(&self.shared),
                id,
                est_bytes,
                sent: false,
                resolved: false,
            }),
            Some(Err(r)) => Err(r),
            None => Err(Refused::ShuttingDown),
        }
    }

    /// `Some((reason, until_wall))` while the institutional route is halted.
    pub(crate) fn inst_halted(&self) -> Option<(String, u64)> {
        self.shared
            .with(|s| s.sched.halts.inst_halted(s.clock.wall))
            .flatten()
    }

    /// Halt the institutional route (persisted; also reflected in `RunReport.halted`). `retry_after_s` is the
    /// publisher's Retry-After if any; the halt lasts `max(retry_after, policy minimum)`. Items queued on an
    /// institutional gate are woken so they see the refusal at once.
    pub(crate) fn halt_inst(&self, reason: HaltReason, retry_after_s: Option<u32>) {
        let wakers = self.shared.with(|s| {
            let wall = s.clock.wall;
            if s.sched
                .halts
                .halt_inst(reason, retry_after_s.map(|r| u64::from(r) * 1000), wall)
            {
                let eff = s.sched.halts_effect(true);
                s.outbox.push(eff);
                s.sched.inst_wakers()
            } else {
                Vec::new()
            }
        });
        wake_all(wakers.unwrap_or_default());
    }

    /// Block one gate key (an institutional family name such as `"wiley"`, or an OA host key) for `for_ms` of
    /// wall time; persisted. Queued waiters of that gate are woken to see the refusal.
    pub(crate) fn block_host(&self, key: &str, for_ms: u64) {
        let key = host_key_norm(key);
        let wakers = self.shared.with(|s| {
            let until = s.clock.wall.saturating_add(for_ms);
            if s.sched.halts.block_host(&key, until) {
                let eff = s.sched.halts_effect(true);
                s.outbox.push(eff);
                let mut w = s
                    .sched
                    .gate_wakers(&GateKey::InstHost(InstKey(key.clone())));
                w.extend(s.sched.gate_wakers(&GateKey::OaHost(key.clone())));
                w
            } else {
                Vec::new()
            }
        });
        wake_all(wakers.unwrap_or_default());
    }

    /// Switch an OA API off for `policy::API_COOLDOWN_MS` of engine time (a 429 that survived its one retry).
    /// `acquire_api(src)` then resolves `Err(HostSkipped)` until the cool-down ends.
    pub(crate) fn disable_api(&self, src: Source) {
        self.disable_api_for(src, policy::API_COOLDOWN_MS);
    }

    /// [`Cx::disable_api`] with an explicit duration (e.g. the API's Retry-After).
    pub(crate) fn disable_api_for(&self, src: Source, for_ms: u64) {
        let wakers = self.shared.with(|s| {
            let until = s.clock.now.saturating_add(for_ms);
            s.sched.hosts.disable_api(src, until);
            s.sched.gate_wakers(&GateKey::Api(src))
        });
        wake_all(wakers.unwrap_or_default());
    }

    /// Skip an OA host for 30 minutes right away (a hard block such as an "access denied" bot page that no
    /// browser tier will change). Items queued on the host are woken and see `Refused::HostSkipped`.
    pub(crate) fn wall_host(&self, host: &str, reason: &str) {
        let wakers = self.shared.with(|s| {
            let now = s.clock.now;
            s.sched.hosts.wall_at(host, reason, now);
            s.sched.gate_wakers(&GateKey::OaHost(host_key_norm(host)))
        });
        wake_all(wakers.unwrap_or_default());
    }

    /// True when the OA host is skipped (5 failures in a row, two challenges, a hard block or a persisted block).
    pub(crate) fn host_skipped(&self, host_key: &str) -> bool {
        let key = GateKey::OaHost(host_key_norm(host_key));
        self.shared
            .with(|s| s.sched.refusal(&key, s.clock.now, s.clock.wall).is_some())
            .unwrap_or(false)
    }

    /// Sticky per-host UA choice.
    pub(crate) fn ua_for(&self, host: &str) -> Ua {
        self.shared
            .with(|s| s.sched.hosts.ua_for(host))
            .unwrap_or(Ua::Default)
    }

    pub(crate) fn pin_plain_ua(&self, host: &str) {
        let _ = self.shared.with(|s| s.sched.hosts.pin_plain_ua(host));
    }

    /// Engine-side timeout for a request class against `host` (slow-host profile applied).
    pub(crate) fn timeout_for(&self, class: FetchClass, host: &str) -> u32 {
        policy::timeout_ms(class, crate::sched::hosts::is_slow_host(host))
    }

    /// Count a challenge on `host`; `true` when the host is now walled for the run. Queued OA-host waiters are
    /// woken so they see the wall.
    pub(crate) fn note_challenge(&self, host: &str) -> bool {
        let walled = self.shared.with(|s| {
            let walled = s.sched.hosts.note_challenge_at(host, s.clock.now);
            let wakers = if walled {
                s.sched.gate_wakers(&GateKey::OaHost(host_key_norm(host)))
            } else {
                Vec::new()
            };
            (walled, wakers)
        });
        match walled {
            Some((walled, wakers)) => {
                wake_all(wakers);
                walled
            }
            None => false,
        }
    }

    /// Cached EZproxy "host configured?" (7-day TTL).
    pub(crate) fn ezproxy_known(&self, host: &str) -> Option<bool> {
        self.shared
            .with(|s| s.sched.hosts.ezproxy_known(host, s.clock.wall))
            .flatten()
    }

    /// Learn an EZproxy probe result and persist the cache.
    pub(crate) fn learn_ezproxy(&self, host: &str, configured: bool) {
        let _ = self.shared.with(|s| {
            let wall = s.clock.wall;
            s.sched.hosts.learn_ezproxy(host, configured, wall);
            s.outbox.push(Effect::Persist {
                key: PersistKey::Ezproxy,
                value: serde_json::to_value(&s.sched.hosts.ezproxy)
                    .unwrap_or(serde_json::Value::Null),
                flush: false,
            });
        });
    }

    /// Cached Alma holdings answer for a DOI (1-day TTL).
    pub(crate) fn alma_known(&self, doi: &str) -> Option<bool> {
        self.shared
            .with(|s| s.sched.hosts.alma_known(doi, s.clock.wall))
            .flatten()
    }

    pub(crate) fn learn_alma(&self, doi: &str, held: bool) {
        let _ = self.shared.with(|s| {
            let wall = s.clock.wall;
            s.sched.hosts.learn_alma(doi, held, wall);
            s.outbox.push(Effect::Persist {
                key: PersistKey::Alma,
                value: serde_json::to_value(&s.sched.hosts.alma).unwrap_or(serde_json::Value::Null),
                flush: false,
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::CapHit;
    use crate::sched::testkit::*;
    use futures_util::FutureExt;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    type Log = Rc<RefCell<Vec<String>>>;

    fn log() -> Log {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn entries(l: &Log) -> Vec<String> {
        l.borrow().clone()
    }

    fn persist_keys(effects: &[Effect]) -> Vec<PersistKey> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Persist { key, .. } => Some(*key),
                _ => None,
            })
            .collect()
    }

    /// `n` tasks that each take the OA host gate "h.org" for `hold_ms`, logging "<i> start <now>" / "<i> end".
    fn host_tasks(w: &mut World, n: usize, hold_ms: u64, l: &Log) {
        for i in 0..n {
            let cx = w.cx();
            let l = Rc::clone(l);
            w.spawn(async move {
                let permit = cx.acquire_oa_host("h.org").await.expect("granted");
                l.borrow_mut().push(format!("{i} start {}", cx.now()));
                cx.sleep(hold_ms).await;
                l.borrow_mut().push(format!("{i} end {}", cx.now()));
                drop(permit);
            });
        }
    }

    // ------------------------------------------------------------------------------------ single gates

    #[test]
    fn three_tasks_on_a_cap_one_host_run_one_at_a_time_with_the_gap() {
        let mut w = World::new();
        let l = log();
        host_tasks(&mut w, 3, 500, &l);
        w.run();
        assert_eq!(entries(&l), vec!["0 start 1000"]);
        w.advance(500);
        w.run();
        assert_eq!(entries(&l), vec!["0 start 1000", "0 end 1500"]);
        // the 1 s OA host gap runs from the release at 1500
        w.advance(999);
        w.run();
        assert_eq!(entries(&l).len(), 2, "gap not over yet");
        assert_eq!(
            w.next_deadline(),
            Some(2_500),
            "exactly one timer, at the end of the gap"
        );
        w.advance(1);
        w.run();
        assert_eq!(entries(&l).last().map(String::as_str), Some("1 start 2500"));
        w.advance(500);
        w.run();
        w.advance(1_000);
        w.run();
        assert_eq!(
            entries(&l),
            vec![
                "0 start 1000",
                "0 end 1500",
                "1 start 2500",
                "1 end 3000",
                "2 start 4000"
            ]
        );
        assert_eq!(w.timers(), 1, "only the third task's sleep is left");
    }

    #[test]
    fn items_slots_follow_the_concurrency_pref_and_resumed_beats_new() {
        let mut cfg = test_config();
        cfg.run.item_concurrency = 2;
        let mut w = World::with_config(cfg);
        let l = log();
        // two holders keep both slots; then a New and a Resumed waiter queue up (New first)
        let cx = w.cx();
        let (hold_a, hold_b) = (Rc::new(RefCell::new(None)), Rc::new(RefCell::new(None)));
        for slot in [&hold_a, &hold_b] {
            let (cx, slot) = (cx.clone(), Rc::clone(slot));
            w.spawn(async move {
                *slot.borrow_mut() = Some(cx.acquire_item_slot(Prio::New).await.expect("slot"));
            });
        }
        w.run();
        assert!(hold_a.borrow().is_some() && hold_b.borrow().is_some());
        for (name, prio) in [("new", Prio::New), ("resumed", Prio::Resumed)] {
            let (cx, l) = (cx.clone(), Rc::clone(&l));
            w.spawn(async move {
                let permit = cx.acquire_item_slot(prio).await.expect("slot");
                l.borrow_mut().push(name.to_owned());
                drop(permit);
            });
            w.run();
        }
        assert!(entries(&l).is_empty());
        // free ONE slot: the resumed waiter jumps the queue
        drop(hold_a.borrow_mut().take());
        w.run();
        assert_eq!(
            entries(&l),
            vec!["resumed", "new"],
            "resumed first; its release then lets 'new' in"
        );
        drop(hold_b.borrow_mut().take());
    }

    #[test]
    fn dropping_a_waiting_acquire_passes_the_baton() {
        let mut w = World::new();
        let l = log();
        let cx = w.cx();
        let holder = Rc::new(RefCell::new(None));
        {
            let (cx, holder) = (cx.clone(), Rc::clone(&holder));
            w.spawn(async move {
                *holder.borrow_mut() = Some(cx.acquire_save().await.expect("save slot"));
            });
        }
        w.run();
        let mut ids = Vec::new();
        for name in ["a", "b"] {
            let (cx, l) = (cx.clone(), Rc::clone(&l));
            ids.push(w.spawn(async move {
                let permit = cx.acquire_save().await.expect("granted");
                l.borrow_mut().push(name.to_owned());
                drop(permit);
            }));
            w.run();
        }
        // a is the head; cancel it while it waits: b must be woken and then get the slot
        w.kill(ids[0]);
        drop(holder.borrow_mut().take());
        w.run();
        assert_eq!(entries(&l), vec!["b"]);
        let waiting = w
            .shared
            .with(|s| s.sched.limiter_mut(&GateKey::Save).waiting())
            .expect("state");
        assert_eq!(waiting, 0);
    }

    #[test]
    fn a_dropped_waiter_removes_its_timer_and_ticket() {
        let mut w = World::new();
        let cx = w.cx();
        // first use of the gap host
        let first = Rc::new(RefCell::new(None));
        {
            let (cx, first) = (cx.clone(), Rc::clone(&first));
            w.spawn(async move {
                let p = cx.acquire_oa_host("h.org").await.expect("granted");
                drop(p); // gap until 2000
                *first.borrow_mut() = Some(());
            });
        }
        w.run();
        let cx2 = w.cx();
        let id = w.spawn(async move {
            let _p = cx2.acquire_oa_host("h.org").await;
        });
        w.run();
        assert_eq!(w.timers(), 1, "waiting out the gap arms a timer");
        w.kill(id);
        assert_eq!(w.timers(), 0, "dropping the future removes the timer");
        assert_eq!(
            w.shared
                .with(|s| s
                    .sched
                    .limiter_mut(&GateKey::OaHost("h.org".into()))
                    .waiting())
                .expect("state"),
            0
        );
    }

    #[test]
    fn token_bucket_api_limiter_spaces_requests() {
        let mut w = World::new();
        let l = log();
        // bioRxiv: burst 1, one token per second, one in flight
        for i in 0..3 {
            let (cx, l) = (w.cx(), Rc::clone(&l));
            w.spawn(async move {
                let p = cx.acquire_api(Source::Biorxiv).await.expect("granted");
                l.borrow_mut().push(format!("{i} {}", cx.now()));
                drop(p);
            });
        }
        w.run();
        assert_eq!(entries(&l), vec!["0 1000"]);
        assert_eq!(
            w.next_deadline(),
            Some(2_000),
            "the second request waits for its token"
        );
        w.advance(1_000);
        w.run();
        assert_eq!(entries(&l), vec!["0 1000", "1 2000"]);
        w.advance(1_000);
        w.run();
        assert_eq!(entries(&l), vec!["0 1000", "1 2000", "2 3000"]);
    }

    #[test]
    fn a_fast_api_allows_its_burst_at_once() {
        let mut w = World::new();
        let l = log();
        for i in 0..12 {
            let (cx, l) = (w.cx(), Rc::clone(&l));
            w.spawn(async move {
                let p = cx.acquire_api(Source::Openalex).await.expect("granted");
                l.borrow_mut().push(format!("{i} {}", cx.now()));
                cx.sleep(1).await;
                drop(p);
            });
        }
        w.run();
        // concurrency 4 in flight at t=1000; each finishes after 1 ms and the 10-token burst is shared
        w.advance(1);
        w.run();
        w.advance(1);
        w.run();
        let at_start = entries(&l).iter().filter(|e| e.ends_with(" 1000")).count();
        assert_eq!(at_start, 4, "capacity 4 limits the first wave");
        let by_1002 = entries(&l).len();
        assert_eq!(
            by_1002,
            10,
            "the burst of 10 tokens is spent within the first few ms: {:?}",
            entries(&l)
        );
        w.advance(100);
        w.run();
        w.advance(100);
        w.run();
        assert_eq!(
            entries(&l).len(),
            12,
            "the last two wait for refills (100 ms per token)"
        );
    }

    #[test]
    fn five_failures_in_a_row_skip_the_host() {
        let mut w = World::new();
        let cx = w.cx();
        let results = log();
        {
            let (cx, results) = (cx.clone(), Rc::clone(&results));
            w.spawn(async move {
                for i in 0..5 {
                    match cx.acquire_oa_host("bad.org").await {
                        Ok(mut p) => {
                            p.report(false);
                            results.borrow_mut().push(format!("try {i}"));
                        }
                        Err(r) => results.borrow_mut().push(format!("refused {r:?}")),
                    }
                    cx.sleep(1_001).await;
                }
            });
        }
        for _ in 0..8 {
            w.run();
            w.advance(1_001);
        }
        w.run();
        assert_eq!(
            entries(&results),
            vec!["try 0", "try 1", "try 2", "try 3", "try 4"]
        );
        assert!(
            w.shared
                .with(|s| s.sched.hosts.is_skipped("bad.org"))
                .expect("state")
        );
        assert!(cx.host_skipped("bad.org"));
        assert!(!cx.host_skipped("good.org"));
        // after the skip every new acquire is refused immediately
        let after = cx
            .acquire_oa_host("bad.org")
            .now_or_never()
            .map(|r| r.err());
        assert_eq!(
            after,
            Some(Some(Refused::HostSkipped {
                host: "bad.org".into()
            }))
        );
    }

    #[test]
    fn a_skipped_host_is_tried_again_after_thirty_minutes() {
        let w = World::new();
        let cx = w.cx();
        for _ in 0..policy::OA_HOST_FAIL_LIMIT {
            let mut p = cx
                .acquire_oa_host("slow.org")
                .now_or_never()
                .and_then(Result::ok)
                .expect("granted");
            p.report(false);
            drop(p);
            w.advance(1_001);
        }
        let walled_at = w.now() - 1_001; // the fifth release
        assert!(cx.host_skipped("slow.org"));
        w.advance(walled_at + policy::HOST_WALL_TTL_MS - 1 - w.now());
        assert!(
            cx.host_skipped("slow.org"),
            "one millisecond before the end"
        );
        w.advance(1);
        assert!(!cx.host_skipped("slow.org"));
        let mut p = cx
            .acquire_oa_host("slow.org")
            .now_or_never()
            .and_then(Result::ok)
            .expect("granted again");
        p.report(true);
        drop(p);
        // the old streak is gone: four more failures do not wall it
        w.advance(1_001);
        for _ in 1..policy::OA_HOST_FAIL_LIMIT {
            let mut p = cx
                .acquire_oa_host("slow.org")
                .now_or_never()
                .and_then(Result::ok)
                .expect("granted");
            p.report(false);
            drop(p);
            w.advance(1_001);
        }
        assert!(!cx.host_skipped("slow.org"));
        // two challenges wall a host for the same 30 minutes
        assert!(!cx.note_challenge("x.org"));
        assert!(cx.note_challenge("x.org"));
        w.advance(policy::HOST_WALL_TTL_MS);
        assert!(!cx.host_skipped("x.org"));
    }

    #[test]
    fn a_success_resets_the_failure_streak() {
        let mut w = World::new();
        let cx = w.cx();
        let task = w.spawn({
            let cx = cx.clone();
            async move {
                for ok in [false, false, false, false, true, false, false, false, false] {
                    let mut p = cx
                        .acquire_oa_host("flaky.org")
                        .await
                        .expect("never skipped");
                    p.report(ok);
                    drop(p);
                    cx.sleep(1_000).await;
                }
            }
        });
        for _ in 0..20 {
            w.run();
            w.advance(1_000);
        }
        w.run();
        assert!(w.finished(task));
        assert!(
            !cx.host_skipped("flaky.org"),
            "4 failures, a success, 4 failures: never 5 in a row"
        );
    }

    #[test]
    fn a_walled_host_wakes_its_waiters_with_a_refusal() {
        let mut w = World::new();
        let cx = w.cx();
        let holder = Rc::new(RefCell::new(None));
        {
            let (cx, holder) = (cx.clone(), Rc::clone(&holder));
            w.spawn(async move {
                *holder.borrow_mut() = Some(cx.acquire_oa_host("c.org").await.expect("first"));
            });
        }
        w.run();
        let got = log();
        {
            let (cx, got) = (cx.clone(), Rc::clone(&got));
            w.spawn(async move {
                let r = cx.acquire_oa_host("c.org").await;
                got.borrow_mut().push(format!("{:?}", r.err()));
            });
        }
        w.run();
        assert!(entries(&got).is_empty());
        // two challenges wall the host while the second task waits behind the holder
        assert!(!cx.note_challenge("c.org"));
        assert!(cx.note_challenge("c.org"));
        w.run();
        assert_eq!(
            entries(&got),
            vec![format!(
                "{:?}",
                Some(Refused::HostSkipped {
                    host: "c.org".into()
                })
            )]
        );
        drop(holder.borrow_mut().take());
    }

    #[test]
    fn disabled_api_refuses_until_the_cooldown_ends() {
        let w = World::new();
        let cx = w.cx();
        cx.disable_api_for(Source::Europepmc, 10_000);
        let try_acquire = |cx: &Cx| {
            cx.acquire_api(Source::Europepmc)
                .now_or_never()
                .map(|r| r.err())
        };
        assert_eq!(
            try_acquire(&cx),
            Some(Some(Refused::HostSkipped {
                host: "europepmc".into()
            }))
        );
        w.advance(9_999);
        assert_eq!(
            try_acquire(&cx),
            Some(Some(Refused::HostSkipped {
                host: "europepmc".into()
            }))
        );
        w.advance(1);
        assert_eq!(try_acquire(&cx), Some(None), "back in service");
        // other APIs were never affected
        assert_eq!(
            cx.acquire_api(Source::Openalex)
                .now_or_never()
                .map(|r| r.is_ok()),
            Some(true)
        );
    }

    #[test]
    fn bytes_budget_is_weighted_and_oversize_goes_through_when_idle() {
        let mut w = World::new();
        let cx = w.cx();
        let l = log();
        let holders = Rc::new(RefCell::new(Vec::new()));
        for (name, mb) in [("a", 150u64), ("b", 100), ("c", 10)] {
            let (cx, l, holders) = (cx.clone(), Rc::clone(&l), Rc::clone(&holders));
            w.spawn(async move {
                let p = cx.acquire_bytes(mb * 1024 * 1024).await.expect("budget");
                l.borrow_mut().push(name.to_owned());
                // asks below the 16 MiB minimum are rounded up to it
                assert_eq!(
                    p.weight(),
                    (mb * 1024 * 1024).max(policy::BYTES_MIN_RESERVE)
                );
                holders.borrow_mut().push(p);
            });
            w.run();
        }
        // a (150) fits in 192; b (100) does not; c is behind b (strict FIFO) even though it would fit
        assert_eq!(entries(&l), vec!["a"]);
        holders.borrow_mut().clear();
        w.run();
        assert_eq!(entries(&l), vec!["a", "b", "c"]);
        // b (100) + c (16) hold 116 MiB, so another minimum-size ask (16) still fits
        let tiny = cx
            .acquire_bytes(5)
            .now_or_never()
            .and_then(Result::ok)
            .expect("fits");
        assert_eq!(tiny.weight(), policy::BYTES_MIN_RESERVE);
        // an estimate above the whole budget is granted alone, once everything is released
        holders.borrow_mut().clear();
        drop(tiny);
        let big = cx
            .acquire_bytes(500 * 1024 * 1024)
            .now_or_never()
            .and_then(Result::ok);
        assert!(big.is_some(), "oversize when idle");
    }

    // ---------------------------------------------------------------------------------- institutional gates

    /// `n` tasks fetching from `family` with `kind`, holding the permit `hold_ms`; log "<name> start <now>".
    fn inst_tasks(
        w: &mut World,
        family: &str,
        kind: FetchKind,
        names: &[&str],
        hold_ms: u64,
        l: &Log,
    ) {
        for name in names {
            let (cx, l, key, name) = (
                w.cx(),
                Rc::clone(l),
                InstKey(family.to_owned()),
                (*name).to_owned(),
            );
            w.spawn(async move {
                match cx.acquire_inst(&key, kind).await {
                    Ok(p) => {
                        l.borrow_mut().push(format!("{name} start {}", cx.now()));
                        cx.sleep(hold_ms).await;
                        drop(p);
                    }
                    Err(r) => l.borrow_mut().push(format!("{name} refused {r:?}")),
                }
            });
        }
    }

    #[test]
    fn one_institutional_request_per_family_and_page_gap_of_two_seconds() {
        let mut w = World::new();
        let l = log();
        inst_tasks(&mut w, "wiley", FetchKind::Page, &["a", "b", "c"], 100, &l);
        w.run();
        assert_eq!(entries(&l), vec!["a start 1000"]);
        w.advance(100); // a releases at 1100; gap 2 s -> b at 3100
        w.run();
        assert_eq!(w.next_deadline(), Some(3_100));
        w.advance(1_999);
        w.run();
        assert_eq!(entries(&l).len(), 1);
        w.advance(1);
        w.run();
        assert_eq!(entries(&l), vec!["a start 1000", "b start 3100"]);
        w.advance(100); // b releases at 3200 -> c at 5200
        w.run();
        w.advance(2_000);
        w.run();
        assert_eq!(
            entries(&l),
            vec!["a start 1000", "b start 3100", "c start 5200"]
        );
    }

    #[test]
    fn pdf_gap_is_five_seconds_plus_jitter_and_prefs_only_raise_it() {
        let mut w = World::new();
        let l = log();
        inst_tasks(&mut w, "elsevier", FetchKind::Pdf, &["a", "b"], 0, &l);
        w.run();
        // a holds for 0 ms and releases immediately; b waits 5000..=8000 ms
        let deadline = w.next_deadline().expect("b is waiting out the gap");
        assert!(
            (1_000 + 5_000..=1_000 + 8_000).contains(&deadline),
            "deadline {deadline}"
        );
        w.advance(deadline - w.now());
        w.run();
        assert_eq!(entries(&l).len(), 2);

        // min_gap_sec = 12 raises the floor
        let mut cfg = test_config();
        cfg.inst.min_gap_sec = 12;
        let mut w = World::with_config(cfg);
        let l = log();
        inst_tasks(&mut w, "elsevier", FetchKind::Pdf, &["a", "b"], 0, &l);
        w.run();
        let deadline = w.next_deadline().expect("gap timer");
        assert!(
            (1_000 + 12_000..=1_000 + 15_000).contains(&deadline),
            "deadline {deadline}"
        );
    }

    #[test]
    fn two_families_run_in_parallel_but_the_global_cap_of_two_holds() {
        let mut w = World::new();
        let l = log();
        inst_tasks(&mut w, "wiley", FetchKind::Page, &["w"], 1_000, &l);
        inst_tasks(&mut w, "elsevier", FetchKind::Page, &["e"], 1_000, &l);
        inst_tasks(&mut w, "lww", FetchKind::Page, &["l"], 1_000, &l);
        w.run();
        assert_eq!(
            entries(&l),
            vec!["w start 1000", "e start 1000"],
            "the third family waits for a global place"
        );
        // the third task is waiting on the GLOBAL gate (its family gate would grant)
        let (host_wait, global_wait) = w
            .shared
            .with(|s| {
                (
                    s.sched
                        .limiter_mut(&GateKey::InstHost(InstKey("lww".into())))
                        .waiting(),
                    s.sched.limiter_mut(&GateKey::InstGlobal).waiting(),
                )
            })
            .expect("state");
        assert_eq!((host_wait, global_wait), (1, 1));
        w.advance(1_000);
        w.run();
        assert_eq!(
            entries(&l),
            vec!["w start 1000", "e start 1000", "l start 2000"]
        );
    }

    #[test]
    fn never_three_in_flight_across_many_families() {
        let mut w = World::new();
        let in_flight = Rc::new(RefCell::new((0u32, 0u32)));
        for (i, family) in ["wiley", "elsevier", "lww", "oup", "bmj", "sage", "cup"]
            .iter()
            .enumerate()
        {
            let (cx, key, flight) = (w.cx(), InstKey((*family).to_owned()), Rc::clone(&in_flight));
            w.spawn(async move {
                let p = cx
                    .acquire_inst(&key, FetchKind::Page)
                    .await
                    .expect("never refused");
                {
                    let mut f = flight.borrow_mut();
                    f.0 += 1;
                    f.1 = f.1.max(f.0);
                }
                cx.sleep(300 + 50 * i as u64).await;
                flight.borrow_mut().0 -= 1;
                drop(p);
            });
        }
        for _ in 0..40 {
            w.run();
            w.advance(500);
        }
        w.run();
        let (now_in_flight, max_in_flight) = *in_flight.borrow();
        assert_eq!(now_in_flight, 0, "everything finished");
        assert_eq!(max_in_flight, 2, "global cap");
    }

    #[test]
    fn a_family_waiting_on_its_gap_does_not_block_other_families() {
        // wiley is cooling down (5 s PDF gap); elsevier is idle: it must be served immediately even though
        // wiley's next request is queued ahead of it on arrival order.
        let mut w = World::new();
        let l = log();
        inst_tasks(&mut w, "wiley", FetchKind::Pdf, &["w1"], 0, &l);
        w.run();
        inst_tasks(&mut w, "wiley", FetchKind::Pdf, &["w2"], 0, &l);
        inst_tasks(&mut w, "elsevier", FetchKind::Page, &["e1"], 10, &l);
        w.run();
        assert!(
            entries(&l).contains(&"e1 start 1000".to_owned()),
            "{:?}",
            entries(&l)
        );
        assert!(
            !entries(&l).iter().any(|e| e.starts_with("w2")),
            "w2 still cooling down"
        );
    }

    #[test]
    fn dropping_a_waiting_inst_acquire_frees_both_queues() {
        let mut w = World::new();
        let cx = w.cx();
        let l = log();
        let holder = Rc::new(RefCell::new(None));
        {
            let (cx, holder) = (cx.clone(), Rc::clone(&holder));
            w.spawn(async move {
                *holder.borrow_mut() = Some(
                    cx.acquire_inst(&InstKey("wiley".into()), FetchKind::Page)
                        .await
                        .expect("first"),
                );
            });
        }
        w.run();
        inst_tasks(&mut w, "wiley", FetchKind::Page, &["x"], 0, &l);
        w.run();
        let ids_waiting = |w: &World| {
            w.shared
                .with(|s| {
                    (
                        s.sched
                            .limiter_mut(&GateKey::InstHost(InstKey("wiley".into())))
                            .waiting(),
                        s.sched.limiter_mut(&GateKey::InstGlobal).waiting(),
                    )
                })
                .expect("state")
        };
        assert_eq!(ids_waiting(&w), (1, 0), "x waits on its family only");
        let id = w.spawn({
            let (cx, key) = (cx.clone(), InstKey("wiley".into()));
            async move {
                let _ = cx.acquire_inst(&key, FetchKind::Page).await;
            }
        });
        w.run();
        assert_eq!(ids_waiting(&w), (2, 0));
        w.kill(id);
        assert_eq!(ids_waiting(&w), (1, 0));
        drop(holder.borrow_mut().take());
        w.run();
        w.advance(2_000);
        w.run();
        assert_eq!(entries(&l), vec!["x start 3000"]);
    }

    #[test]
    fn a_halt_refuses_waiting_inst_acquires_and_wakes_them() {
        let mut w = World::new();
        let cx = w.cx();
        let l = log();
        let holder = Rc::new(RefCell::new(None));
        {
            let (cx, holder) = (cx.clone(), Rc::clone(&holder));
            w.spawn(async move {
                *holder.borrow_mut() = Some(
                    cx.acquire_inst(&InstKey("wiley".into()), FetchKind::Pdf)
                        .await
                        .expect("first"),
                );
            });
        }
        w.run();
        inst_tasks(&mut w, "wiley", FetchKind::Page, &["a", "b"], 0, &l);
        w.run();
        assert!(entries(&l).is_empty());
        // a 429 halts the route while a and b are queued
        cx.halt_inst(HaltReason::RateLimited, Some(60));
        w.run();
        let got = entries(&l);
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(
            got.iter()
                .all(|e| e.contains("refused InstHalted") && e.contains("rate_limited")),
            "{got:?}"
        );
        // the halt was persisted and is visible
        let effects = w.effects();
        assert_eq!(persist_keys(&effects), vec![PersistKey::Halts]);
        assert_eq!(
            cx.inst_halted().map(|(r, _)| r),
            Some("rate_limited".to_owned())
        );
        // new acquires are refused at once, no matter the family, and no ticket is left behind
        let again = futures_util::FutureExt::now_or_never(
            cx.acquire_inst(&InstKey("lww".into()), FetchKind::Page),
        );
        assert!(matches!(again, Some(Err(Refused::InstHalted { .. }))));
        drop(holder.borrow_mut().take());
        w.run();
        let waiting = w
            .shared
            .with(|s| {
                (
                    s.sched
                        .limiter_mut(&GateKey::InstHost(InstKey("wiley".into())))
                        .waiting(),
                    s.sched.limiter_mut(&GateKey::InstGlobal).waiting(),
                )
            })
            .expect("state");
        assert_eq!(waiting, (0, 0));
    }

    #[test]
    fn halting_twice_persists_once_and_never_shortens() {
        let w = World::new();
        let cx = w.cx();
        cx.halt_inst(HaltReason::EzproxySuspended, None);
        assert_eq!(persist_keys(&w.effects()), vec![PersistKey::Halts]);
        cx.halt_inst(HaltReason::RateLimited, Some(1));
        assert!(w.effects().is_empty(), "no change, no persist");
        let (reason, until) = cx.inst_halted().expect("halted");
        assert_eq!(reason, "ezproxy_suspended");
        assert_eq!(until, w.wall() + policy::SUSPEND_COOLDOWN_MS);
    }

    #[test]
    fn a_blocked_family_refuses_only_that_family() {
        let mut w = World::new();
        let cx = w.cx();
        cx.block_host("wiley", 60_000);
        assert_eq!(persist_keys(&w.effects()), vec![PersistKey::Halts]);
        let wiley = futures_util::FutureExt::now_or_never(
            cx.acquire_inst(&InstKey("wiley".into()), FetchKind::Page),
        );
        assert!(matches!(wiley, Some(Err(Refused::HostSkipped { ref host })) if host == "wiley"));
        let l = log();
        inst_tasks(&mut w, "elsevier", FetchKind::Page, &["e"], 0, &l);
        w.run();
        assert_eq!(entries(&l), vec!["e start 1000"]);
        w.advance(60_000);
        let again = futures_util::FutureExt::now_or_never(
            cx.acquire_inst(&InstKey("wiley".into()), FetchKind::Page),
        );
        assert!(matches!(again, Some(Ok(_))));
    }

    #[test]
    fn inst_permit_reports_kind_and_family() {
        let w = World::new();
        let cx = w.cx();
        let p = futures_util::FutureExt::now_or_never(
            cx.acquire_inst(&InstKey("karger".into()), FetchKind::Pdf),
        )
        .and_then(Result::ok)
        .expect("granted");
        assert_eq!((p.kind(), p.host().0.as_str()), (FetchKind::Pdf, "karger"));
        let (host_used, global_used) = w
            .shared
            .with(|s| {
                (
                    s.sched
                        .limiter_mut(&GateKey::InstHost(InstKey("karger".into())))
                        .in_use,
                    s.sched.limiter_mut(&GateKey::InstGlobal).in_use,
                )
            })
            .expect("state");
        assert_eq!((host_used, global_used), (1, 1));
        drop(p);
        let (host_used, global_used) = w
            .shared
            .with(|s| {
                (
                    s.sched
                        .limiter_mut(&GateKey::InstHost(InstKey("karger".into())))
                        .in_use,
                    s.sched.limiter_mut(&GateKey::InstGlobal).in_use,
                )
            })
            .expect("state");
        assert_eq!((host_used, global_used), (0, 0));
    }

    // ------------------------------------------------------------------------------------------ the ledger

    #[test]
    fn reserve_licence_persists_before_returning_so_it_precedes_the_barrier_fetch() {
        let w = World::new();
        let cx = w.cx();
        let ticket = cx.reserve_licence(2_000_000).expect("reserved");
        // the barrier fetch is emitted AFTER this call; the outbox already holds the Persist
        let effects = w.effects();
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::Persist {
                key: PersistKey::Ledger,
                value,
                flush: true,
            } => {
                let state: LedgerState =
                    serde_json::from_value(value.clone()).expect("ledger shape");
                assert_eq!(state.reserved.len(), 1);
                assert_eq!(state.reserved[0].bytes, 2_000_000);
            }
            other => panic!("expected a flushed ledger persist, got {other:?}"),
        }
        // emulate the fetch effect going out next
        cx.emit(Effect::Log {
            level: LogLevel::Info,
            msg: "barrier fetch goes here".into(),
        });
        let after = w.effects();
        assert!(matches!(after[0], Effect::Log { .. }));
        ticket.rollback();
    }

    #[test]
    fn commit_persists_the_actual_size() {
        let w = World::new();
        let cx = w.cx();
        let ticket = cx.reserve_licence(1_000_000).expect("reserved");
        w.effects();
        ticket.commit(3_300_000);
        let effects = w.effects();
        assert_eq!(persist_keys(&effects), vec![PersistKey::Ledger]);
        let usage = w
            .shared
            .with(|s| s.sched.usage(s.clock.wall))
            .expect("state");
        assert_eq!((usage.hour, usage.mb_hour), (1, 4), "3.3 MB rounds up to 4");
        let state = w
            .shared
            .with(|s| s.sched.ledger.state().clone())
            .expect("state");
        assert!(state.reserved.is_empty());
        assert_eq!(state.committed, vec![(w.wall(), 3_300_000)]);
    }

    #[test]
    fn an_unsent_ticket_rolls_back_on_drop_and_a_sent_one_commits_its_estimate() {
        let w = World::new();
        let cx = w.cx();
        // never sent: the item gave up before the request went out
        let t = cx.reserve_licence(1_000_000).expect("reserved");
        drop(t);
        assert_eq!(
            w.shared
                .with(|s| s.sched.usage(s.clock.wall).hour)
                .expect("state"),
            0
        );
        // sent but unresolved (item cancelled mid-fetch): the estimate is committed, never under-counted
        let mut t = cx.reserve_licence(1_500_000).expect("reserved");
        t.mark_sent();
        drop(t);
        let state = w
            .shared
            .with(|s| s.sched.ledger.state().clone())
            .expect("state");
        assert!(state.reserved.is_empty());
        assert_eq!(state.committed, vec![(w.wall(), 1_500_000)]);
        // every change was persisted
        assert_eq!(persist_keys(&w.effects()), vec![PersistKey::Ledger; 4]);
    }

    #[test]
    fn a_ticket_is_counted_when_its_task_is_cancelled_after_sending() {
        let mut w = World::new();
        let cx = w.cx();
        let id = w.spawn(async move {
            let mut t = cx.reserve_licence(1_000_000).expect("reserved");
            t.mark_sent();
            cx.sleep(60_000).await; // the fetch is "in flight"
            t.commit(1);
        });
        w.run();
        w.effects();
        w.kill(id); // user cancel
        assert_eq!(
            w.shared
                .with(|s| s.sched.usage(s.clock.wall).hour)
                .expect("state"),
            1
        );
        assert_eq!(persist_keys(&w.effects()), vec![PersistKey::Ledger]);
    }

    #[test]
    fn a_cap_hit_names_the_reset_time_and_leaves_nothing_reserved() {
        let mut cfg = test_config();
        cfg.inst.hourly_cap = 2;
        let w = World::with_config(cfg);
        let cx = w.cx();
        let a = cx.reserve_licence(1_000).expect("1");
        let b = cx.reserve_licence(1_000).expect("2");
        w.effects();
        let hit = cx
            .reserve_licence(1_000)
            .err()
            .expect("third must be refused");
        assert_eq!(
            hit,
            Refused::Cap(CapHit {
                kind: CapKind::Hourly,
                reset_at: w.wall() + policy::HOUR_MS
            })
        );
        assert!(
            w.effects().is_empty(),
            "a refused reservation changes nothing and persists nothing"
        );
        a.rollback();
        b.rollback();
        assert!(cx.reserve_licence(1_000).is_ok());
    }

    #[test]
    fn reserve_licence_is_refused_while_halted_or_shutting_down() {
        let w = World::new();
        let cx = w.cx();
        cx.halt_inst(HaltReason::BlockSignals, None);
        w.effects();
        assert!(matches!(
            cx.reserve_licence(1).err(),
            Some(Refused::InstHalted { .. })
        ));
        assert!(w.effects().is_empty());
        let w = World::new();
        let _ = w.shared.with(|s| s.sched.begin_shutdown());
        assert_eq!(w.cx().reserve_licence(1).err(), Some(Refused::ShuttingDown));
    }

    // --------------------------------------------------------------------------------------- knowledge helpers

    #[test]
    fn ua_timeout_and_cache_helpers_delegate() {
        let w = World::new();
        let cx = w.cx();
        assert_eq!(cx.ua_for("a.org"), Ua::Default);
        cx.pin_plain_ua("a.org");
        assert_eq!(cx.ua_for("a.org"), Ua::Plain);
        assert_eq!(
            cx.timeout_for(FetchClass::Landing, "onlinelibrary.wiley.com"),
            60_000
        );
        assert_eq!(cx.timeout_for(FetchClass::Landing, "bmj.com"), 20_000);
        assert_eq!(cx.timeout_for(FetchClass::Pdf, "www.wiley.com"), 120_000);

        assert_eq!(cx.ezproxy_known("www.bmj.com"), None);
        cx.learn_ezproxy("www.bmj.com", true);
        assert_eq!(cx.ezproxy_known("www.bmj.com"), Some(true));
        assert_eq!(cx.alma_known("10.1/x"), None);
        cx.learn_alma("10.1/x", false);
        assert_eq!(cx.alma_known("10.1/x"), Some(false));
        let keys = persist_keys(&w.effects());
        assert_eq!(keys, vec![PersistKey::Ezproxy, PersistKey::Alma]);
        // the caches expire with the wall clock
        w.advance(policy::DAY_MS);
        assert_eq!(cx.alma_known("10.1/x"), None);
        assert_eq!(cx.ezproxy_known("www.bmj.com"), Some(true));
        w.advance(6 * policy::DAY_MS);
        assert_eq!(cx.ezproxy_known("www.bmj.com"), None);
    }

    // ------------------------------------------------------------------------------------------ stress

    /// What the stress test observes: who is in flight where, and when each gate may serve again.
    #[derive(Default)]
    struct Book {
        global: u32,
        max_global: u32,
        counts: BTreeMap<String, u32>,
        ready_at: BTreeMap<String, u64>,
        violations: Vec<String>,
        grants: u32,
    }

    /// RAII record of one grant: entering checks the invariants, dropping (finish OR cancel) records the release.
    struct Flight {
        book: Rc<RefCell<Book>>,
        cx: Cx,
        name: String,
        gap_floor_ms: u64,
        inst: bool,
        cap: u32,
    }

    impl Flight {
        fn enter(
            book: &Rc<RefCell<Book>>,
            cx: &Cx,
            name: &str,
            gap_floor_ms: u64,
            inst: bool,
            cap: u32,
        ) -> Flight {
            let now = cx.now();
            let mut b = book.borrow_mut();
            if let Some(ready) = b.ready_at.get(name).copied()
                && now < ready
            {
                b.violations
                    .push(format!("{name} granted at {now}, gap lasts until {ready}"));
            }
            let n = {
                let n = b.counts.entry(name.to_owned()).or_insert(0);
                *n += 1;
                *n
            };
            if n > cap {
                b.violations
                    .push(format!("{name}: {n} in flight, cap {cap}"));
            }
            if inst {
                b.global += 1;
                b.max_global = b.max_global.max(b.global);
                if b.global > 2 {
                    let g = b.global;
                    b.violations
                        .push(format!("{g} institutional requests in flight"));
                }
            }
            b.grants += 1;
            Flight {
                book: Rc::clone(book),
                cx: cx.clone(),
                name: name.to_owned(),
                gap_floor_ms,
                inst,
                cap,
            }
        }
    }

    impl Drop for Flight {
        fn drop(&mut self) {
            let now = self.cx.now();
            let mut b = self.book.borrow_mut();
            if let Some(n) = b.counts.get_mut(&self.name) {
                *n = n.saturating_sub(1);
            }
            if self.inst {
                b.global = b.global.saturating_sub(1);
            }
            let _ = self.cap;
            b.ready_at
                .insert(self.name.clone(), now + self.gap_floor_ms);
        }
    }

    fn stress_task(w: &mut World, rng: &mut Lcg, book: &Rc<RefCell<Book>>) -> usize {
        let (cx, book) = (w.cx(), Rc::clone(book));
        let (start, hold, which) = (rng.below(3_000), rng.below(1_500), rng.below(10));
        let family = ["wiley", "elsevier", "lww", "oup"][(which % 4) as usize];
        let kind = if hold % 2 == 0 {
            FetchKind::Pdf
        } else {
            FetchKind::Page
        };
        let prio = if hold % 3 == 0 {
            Prio::Resumed
        } else {
            Prio::New
        };
        w.spawn(async move {
            cx.sleep(start).await;
            match which {
                0..=5 => {
                    if let Ok(p) = cx.acquire_inst(&InstKey(family.to_owned()), kind).await {
                        let floor = if kind == FetchKind::Pdf { 5_000 } else { 2_000 };
                        let flight = Flight::enter(&book, &cx, family, floor, true, 1);
                        cx.sleep(hold).await;
                        drop(flight);
                        drop(p);
                    }
                }
                6..=7 => {
                    let host = if which == 6 { "h1.org" } else { "h2.org" };
                    if let Ok(mut p) = cx.acquire_oa_host(host).await {
                        let flight = Flight::enter(&book, &cx, host, 1_000, false, 1);
                        cx.sleep(hold).await;
                        p.report(true);
                        drop(flight);
                        drop(p);
                    }
                }
                _ => {
                    if let Ok(p) = cx.acquire_item_slot(prio).await {
                        let flight = Flight::enter(&book, &cx, "items", 0, false, 4);
                        cx.sleep(hold).await;
                        drop(flight);
                        drop(p);
                    }
                }
            }
        })
    }

    #[test]
    fn random_load_respects_every_limit_and_never_strands_a_waiter() {
        let (mut saw_two_in_flight, mut kills) = (false, 0usize);
        for seed in 1..=30u64 {
            let mut w = World::new();
            let mut rng = Lcg(seed.wrapping_mul(7_919));
            let book = Rc::new(RefCell::new(Book::default()));
            let ids: Vec<usize> = (0..48)
                .map(|_| stress_task(&mut w, &mut rng, &book))
                .collect();
            let mut killed = BTreeSet::new();
            for _ in 0..120 {
                w.run();
                if rng.below(4) == 0 {
                    let victim = ids[rng.below(ids.len() as u64) as usize];
                    if !w.finished(victim) {
                        w.kill(victim);
                        killed.insert(victim);
                    }
                }
                w.advance(rng.below(900) + 1);
            }
            for _ in 0..300 {
                w.run();
                w.advance(1_000);
            }
            w.run();
            let b = book.borrow();
            assert!(b.violations.is_empty(), "seed {seed}: {:?}", b.violations);
            assert!(b.grants > 0);
            for id in &ids {
                assert!(
                    killed.contains(id) || w.finished(*id),
                    "seed {seed}: task {id} was stranded"
                );
            }
            let idle = w
                .shared
                .with(|s| {
                    s.sched
                        .limiters
                        .iter()
                        .map(|(k, l)| (format!("{k:?}"), l.in_use, l.waiting()))
                        .filter(|(_, u, n)| *u != 0 || *n != 0)
                        .collect::<Vec<_>>()
                })
                .expect("state");
            assert!(
                idle.is_empty(),
                "seed {seed}: gates not idle at the end: {idle:?}"
            );
            assert_eq!(w.timers(), 0, "seed {seed}: timers left armed");
            assert_eq!(b.global, 0);
            saw_two_in_flight |= b.max_global == 2;
            kills += killed.len();
        }
        // the load really reaches the global cap, and cancellations really happen while tasks wait or hold
        assert!(saw_two_in_flight);
        assert!(kills > 10, "only {kills} cancellations");
    }

    #[test]
    fn three_failed_api_permits_switch_the_api_off_for_five_minutes() {
        let w = World::new();
        let cx = w.cx();
        let take = |src| cx.acquire_api(src).now_or_never().and_then(Result::ok);
        for _ in 0..2 {
            let mut p = take(Source::Europepmc).expect("granted");
            p.report(false);
        }
        // a success in between would have reset the streak; here the third failure trips the breaker
        let mut p = take(Source::Europepmc).expect("still on after two failures");
        p.report(false);
        drop(p);
        let refused = cx
            .acquire_api(Source::Europepmc)
            .now_or_never()
            .map(|r| r.err());
        assert_eq!(
            refused,
            Some(Some(Refused::HostSkipped {
                host: "europepmc".into()
            }))
        );
        assert!(
            take(Source::Openalex).is_some(),
            "other APIs are unaffected"
        );
        w.advance(policy::API_COOLDOWN_MS - 1);
        assert!(
            cx.acquire_api(Source::Europepmc)
                .now_or_never()
                .is_some_and(|r| r.is_err())
        );
        w.advance(1);
        let mut p = take(Source::Europepmc).expect("back after the cool-down");
        p.report(true);
    }

    #[test]
    fn a_successful_api_permit_resets_the_failure_streak() {
        let w = World::new();
        let cx = w.cx();
        for ok in [false, false, true, false, false] {
            let mut p = cx
                .acquire_api(Source::Core)
                .now_or_never()
                .and_then(Result::ok)
                .expect("never switched off");
            p.report(ok);
            drop(p);
            w.advance(6_000); // CORE refills one token per 6 s
        }
    }

    #[test]
    fn a_tripped_api_refuses_the_queue_behind_it_at_once() {
        let mut w = World::new();
        let cx = w.cx();
        // bioRxiv allows 1 in flight: hold it, queue a second request, fail the holder three times over
        let holder = Rc::new(RefCell::new(None));
        let got = log();
        {
            let (cx, holder) = (cx.clone(), Rc::clone(&holder));
            w.spawn(async move {
                *holder.borrow_mut() = Some(cx.acquire_api(Source::Biorxiv).await.expect("first"));
            });
        }
        w.run();
        {
            let (cx, got) = (cx.clone(), Rc::clone(&got));
            w.spawn(async move {
                let r = cx.acquire_api(Source::Biorxiv).await;
                got.borrow_mut().push(format!("{:?}", r.err()));
            });
        }
        w.run();
        let _ = w.shared.with(|s| {
            s.sched.hosts.note_api_outcome(Source::Biorxiv, false, 0);
            s.sched.hosts.note_api_outcome(Source::Biorxiv, false, 0);
        });
        if let Some(p) = holder.borrow_mut().as_mut() {
            p.report(false); // the third failure
        }
        drop(holder.borrow_mut().take());
        w.run();
        assert_eq!(
            entries(&got),
            vec![format!(
                "{:?}",
                Some(Refused::HostSkipped {
                    host: "biorxiv".into()
                })
            )]
        );
    }

    #[test]
    fn www_and_the_bare_host_share_one_oa_gate() {
        let w = World::new();
        let cx = w.cx();
        let first = cx
            .acquire_oa_host("WWW.Example.org")
            .now_or_never()
            .and_then(Result::ok)
            .expect("first");
        let mut second = Box::pin(cx.acquire_oa_host("example.org"));
        let (flag, waker) = flag_waker();
        assert!(
            second
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending(),
            "same gate: it has to wait"
        );
        drop(first);
        assert_eq!(flag.count(), 1, "released permit wakes the waiter");
        // failures and challenges are counted per normalised host as well
        cx.note_challenge("www.h.org");
        assert!(cx.note_challenge("H.ORG"));
        assert!(cx.host_skipped("www.h.org"));
    }

    #[test]
    fn a_hard_block_walls_the_host_for_thirty_minutes_and_refuses_its_queue() {
        let mut w = World::new();
        let cx = w.cx();
        let holder = Rc::new(RefCell::new(None));
        {
            let (cx, holder) = (cx.clone(), Rc::clone(&holder));
            w.spawn(async move {
                *holder.borrow_mut() =
                    Some(cx.acquire_oa_host("blocked.org").await.expect("first"));
            });
        }
        w.run();
        let got = log();
        {
            let (cx, got) = (cx.clone(), Rc::clone(&got));
            w.spawn(async move {
                let r = cx.acquire_oa_host("blocked.org").await;
                got.borrow_mut().push(format!("{:?}", r.err()));
            });
        }
        w.run();
        cx.wall_host("www.blocked.org", "blocked");
        w.run();
        assert_eq!(
            entries(&got),
            vec![format!(
                "{:?}",
                Some(Refused::HostSkipped {
                    host: "blocked.org".into()
                })
            )]
        );
        assert!(cx.host_skipped("blocked.org"));
        assert!(!cx.host_skipped("other.org"));
        drop(holder.borrow_mut().take());
        w.advance(policy::HOST_WALL_TTL_MS);
        assert!(!cx.host_skipped("blocked.org"));
    }

    #[test]
    fn a_blocked_oa_host_refuses_its_queue() {
        let mut w = World::new();
        let cx = w.cx();
        let holder = Rc::new(RefCell::new(None));
        {
            let (cx, holder) = (cx.clone(), Rc::clone(&holder));
            w.spawn(async move {
                *holder.borrow_mut() = Some(cx.acquire_oa_host("b.org").await.expect("first"));
            });
        }
        w.run();
        let got = log();
        {
            let (cx, got) = (cx.clone(), Rc::clone(&got));
            w.spawn(async move {
                let r = cx.acquire_oa_host("www.b.org").await;
                got.borrow_mut().push(format!("{:?}", r.err()));
            });
        }
        w.run();
        cx.block_host("www.b.org", 60_000);
        w.run();
        assert_eq!(
            entries(&got),
            vec![format!(
                "{:?}",
                Some(Refused::HostSkipped {
                    host: "b.org".into()
                })
            )]
        );
        drop(holder.borrow_mut().take());
    }

    #[test]
    fn shutdown_refuses_new_and_waiting_acquires() {
        let mut w = World::new();
        let cx = w.cx();
        let holder = Rc::new(RefCell::new(None));
        {
            let (cx, holder) = (cx.clone(), Rc::clone(&holder));
            w.spawn(async move {
                *holder.borrow_mut() = Some(cx.acquire_save().await.expect("first"));
            });
        }
        w.run();
        let got = log();
        {
            let (cx, got) = (cx.clone(), Rc::clone(&got));
            w.spawn(async move {
                let r = cx.acquire_save().await;
                got.borrow_mut().push(format!("{:?}", r.err()));
            });
        }
        w.run();
        let wakers = w.shared.with(|s| {
            s.sched.begin_shutdown();
            s.sched.gate_wakers(&GateKey::Save)
        });
        wakers.unwrap_or_default().into_iter().for_each(Waker::wake);
        w.run();
        assert_eq!(entries(&got), vec!["Some(ShuttingDown)".to_owned()]);
        drop(holder.borrow_mut().take());
    }
}
