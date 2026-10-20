//! The reactor and the leaf futures: how an `.await` becomes an effect and a completion becomes a wake-up.
//!
//! # Contract
//! * Constructing an op future does nothing. The effect is emitted on the FIRST POLL (so a future that is built
//!   and dropped without polling costs nothing, and effect order equals poll order).
//! * First poll: `Reactor::submit` allocates a fresh `OpId` (u64, starts at 1, never reused), the effect is pushed
//!   to `State::outbox`, the slot is `Waiting`. Every later poll refreshes the stored `Waker`.
//! * `Engine::step` delivers a completion with `Reactor::complete`: slot -> `Done(msg)`, the stored waker is
//!   returned and woken AFTER the `with` closure ends. The next poll `take`s the message and decodes it.
//! * A completion for an unknown or already-cancelled op id is recorded in `ignored` (reported in
//!   `StepOut.ignored`; JS releases any body it carried). It is never an error.
//! * Dropping a `Waiting` op future removes the slot and pushes `Effect::Cancel { op }` (idempotent on the JS
//!   side; a late completion lands in `ignored`). Dropping a future whose completion arrived but was never polled
//!   (the item was cancelled in the step that delivered the answer) removes the `Done` slot and pushes a
//!   `Release` for every body handle the answer carried: nobody else would. Dropping an unpolled or finished
//!   future pushes nothing.
//! * Bodies: a `BodyRef` received in a completion is wrapped in [`Body`]; dropping a `Body` pushes
//!   `Effect::Release { handle }`. The engine therefore releases every handle exactly once, including after a
//!   `SavePdf` (JS must treat `Release` of an unknown/consumed handle as a no-op) and when an item is cancelled.
//!   A payload that cannot be decoded (`OpErrKind::Internal`) releases the handles it carried at once.
//! * Timers are virtual: `Sleep` registers `(deadline, seq) -> Waker`; `Engine::step` fires `due_timers(now)`;
//!   `next_deadline()` becomes `StepOut.next_timer`. Dropping a `Sleep` removes its entry.
//! * `Timeout` polls the inner future before the timer (a result that is ready wins a tie) and drops the loser
//!   the moment the timer wins, so its ops are cancelled in the same poll that reports `TimedOut`.
use crate::cx::{Cx, Owner, Shared};
use crate::proto::*;
use fulltext_core::model::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Result of a typed op: the decoded payload or the JS-reported / decode error.
pub(crate) type OpResult<T> = Result<T, OpErr>;

/// An `OpErr` for engine-side faults (undecodable payload, reactor re-entered).
pub(crate) fn internal_err(message: impl Into<String>) -> OpErr {
    OpErr {
        kind: OpErrKind::Internal,
        message: message.into(),
        after_ms: None,
        status: None,
    }
}

/// Which effect an op slot belongs to (for stats and `DumpState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum OpKind {
    Http,
    BrowserLoad,
    LoginWindow,
    RunTranslators,
    ZoteroOa,
    SavePdf,
    Confirm,
}

/// One variant per op-carrying effect; `into_effect` fills `op`/`run`/`item`.
pub(crate) enum OpRequest {
    Http {
        route: Route,
        barrier: bool,
        req: HttpReq,
        retain: Retain,
        inspect: InspectSpec,
    },
    BrowserLoad {
        route: Route,
        barrier: bool,
        req: BrowserReq,
        inspect: InspectSpec,
    },
    LoginWindow {
        req: LoginReq,
    },
    RunTranslators {
        page: BodyRef,
        url: String,
    },
    ZoteroOa {
        doi: String,
    },
    SavePdf {
        req: SaveReq,
    },
    Confirm {
        req: ConfirmReq,
    },
}

impl OpRequest {
    pub(crate) fn kind(&self) -> OpKind {
        match self {
            OpRequest::Http { .. } => OpKind::Http,
            OpRequest::BrowserLoad { .. } => OpKind::BrowserLoad,
            OpRequest::LoginWindow { .. } => OpKind::LoginWindow,
            OpRequest::RunTranslators { .. } => OpKind::RunTranslators,
            OpRequest::ZoteroOa { .. } => OpKind::ZoteroOa,
            OpRequest::SavePdf { .. } => OpKind::SavePdf,
            OpRequest::Confirm { .. } => OpKind::Confirm,
        }
    }

    /// Build the wire effect.
    pub(crate) fn into_effect(self, op: OpId, run: RunId, item: ItemId) -> Effect {
        match self {
            OpRequest::Http {
                route,
                barrier,
                req,
                retain,
                inspect,
            } => Effect::Http {
                op,
                run,
                item,
                route,
                barrier,
                req,
                retain,
                inspect,
            },
            OpRequest::BrowserLoad {
                route,
                barrier,
                req,
                inspect,
            } => Effect::BrowserLoad {
                op,
                run,
                item,
                route,
                barrier,
                req,
                inspect,
            },
            OpRequest::LoginWindow { req } => Effect::LoginWindow { op, run, item, req },
            OpRequest::RunTranslators { page, url } => Effect::RunTranslators {
                op,
                run,
                item,
                page,
                url,
            },
            OpRequest::ZoteroOa { doi } => Effect::ZoteroOa { op, run, item, doi },
            OpRequest::SavePdf { req } => Effect::SavePdf { op, run, item, req },
            OpRequest::Confirm { req } => Effect::Confirm { op, run, req },
        }
    }
}

/// Key of a timer entry: (deadline ms, sequence) so equal deadlines stay FIFO and entries are unique.
pub(crate) type TimerKey = (u64, u64);

enum Slot {
    Waiting {
        waker: Option<Waker>,
        owner: Owner,
        kind: OpKind,
    },
    Done(CompletionMsg),
}

/// What [`Reactor::drop_op`] found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// Still waiting for JS: the caller pushes `Effect::Cancel`.
    Waiting,
    /// JS had already answered but nobody polled the result: release these body handles.
    Completed(Vec<u32>),
    /// No slot (never submitted, or already consumed).
    Unknown,
}

/// Body handles (`body`, `pdf`, `page`) in a completion payload, found without decoding the payload so that a
/// malformed or abandoned completion can still be released.
fn body_handles(ok: &Value) -> Vec<u32> {
    ["body", "pdf", "page"]
        .iter()
        .filter_map(|key| ok.get(*key)?.get("handle")?.as_u64())
        .filter_map(|h| u32::try_from(h).ok())
        .collect()
}

/// Op slots + virtual timers. Plain data; lives inside `State`.
#[derive(Default)]
pub(crate) struct Reactor {
    next_op: OpId,
    next_timer_seq: u64,
    slots: BTreeMap<OpId, Slot>,
    timers: BTreeMap<TimerKey, Waker>,
    ignored: Vec<OpId>,
}

impl Reactor {
    pub(crate) fn new() -> Self {
        Self {
            next_op: 1,
            ..Self::default()
        }
    }

