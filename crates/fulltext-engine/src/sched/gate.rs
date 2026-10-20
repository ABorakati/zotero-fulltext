//! `Limiter`: one generic gate = weighted concurrency cap + minimum gap after release (+ jitter) + optional
//! token bucket + a FIFO wait list with priorities. It is a pure state machine with explicit time; the async
//! layer (`acquire.rs`) only polls it. Used for: item slots, OA API limits, OA host gates, institutional host
//! gates, the institutional global cap, browser slots, the save slot, and the retained-bytes budget.
//!
//! # Contract
//! * `enqueue(prio)` joins the wait list and returns a `Ticket`. Order: `Prio::Resumed` before `Prio::New`,
//!   then arrival order. A ticket is granted only when it is the best-ranked waiter that can be served now
//!   (strict FIFO per limiter: a later ticket never overtakes an earlier one on the same limiter).
//! * `poll(ticket, now, weight, waker)` returns `Granted` (the ticket is consumed, `weight` is added to
//!   `in_use`, one token is taken) or `Wait { retry_at }`: `retry_at = Some(t)` when the only obstacle is
//!   time (gap or token), so the future should arm a timer for `t`; `None` when it must wait for a release.
//!   A `Wait` stores `waker` on the ticket so `release` / `forget` / a grant ahead of it can wake it.
//! * `check` + `take` are the same decision split in two: `check` is a dry run (`&self`), `take` commits it.
//!   `AcquireInst` dry-runs the family gate and the global gate and takes both only when both would grant.
//! * `release(weight, now, gap_ms)` frees capacity and sets `next_at = now + max(gap_ms, spec.min_gap_ms)` (the
//!   caller adds jitter via `Rng` before calling) so the next grant respects the spacing. It returns the waker of
//!   the head waiter (the only one that may be served next); the caller wakes it outside the borrow if it can,
//!   or inside (waking is a queue push).
//! * `forget(ticket)` removes a waiter whose future was dropped (cancellation); it also passes the baton: if the
//!   forgotten ticket was the head, the next waiter's waker is returned.
//! * A grant wakes the NEW head (capacity may remain for it); without this two releases that race ahead of the
//!   head's poll would strand the second waiter.
//! * Capacity 0 never grants. `weight > capacity` is granted when `in_use == 0` (a big item must not starve).
//!
//! The token bucket is kept in integer "credit milliseconds" (one token = `refill_ms` of credit, at most
//! `burst * refill_ms`), so there is no float rounding and `retry_at` is exact.
use std::task::Waker;

/// Waiter priority. `Resumed` (an item coming back from the login gate) is served before `New`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Prio {
    Resumed,
    New,
}

/// Opaque wait-list handle.
pub(crate) type Ticket = u64;

/// Deterministic xorshift64* generator seeded from `Configure.seed`. Used for jitter only.
#[derive(Clone, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..=max`.
    pub(crate) fn jitter(&mut self, max: u32) -> u32 {
        if max == 0 {
            return 0;
        }
        let span = u64::from(max) + 1;
        u32::try_from(self.next_u64() % span).unwrap_or(0)
    }
}

/// Token bucket: `burst` tokens, one refilled every `refill_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rate {
    pub burst: u32,
    pub refill_ms: u32,
}

/// Static description of a limiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Spec {
    /// Total weight that may be held at once (concurrent requests, MiB, ...).
    pub capacity: u64,
    /// Minimum ms between a release and the next grant (0 = none).
    pub min_gap_ms: u32,
    pub rate: Option<Rate>,
}

/// Answer of `Limiter::poll`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GatePoll {
    Granted,
    /// Not now. `retry_at` is a virtual-time instant when only time is missing, `None` = wait for a release.
    Wait {
        retry_at: Option<u64>,
    },
}

#[derive(Debug)]
struct Waiter {
    prio: Prio,
    ticket: Ticket,
    waker: Option<Waker>,
}

