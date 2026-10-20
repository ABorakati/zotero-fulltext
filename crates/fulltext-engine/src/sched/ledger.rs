//! Reserve-then-commit licence ledger for institutional PDFs (hourly count, rolling-24 h count, MB per hour).
//!
//! # Contract
//! * `reserve(wall, est_bytes)` counts the reservation against ALL caps immediately. It fails closed with a
//!   [`CapHit`] naming the binding cap and the wall time at which it frees (`reset_at`). It is called BEFORE the
//!   fetch effect is emitted, and the caller pushes `Effect::Persist { key: Ledger, flush: true }` (from
//!   `state()`) BEFORE the `Http { barrier: true }` effect, so a crash or quit mid-fetch can never under-count.
//! * `commit(id, wall, bytes)` turns the reservation into a committed `(wall_ms, bytes)` entry (actual size). The
//!   entry keeps the reservation's time (capped at `wall`): the request started then, and the invariant "no
//!   window ever holds more than its cap" then survives any commit delay.
//! * `rollback(id)` drops the reservation (non-PDF response, cancelled before the request was sent).
//! * Caps are `min(pref, ceiling)` (`policy::CEIL_*`): prefs may only lower them. `Ledger::new` clamps.
//! * `load` after a restart: committed entries older than 24 h are pruned; reservations older than
//!   `RESERVATION_STALE_MS` are converted to committed entries (the fetch may have happened).
//! * Hour window = entries with `wall_ms > wall - 1 h`; day window = `> wall - 24 h`; MB/hour sums bytes of the
//!   hour window (committed actual + reserved estimates). 1 MB = 10^6 bytes.
//!
//! # Details worth knowing
//! * A cap of 0 means "none allowed": an immediate hit with `reset_at = wall + 24 h` (it never frees by itself).
//! * When several caps bind at once the hit reports the one that frees LAST (the item cannot proceed before
//!   then); ties go to hourly, then daily, then MB/hour.
//! * `reset_at` is exact: the wall time at which enough of the oldest window entries have aged out for the
//!   request to fit (`entry.wall_ms + window`, because an entry is inside a window only while
//!   `wall_ms + window > wall`).
//! * The byte estimate of one reservation is clamped to the MB/hour cap, so an estimate (4 MiB by default)
//!   larger than a tiny user cap cannot lock the route out for ever; the actual size is what `commit` records,
//!   and a commit that is larger than reserved can push the hour over the cap (the next reserve then fails).
//! * Entries are never dropped while inside the 24 h window; `reserve` prunes older ones.
use crate::proto::Effect;
use crate::sched::policy::{self, DAY_MS, HOUR_MS};
use fulltext_core::model::*;

/// Effective caps after clamping to the licence ceilings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Caps {
    pub hourly: u32,
    pub daily: u32,
    pub mb_per_hour: u32,
}

impl Caps {
    /// `min(pref, ceiling)` per cap; a pref of 0 means "none allowed".
    pub(crate) fn clamp(hourly: u32, daily: u32, mb_per_hour: u32) -> Caps {
        Caps {
            hourly: hourly.min(policy::CEIL_HOURLY),
            daily: daily.min(policy::CEIL_DAILY),
            mb_per_hour: mb_per_hour.min(policy::CEIL_MB_PER_HOUR),
        }
    }
}

/// The binding cap and when it frees up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CapHit {
    pub kind: CapKind,
    pub reset_at: u64,
}

const MB: u64 = 1_000_000;

/// `true` while an entry stamped `at` is still inside a window of `window` ms ending at `wall`.
fn in_window(at: u64, wall: u64, window: u64) -> bool {
    at.saturating_add(window) > wall
}

#[derive(Clone, Debug)]
pub(crate) struct Ledger {
    state: LedgerState,
    caps: Caps,
    next_id: u64,
}