    /// A fresh op id with NO slot, for effects that carry an `op` but expect no awaited completion
    /// (`Effect::LoginUpdate`: JS completes it immediately and the completion lands in `StepOut.ignored`).
    pub(crate) fn alloc_id(&mut self) -> OpId {
        let op = self.next_op.max(1);
        self.next_op = op.saturating_add(1);
        op
    }

    /// Allocate an op id and a `Waiting` slot (no waker yet; the first `park` stores it).
    pub(crate) fn submit(&mut self, owner: Owner, kind: OpKind) -> OpId {
        let op = self.next_op.max(1);
        self.next_op = op.saturating_add(1);
        self.slots.insert(
            op,
            Slot::Waiting {
                waker: None,
                owner,
                kind,
            },
        );
        op
    }

    /// Store / refresh the waker of a waiting op. No-op when the slot is gone or done.
    pub(crate) fn park(&mut self, op: OpId, waker: &Waker) {
        if let Some(Slot::Waiting { waker: w, .. }) = self.slots.get_mut(&op) {
            match w {
                Some(old) if old.will_wake(waker) => {}
                _ => *w = Some(waker.clone()),
            }
        }
    }

    /// Take the completion of a finished op (removes the slot). `None` while still waiting or unknown.
    pub(crate) fn take(&mut self, op: OpId) -> Option<CompletionMsg> {
        match self.slots.remove(&op) {
            Some(Slot::Done(msg)) => Some(msg),
            Some(waiting) => {
                self.slots.insert(op, waiting);
                None
            }
            None => None,
        }
    }

    /// Forget a waiting op. `true` when it was waiting (the caller then pushes `Effect::Cancel`).
    #[cfg(test)]
    pub(crate) fn cancel(&mut self, op: OpId) -> bool {
        matches!(self.drop_op(op), Dropped::Waiting)
    }

    /// Forget an op whose future is being dropped, whatever state the slot is in. A `Done` slot is removed too
    /// (nothing will ever poll it) and the body handles its completion carried are returned so the caller can
    /// release them: JS handed us those bodies, nobody else will.
    pub(crate) fn drop_op(&mut self, op: OpId) -> Dropped {
        match self.slots.remove(&op) {
            Some(Slot::Waiting { .. }) => Dropped::Waiting,
            Some(Slot::Done(msg)) => {
                Dropped::Completed(msg.ok.as_ref().map(body_handles).unwrap_or_default())
            }
            None => Dropped::Unknown,
        }
    }

    /// Deliver a completion. Returns the waker to call (outside the `with` closure) when it was waiting;
    /// unknown / cancelled / duplicate completions are recorded in `ignored` and return `None`.
    pub(crate) fn complete(&mut self, msg: CompletionMsg) -> Option<Waker> {
        let op = msg.op;
        match self.slots.get_mut(&op) {
            Some(slot @ Slot::Waiting { .. }) => {
                let old = std::mem::replace(slot, Slot::Done(msg));
                match old {
                    Slot::Waiting { waker, .. } => waker,
                    Slot::Done(_) => None,
                }
            }
            _ => {
                self.ignored.push(op);
                None
            }
        }
    }

    pub(crate) fn take_ignored(&mut self) -> Vec<OpId> {
        std::mem::take(&mut self.ignored)
    }

    /// Register a one-shot timer.
    pub(crate) fn add_timer(&mut self, deadline: u64, waker: &Waker) -> TimerKey {
        self.next_timer_seq = self.next_timer_seq.saturating_add(1);
        let key = (deadline, self.next_timer_seq);
        self.timers.insert(key, waker.clone());
        key
    }

    pub(crate) fn remove_timer(&mut self, key: TimerKey) {
        self.timers.remove(&key);
    }

    /// Refresh the waker of a pending timer (a `Sleep` re-polled by a different task waker).
    pub(crate) fn park_timer(&mut self, key: TimerKey, waker: &Waker) {
        if let Some(w) = self.timers.get_mut(&key)
            && !w.will_wake(waker)
        {
            *w = waker.clone();
        }
    }

    pub(crate) fn timer_pending(&self, key: TimerKey) -> bool {
        self.timers.contains_key(&key)
    }

    /// Remove and return the wakers of all timers with `deadline <= now` (earliest first).
    pub(crate) fn due_timers(&mut self, now: u64) -> Vec<Waker> {
        let later = self.timers.split_off(&(now.saturating_add(1), 0));
        let due = std::mem::replace(&mut self.timers, later);
        due.into_values().collect()
    }

    /// Earliest pending deadline -> `StepOut.next_timer`.
    pub(crate) fn next_deadline(&self) -> Option<u64> {
        self.timers.keys().next().map(|(deadline, _)| *deadline)
    }

    /// Ops waiting for a completion (`Stats.ops_inflight`).
    pub(crate) fn inflight(&self) -> usize {
        self.slots
            .values()
            .filter(|s| matches!(s, Slot::Waiting { .. }))
            .count()
    }

    pub(crate) fn timer_count(&self) -> usize {
        self.timers.len()
    }

    /// (op, owner, kind) of every waiting op, for `DumpState` and invariants.
    pub(crate) fn waiting(&self) -> Vec<(OpId, Owner, OpKind)> {
        self.slots
            .iter()
            .filter_map(|(op, s)| match s {
                Slot::Waiting { owner, kind, .. } => Some((*op, *owner, *kind)),
                Slot::Done(_) => None,
            })
            .collect()
    }
}

// --------------------------------------------------------------------------------------------- leaf futures

enum OpState {
    Unsent(Box<OpRequest>),
    Waiting(OpId),
    Finished,
}

/// The one leaf future that talks to JS. Output is the raw `ok` JSON (`Null` when JS sent neither `ok` nor
/// `err`) or the `OpErr`. Typed wrappers on [`Cx`] decode it.
pub(crate) struct OpFuture {
    shared: Rc<Shared>,
    owner: Owner,
    run: RunId,
    item: ItemId,
    state: OpState,
}

impl Future for OpFuture {
    type Output = OpResult<Value>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match std::mem::replace(&mut this.state, OpState::Finished) {
            OpState::Unsent(req) => {
                let (owner, run, item) = (this.owner, this.run, this.item);
                let kind = req.kind();
                let submitted = this.shared.with(|s| {
                    let op = s.reactor.submit(owner, kind);
                    s.reactor.park(op, cx.waker());
                    s.outbox.push((*req).into_effect(op, run, item));
                    op
                });
                match submitted {
                    Some(op) => {
                        this.state = OpState::Waiting(op);
                        Poll::Pending
                    }
                    None => Poll::Ready(Err(internal_err("reactor re-entered on submit"))),
                }
            }
            OpState::Waiting(op) => {
                let taken = this.shared.with(|s| {
                    let msg = s.reactor.take(op);
                    if msg.is_none() {
                        s.reactor.park(op, cx.waker());
                    }
                    msg
                });
                match taken {
                    Some(Some(msg)) => Poll::Ready(match msg.err {
                        Some(e) => Err(e),
                        None => Ok(msg.ok.unwrap_or(Value::Null)),
                    }),
                    Some(None) => {
                        this.state = OpState::Waiting(op);
                        Poll::Pending
                    }
                    None => Poll::Ready(Err(internal_err("reactor re-entered on poll"))),
                }
            }
            OpState::Finished => {
                Poll::Ready(Err(internal_err("op future polled after completion")))
            }
        }
    }
}