/// See the module docs.
#[derive(Debug)]
pub(crate) struct Limiter {
    pub(crate) spec: Spec,
    pub(crate) in_use: u64,
    pub(crate) next_at: u64,
    /// Token-bucket credit in ms as of `refilled_at` (one token = `refill_ms`).
    credit_ms: u64,
    refilled_at: u64,
    next_ticket: Ticket,
    /// Kept sorted by (priority, ticket); the first entry is the only one that may be granted.
    waiters: Vec<Waiter>,
}

impl Limiter {
    pub(crate) fn new(spec: Spec) -> Self {
        let mut l = Self {
            spec,
            in_use: 0,
            next_at: 0,
            credit_ms: 0,
            refilled_at: 0,
            next_ticket: 1,
            waiters: Vec::new(),
        };
        l.credit_ms = l.bucket().map_or(0, |(_, cap)| cap);
        l
    }

    /// `(refill_ms, capacity_ms)` of the bucket; `None` when the limiter has no (usable) rate.
    fn bucket(&self) -> Option<(u64, u64)> {
        let rate = self.spec.rate?;
        if rate.refill_ms == 0 {
            return None;
        }
        let refill = u64::from(rate.refill_ms);
        Some((refill, u64::from(rate.burst.max(1)).saturating_mul(refill)))
    }

    /// Credit available at `now` (refill is linear, capped at the burst).
    fn credit_at(&self, now: u64) -> u64 {
        match self.bucket() {
            None => 0,
            Some((_, cap)) => cap.min(
                self.credit_ms
                    .saturating_add(now.saturating_sub(self.refilled_at)),
            ),
        }
    }

    fn head_waker(&self) -> Option<Waker> {
        self.waiters.first().and_then(|w| w.waker.clone())
    }

    /// Join the wait list.
    pub(crate) fn enqueue(&mut self, prio: Prio) -> Ticket {
        let t = self.next_ticket;
        self.next_ticket = t.saturating_add(1);
        let at = self.waiters.partition_point(|w| w.prio <= prio);
        self.waiters.insert(
            at,
            Waiter {
                prio,
                ticket: t,
                waker: None,
            },
        );
        t
    }

    /// Dry run of the grant decision for `ticket` (see the module docs for the order of the checks).
    pub(crate) fn check(&self, ticket: Ticket, now: u64, weight: u64) -> GatePoll {
        if self.waiters.first().map(|w| w.ticket) != Some(ticket) {
            return GatePoll::Wait { retry_at: None };
        }
        if self.spec.capacity == 0 {
            return GatePoll::Wait { retry_at: None };
        }
        let fits = self.in_use == 0 || self.in_use.saturating_add(weight) <= self.spec.capacity;
        if !fits {
            return GatePoll::Wait { retry_at: None };
        }
        if now < self.next_at {
            return GatePoll::Wait {
                retry_at: Some(self.next_at),
            };
        }
        if let Some((refill, _)) = self.bucket() {
            let credit = self.credit_at(now);
            if credit < refill {
                return GatePoll::Wait {
                    retry_at: Some(now.saturating_add(refill - credit)),
                };
            }
        }
        GatePoll::Granted
    }

    /// Commit a grant that `check` approved: consume the ticket, add the weight, take one token. Returns the
    /// waker of the new head (the caller wakes it: capacity or tokens may remain for it).
    pub(crate) fn take(&mut self, ticket: Ticket, now: u64, weight: u64) -> Vec<Waker> {
        let Some(pos) = self.waiters.iter().position(|w| w.ticket == ticket) else {
            return Vec::new();
        };
        self.waiters.remove(pos);
        self.in_use = self.in_use.saturating_add(weight);
        if let Some((refill, _)) = self.bucket() {
            self.credit_ms = self.credit_at(now).saturating_sub(refill);
            self.refilled_at = self.refilled_at.max(now);
        }
        self.head_waker().into_iter().collect()
    }

