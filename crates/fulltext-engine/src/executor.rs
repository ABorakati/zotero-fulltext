//! Single-threaded executor: a generation-checked task table, real `Waker`s, a poll budget.
//!
//! No `unsafe`, no thread-locals, no globals. Valid on `wasm32-unknown-unknown` without atomics: `Arc`,
//! `AtomicBool` and `Mutex` lower to plain loads/stores there (`Mutex` never blocks because nothing holds it
//! across a call to user code).
//!
//! # Contract
//! * One task per item (plus a few helper tasks). Concurrency *inside* an item is sub-futures
//!   (`FuturesUnordered`, `select_biased!`, `Timeout`), never tasks.
//! * `TaskKey { index, epoch }`: a wake carrying an old epoch (slot reused) is ignored.
//! * `TaskWaker::wake` = set `queued` (dedupe) + push the key onto the `ReadyQueue`. It touches nothing else, so it
//!   is safe to call anywhere, including inside `Shared::with`.
//! * `run(budget)` polls ready tasks FIFO until the queue is empty (`more = false`) or `budget` polls were done
//!   (`more = true`). A wake that arrives *during* a poll re-queues the task (clear `queued` before polling).
//! * Dropping a task's future runs the `Drop` of its leaf futures, which emit `Effect::Cancel` / `Effect::Release`
//!   into the outbox. `cancel_where` therefore must be called outside `Shared::with` (see cx.rs rule 4).
//! * Lost-wake detection: `ReadyQueue::sweep` is set by `wake_all()`. `Engine::step` calls `wake_all()` once when
//!   it detects a possible stall (tasks alive, nothing in flight, no timer, nobody waiting for the user); if that
//!   sweep makes any task progress it counts one `Stats.lost_wakes`. Tests assert it stays 0.
use crate::cx::{LocalFuture, Owner};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Wake, Waker};

/// Identifies a live task; stale keys (retired slot) never match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TaskKey {
    pub index: u32,
    /// Reuse counter for the slot (named `epoch` because `gen` is a keyword in edition 2024).
    pub epoch: u32,
}

/// `spawn` failure: the table is full (`max_tasks`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExecFull;

/// Result of one `run` call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RunStats {
    pub polls: u32,
    /// Budget exhausted with tasks still ready: the host should call `step` again soon.
    pub more: bool,
}

/// FIFO of tasks to poll. Shared (`Arc`) between the executor and every `TaskWaker`.
#[derive(Default)]
pub(crate) struct ReadyQueue {
    queue: Mutex<VecDeque<TaskKey>>,
    /// Set by `Executor::wake_all`: requeue every live task at the start of the next `run`.
    sweep: AtomicBool,
}

impl ReadyQueue {
    pub(crate) fn push(&self, key: TaskKey) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(key);
    }

    pub(crate) fn pop(&self) -> Option<TaskKey> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    pub(crate) fn len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub(crate) fn request_sweep(&self) {
        self.sweep.store(true, Ordering::Release);
    }

    pub(crate) fn take_sweep(&self) -> bool {
        self.sweep.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn sweep_pending(&self) -> bool {
        self.sweep.load(Ordering::Acquire)
    }
}

/// The waker of one task. `Arc<TaskWaker>` converts to `std::task::Waker` via `Waker::from`.
pub(crate) struct TaskWaker {
    pub(crate) key: TaskKey,
    pub(crate) queued: AtomicBool,
    pub(crate) ready: Arc<ReadyQueue>,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // Dedupe: only the first wake since the last poll pushes.
        if !self.queued.swap(true, Ordering::AcqRel) {
            self.ready.push(self.key);
        }
    }
}

struct Task {
    epoch: u32,
    owner: Owner,
    waker: Arc<TaskWaker>,
    /// `None` only while the task is being polled.
    fut: Option<LocalFuture>,
}

/// The task table. See the module docs.
pub(crate) struct Executor {
    ready: Arc<ReadyQueue>,
    slots: Vec<Option<Task>>,
    /// Reuse counter per slot; bumped when a task is retired so keys of the old occupant never match again.
    epochs: Vec<u32>,
    free: Vec<u32>,
    max_tasks: u32,
}