impl Drop for OpFuture {
    fn drop(&mut self) {
        if let OpState::Waiting(op) = self.state {
            let _ = self.shared.with(|s| match s.reactor.drop_op(op) {
                Dropped::Waiting => s.outbox.push(Effect::Cancel { op }),
                Dropped::Completed(handles) => {
                    s.outbox
                        .extend(handles.into_iter().map(|handle| Effect::Release { handle }));
                }
                Dropped::Unknown => {}
            });
        }
    }
}

/// Virtual-time sleep. Resolves once `StepIn.now >= deadline`.
pub(crate) struct Sleep {
    shared: Rc<Shared>,
    deadline: u64,
    timer: Option<TimerKey>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        let deadline = this.deadline;
        let timer = this.timer;
        let step = this.shared.with(|s| {
            if s.clock.now >= deadline {
                if let Some(k) = timer {
                    s.reactor.remove_timer(k);
                }
                return (true, None);
            }
            match timer {
                Some(k) if s.reactor.timer_pending(k) => {
                    s.reactor.park_timer(k, cx.waker());
                    (false, Some(k))
                }
                _ => (false, Some(s.reactor.add_timer(deadline, cx.waker()))),
            }
        });
        match step {
            Some((true, _)) | None => {
                this.timer = None;
                Poll::Ready(())
            }
            Some((false, key)) => {
                this.timer = key;
                Poll::Pending
            }
        }
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(k) = self.timer.take() {
            let _ = self.shared.with(|s| s.reactor.remove_timer(k));
        }
    }
}

/// `Err(TimedOut)` when the deadline fired before the inner future completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimedOut;