    /// Store / refresh the waker of a waiting ticket.
    pub(crate) fn park(&mut self, ticket: Ticket, waker: &Waker) {
        if let Some(w) = self.waiters.iter_mut().find(|w| w.ticket == ticket) {
            match &w.waker {
                Some(old) if old.will_wake(waker) => {}
                _ => w.waker = Some(waker.clone()),
            }
        }
    }

    /// Try to be served. Stores `waker` for the wake-up on release / forget. A grant wakes the new head itself
    /// (waking only pushes onto the executor's ready queue, so it is allowed inside `Shared::with`).
    pub(crate) fn poll(
        &mut self,
        ticket: Ticket,
        now: u64,
        weight: u64,
        waker: &Waker,
    ) -> GatePoll {
        match self.check(ticket, now, weight) {
            GatePoll::Granted => {
                for w in self.take(ticket, now, weight) {
                    w.wake();
                }
                GatePoll::Granted
            }
            wait => {
                self.park(ticket, waker);
                wait
            }
        }
    }

    /// Free `weight`, stamp the gap. Returns the wakers to call (the head waiter's).
    pub(crate) fn release(&mut self, weight: u64, now: u64, gap_ms: u32) -> Vec<Waker> {
        self.in_use = self.in_use.saturating_sub(weight);
        let gap = u64::from(gap_ms.max(self.spec.min_gap_ms));
        self.next_at = self.next_at.max(now.saturating_add(gap));
        self.head_waker().into_iter().collect()
    }

    /// Remove a waiter (dropped future). Returns wakers of waiters that may now be servable.
    pub(crate) fn forget(&mut self, ticket: Ticket) -> Vec<Waker> {
        let was_head = self.waiters.first().is_some_and(|w| w.ticket == ticket);
        self.waiters.retain(|w| w.ticket != ticket);
        if was_head {
            self.head_waker().into_iter().collect()
        } else {
            Vec::new()
        }
    }

    /// Replace the static spec (a new `Configure` changed a capacity). Waiters and holders are kept; the
    /// new capacity applies from the next grant. Returns the head's waker (more room may have appeared).
    pub(crate) fn set_spec(&mut self, spec: Spec) -> Vec<Waker> {
        let old_cap = self.bucket().map(|(_, cap)| cap);
        self.spec = spec;
        match self.bucket() {
            None => self.credit_ms = 0,
            Some((_, cap)) => {
                self.credit_ms = if old_cap.is_none() {
                    cap
                } else {
                    self.credit_ms.min(cap)
                }
            }
        }
        self.head_waker().into_iter().collect()
    }

    /// Every stored waker (to re-evaluate all waiters after a refusal condition appeared, e.g. a halt).
    pub(crate) fn all_wakers(&self) -> Vec<Waker> {
        self.waiters
            .iter()
            .filter_map(|w| w.waker.clone())
            .collect()
    }

    /// Waiting tickets (for invariants / `DumpState`).
    pub(crate) fn waiting(&self) -> usize {
        self.waiters.len()
    }