impl Executor {
    /// `max_tasks` bounds the table (production: 256).
    pub(crate) fn new(max_tasks: u32) -> Self {
        Self {
            ready: Arc::new(ReadyQueue::default()),
            slots: Vec::new(),
            epochs: Vec::new(),
            free: Vec::new(),
            max_tasks,
        }
    }

    /// Add a task and schedule its first poll. `Err(ExecFull)` when `max_tasks` tasks are alive; the future is
    /// dropped (it has never been polled, so that emits nothing).
    pub(crate) fn spawn(&mut self, owner: Owner, fut: LocalFuture) -> Result<TaskKey, ExecFull> {
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                if self.slots.len() >= self.max_tasks as usize {
                    return Err(ExecFull);
                }
                let index = u32::try_from(self.slots.len()).map_err(|_| ExecFull)?;
                self.slots.push(None);
                self.epochs.push(0);
                index
            }
        };
        let epoch = self.epochs.get(index as usize).copied().unwrap_or_default();
        let key = TaskKey { index, epoch };
        let waker = Arc::new(TaskWaker {
            key,
            queued: AtomicBool::new(false),
            ready: Arc::clone(&self.ready),
        });
        let Some(slot) = self.slots.get_mut(index as usize) else {
            // Unreachable by construction (slots and epochs only ever grow together); keep the index reusable.
            self.free.push(index);
            return Err(ExecFull);
        };
        *slot = Some(Task {
            epoch,
            owner,
            waker: Arc::clone(&waker),
            fut: Some(fut),
        });
        waker.wake_by_ref();
        Ok(key)
    }

    /// Drop every task whose owner matches. Returns the owners of the dropped tasks, in slot order (so `run.rs`
    /// can record `Cancelled` outcomes). Dropping a future runs the `Drop` of its leaf futures, which push
    /// `Cancel` / `Release` effects, so this must be called outside `Shared::with`.
    pub(crate) fn cancel_where(&mut self, pred: impl Fn(&Owner) -> bool) -> Vec<Owner> {
        let doomed: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.as_ref()
                    .filter(|task| pred(&task.owner))
                    .map(|_| index)
            })
            .collect();
        doomed
            .into_iter()
            .filter_map(|index| self.retire(index))
            .map(|task| task.owner)
            .collect()
    }

    /// Poll ready tasks until the queue is empty or `budget` polls were made. Finished tasks are retired
    /// (slot epoch bumped, slot freed).
    pub(crate) fn run(&mut self, budget: u32) -> RunStats {
        let mut polls = 0u32;
        while polls < budget {
            if self.ready.take_sweep() {
                self.sweep();
            }
            let Some(key) = self.ready.pop() else { break };
            let index = key.index as usize;
            let Some(task) = self.slots.get_mut(index).and_then(Option::as_mut) else {
                continue;
            };
            if task.epoch != key.epoch {
                continue; // a wake from a task that has since been retired
            }
            // Clear BEFORE polling: a wake that arrives during the poll must queue the task again.
            task.waker.queued.store(false, Ordering::Release);
            let Some(mut fut) = task.fut.take() else {
                continue;
            };
            let waker = Waker::from(Arc::clone(&task.waker));
            let mut cx = Context::from_waker(&waker);
            polls += 1;
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(()) => {
                    // Drop the finished future (its locals' `Drop`s may push effects) and free the slot.
                    drop(fut);
                    drop(self.retire(index));
                }
                Poll::Pending => {
                    if let Some(task) = self.slots.get_mut(index).and_then(Option::as_mut) {
                        task.fut = Some(fut);
                    }
                }
            }
        }
        RunStats {
            polls,
            more: self.ready.len() > 0 || self.ready.sweep_pending(),
        }
    }

    /// Schedule every live task for one poll (stall fallback). Takes effect at the start of the next `run`.
    pub(crate) fn wake_all(&mut self) {
        self.ready.request_sweep();
    }

    /// Number of live tasks.
    pub(crate) fn live(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Tasks currently queued to be polled (stale entries of retired tasks count until `run` pops them).
    pub(crate) fn ready_len(&self) -> usize {
        self.ready.len()
    }

    /// A `wake_all` sweep that the next `run` has not applied yet.
    pub(crate) fn sweep_pending(&self) -> bool {
        self.ready.sweep_pending()
    }

    /// Owners of the live tasks (for `DumpState` and invariants).
    pub(crate) fn owners(&self) -> Vec<Owner> {
        self.slots.iter().flatten().map(|t| t.owner).collect()
    }

    pub(crate) fn max_tasks(&self) -> u32 {
        self.max_tasks
    }

    /// Wake every live task through its own waker (so the `queued` flag keeps dedupe intact).
    fn sweep(&self) {
        for task in self.slots.iter().flatten() {
            task.waker.wake_by_ref();
        }
    }

    /// Empty the slot, bump its epoch and put it on the free list. The caller drops the returned task (and so its
    /// future) outside any `Shared::with`.
    fn retire(&mut self, index: usize) -> Option<Task> {
        let task = self.slots.get_mut(index)?.take()?;
        if let Some(epoch) = self.epochs.get_mut(index) {
            *epoch = epoch.wrapping_add(1);
        }
        if let Ok(index) = u32::try_from(index) {
            self.free.push(index);
        }
        Some(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use futures_util::stream::FuturesUnordered;
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::rc::Rc;

    fn item(n: u32) -> Owner {
        Owner::Item { run: 1, item: n }
    }

    /// Shared handle to a [`Park`] future: lets a test finish it, wake it, or count its polls from outside.
    #[derive(Clone, Default)]
    struct Switch {
        done: Rc<Cell<bool>>,
        polls: Rc<Cell<u32>>,
        waker: Rc<RefCell<Option<Waker>>>,
    }

    impl Switch {
        fn park(&self) -> Park {
            Park(self.clone())
        }

        fn wake(&self) {
            let waker = self.waker.borrow().clone();
            if let Some(w) = waker {
                w.wake();
            }
        }

        fn finish(&self) {
            self.done.set(true);
            self.wake();
        }

        fn polls(&self) -> u32 {
            self.polls.get()
        }
    }

    /// Pending (with its waker stored) until the switch says done.
    struct Park(Switch);

    impl Future for Park {
        type Output = ();
        fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let s = &self.0;
            s.polls.set(s.polls.get() + 1);
            if s.done.get() {
                return Poll::Ready(());
            }
            *s.waker.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn spawn_ok(
        ex: &mut Executor,
        owner: Owner,
        fut: impl Future<Output = ()> + 'static,
    ) -> TaskKey {
        ex.spawn(owner, Box::pin(fut)).expect("spawn")
    }

    #[test]
    fn polls_in_fifo_order() {
        let order = Rc::new(RefCell::new(Vec::new()));
        let mut ex = Executor::new(8);
        for n in 0..3 {
            let o = Rc::clone(&order);
            spawn_ok(&mut ex, item(n), async move { o.borrow_mut().push(n) });
        }
        assert_eq!(ex.ready_len(), 3);
        let st = ex.run(100);
        assert_eq!(*order.borrow(), vec![0, 1, 2]);
        assert_eq!(
            st,
            RunStats {
                polls: 3,
                more: false
            }
        );
        assert_eq!(ex.live(), 0);
    }

    #[test]
    fn wake_storm_dedupes_to_one_poll() {
        let mut ex = Executor::new(8);
        let sw = Switch::default();
        spawn_ok(&mut ex, item(1), sw.park());
        assert_eq!(ex.run(100).polls, 1);
        for _ in 0..100 {
            sw.wake();
        }
        assert_eq!(ex.ready_len(), 1);
        assert_eq!(ex.run(100).polls, 1);
        assert_eq!(sw.polls(), 2);
        assert_eq!(ex.ready_len(), 0);
    }

    #[test]
    fn stale_wake_after_slot_reuse_is_ignored() {
        let mut ex = Executor::new(8);
        let stash: Rc<RefCell<Option<Waker>>> = Rc::default();
        let s = Rc::clone(&stash);
        let a = spawn_ok(
            &mut ex,
            item(1),
            std::future::poll_fn(move |cx| {
                *s.borrow_mut() = Some(cx.waker().clone());
                Poll::Ready(())
            }),
        );
        assert_eq!(ex.run(10).polls, 1);
        assert_eq!(ex.live(), 0);

        let sw = Switch::default();
        let b = spawn_ok(&mut ex, item(2), sw.park());
        assert_eq!((b.index, b.epoch), (a.index, a.epoch + 1));

        // The retired task's waker fires: its key carries the old epoch and must not poll task B.
        let stale = stash.borrow().clone().expect("waker stashed");
        stale.wake();
        assert_eq!(ex.ready_len(), 2);
        let st = ex.run(10);
        assert_eq!(
            st,
            RunStats {
                polls: 1,
                more: false
            }
        );
        assert_eq!(sw.polls(), 1);
        assert_eq!(ex.live(), 1);
    }

    #[test]
    fn budget_exhaustion_sets_more() {
        let mut ex = Executor::new(8);
        for n in 0..5 {
            spawn_ok(&mut ex, item(n), async {});
        }
        assert_eq!(
            ex.run(3),
            RunStats {
                polls: 3,
                more: true
            }
        );
        assert_eq!(
            ex.run(10),
            RunStats {
                polls: 2,
                more: false
            }
        );
        assert_eq!(ex.run(0), RunStats::default());
    }

    #[test]
    fn zero_budget_polls_nothing() {
        let mut ex = Executor::new(8);
        let sw = Switch::default();
        spawn_ok(&mut ex, item(1), sw.park());
        assert_eq!(
            ex.run(0),
            RunStats {
                polls: 0,
                more: true
            }
        );
        assert_eq!(sw.polls(), 0);
    }

    #[test]
    fn self_wake_during_poll_requeues_exactly_once() {
        let mut ex = Executor::new(8);
        let polls = Rc::new(Cell::new(0u32));
        let p = Rc::clone(&polls);
        spawn_ok(
            &mut ex,
            item(1),
            std::future::poll_fn(move |cx| {
                p.set(p.get() + 1);
                if p.get() == 1 {
                    // Three wakes inside one poll still re-queue the task only once.
                    cx.waker().wake_by_ref();
                    cx.waker().wake_by_ref();
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            }),
        );
        let st = ex.run(100);
        assert_eq!(
            st,
            RunStats {
                polls: 2,
                more: false
            }
        );
        assert_eq!(polls.get(), 2);
        assert_eq!(ex.live(), 0);
    }

    struct DropFlag(Rc<Cell<bool>>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    fn held_forever(flag: &Rc<Cell<bool>>) -> impl Future<Output = ()> + 'static {
        let flag = DropFlag(Rc::clone(flag));
        async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        }
    }

    #[test]
    fn cancel_where_drops_futures_and_returns_owners() {
        let mut ex = Executor::new(8);
        let flags: Vec<Rc<Cell<bool>>> = (0..3).map(|_| Rc::default()).collect();
        let owners = [item(1), item(2), Owner::System];
        for (flag, owner) in flags.iter().zip(owners) {
            spawn_ok(&mut ex, owner, held_forever(flag));
        }
        ex.run(10);
        assert_eq!(ex.live(), 3);
        assert!(flags.iter().all(|f| !f.get()));

        let gone = ex.cancel_where(|o| matches!(o, Owner::Item { .. }));
        assert_eq!(gone, vec![item(1), item(2)]);
        assert_eq!(
            flags.iter().map(|f| f.get()).collect::<Vec<_>>(),
            vec![true, true, false]
        );
        assert_eq!(ex.owners(), vec![Owner::System]);

        // The sweep only reaches the live task.
        ex.wake_all();
        assert_eq!(ex.run(10).polls, 1);
        assert!(ex.cancel_where(|_| false).is_empty());
    }

    #[test]
    fn cancelled_slot_is_reused_with_a_new_epoch() {
        let mut ex = Executor::new(1);
        let flag = Rc::new(Cell::new(false));
        let a = spawn_ok(&mut ex, item(1), held_forever(&flag));
        ex.run(10);
        ex.cancel_where(|_| true);
        let b = ex
            .spawn(item(2), Box::pin(async {}))
            .expect("slot freed by cancel");
        assert_eq!((b.index, b.epoch), (a.index, a.epoch + 1));
    }

    #[test]
    fn spawn_past_max_tasks_is_exec_full() {
        let mut ex = Executor::new(2);
        let sw = Switch::default();
        spawn_ok(&mut ex, item(1), sw.park());
        spawn_ok(&mut ex, item(2), Switch::default().park());
        let dropped = Rc::new(Cell::new(false));
        let full = ex.spawn(item(3), Box::pin(held_forever(&dropped)));
        assert_eq!(full, Err(ExecFull));
        assert!(dropped.get(), "the rejected future is dropped");
        assert_eq!(ex.live(), 2);
        assert_eq!(ex.max_tasks(), 2);

        // Finishing one task makes room again.
        ex.run(10);
        sw.finish();
        ex.run(10);
        assert_eq!(ex.live(), 1);
        assert!(ex.spawn(item(4), Box::pin(async {})).is_ok());
    }

    #[test]
    fn finished_task_frees_its_slot_for_the_next_spawn() {
        let mut ex = Executor::new(4);
        let first = spawn_ok(&mut ex, item(1), async {});
        ex.run(10);
        let second = spawn_ok(&mut ex, item(2), async {});
        assert_eq!(second.index, first.index);
        assert_eq!(second.epoch, first.epoch + 1);
        // A task spawned while another is alive takes a fresh slot at epoch 0.
        let sw = Switch::default();
        let third = spawn_ok(&mut ex, item(3), sw.park());
        assert_ne!(third.index, second.index);
        assert_eq!(third.epoch, 0);
    }

    #[test]
    fn futures_unordered_children_wake_the_parent() {
        let mut ex = Executor::new(4);
        let children: Vec<Switch> = (0..3).map(|_| Switch::default()).collect();
        let order = Rc::new(RefCell::new(Vec::new()));
        let finished = Rc::new(Cell::new(false));
        let (cs, o, f) = (children.clone(), Rc::clone(&order), Rc::clone(&finished));
        spawn_ok(&mut ex, item(1), async move {
            let mut set = FuturesUnordered::new();
            for (n, c) in cs.iter().enumerate() {
                let park = c.park();
                set.push(async move {
                    park.await;
                    n
                });
            }
            while let Some(n) = set.next().await {
                o.borrow_mut().push(n);
            }
            f.set(true);
        });
        ex.run(10);
        assert!(order.borrow().is_empty());
        for n in [2usize, 0, 1] {
            children[n].finish();
            let st = ex.run(10);
            assert_eq!(
                st.polls, 1,
                "each child completion wakes the parent exactly once"
            );
        }
        assert_eq!(*order.borrow(), vec![2, 0, 1]);
        assert!(finished.get());
        assert_eq!(ex.live(), 0);
    }

    #[test]
    fn sweep_wakes_a_task_that_lost_its_wake() {
        let mut ex = Executor::new(4);
        let flag = Rc::new(Cell::new(false));
        let polls = Rc::new(Cell::new(0u32));
        let (fl, p) = (Rc::clone(&flag), Rc::clone(&polls));
        // Never registers a waker: only a sweep can revive it.
        spawn_ok(
            &mut ex,
            item(1),
            std::future::poll_fn(move |_cx| {
                p.set(p.get() + 1);
                if fl.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }),
        );
        ex.run(10);
        flag.set(true);
        assert_eq!(ex.run(10).polls, 0, "nothing wakes it");
        ex.wake_all();
        assert!(ex.sweep_pending());
        assert_eq!(ex.run(10).polls, 1);
        assert!(!ex.sweep_pending());
        assert_eq!(polls.get(), 2);
        assert_eq!(ex.live(), 0);
    }

    #[test]
    fn sweep_does_not_double_queue_already_ready_tasks() {
        let mut ex = Executor::new(4);
        let sw = Switch::default();
        spawn_ok(&mut ex, item(1), sw.park());
        ex.run(10);
        sw.wake();
        ex.wake_all();
        assert_eq!(ex.run(10).polls, 1);
        assert_eq!(sw.polls(), 2);
    }
}