/// `timeout(ms, fut)`: polls `fut` first, then the timer (so a result that is already ready wins a tie). The
/// losing future is dropped the moment the timer wins (not when the `Timeout` itself is dropped), which cancels
/// its ops in the same poll that reports `TimedOut`.
pub(crate) struct Timeout<F> {
    /// `None` once the timer has won; polling again keeps answering `TimedOut`.
    fut: Option<Pin<Box<F>>>,
    sleep: Sleep,
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, TimedOut>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if let Some(fut) = this.fut.as_mut()
            && let Poll::Ready(v) = fut.as_mut().poll(cx)
        {
            this.fut = None;
            return Poll::Ready(Ok(v));
        }
        match Pin::new(&mut this.sleep).poll(cx) {
            Poll::Ready(()) => {
                this.fut = None;
                Poll::Ready(Err(TimedOut))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// Both fields are `Unpin` (`Pin<Box<F>>`, `Sleep`).
impl<F> Unpin for Timeout<F> {}

// ------------------------------------------------------------------------------------------------- Body

/// A body held by JS. Dropping it releases the handle (`Effect::Release`).
pub(crate) struct Body {
    r: BodyRef,
    shared: Rc<Shared>,
}

impl Body {
    pub(crate) fn new(shared: Rc<Shared>, r: BodyRef) -> Self {
        Self { r, shared }
    }

    pub(crate) fn body_ref(&self) -> BodyRef {
        self.r
    }

    pub(crate) fn len(&self) -> u64 {
        self.r.len
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        let handle = self.r.handle;
        let _ = self
            .shared
            .with(|s| s.outbox.push(Effect::Release { handle }));
    }
}

// ---------------------------------------------------------------------------------------- typed responses

/// Decoded `HttpOk`; `body` is RAII.
pub(crate) struct HttpResp {
    pub status: u16,
    pub final_url: String,
    pub headers: BTreeMap<String, String>,
    pub bytes: u64,
    #[allow(dead_code)] // kept so the struct mirrors its source; nothing reads it yet
    pub ms: u32,
    pub inspect: Inspection,
    pub body: Option<Body>,
}

/// Decoded `BrowserOk`.
pub(crate) struct BrowserResp {
    pub final_url: String,
    pub status: Option<u16>,
    #[allow(dead_code)] // kept so the struct mirrors its source; nothing reads it yet
    pub ms: u32,
    #[allow(dead_code)] // kept so the struct mirrors its source; nothing reads it yet
    pub url_trail: Vec<String>,
    pub inspect: Inspection,
    pub pdf: Option<Body>,
    pub page: Option<Body>,
}

/// Decoded `LoginOk`.
pub(crate) struct LoginResp {
    pub result: LoginResult,
    pub final_url: String,
    pub ca_error: Option<String>,
    pub inspect: Inspection,
    pub pdf: Option<Body>,
    pub page: Option<Body>,
}

// ------------------------------------------------------------------------------------------ Cx op wrappers

impl Cx {
    /// Decode an `ok` payload. A payload that does not fit `T` is an `Internal` op error, never a panic; the body
    /// handles it carried are released at once because no `Body` will ever own them.
    fn decode<T: DeserializeOwned>(&self, v: Value) -> OpResult<T> {
        let handles = body_handles(&v);
        serde_json::from_value(v).map_err(|e| {
            for handle in handles {
                self.emit(Effect::Release { handle });
            }
            internal_err(format!("malformed completion payload: {e}"))
        })
    }

    /// Raw op future for any request.
    pub(crate) fn raw_op(&self, req: OpRequest) -> OpFuture {
        OpFuture {
            shared: Rc::clone(&self.shared),
            owner: self.owner,
            run: self.run,
            item: self.item,
            state: OpState::Unsent(Box::new(req)),
        }
    }

    /// Virtual sleep for `ms` from the current step's `now`.
    pub(crate) fn sleep(&self, ms: u64) -> Sleep {
        self.sleep_until(self.now().saturating_add(ms))
    }

    pub(crate) fn sleep_until(&self, deadline: u64) -> Sleep {
        Sleep {
            shared: Rc::clone(&self.shared),
            deadline,
            timer: None,
        }
    }

    /// Race `fut` against a virtual timer of `ms`.
    pub(crate) fn timeout<F: Future>(&self, ms: u64, fut: F) -> Timeout<F> {
        Timeout {
            fut: Some(Box::pin(fut)),
            sleep: self.sleep(ms),
        }
    }

    /// `Effect::Http` -> `HttpOk` (body wrapped in RAII `Body`).
    pub(crate) async fn http(
        &self,
        route: Route,
        barrier: bool,
        req: HttpReq,
        retain: Retain,
        inspect: InspectSpec,
    ) -> OpResult<HttpResp> {
        let v = self
            .raw_op(OpRequest::Http {
                route,
                barrier,
                req,
                retain,
                inspect,
            })
            .await?;
        let ok: HttpOk = self.decode(v)?;
        Ok(HttpResp {
            status: ok.status,
            final_url: ok.final_url,
            headers: ok.headers,
            bytes: ok.bytes,
            ms: ok.ms,
            inspect: ok.inspect,
            body: ok.body.map(|r| Body::new(Rc::clone(&self.shared), r)),
        })
    }

    /// `Effect::BrowserLoad` -> `BrowserOk`.
    pub(crate) async fn browser_load(
        &self,
        route: Route,
        barrier: bool,
        req: BrowserReq,
        inspect: InspectSpec,
    ) -> OpResult<BrowserResp> {
        let v = self
            .raw_op(OpRequest::BrowserLoad {
                route,
                barrier,
                req,
                inspect,
            })
            .await?;
        let ok: BrowserOk = self.decode(v)?;
        Ok(BrowserResp {
            final_url: ok.final_url,
            status: ok.status,
            ms: ok.ms,
            url_trail: ok.url_trail,
            inspect: ok.inspect,
            pdf: ok.pdf.map(|r| Body::new(Rc::clone(&self.shared), r)),
            page: ok.page.map(|r| Body::new(Rc::clone(&self.shared), r)),
        })
    }

    /// `Effect::LoginWindow` -> `LoginOk`. Low level: pipeline code goes through `login.rs`
    /// (`session_*`, `challenge_window`) so that only one window is ever open.
    pub(crate) async fn login_window(&self, req: LoginReq) -> OpResult<LoginResp> {
        let v = self.raw_op(OpRequest::LoginWindow { req }).await?;
        let ok: LoginOk = self.decode(v)?;
        Ok(LoginResp {
            result: ok.result,
            final_url: ok.final_url,
            ca_error: ok.ca_error,
            inspect: ok.inspect,
            pdf: ok.pdf.map(|r| Body::new(Rc::clone(&self.shared), r)),
            page: ok.page.map(|r| Body::new(Rc::clone(&self.shared), r)),
        })
    }

    /// `Effect::RunTranslators` -> `TranslatorsOk` (the page body stays owned by the caller).
    pub(crate) async fn run_translators(&self, page: &Body, url: &str) -> OpResult<TranslatorsOk> {
        let v = self
            .raw_op(OpRequest::RunTranslators {
                page: page.body_ref(),
                url: url.to_owned(),
            })
            .await?;
        self.decode(v)
    }

    /// `Effect::ZoteroOa` -> JSON array of `ZoteroOaHit`.
    pub(crate) async fn zotero_oa(&self, doi: &str) -> OpResult<Vec<ZoteroOaHit>> {
        let v = self
            .raw_op(OpRequest::ZoteroOa {
                doi: doi.to_owned(),
            })
            .await?;
        self.decode(v)
    }

    /// `Effect::SavePdf` -> `SaveOk`. The caller keeps the `Body` alive until this returns.
    pub(crate) async fn save_pdf(&self, req: SaveReq) -> OpResult<SaveOk> {
        let v = self.raw_op(OpRequest::SavePdf { req }).await?;
        self.decode(v)
    }

    /// `Effect::Confirm` -> `ConfirmOk`.
    pub(crate) async fn confirm(&self, req: ConfirmReq) -> OpResult<ConfirmOk> {
        let v = self.raw_op(OpRequest::Confirm { req }).await?;
        self.decode(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::Executor;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts wake-ups so tests can tell whether a completion / timer woke the poller.
    struct Counter(AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A shared state, a `Cx` and a manual poller (no executor involved).
    struct Rig {
        shared: Rc<Shared>,
        cx: Cx,
        counter: Arc<Counter>,
        waker: Waker,
    }

    fn rig() -> Rig {
        let shared = Shared::new();
        let cx = Cx::new(Rc::clone(&shared), Owner::Item { run: 3, item: 7 });
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        Rig {
            shared,
            cx,
            counter,
            waker,
        }
    }

    impl Rig {
        fn poll<F: Future>(&self, f: &mut Pin<Box<F>>) -> Poll<F::Output> {
            f.as_mut().poll(&mut Context::from_waker(&self.waker))
        }

        fn outbox(&self) -> Vec<Effect> {
            self.shared.with(|s| s.outbox.clone()).unwrap_or_default()
        }

        fn take_outbox(&self) -> Vec<Effect> {
            self.shared
                .with(|s| std::mem::take(&mut s.outbox))
                .unwrap_or_default()
        }

        fn wakes(&self) -> usize {
            self.counter.0.load(Ordering::SeqCst)
        }

        /// Deliver a completion the way `Engine::step` does: complete inside `with`, wake outside.
        fn complete(&self, msg: CompletionMsg) -> bool {
            let waker = self.shared.with(|s| s.reactor.complete(msg)).flatten();
            let woke = waker.is_some();
            if let Some(w) = waker {
                w.wake();
            }
            woke
        }

        fn set_now(&self, now: u64) {
            let _ = self.shared.with(|s| s.clock.advance(now, now));
        }

        /// Fire the timers due at `now` (as `Engine::step` does) and return how many woke.
        fn fire_timers(&self, now: u64) -> usize {
            self.set_now(now);
            let wakers = self
                .shared
                .with(|s| s.reactor.due_timers(now))
                .unwrap_or_default();
            let n = wakers.len();
            wakers.into_iter().for_each(Waker::wake);
            n
        }

        fn timers(&self) -> usize {
            self.shared
                .with(|s| s.reactor.timer_count())
                .unwrap_or_default()
        }

        fn next_deadline(&self) -> Option<u64> {
            self.shared.with(|s| s.reactor.next_deadline()).flatten()
        }

        fn ignored(&self) -> Vec<OpId> {
            self.shared
                .with(|s| s.reactor.take_ignored())
                .unwrap_or_default()
        }

        fn inflight(&self) -> usize {
            self.shared
                .with(|s| s.reactor.inflight())
                .unwrap_or_default()
        }
    }

    fn oa_req(doi: &str) -> OpRequest {
        OpRequest::ZoteroOa {
            doi: doi.to_owned(),
        }
    }

    fn ok_msg(op: OpId, v: Value) -> CompletionMsg {
        CompletionMsg {
            op,
            ok: Some(v),
            err: None,
        }
    }

    fn err_msg(op: OpId, kind: OpErrKind) -> CompletionMsg {
        CompletionMsg {
            op,
            ok: None,
            err: Some(OpErr {
                kind,
                message: "boom".into(),
                after_ms: Some(5),
                status: Some(503),
            }),
        }
    }

    fn op_of(e: &Effect) -> Option<OpId> {
        match e {
            Effect::ZoteroOa { op, .. }
            | Effect::Http { op, .. }
            | Effect::BrowserLoad { op, .. } => Some(*op),
            _ => None,
        }
    }

    fn http_req() -> HttpReq {
        HttpReq {
            method: Method::Get,
            url: "https://example.org/x".into(),
            headers: BTreeMap::new(),
            body: None,
            ua: Ua::Default,
            timeout_ms: 1000,
            follow_redirects: true,
            max_bytes: 1024,
        }
    }

    // ------------------------------------------------------------------------------------------ OpFuture

    #[test]
    fn first_poll_emits_exactly_one_effect_with_a_fresh_op() {
        let r = rig();
        let mut a = Box::pin(r.cx.raw_op(oa_req("10.1/a")));
        let mut b = Box::pin(r.cx.raw_op(oa_req("10.1/b")));
        assert!(r.outbox().is_empty(), "constructing emits nothing");
        assert!(r.poll(&mut a).is_pending());
        assert!(
            r.poll(&mut a).is_pending(),
            "a second poll must not re-emit"
        );
        assert!(r.poll(&mut b).is_pending());
        let out = r.take_outbox();
        assert_eq!(out.len(), 2);
        assert!(
            matches!(&out[0], Effect::ZoteroOa { op: 1, run: 3, item: 7, doi } if doi == "10.1/a")
        );
        assert!(
            matches!(&out[1], Effect::ZoteroOa { op: 2, run: 3, item: 7, doi } if doi == "10.1/b")
        );
        assert_eq!(r.inflight(), 2);
    }

    #[test]
    fn constructing_without_polling_costs_nothing() {
        let r = rig();
        let fut = r.cx.raw_op(oa_req("10.1/a"));
        drop(fut);
        assert!(r.outbox().is_empty());
        assert_eq!(r.inflight(), 0);
        // The id sequence was not consumed either.
        let mut next = Box::pin(r.cx.raw_op(oa_req("10.1/b")));
        let _ = r.poll(&mut next);
        assert_eq!(r.outbox().first().and_then(op_of), Some(1));
    }

    #[test]
    fn completion_resolves_the_next_poll() {
        let r = rig();
        let mut ok = Box::pin(r.cx.raw_op(oa_req("a")));
        let mut err = Box::pin(r.cx.raw_op(oa_req("b")));
        let mut neither = Box::pin(r.cx.raw_op(oa_req("c")));
        for f in [&mut ok, &mut err, &mut neither] {
            assert!(r.poll(f).is_pending());
        }
        assert!(r.complete(ok_msg(1, json!({"k": 1}))));
        assert!(r.complete(err_msg(2, OpErrKind::Timeout)));
        assert!(r.complete(CompletionMsg {
            op: 3,
            ok: None,
            err: None
        }));
        assert_eq!(r.wakes(), 3, "each completion wakes the waiting poller");
        assert_eq!(r.poll(&mut ok), Poll::Ready(Ok(json!({"k": 1}))));
        match r.poll(&mut err) {
            Poll::Ready(Err(e)) => {
                assert_eq!(
                    (e.kind, e.after_ms, e.status),
                    (OpErrKind::Timeout, Some(5), Some(503))
                );
            }
            other => panic!("expected an op error, got {other:?}"),
        }
        assert_eq!(r.poll(&mut neither), Poll::Ready(Ok(Value::Null)));
        assert_eq!(r.inflight(), 0);
        assert!(
            r.take_outbox()
                .iter()
                .all(|e| !matches!(e, Effect::Cancel { .. })),
            "finished ops are not cancelled on drop"
        );
    }

    #[test]
    fn completion_does_not_wake_before_it_arrives() {
        let r = rig();
        let mut f = Box::pin(r.cx.raw_op(oa_req("a")));
        assert!(r.poll(&mut f).is_pending());
        assert_eq!(r.wakes(), 0);
        assert!(
            r.poll(&mut f).is_pending(),
            "a spurious re-poll stays pending"
        );
        assert!(r.complete(ok_msg(1, json!(1))));
        assert_eq!(r.wakes(), 1);
    }

    #[test]
    fn unknown_and_duplicate_completions_are_ignored() {
        let r = rig();
        assert!(!r.complete(ok_msg(99, json!(null))), "unknown op");
        let mut f = Box::pin(r.cx.raw_op(oa_req("a")));
        let _ = r.poll(&mut f);
        assert!(r.complete(ok_msg(1, json!(1))));
        assert!(
            !r.complete(ok_msg(1, json!(2))),
            "duplicate while the first is unread"
        );
        assert_eq!(
            r.poll(&mut f),
            Poll::Ready(Ok(json!(1))),
            "the first answer wins"
        );
        assert!(
            !r.complete(ok_msg(1, json!(3))),
            "duplicate after it was consumed"
        );
        assert_eq!(r.ignored(), vec![99, 1, 1]);
        assert!(r.ignored().is_empty(), "take_ignored drains");
    }

    #[test]
    fn dropping_a_waiting_op_pushes_one_cancel_and_the_late_completion_is_ignored() {
        let r = rig();
        let mut f = Box::pin(r.cx.raw_op(oa_req("a")));
        let _ = r.poll(&mut f);
        r.take_outbox();
        drop(f);
        assert_eq!(r.take_outbox(), vec![Effect::Cancel { op: 1 }]);
        assert_eq!(r.inflight(), 0);
        assert!(!r.complete(ok_msg(1, json!(1))));
        assert_eq!(r.ignored(), vec![1]);
        assert!(r.outbox().is_empty());
    }

    #[test]
    fn dropping_after_completion_pushes_nothing() {
        let r = rig();
        let mut f = Box::pin(r.cx.raw_op(oa_req("a")));
        let _ = r.poll(&mut f);
        r.take_outbox();
        r.complete(ok_msg(1, json!([])));
        assert!(r.poll(&mut f).is_ready());
        drop(f);
        assert!(r.outbox().is_empty());
    }

    #[test]
    fn dropping_a_completed_but_unread_op_releases_its_bodies_without_cancel() {
        let r = rig();
        let mut f = Box::pin(r.cx.raw_op(oa_req("a")));
        let _ = r.poll(&mut f);
        r.take_outbox();
        // JS answered (with a retained PDF) in the step in which the item was cancelled.
        r.complete(ok_msg(1, json!({"status": 200, "body": {"handle": 11, "len": 5}, "pdf": {"handle": 12, "len": 9}})));
        drop(f);
        assert_eq!(
            r.take_outbox(),
            vec![
                Effect::Release { handle: 11 },
                Effect::Release { handle: 12 }
            ]
        );
        assert_eq!(r.inflight(), 0);
    }

    #[test]
    fn a_polled_after_completion_future_reports_an_internal_error() {
        let r = rig();
        let mut f = Box::pin(r.cx.raw_op(oa_req("a")));
        let _ = r.poll(&mut f);
        r.complete(ok_msg(1, json!(1)));
        assert!(r.poll(&mut f).is_ready());
        match r.poll(&mut f) {
            Poll::Ready(Err(e)) => assert_eq!(e.kind, OpErrKind::Internal),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn op_ids_are_unique_across_alloc_id_and_submit() {
        let mut reactor = Reactor::new();
        let mut seen = Vec::new();
        for i in 0..20 {
            let id = if i % 3 == 0 {
                reactor.alloc_id()
            } else {
                reactor.submit(Owner::System, OpKind::Http)
            };
            seen.push(id);
        }
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "no collisions");
        assert_eq!(seen.first(), Some(&1));
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "strictly increasing");
        // alloc_id makes no slot: completing it is "unknown".
        assert!(reactor.complete(ok_msg(1, json!(null))).is_none());
        assert_eq!(reactor.take_ignored(), vec![1]);
    }

    #[test]
    fn reactor_cancel_and_drop_op_classify_the_slot() {
        let mut reactor = Reactor::new();
        let waiting = reactor.submit(Owner::System, OpKind::Http);
        let done = reactor.submit(Owner::System, OpKind::Http);
        reactor.complete(ok_msg(done, json!({"pdf": {"handle": 3, "len": 1}})));
        assert!(reactor.cancel(waiting), "a waiting op is cancelled");
        assert!(!reactor.cancel(waiting), "a second cancel finds nothing");
        assert_eq!(reactor.drop_op(done), Dropped::Completed(vec![3]));
        assert_eq!(reactor.drop_op(done), Dropped::Unknown);
        assert_eq!(reactor.inflight(), 0);
    }

    #[test]
    fn waiting_lists_owner_and_kind() {
        let mut reactor = Reactor::new();
        let a = reactor.submit(Owner::System, OpKind::LoginWindow);
        let b = reactor.submit(Owner::Run(2), OpKind::Confirm);
        reactor.complete(ok_msg(a, json!({})));
        assert_eq!(reactor.waiting(), vec![(b, Owner::Run(2), OpKind::Confirm)]);
        assert_eq!(reactor.inflight(), 1);
    }

    // ------------------------------------------------------------------------------------------ Body

    #[test]
    fn body_drop_pushes_release() {
        let r = rig();
        let body = Body::new(
            Rc::clone(&r.shared),
            BodyRef {
                handle: 4,
                len: 100,
            },
        );
        assert_eq!((body.body_ref().handle, body.len()), (4, 100));
        assert!(r.outbox().is_empty());
        drop(body);
        assert_eq!(r.take_outbox(), vec![Effect::Release { handle: 4 }]);
    }

    #[test]
    fn cancelling_the_owning_task_releases_its_bodies_and_cancels_its_ops() {
        let r = rig();
        let mut ex = Executor::new(4);
        let body = Body::new(Rc::clone(&r.shared), BodyRef { handle: 9, len: 1 });
        let cx = r.cx.clone();
        ex.spawn(
            cx.owner,
            Box::pin(async move {
                let _body = body;
                let _ = cx.zotero_oa("10.1/x").await;
            }),
        )
        .expect("spawn");
        ex.run(10);
        assert_eq!(r.take_outbox().len(), 1, "only the ZoteroOa effect so far");
        let gone = ex.cancel_where(|_| true);
        assert_eq!(gone.len(), 1);
        let out = r.take_outbox();
        assert_eq!(out.len(), 2);
        assert!(out.contains(&Effect::Cancel { op: 1 }));
        assert!(out.contains(&Effect::Release { handle: 9 }));
    }

    // ------------------------------------------------------------------------------------------ timers

    #[test]
    fn sleep_fires_when_deadline_is_reached() {
        let r = rig();
        r.set_now(1_000);
        let mut s = Box::pin(r.cx.sleep(100));
        assert!(r.poll(&mut s).is_pending());
        assert_eq!(r.next_deadline(), Some(1_100));
        assert_eq!(r.fire_timers(1_099), 0);
        assert!(r.poll(&mut s).is_pending());
        assert_eq!(r.wakes(), 0);
        assert_eq!(r.fire_timers(1_100), 1, "deadline == now fires");
        assert_eq!(r.wakes(), 1);
        assert!(r.poll(&mut s).is_ready());
        assert_eq!(r.timers(), 0);
    }

    #[test]
    fn sleep_that_is_already_due_resolves_without_a_timer() {
        let r = rig();
        r.set_now(500);
        let mut zero = Box::pin(r.cx.sleep(0));
        assert!(r.poll(&mut zero).is_ready());
        let mut past = Box::pin(r.cx.sleep_until(400));
        assert!(r.poll(&mut past).is_ready());
        assert_eq!(r.timers(), 0);
        assert_eq!(r.next_deadline(), None);
    }

    #[test]
    fn dropping_a_sleep_removes_its_timer() {
        let r = rig();
        r.set_now(0);
        let mut a = Box::pin(r.cx.sleep(50));
        let mut b = Box::pin(r.cx.sleep(80));
        let _ = r.poll(&mut a);
        let _ = r.poll(&mut b);
        assert_eq!((r.timers(), r.next_deadline()), (2, Some(50)));
        drop(a);
        assert_eq!(
            (r.timers(), r.next_deadline()),
            (1, Some(80)),
            "next_deadline reflects the removal"
        );
        drop(b);
        assert_eq!((r.timers(), r.next_deadline()), (0, None));
        // Dropping a sleep that never registered is a no-op.
        drop(r.cx.sleep(10));
    }

    #[test]
    fn a_sleep_polled_by_a_new_waker_wakes_the_new_one() {
        let r = rig();
        r.set_now(0);
        let mut s = Box::pin(r.cx.sleep(10));
        assert!(r.poll(&mut s).is_pending());
        let other = Arc::new(Counter(AtomicUsize::new(0)));
        let other_waker = Waker::from(Arc::clone(&other));
        assert!(
            s.as_mut()
                .poll(&mut Context::from_waker(&other_waker))
                .is_pending()
        );
        assert_eq!(r.timers(), 1, "re-polling must not register a second timer");
        r.fire_timers(10);
        assert_eq!((r.wakes(), other.0.load(Ordering::SeqCst)), (0, 1));
    }

    #[test]
    fn due_timers_come_out_earliest_first_and_fifo_on_ties() {
        let mut reactor = Reactor::new();
        let log = Arc::new(Counter(AtomicUsize::new(0)));
        let w = Waker::from(Arc::clone(&log));
        let k_late = reactor.add_timer(300, &w);
        let k_a = reactor.add_timer(100, &w);
        let k_b = reactor.add_timer(100, &w);
        let k_mid = reactor.add_timer(200, &w);
        assert!(k_a < k_b, "equal deadlines stay in creation order");
        assert_eq!(reactor.next_deadline(), Some(100));
        assert_eq!(reactor.due_timers(99).len(), 0);
        assert_eq!(reactor.due_timers(200).len(), 3, "100, 100, 200");
        assert!(
            !reactor.timer_pending(k_a)
                && !reactor.timer_pending(k_b)
                && !reactor.timer_pending(k_mid)
        );
        assert!(reactor.timer_pending(k_late));
        assert_eq!(reactor.next_deadline(), Some(300));
        reactor.remove_timer(k_late);
        assert_eq!((reactor.next_deadline(), reactor.timer_count()), (None, 0));
    }

    // ------------------------------------------------------------------------------------------ Timeout

    #[test]
    fn timeout_returns_the_inner_value_when_it_finishes_first() {
        let r = rig();
        r.set_now(0);
        let mut t = Box::pin(r.cx.timeout(100, r.cx.raw_op(oa_req("a"))));
        assert!(r.poll(&mut t).is_pending());
        assert_eq!(r.timers(), 1);
        r.complete(ok_msg(1, json!([])));
        assert!(matches!(r.poll(&mut t), Poll::Ready(Ok(Ok(_)))));
        drop(t);
        assert_eq!(r.timers(), 0, "the timer is removed with the Timeout");
        assert!(
            r.take_outbox()
                .iter()
                .all(|e| !matches!(e, Effect::Cancel { .. }))
        );
    }

    #[test]
    fn timeout_expiry_drops_the_inner_future_and_cancels_its_op_at_once() {
        let r = rig();
        r.set_now(0);
        let mut t = Box::pin(r.cx.timeout(100, r.cx.raw_op(oa_req("a"))));
        assert!(r.poll(&mut t).is_pending());
        r.take_outbox();
        assert_eq!(r.fire_timers(100), 1);
        assert_eq!(r.poll(&mut t), Poll::Ready(Err(TimedOut)));
        // The Cancel is pushed by that same poll, while the Timeout value is still alive.
        assert_eq!(r.take_outbox(), vec![Effect::Cancel { op: 1 }]);
        assert!(
            r.poll(&mut t) == Poll::Ready(Err(TimedOut)),
            "polling again keeps answering TimedOut"
        );
        drop(t);
        assert!(r.outbox().is_empty(), "exactly one Cancel");
    }

    #[test]
    fn timeout_lets_a_ready_inner_future_win_a_tie() {
        let r = rig();
        r.set_now(0);
        // Inner already complete AND the timer due in the same step: the value wins.
        let mut t = Box::pin(r.cx.timeout(100, r.cx.raw_op(oa_req("a"))));
        assert!(r.poll(&mut t).is_pending());
        r.complete(ok_msg(1, json!([7])));
        r.fire_timers(100);
        assert!(matches!(r.poll(&mut t), Poll::Ready(Ok(Ok(_)))));
        // A zero timeout over a future that is ready on first poll also yields the value.
        let mut z = Box::pin(r.cx.timeout(0, std::future::ready(5)));
        assert_eq!(r.poll(&mut z), Poll::Ready(Ok(5)));
    }

    #[test]
    fn timeout_over_pending_future_with_zero_ms_times_out_on_first_poll() {
        let r = rig();
        r.set_now(10);
        let mut t = Box::pin(r.cx.timeout(0, std::future::pending::<()>()));
        assert_eq!(r.poll(&mut t), Poll::Ready(Err(TimedOut)));
    }

    #[test]
    fn nested_timeouts_cancel_only_the_loser() {
        let r = rig();
        r.set_now(0);
        let cx = r.cx.clone();
        let mut t = Box::pin(async move {
            let inner = cx.timeout(50, cx.raw_op(oa_req("a")));
            cx.timeout(1_000, inner).await
        });
        assert!(r.poll(&mut t).is_pending());
        r.take_outbox();
        r.fire_timers(50);
        assert!(matches!(r.poll(&mut t), Poll::Ready(Ok(Err(TimedOut)))));
        assert_eq!(r.take_outbox(), vec![Effect::Cancel { op: 1 }]);
        assert_eq!(r.timers(), 0);
    }

    // ------------------------------------------------------------------------------------------ typed wrappers

    #[test]
    fn http_wrapper_decodes_and_wraps_the_body() {
        let r = rig();
        let cx = r.cx.clone();
        let mut f = Box::pin(async move {
            cx.http(Route::Oa, false, http_req(), Retain::Pdf, InspectSpec::None)
                .await
        });
        assert!(r.poll(&mut f).is_pending());
        let out = r.take_outbox();
        assert!(matches!(
            &out[0],
            Effect::Http {
                op: 1,
                run: 3,
                item: 7,
                route: Route::Oa,
                barrier: false,
                retain: Retain::Pdf,
                ..
            }
        ));
        r.complete(ok_msg(
            1,
            json!({"status": 200, "final_url": "https://example.org/x.pdf", "bytes": 42, "ms": 12,
                   "body": {"handle": 5, "len": 42}, "inspect": {"expect": "none"}}),
        ));
        let resp = match r.poll(&mut f) {
            Poll::Ready(Ok(resp)) => resp,
            other => panic!(
                "expected a response, got {:?}",
                other.map(|r| r.map(|h| h.status))
            ),
        };
        assert_eq!((resp.status, resp.bytes, resp.ms), (200, 42, 12));
        assert_eq!(resp.inspect, Inspection::None);
        let body = resp.body.as_ref().expect("body");
        assert_eq!((body.body_ref().handle, body.len()), (5, 42));
        assert!(r.outbox().is_empty());
        drop(resp);
        assert_eq!(r.take_outbox(), vec![Effect::Release { handle: 5 }]);
    }

    #[test]
    fn browser_and_login_wrappers_wrap_both_bodies() {
        let r = rig();
        let cx = r.cx.clone();
        let breq = BrowserReq {
            url: "https://example.org".into(),
            ua: Ua::Plain,
            capture_pdf: true,
            want_page: true,
            settle_ms: 1000,
            timeout_ms: 5000,
            hold_while: Vec::new(),
        };
        let lreq = LoginReq {
            kind: LoginKind::Login,
            realm: Realm::Openathens,
            entry_url: "https://go.openathens.net/redirector/x".into(),
            item_title: "t".into(),
            waiting: 1,
            timeout_ms: 1000,
            capture_pdf: false,
        };
        let mut f = Box::pin(async move {
            let b = cx
                .browser_load(Route::Inst, true, breq, InspectSpec::None)
                .await
                .map(|b| (b.final_url.clone(), b));
            let l = cx.login_window(lreq).await;
            (b, l)
        });
        assert!(r.poll(&mut f).is_pending());
        r.complete(ok_msg(
            1,
            json!({"final_url": "https://example.org/a", "ms": 3, "pdf": {"handle": 1, "len": 10}, "page": {"handle": 2, "len": 20},
                   "inspect": {"expect": "none"}}),
        ));
        assert!(r.poll(&mut f).is_pending());
        r.complete(ok_msg(2, json!({"result": "landed", "final_url": "https://example.org/b", "inspect": {"expect": "none"}, "page": {"handle": 3, "len": 1}})));
        let (b, l) = match r.poll(&mut f) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("still pending"),
        };
        let (url, b) = b.expect("browser ok");
        assert_eq!(url, "https://example.org/a");
        assert!(b.pdf.is_some() && b.page.is_some());
        let l = l.expect("login ok");
        assert_eq!(l.result, LoginResult::Landed);
        assert!(l.pdf.is_none() && l.page.is_some());
        r.take_outbox();
        drop((b, l));
        let mut handles: Vec<u32> = r
            .take_outbox()
            .into_iter()
            .filter_map(|e| match e {
                Effect::Release { handle } => Some(handle),
                _ => None,
            })
            .collect();
        handles.sort_unstable();
        assert_eq!(handles, vec![1, 2, 3]);
    }

    #[test]
    fn small_wrappers_decode_their_payloads() {
        let r = rig();
        let cx = r.cx.clone();
        let page = Body::new(Rc::clone(&r.shared), BodyRef { handle: 8, len: 3 });
        let mut f = Box::pin(async move {
            let t = cx.run_translators(&page, "https://example.org/").await;
            let z = cx.zotero_oa("10.1/x").await;
            let s = cx.save_pdf(save_req()).await;
            let c = cx
                .confirm(ConfirmReq {
                    kind: ConfirmKind::LargeBatch,
                    n: 30,
                    cap: 20,
                    library: String::new(),
                })
                .await;
            (t, z, s, c)
        });
        let _ = r.poll(&mut f);
        r.complete(ok_msg(
            1,
            json!({"found": {"url": "https://x/y.pdf", "mime": "application/pdf"}, "ms": 9}),
        ));
        let _ = r.poll(&mut f);
        r.complete(ok_msg(
            2,
            json!([{"url": "https://x/z.pdf", "version": "published"}]),
        ));
        let _ = r.poll(&mut f);
        r.complete(ok_msg(
            3,
            json!({"attachment_key": "1/ABCD", "filename": "A.pdf"}),
        ));
        let _ = r.poll(&mut f);
        r.complete(ok_msg(4, json!({"choice": "oa_only"})));
        let Poll::Ready((t, z, s, c)) = r.poll(&mut f) else {
            panic!("still pending")
        };
        assert_eq!(
            t.expect("translators").found.map(|h| h.mime),
            Some("application/pdf".to_owned())
        );
        assert_eq!(z.expect("zotero oa").len(), 1);
        assert_eq!(s.expect("save").attachment_key, "1/ABCD");
        assert_eq!(c.expect("confirm").choice, ConfirmChoice::OaOnly);
        let out = r.take_outbox();
        assert!(out.iter().any(|e| matches!(
            e,
            Effect::RunTranslators {
                page: BodyRef { handle: 8, .. },
                ..
            }
        )));
        assert!(
            out.iter()
                .any(|e| matches!(e, Effect::Confirm { run: 3, .. }))
        );
        assert!(
            out.iter()
                .any(|e| matches!(e, Effect::SavePdf { item: 7, .. }))
        );
        // The page body was moved into the future and dropped with it.
        assert!(out.contains(&Effect::Release { handle: 8 }));
    }

    fn save_req() -> SaveReq {
        SaveReq {
            item: 7,
            body: BodyRef { handle: 1, len: 1 },
            trim_prefix: 0,
            version: Version::Published,
            url_to_store: "https://doi.org/10.1/x".into(),
            route: Route::Oa,
            source: "openalex".into(),
        }
    }

    #[test]
    fn malformed_payloads_become_internal_errors_and_release_their_bodies() {
        let r = rig();
        let cx = r.cx.clone();
        let mut f = Box::pin(async move {
            let a = cx
                .http(
                    Route::Oa,
                    false,
                    http_req(),
                    Retain::None,
                    InspectSpec::None,
                )
                .await
                .map(|h| h.status);
            let b = cx.zotero_oa("x").await.map(|v| v.len());
            let c = cx
                .confirm(ConfirmReq {
                    kind: ConfirmKind::GroupLibrary,
                    n: 1,
                    cap: 1,
                    library: "g".into(),
                })
                .await
                .map(|c| c.choice);
            let d = cx
                .browser_load(
                    Route::Oa,
                    false,
                    BrowserReq {
                        url: "u".into(),
                        ua: Ua::Default,
                        capture_pdf: false,
                        want_page: false,
                        settle_ms: 0,
                        timeout_ms: 0,
                        hold_while: vec![],
                    },
                    InspectSpec::None,
                )
                .await
                .map(|b| b.ms);
            (a, b, c, d)
        });
        let _ = r.poll(&mut f);
        // `inspect` is missing: the JSON is not an HttpOk, but JS did retain a body for it.
        r.complete(ok_msg(1, json!({"status": 200, "final_url": "u", "bytes": 1, "ms": 1, "body": {"handle": 21, "len": 1}})));
        let _ = r.poll(&mut f);
        r.complete(ok_msg(2, json!({"not": "an array"})));
        let _ = r.poll(&mut f);
        r.complete(ok_msg(3, json!("garbage")));
        let _ = r.poll(&mut f);
        r.complete(ok_msg(4, json!({"final_url": 5, "pdf": {"handle": 22, "len": 1}, "page": {"handle": 23, "len": 1}})));
        let Poll::Ready((a, b, c, d)) = r.poll(&mut f) else {
            panic!("still pending")
        };
        for e in [a.err(), b.err(), c.err(), d.err()] {
            let e = e.expect("an error");
            assert_eq!(e.kind, OpErrKind::Internal);
            assert!(
                e.message.contains("malformed completion payload"),
                "{}",
                e.message
            );
        }
        let mut released: Vec<u32> = r
            .take_outbox()
            .into_iter()
            .filter_map(|e| match e {
                Effect::Release { handle } => Some(handle),
                _ => None,
            })
            .collect();
        released.sort_unstable();
        assert_eq!(
            released,
            vec![21, 22, 23],
            "bodies of undecodable completions are released, not leaked"
        );
    }

    #[test]
    fn a_js_error_passes_through_the_typed_wrappers_unchanged() {
        let r = rig();
        let cx = r.cx.clone();
        let mut f = Box::pin(async move { cx.zotero_oa("x").await });
        let _ = r.poll(&mut f);
        r.complete(err_msg(1, OpErrKind::Offline));
        match r.poll(&mut f) {
            Poll::Ready(Err(e)) => assert_eq!((e.kind, e.status), (OpErrKind::Offline, Some(503))),
            other => panic!(
                "expected Offline, got {:?}",
                other.map(|r| r.map(|v| v.len()))
            ),
        }
    }

    #[test]
    fn body_handles_scan_ignores_junk() {
        assert_eq!(
            body_handles(&json!({"body": {"handle": 1}, "pdf": null, "page": {"handle": "x"}})),
            vec![1]
        );
        assert_eq!(
            body_handles(&json!({"body": {"handle": 4294967296u64}})),
            Vec::<u32>::new()
        );
        assert_eq!(body_handles(&json!([1, 2])), Vec::<u32>::new());
        assert_eq!(body_handles(&Value::Null), Vec::<u32>::new());
    }

    #[test]
    fn op_request_kinds_and_effects_line_up() {
        let e = oa_req("d").into_effect(5, 1, 2);
        assert_eq!(
            e,
            Effect::ZoteroOa {
                op: 5,
                run: 1,
                item: 2,
                doi: "d".into()
            }
        );
        assert_eq!(oa_req("d").kind(), OpKind::ZoteroOa);
        let c = OpRequest::Confirm {
            req: ConfirmReq {
                kind: ConfirmKind::LargeBatch,
                n: 1,
                cap: 1,
                library: String::new(),
            },
        };
        assert_eq!(c.kind(), OpKind::Confirm);
        // Confirm is run-level: it carries no item field.
        assert!(matches!(
            c.into_effect(6, 4, 0),
            Effect::Confirm { op: 6, run: 4, .. }
        ));
    }
}