impl Ledger {
    /// Caps are clamped again here, so the invariant holds whoever built the `Caps` value.
    pub(crate) fn new(state: LedgerState, caps: Caps) -> Self {
        let next_id = state
            .reserved
            .iter()
            .map(|r| r.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Self {
            state,
            caps: Caps::clamp(caps.hourly, caps.daily, caps.mb_per_hour),
            next_id,
        }
    }

    /// Re-apply caps (a new `Configure`) without touching entries.
    pub(crate) fn set_caps(&mut self, caps: Caps) {
        self.caps = Caps::clamp(caps.hourly, caps.daily, caps.mb_per_hour);
    }

    /// Prune old entries and convert stale reservations (see module docs).
    pub(crate) fn load(&mut self, wall: u64) {
        let mut stale = Vec::new();
        self.state.reserved.retain(|r| {
            let is_stale = r.wall_ms.saturating_add(policy::RESERVATION_STALE_MS) <= wall;
            if is_stale {
                stale.push((r.wall_ms, r.bytes));
            }
            !is_stale
        });
        self.state.committed.extend(stale);
        self.state.committed.sort_by_key(|(at, _)| *at);
        self.prune(wall);
        let max_live = self.state.reserved.iter().map(|r| r.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(max_live.saturating_add(1));
    }

    /// Drop everything older than the day window (reservations included: they can no longer count).
    fn prune(&mut self, wall: u64) {
        self.state
            .committed
            .retain(|(at, _)| in_window(*at, wall, DAY_MS));
        self.state
            .reserved
            .retain(|r| in_window(r.wall_ms, wall, DAY_MS));
    }

    /// Committed and reserved entries as `(wall_ms, bytes)`.
    fn entries(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.state
            .committed
            .iter()
            .copied()
            .chain(self.state.reserved.iter().map(|r| (r.wall_ms, r.bytes)))
    }

    /// Count a new reservation against every cap, or fail closed with the cap that binds longest.
    pub(crate) fn reserve(&mut self, wall: u64, est_bytes: u64) -> Result<Reservation, CapHit> {
        self.prune(wall);
        let caps = self.caps;
        let zero_hit = |kind| CapHit {
            kind,
            reset_at: wall.saturating_add(DAY_MS),
        };
        if caps.hourly == 0 {
            return Err(zero_hit(CapKind::Hourly));
        }
        if caps.daily == 0 {
            return Err(zero_hit(CapKind::Daily));
        }
        if caps.mb_per_hour == 0 {
            return Err(zero_hit(CapKind::MbPerHour));
        }

        let mut hour: Vec<(u64, u64)> = self
            .entries()
            .filter(|(at, _)| in_window(*at, wall, HOUR_MS))
            .collect();
        hour.sort_by_key(|(at, _)| *at);
        let mut day: Vec<u64> = self
            .entries()
            .map(|(at, _)| at)
            .filter(|at| in_window(*at, wall, DAY_MS))
            .collect();
        day.sort_unstable();

        let mut hits: Vec<CapHit> = Vec::new();
        let hourly = usize::try_from(caps.hourly).unwrap_or(usize::MAX);
        if hour.len() >= hourly {
            let shed = hour.len() - hourly + 1;
            if let Some((at, _)) = hour.get(shed - 1) {
                hits.push(CapHit {
                    kind: CapKind::Hourly,
                    reset_at: at.saturating_add(HOUR_MS),
                });
            }
        }
        let daily = usize::try_from(caps.daily).unwrap_or(usize::MAX);
        if day.len() >= daily {
            let shed = day.len() - daily + 1;
            if let Some(at) = day.get(shed - 1) {
                hits.push(CapHit {
                    kind: CapKind::Daily,
                    reset_at: at.saturating_add(DAY_MS),
                });
            }
        }
        let cap_bytes = u64::from(caps.mb_per_hour) * MB;
        let est = est_bytes.min(cap_bytes);
        let used = hour.iter().fold(0u64, |acc, (_, b)| acc.saturating_add(*b));
        if used.saturating_add(est) > cap_bytes {
            let excess = used.saturating_add(est) - cap_bytes;
            let mut shed = 0u64;
            let mut reset_at = wall.saturating_add(HOUR_MS);
            for (at, bytes) in &hour {
                shed = shed.saturating_add(*bytes);
                reset_at = at.saturating_add(HOUR_MS);
                if shed >= excess {
                    break;
                }
            }
            hits.push(CapHit {
                kind: CapKind::MbPerHour,
                reset_at,
            });
        }
        // The cap that frees last binds the request; `max_by_key` keeps the LAST maximum, so reverse first to
        // prefer hourly > daily > MB/hour on a tie.
        if let Some(hit) = hits.iter().rev().max_by_key(|h| h.reset_at) {
            return Err(*hit);
        }

        let r = Reservation {
            id: self.next_id(),
            wall_ms: wall,
            bytes: est,
        };
        self.state.reserved.push(r.clone());
        Ok(r)
    }

    /// Reservation -> committed entry with the actual byte count. The entry keeps the RESERVATION's time (the
    /// request started then, which is what a publisher counts, and a late commit can never re-enter a window the
    /// reservation had already left); it is never stamped later than `wall`. `false` when `id` is unknown.
    pub(crate) fn commit(&mut self, id: u64, wall: u64, bytes: u64) -> bool {
        let Some(pos) = self.state.reserved.iter().position(|r| r.id == id) else {
            return false;
        };
        let reserved = self.state.reserved.remove(pos);
        let stamp = reserved.wall_ms.min(wall);
        let at = self.state.committed.partition_point(|(w, _)| *w <= stamp);
        self.state.committed.insert(at, (stamp, bytes));
        true
    }

    /// Drop a reservation. `false` when `id` is unknown.
    pub(crate) fn rollback(&mut self, id: u64) -> bool {
        let before = self.state.reserved.len();
        self.state.reserved.retain(|r| r.id != id);
        self.state.reserved.len() != before
    }

    /// Usage for the run report. Counts committed + reserved.
    pub(crate) fn usage(&self, wall: u64) -> LedgerUsage {
        let (mut hour, mut day, mut hour_bytes) = (0u32, 0u32, 0u64);
        for (at, bytes) in self.entries() {
            if in_window(at, wall, DAY_MS) {
                day = day.saturating_add(1);
            }
            if in_window(at, wall, HOUR_MS) {
                hour = hour.saturating_add(1);
                hour_bytes = hour_bytes.saturating_add(bytes);
            }
        }
        LedgerUsage {
            hour,
            hour_cap: self.caps.hourly,
            day,
            day_cap: self.caps.daily,
            mb_hour: u32::try_from(hour_bytes.div_ceil(MB)).unwrap_or(u32::MAX),
            mb_hour_cap: self.caps.mb_per_hour,
        }
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &LedgerState {
        &self.state
    }

    #[cfg(test)]
    pub(crate) fn caps(&self) -> Caps {
        self.caps
    }

    pub(crate) fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = id.saturating_add(1);
        id
    }

    /// The `Persist` effect for the current state.
    pub(crate) fn persist_effect(&self, flush: bool) -> Effect {
        Effect::Persist {
            key: crate::proto::PersistKey::Ledger,
            value: serde_json::to_value(&self.state).unwrap_or(serde_json::Value::Null),
            flush,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::testkit::Lcg;

    const T0: u64 = 1_792_496_400_000;
    const MIN: u64 = 60_000;

    fn ledger(h: u32, d: u32, mb: u32) -> Ledger {
        Ledger::new(
            LedgerState::default(),
            Caps {
                hourly: h,
                daily: d,
                mb_per_hour: mb,
            },
        )
    }

    fn default_ledger() -> Ledger {
        Ledger::new(LedgerState::default(), Caps::clamp(30, 80, 300))
    }

    #[test]
    fn prefs_can_only_lower_the_ceilings() {
        assert_eq!(
            Caps::clamp(30, 80, 300),
            Caps {
                hourly: 30,
                daily: 80,
                mb_per_hour: 300
            }
        );
        assert_eq!(
            Caps::clamp(1_000, 5_000, 99_999),
            Caps {
                hourly: 30,
                daily: 80,
                mb_per_hour: 300
            }
        );
        assert_eq!(
            Caps::clamp(10, 20, 50),
            Caps {
                hourly: 10,
                daily: 20,
                mb_per_hour: 50
            }
        );
        assert_eq!(
            Caps::clamp(0, 0, 0),
            Caps {
                hourly: 0,
                daily: 0,
                mb_per_hour: 0
            }
        );
        // `Ledger::new` and `set_caps` clamp as well, whoever built the value
        let mut l = ledger(500, 500, 500);
        assert_eq!(
            l.caps(),
            Caps {
                hourly: 30,
                daily: 80,
                mb_per_hour: 300
            }
        );
        l.set_caps(Caps {
            hourly: 9_999,
            daily: 7,
            mb_per_hour: 1,
        });
        assert_eq!(
            l.caps(),
            Caps {
                hourly: 30,
                daily: 7,
                mb_per_hour: 1
            }
        );
    }

    #[test]
    fn hourly_cap_fails_closed_and_frees_when_the_oldest_ages_out() {
        let mut l = default_ledger();
        for i in 0..30 {
            l.reserve(T0 + i * 1_000, 1_000).expect("under the cap");
        }
        let hit = l.reserve(T0 + 40_000, 1_000).expect_err("31st must fail");
        assert_eq!(hit.kind, CapKind::Hourly);
        // the oldest entry (T0) leaves the hour window at T0 + 1 h
        assert_eq!(hit.reset_at, T0 + HOUR_MS);
        assert!(
            l.reserve(T0 + HOUR_MS - 1, 1_000).is_err(),
            "still inside the window"
        );
        assert!(
            l.reserve(T0 + HOUR_MS, 1_000).is_ok(),
            "the first entry has aged out"
        );
        // only ONE slot opened: the next call fails again, now until the second-oldest ages out
        let again = l.reserve(T0 + HOUR_MS, 1_000).expect_err("one slot only");
        assert_eq!(again.reset_at, T0 + 1_000 + HOUR_MS);
    }

    #[test]
    fn daily_cap_is_rolling_and_can_be_the_binding_one() {
        // an hourly cap of 30 never binds here: 80 PDFs spread over 23 hours, then a burst in hour 24
        let mut l = default_ledger();
        for i in 0..80u64 {
            let at = T0 + i * 17 * MIN; // 17 min apart => never more than 4 per hour
            l.reserve(at, 1_000).expect("within all caps");
        }
        let last = T0 + 79 * 17 * MIN;
        let hit = l.reserve(last + 1_000, 1_000).expect_err("81st in 24 h");
        assert_eq!(hit.kind, CapKind::Daily);
        assert_eq!(hit.reset_at, T0 + DAY_MS);
        assert!(l.reserve(T0 + DAY_MS, 1_000).is_ok());
    }

    #[test]
    fn mb_per_hour_counts_estimates_and_frees_by_bytes() {
        let mut l = default_ledger();
        // 300 MB cap: three 100 MB reservations fill it exactly
        for i in 0..3 {
            l.reserve(T0 + i * MIN, 100 * MB).expect("fits");
        }
        assert_eq!(l.usage(T0 + 3 * MIN).mb_hour, 300);
        let hit = l.reserve(T0 + 3 * MIN, 1).expect_err("cap is full");
        assert_eq!(hit.kind, CapKind::MbPerHour);
        assert_eq!(
            hit.reset_at,
            T0 + HOUR_MS,
            "shedding the first 100 MB entry frees one byte"
        );
        // a 150 MB request must shed two entries
        let hit = l
            .reserve(T0 + 3 * MIN, 150 * MB)
            .expect_err("needs two entries gone");
        assert_eq!(hit.reset_at, T0 + MIN + HOUR_MS);
        assert!(l.reserve(T0 + HOUR_MS, 100 * MB).is_ok());
    }

    #[test]
    fn rollback_frees_the_reservation() {
        let mut l = ledger(2, 80, 300);
        let a = l.reserve(T0, 1_000).expect("a");
        let _b = l.reserve(T0, 1_000).expect("b");
        assert!(l.reserve(T0, 1_000).is_err());
        assert!(l.rollback(a.id));
        assert!(!l.rollback(a.id), "second rollback finds nothing");
        assert!(l.reserve(T0, 1_000).is_ok());
        assert_eq!(l.usage(T0).hour, 2);
    }

    #[test]
    fn commit_records_the_actual_size_and_a_big_pdf_blocks_the_next_reserve() {
        let mut l = ledger(30, 80, 10);
        let a = l.reserve(T0, 1_000_000).expect("1 MB estimate");
        assert_eq!(l.usage(T0).mb_hour, 1);
        assert!(
            l.commit(a.id, T0 + 5_000, 11 * MB),
            "the PDF was 11 MB, not 1 MB"
        );
        assert!(!l.commit(a.id, T0, 1), "already committed");
        let u = l.usage(T0 + 5_000);
        assert_eq!(
            (u.hour, u.mb_hour, u.mb_hour_cap),
            (1, 11, 10),
            "committed bytes may exceed the cap"
        );
        let hit = l.reserve(T0 + 6_000, 1).expect_err("over the MB cap now");
        assert_eq!(hit.kind, CapKind::MbPerHour);
        assert_eq!(
            hit.reset_at,
            T0 + HOUR_MS,
            "frees when the big entry (stamped when its request started) ages out"
        );
        assert_eq!(l.state().committed, vec![(T0, 11 * MB)]);
        assert!(l.state().reserved.is_empty());
    }

    #[test]
    fn a_commit_keeps_the_time_the_request_started() {
        let mut l = ledger(30, 80, 300);
        let a = l.reserve(T0, 1_000).expect("a");
        // a normal commit 90 s later: the entry still ages out one hour after the REQUEST
        assert!(l.commit(a.id, T0 + 90_000, 5_000));
        assert_eq!(l.state().committed, vec![(T0, 5_000)]);
        assert_eq!(l.usage(T0 + HOUR_MS - 1).hour, 1);
        assert_eq!(l.usage(T0 + HOUR_MS).hour, 0);
        // a commit that comes 2 hours late (cannot happen in practice) must not re-enter the hour window
        let b = l.reserve(T0 + HOUR_MS, 1_000).expect("b");
        assert!(l.commit(b.id, T0 + 3 * HOUR_MS, 5_000));
        assert_eq!(l.usage(T0 + 3 * HOUR_MS).hour, 0);
        // an entry is never stamped in the future (clock stepped back between reserve and commit)
        let c = l.reserve(T0 + 5 * HOUR_MS, 1_000).expect("c");
        assert!(l.commit(c.id, T0 + 4 * HOUR_MS, 9));
        assert_eq!(l.state().committed.last(), Some(&(T0 + 4 * HOUR_MS, 9)));
    }

    #[test]
    fn usage_rounds_megabytes_up() {
        let mut l = default_ledger();
        l.reserve(T0, 1).expect("tiny");
        assert_eq!(l.usage(T0).mb_hour, 1);
        let mut l = default_ledger();
        l.reserve(T0, 2 * MB).expect("2 MB");
        assert_eq!(l.usage(T0).mb_hour, 2);
        l.reserve(T0, 1).expect("+1 byte");
        assert_eq!(l.usage(T0).mb_hour, 3);
        assert_eq!(default_ledger().usage(T0).mb_hour, 0);
    }

    #[test]
    fn a_zero_cap_is_an_immediate_hit() {
        for (caps, kind) in [
            ((0, 80, 300), CapKind::Hourly),
            ((30, 0, 300), CapKind::Daily),
            ((30, 80, 0), CapKind::MbPerHour),
        ] {
            let mut l = ledger(caps.0, caps.1, caps.2);
            let hit = l.reserve(T0, 1).expect_err("zero cap");
            assert_eq!(
                hit,
                CapHit {
                    kind,
                    reset_at: T0 + DAY_MS
                }
            );
            assert!(l.state().reserved.is_empty());
        }
    }

    #[test]
    fn the_binding_cap_is_the_one_that_frees_last() {
        // hourly 2, MB 3: two 1.5 MB entries; hourly frees when #1 ages out, MB needs #1 and #2 gone for a 3 MB ask
        let mut l = ledger(2, 80, 3);
        l.reserve(T0, 1_500_000).expect("a");
        l.reserve(T0 + MIN, 1_500_000).expect("b");
        let hit = l.reserve(T0 + 2 * MIN, 3 * MB).expect_err("both caps bind");
        assert_eq!(hit.kind, CapKind::MbPerHour);
        assert_eq!(hit.reset_at, T0 + MIN + HOUR_MS);
        // a tiny ask frees at the same instant under both caps; the count cap is reported first
        let hit = l.reserve(T0 + 2 * MIN, 10).expect_err("count cap");
        assert_eq!(
            hit,
            CapHit {
                kind: CapKind::Hourly,
                reset_at: T0 + HOUR_MS
            }
        );
    }

    #[test]
    fn an_oversized_estimate_is_clamped_to_the_cap() {
        let mut l = ledger(30, 80, 4);
        let r = l
            .reserve(T0, 50 * MB)
            .expect("one reservation always fits an empty ledger");
        assert_eq!(r.bytes, 4 * MB);
        assert!(
            l.reserve(T0, 1).is_err(),
            "the clamped reservation fills the hour"
        );
        assert!(l.rollback(r.id));
        assert!(l.reserve(T0, 1).is_ok());
    }

    #[test]
    fn state_round_trips_through_serde() {
        let mut l = default_ledger();
        let a = l.reserve(T0, 2 * MB).expect("a");
        let b = l.reserve(T0 + 1_000, 3 * MB).expect("b");
        l.commit(a.id, T0 + 2_000, 2_500_000);
        let json = serde_json::to_string(l.state()).expect("serialise");
        let back: LedgerState = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(&back, l.state());
        let mut l2 = Ledger::new(back, Caps::clamp(30, 80, 300));
        assert_eq!(l2.usage(T0 + 3_000), l.usage(T0 + 3_000));
        // ids continue after the highest live reservation: no collision with `b`
        let c = l2.reserve(T0 + 3_000, 1_000).expect("c");
        assert!(c.id > b.id);
        // the persist effect carries exactly this state
        match l.persist_effect(true) {
            Effect::Persist {
                key: crate::proto::PersistKey::Ledger,
                value,
                flush: true,
            } => {
                assert_eq!(
                    serde_json::from_value::<LedgerState>(value).expect("persisted shape"),
                    *l.state()
                );
            }
            other => panic!("unexpected effect {other:?}"),
        }
    }

    #[test]
    fn load_prunes_old_entries_and_commits_stale_reservations() {
        let state = LedgerState {
            committed: vec![
                (T0 - DAY_MS - 5, 10),
                (T0 - DAY_MS + 5, 20),
                (T0 - 3 * MIN, 30),
            ],
            reserved: vec![
                Reservation {
                    id: 3,
                    wall_ms: T0 - 11 * MIN,
                    bytes: 111,
                },
                Reservation {
                    id: 4,
                    wall_ms: T0 - MIN,
                    bytes: 222,
                },
                Reservation {
                    id: 5,
                    wall_ms: T0 - 2 * DAY_MS,
                    bytes: 333,
                },
            ],
        };
        let mut l = Ledger::new(state, Caps::clamp(30, 80, 300));
        l.load(T0);
        // the 24 h-old entry and the 2-day-old reservation are gone; the 11-min-old reservation became committed
        assert_eq!(
            l.state().committed,
            vec![
                (T0 - DAY_MS + 5, 20),
                (T0 - 11 * MIN, 111),
                (T0 - 3 * MIN, 30)
            ]
        );
        assert_eq!(
            l.state().reserved,
            vec![Reservation {
                id: 4,
                wall_ms: T0 - MIN,
                bytes: 222
            }]
        );
        // everything that survived still counts: the stale reservation did not vanish (never under-count)
        let u = l.usage(T0);
        assert_eq!((u.hour, u.day), (3, 4));
        // new ids never collide with the surviving reservation
        let r = l.reserve(T0, 1).expect("fits");
        assert!(r.id > 4);
    }

    #[test]
    fn a_restart_with_a_persisted_reservation_keeps_counting_it() {
        // engine trapped mid-fetch: the reservation was persisted before the barrier fetch
        let mut before = ledger(2, 80, 300);
        before.reserve(T0, 1_000).expect("a");
        before.reserve(T0, 1_000).expect("b");
        let persisted: LedgerState =
            serde_json::from_value(serde_json::to_value(before.state()).expect("to_value"))
                .expect("from_value");
        let mut after = Ledger::new(persisted, Caps::clamp(2, 80, 300));
        after.load(T0 + 30_000);
        assert!(
            after.reserve(T0 + 30_000, 1_000).is_err(),
            "the crashed fetches still count"
        );
        // ... and after the 10 min stale limit they are committed entries, still counted for their hour
        let mut later = Ledger::new(after.state().clone(), Caps::clamp(2, 80, 300));
        later.load(T0 + 20 * MIN);
        assert!(later.state().reserved.is_empty());
        assert_eq!(later.usage(T0 + 20 * MIN).hour, 2);
        assert!(later.reserve(T0 + 20 * MIN, 1_000).is_err());
        assert!(later.reserve(T0 + HOUR_MS, 1_000).is_ok());
    }

    #[test]
    fn random_interleavings_never_exceed_the_caps() {
        for seed in 1..=40u64 {
            let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9));
            let caps = Caps::clamp(
                5 + (rng.below(30)) as u32,
                10 + (rng.below(80)) as u32,
                20 + (rng.below(300)) as u32,
            );
            let mut l = Ledger::new(LedgerState::default(), caps);
            let mut wall = T0;
            // Tickets may stay open for hours here (far longer than the engine ever keeps one): the invariant
            // must hold regardless, because a commit keeps the time its request started.
            let mut live: Vec<(u64, u64)> = Vec::new(); // (id, est)
            for _ in 0..400 {
                match rng.below(10) {
                    0..=4 => {
                        let est = rng.below(25) * MB;
                        match l.reserve(wall, est) {
                            Ok(r) => live.push((r.id, r.bytes)),
                            Err(hit) => assert!(
                                hit.reset_at > wall,
                                "a hit must name a future time (seed {seed})"
                            ),
                        }
                    }
                    5..=6 if !live.is_empty() => {
                        let (id, est) = live.remove(rng.below(live.len() as u64) as usize);
                        // never larger than reserved here: the cap is a hard bound in this test
                        assert!(l.commit(
                            id,
                            wall,
                            est.saturating_sub(rng.below(est.max(1) / 2 + 1))
                        ));
                    }
                    7 if !live.is_empty() => {
                        let (id, _) = live.remove(rng.below(live.len() as u64) as usize);
                        assert!(l.rollback(id));
                    }
                    _ => wall += rng.below(25 * MIN),
                }
                let u = l.usage(wall);
                assert!(
                    u.hour <= u.hour_cap,
                    "hour {} > {} (seed {seed})",
                    u.hour,
                    u.hour_cap
                );
                assert!(
                    u.day <= u.day_cap,
                    "day {} > {} (seed {seed})",
                    u.day,
                    u.day_cap
                );
                assert!(
                    u.mb_hour <= u.mb_hour_cap,
                    "mb {} > {} (seed {seed})",
                    u.mb_hour,
                    u.mb_hour_cap
                );
            }
        }
    }

    #[test]
    fn a_hit_names_the_instant_the_request_would_succeed() {
        // follow the advice of every hit: retrying at reset_at must succeed (or hit a LATER reset)
        let mut rng = Lcg(99);
        let mut l = ledger(6, 12, 40);
        let mut wall = T0;
        for _ in 0..200 {
            let est = rng.below(15) * MB;
            let mut tries = 0;
            loop {
                match l.reserve(wall, est) {
                    Ok(r) => {
                        l.commit(r.id, wall, est);
                        break;
                    }
                    Err(hit) => {
                        assert!(hit.reset_at > wall);
                        wall = hit.reset_at;
                        tries += 1;
                        assert!(tries < 50, "reset_at must make progress");
                    }
                }
            }
            wall += rng.below(5 * MIN);
        }
    }
}