    /// True when `ticket` is still on the wait list.
    #[cfg(test)]
    pub(crate) fn is_waiting(&self, ticket: Ticket) -> bool {
        self.waiters.iter().any(|w| w.ticket == ticket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::testkit::flag_waker;

    fn spec(capacity: u64, min_gap_ms: u32) -> Spec {
        Spec {
            capacity,
            min_gap_ms,
            rate: None,
        }
    }

    #[test]
    fn rng_is_deterministic_and_bounded() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        let xs: Vec<u32> = (0..50).map(|_| a.jitter(3_000)).collect();
        let ys: Vec<u32> = (0..50).map(|_| b.jitter(3_000)).collect();
        assert_eq!(xs, ys);
        assert!(xs.iter().all(|x| *x <= 3_000));
        assert!(xs.iter().any(|x| *x != xs[0]), "jitter should vary");
        assert_eq!(Rng::new(0).jitter(0), 0);
        // a zero seed is replaced, not stuck at zero
        assert_ne!(Rng::new(0).next_u64(), 0);
    }

    #[test]
    fn grants_in_fifo_order_one_at_a_time() {
        let mut l = Limiter::new(spec(1, 0));
        let (_, w) = flag_waker();
        let a = l.enqueue(Prio::New);
        let b = l.enqueue(Prio::New);
        let c = l.enqueue(Prio::New);
        // not the head: refused even though the limiter has room only for the head
        assert_eq!(l.poll(c, 0, 1, &w), GatePoll::Wait { retry_at: None });
        assert_eq!(l.poll(b, 0, 1, &w), GatePoll::Wait { retry_at: None });
        assert_eq!(l.poll(a, 0, 1, &w), GatePoll::Granted);
        assert_eq!(l.in_use, 1);
        // b is head now but the single slot is taken
        assert_eq!(l.poll(b, 0, 1, &w), GatePoll::Wait { retry_at: None });
        l.release(1, 0, 0);
        assert_eq!(
            l.poll(c, 0, 1, &w),
            GatePoll::Wait { retry_at: None },
            "c must not overtake b"
        );
        assert_eq!(l.poll(b, 0, 1, &w), GatePoll::Granted);
        l.release(1, 0, 0);
        assert_eq!(l.poll(c, 0, 1, &w), GatePoll::Granted);
        assert_eq!(l.waiting(), 0);
    }

    #[test]
    fn resumed_is_served_before_new_but_fifo_inside_a_class() {
        let mut l = Limiter::new(spec(1, 0));
        let (_, w) = flag_waker();
        let n1 = l.enqueue(Prio::New);
        let n2 = l.enqueue(Prio::New);
        let r1 = l.enqueue(Prio::Resumed);
        let r2 = l.enqueue(Prio::Resumed);
        assert_eq!(
            l.poll(n1, 0, 1, &w),
            GatePoll::Wait { retry_at: None },
            "resumed waiters are ahead of n1"
        );
        assert_eq!(l.poll(r1, 0, 1, &w), GatePoll::Granted);
        l.release(1, 0, 0);
        assert_eq!(l.poll(r2, 0, 1, &w), GatePoll::Granted);
        l.release(1, 0, 0);
        assert_eq!(l.poll(n2, 0, 1, &w), GatePoll::Wait { retry_at: None });
        assert_eq!(l.poll(n1, 0, 1, &w), GatePoll::Granted);
        l.release(1, 0, 0);
        assert_eq!(l.poll(n2, 0, 1, &w), GatePoll::Granted);
    }

    #[test]
    fn gap_after_release_is_a_timer_obstacle() {
        let mut l = Limiter::new(spec(1, 0));
        let (_, w) = flag_waker();
        let a = l.enqueue(Prio::New);
        assert_eq!(l.poll(a, 100, 1, &w), GatePoll::Granted);
        l.release(1, 1_000, 2_000);
        assert_eq!(l.next_at, 3_000);
        let b = l.enqueue(Prio::New);
        assert_eq!(
            l.poll(b, 1_000, 1, &w),
            GatePoll::Wait {
                retry_at: Some(3_000)
            }
        );
        assert_eq!(
            l.poll(b, 2_999, 1, &w),
            GatePoll::Wait {
                retry_at: Some(3_000)
            }
        );
        assert_eq!(l.poll(b, 3_000, 1, &w), GatePoll::Granted);
        // a smaller later gap never shortens the earlier one; the spec minimum is a floor
        l.release(1, 3_000, 0);
        assert_eq!(l.next_at, 3_000);
        let mut m = Limiter::new(spec(1, 750));
        let c = m.enqueue(Prio::New);
        assert_eq!(m.poll(c, 0, 1, &w), GatePoll::Granted);
        m.release(1, 10, 0);
        assert_eq!(m.next_at, 760);
        m.release(1, 20, 5_000);
        assert_eq!(m.next_at, 5_020);
    }

    #[test]
    fn capacity_wait_has_no_retry_instant_even_with_a_gap() {
        let mut l = Limiter::new(spec(1, 0));
        let (_, w) = flag_waker();
        let a = l.enqueue(Prio::New);
        assert_eq!(l.poll(a, 0, 1, &w), GatePoll::Granted);
        l.release(1, 10, 5_000);
        let b = l.enqueue(Prio::New);
        let c = l.enqueue(Prio::New);
        assert_eq!(
            l.poll(b, 10, 1, &w),
            GatePoll::Wait {
                retry_at: Some(5_010)
            }
        );
        assert_eq!(l.poll(b, 5_010, 1, &w), GatePoll::Granted);
        // c waits for capacity, so there is no instant to arm a timer for
        assert_eq!(l.poll(c, 5_010, 1, &w), GatePoll::Wait { retry_at: None });
    }

    #[test]
    fn token_bucket_allows_a_burst_then_spaces_requests() {
        let rate = Rate {
            burst: 3,
            refill_ms: 100,
        };
        let mut l = Limiter::new(Spec {
            capacity: 10,
            min_gap_ms: 0,
            rate: Some(rate),
        });
        let (_, w) = flag_waker();
        for _ in 0..3 {
            let t = l.enqueue(Prio::New);
            assert_eq!(l.poll(t, 1_000, 1, &w), GatePoll::Granted);
            l.release(1, 1_000, 0);
        }
        let t = l.enqueue(Prio::New);
        // the burst is spent: the next token exists 100 ms later
        assert_eq!(
            l.poll(t, 1_000, 1, &w),
            GatePoll::Wait {
                retry_at: Some(1_100)
            }
        );
        assert_eq!(
            l.poll(t, 1_099, 1, &w),
            GatePoll::Wait {
                retry_at: Some(1_100)
            }
        );
        assert_eq!(l.poll(t, 1_100, 1, &w), GatePoll::Granted);
        l.release(1, 1_100, 0);
        // idle for 10 s refills only up to the burst
        let burst: Vec<_> = (0..4)
            .map(|_| {
                let t = l.enqueue(Prio::New);
                let r = l.poll(t, 11_100, 1, &w);
                if r == GatePoll::Granted {
                    l.release(1, 11_100, 0);
                } else {
                    l.forget(t);
                }
                r
            })
            .collect();
        assert_eq!(burst.iter().filter(|r| **r == GatePoll::Granted).count(), 3);
    }

    #[test]
    fn token_bucket_is_exact_with_fractional_waits() {
        let mut l = Limiter::new(Spec {
            capacity: 1,
            min_gap_ms: 0,
            rate: Some(Rate {
                burst: 1,
                refill_ms: 1_000,
            }),
        });
        let (_, w) = flag_waker();
        let a = l.enqueue(Prio::New);
        assert_eq!(l.poll(a, 0, 1, &w), GatePoll::Granted);
        l.release(1, 0, 0);
        let b = l.enqueue(Prio::New);
        assert_eq!(
            l.poll(b, 400, 1, &w),
            GatePoll::Wait {
                retry_at: Some(1_000)
            }
        );
        assert_eq!(
            l.poll(b, 999, 1, &w),
            GatePoll::Wait {
                retry_at: Some(1_000)
            }
        );
        assert_eq!(l.poll(b, 1_000, 1, &w), GatePoll::Granted);
    }

    #[test]
    fn weighted_capacity_and_oversize_when_idle() {
        let mut l = Limiter::new(spec(100, 0));
        let (_, w) = flag_waker();
        let a = l.enqueue(Prio::New);
        let b = l.enqueue(Prio::New);
        let c = l.enqueue(Prio::New);
        assert_eq!(l.poll(a, 0, 60, &w), GatePoll::Granted);
        assert_eq!(
            l.poll(b, 0, 50, &w),
            GatePoll::Wait { retry_at: None },
            "60 + 50 > 100"
        );
        assert_eq!(
            l.poll(c, 0, 10, &w),
            GatePoll::Wait { retry_at: None },
            "strict FIFO: c waits behind b"
        );
        l.release(60, 0, 0);
        assert_eq!(l.poll(b, 0, 50, &w), GatePoll::Granted);
        assert_eq!(
            l.poll(c, 0, 50, &w),
            GatePoll::Granted,
            "50 + 50 fits exactly"
        );
        l.release(50, 0, 0);
        l.release(50, 0, 0);
        // a weight above the capacity is granted when nothing is held
        let big = l.enqueue(Prio::New);
        assert_eq!(l.poll(big, 0, 500, &w), GatePoll::Granted);
        assert_eq!(l.in_use, 500);
        let small = l.enqueue(Prio::New);
        assert_eq!(
            l.poll(small, 0, 1, &w),
            GatePoll::Wait { retry_at: None },
            "oversize holder blocks everyone"
        );
        l.release(500, 0, 0);
        assert_eq!(l.poll(small, 0, 1, &w), GatePoll::Granted);
    }

    #[test]
    fn capacity_zero_never_grants() {
        let mut l = Limiter::new(spec(0, 0));
        let (_, w) = flag_waker();
        let a = l.enqueue(Prio::New);
        assert_eq!(l.poll(a, 0, 1, &w), GatePoll::Wait { retry_at: None });
        assert_eq!(
            l.poll(a, 1_000_000, 0, &w),
            GatePoll::Wait { retry_at: None }
        );
        // raising the capacity later lets the head through and wakes it
        let (flag, w2) = flag_waker();
        assert_eq!(l.poll(a, 0, 1, &w2), GatePoll::Wait { retry_at: None });
        let woken = l.set_spec(spec(1, 0));
        assert_eq!(woken.len(), 1);
        woken.into_iter().for_each(Waker::wake);
        assert_eq!(flag.count(), 1);
        assert_eq!(l.poll(a, 0, 1, &w2), GatePoll::Granted);
    }

    #[test]
    fn release_wakes_exactly_the_head() {
        let mut l = Limiter::new(spec(1, 0));
        let (fa, wa) = flag_waker();
        let (fb, wb) = flag_waker();
        let (fc, wc) = flag_waker();
        let a = l.enqueue(Prio::New);
        let b = l.enqueue(Prio::New);
        let c = l.enqueue(Prio::New);
        assert_eq!(l.poll(a, 0, 1, &wa), GatePoll::Granted);
        l.poll(b, 0, 1, &wb);
        l.poll(c, 0, 1, &wc);
        let woken = l.release(1, 0, 0);
        assert_eq!(woken.len(), 1);
        woken.into_iter().for_each(Waker::wake);
        assert_eq!((fa.count(), fb.count(), fc.count()), (0, 1, 0));
    }

    #[test]
    fn a_grant_wakes_the_next_head_so_spare_capacity_is_not_stranded() {
        let mut l = Limiter::new(spec(2, 0));
        let (_, wa) = flag_waker();
        let (_, wb) = flag_waker();
        let (fc, wc) = flag_waker();
        let (fd, wd) = flag_waker();
        let (a, b, c, d) = (
            l.enqueue(Prio::New),
            l.enqueue(Prio::New),
            l.enqueue(Prio::New),
            l.enqueue(Prio::New),
        );
        assert_eq!(l.poll(a, 0, 1, &wa), GatePoll::Granted);
        assert_eq!(l.poll(b, 0, 1, &wb), GatePoll::Granted);
        l.poll(c, 0, 1, &wc);
        l.poll(d, 0, 1, &wd);
        // two holders release before c polls: only the head (c) was woken by the releases ...
        let mut woken = l.release(1, 0, 0);
        woken.extend(l.release(1, 0, 0));
        woken.into_iter().for_each(Waker::wake);
        assert_eq!((fc.count(), fd.count()), (2, 0));
        // ... so c's grant must wake d, which still fits
        assert_eq!(l.poll(c, 0, 1, &wc), GatePoll::Granted);
        assert_eq!(fd.count(), 1);
        assert_eq!(l.poll(d, 0, 1, &wd), GatePoll::Granted);
    }

    #[test]
    fn forgetting_the_head_passes_the_baton() {
        let mut l = Limiter::new(spec(1, 0));
        let (fa, wa) = flag_waker();
        let (fb, wb) = flag_waker();
        let (fc, wc) = flag_waker();
        let (_, wh) = flag_waker();
        let holder = l.enqueue(Prio::New);
        assert_eq!(l.poll(holder, 0, 1, &wh), GatePoll::Granted);
        let a = l.enqueue(Prio::New);
        let b = l.enqueue(Prio::New);
        let c = l.enqueue(Prio::New);
        l.poll(a, 0, 1, &wa);
        l.poll(b, 0, 1, &wb);
        l.poll(c, 0, 1, &wc);
        // forgetting a non-head wakes nobody
        assert!(l.forget(c).is_empty());
        // forgetting the head returns the next waiter's waker (b)
        let woken = l.forget(a);
        assert_eq!(woken.len(), 1);
        woken.into_iter().for_each(Waker::wake);
        assert_eq!((fa.count(), fb.count(), fc.count()), (0, 1, 0));
        assert_eq!(l.waiting(), 1);
        assert!(!l.is_waiting(a) && l.is_waiting(b));
        assert_eq!(
            l.poll(b, 0, 1, &wb),
            GatePoll::Wait { retry_at: None },
            "the holder still has the slot"
        );
        l.release(1, 0, 0);
        assert_eq!(l.poll(b, 0, 1, &wb), GatePoll::Granted);
    }

    #[test]
    fn check_is_a_dry_run() {
        let mut l = Limiter::new(Spec {
            capacity: 1,
            min_gap_ms: 0,
            rate: Some(Rate {
                burst: 1,
                refill_ms: 500,
            }),
        });
        let a = l.enqueue(Prio::New);
        let b = l.enqueue(Prio::New);
        assert_eq!(l.check(a, 0, 1), GatePoll::Granted);
        assert_eq!(
            l.check(a, 0, 1),
            GatePoll::Granted,
            "check does not consume anything"
        );
        assert_eq!(
            l.check(b, 0, 1),
            GatePoll::Wait { retry_at: None },
            "b is not the head"
        );
        assert_eq!(l.in_use, 0);
        assert!(l.take(a, 0, 1).is_empty());
        assert_eq!(l.in_use, 1);
        l.release(1, 0, 0);
        assert_eq!(
            l.check(b, 0, 1),
            GatePoll::Wait {
                retry_at: Some(500)
            }
        );
        // an unknown ticket is never granted
        assert_eq!(l.check(999, 0, 1), GatePoll::Wait { retry_at: None });
        assert!(l.take(999, 0, 1).is_empty());
        assert_eq!(l.in_use, 0);
    }

    #[test]
    fn set_spec_keeps_waiters_and_clamps_credit() {
        let mut l = Limiter::new(Spec {
            capacity: 2,
            min_gap_ms: 0,
            rate: Some(Rate {
                burst: 5,
                refill_ms: 100,
            }),
        });
        let (_, w) = flag_waker();
        let a = l.enqueue(Prio::New);
        assert_eq!(l.poll(a, 0, 1, &w), GatePoll::Granted);
        l.set_spec(Spec {
            capacity: 3,
            min_gap_ms: 0,
            rate: Some(Rate {
                burst: 1,
                refill_ms: 100,
            }),
        });
        assert_eq!(l.spec.capacity, 3);
        assert_eq!(l.in_use, 1);
        let b = l.enqueue(Prio::New);
        assert_eq!(
            l.poll(b, 0, 1, &w),
            GatePoll::Granted,
            "one token of credit is left after the clamp"
        );
        let c = l.enqueue(Prio::New);
        assert_eq!(
            l.poll(c, 0, 1, &w),
            GatePoll::Wait {
                retry_at: Some(100)
            }
        );
    }
}
