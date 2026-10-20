//! Scheduler: every limit lives here. `Sched` is plain data inside `State`; the async layer (`acquire.rs`)
//! turns its state machines into futures and RAII permits.
//!
//! | resource | key | limit (policy.rs) |
//! |---|---|---|
//! | items in flight | `Items` | `RunPrefs.item_concurrency` (1-6, default 4); parked items release their slot |
//! | OA APIs | `Api(Source)` | token bucket + concurrency per API |
//! | OA publisher host | `OaHost(oa_host_key)` | 1 in flight, >= 1 s gap, skipped after 5 consecutive failures |
//! | institutional publisher | `InstHost(family)` + `InstGlobal` | 1 per family, 2 overall, >= 2 s after a page, >= 5 s + 0-3 s jitter after a PDF |
//! | hidden browsers | `Browser` | 2 |
//! | save | `Save` | 1 |
//! | retained bodies | `Bytes` | 192 MiB (weighted) |
//! | licence ledger | `Ledger` | hourly / rolling-24 h / MB-per-hour, reserve-then-commit, persisted |
//! | halts | `Halts` | 429 / suspend / challenge loop / block signals stop the institutional route (persisted) |
//!
//! Lock order for institutional fetches: the host gate and the global gate are taken TOGETHER (all-or-nothing in
//! one `with`), never one after the other, so a holder of one is never waiting for the other (no deadlock, no
//! head-of-line blocking on the global cap by items waiting for a busy host: an item only joins the global queue
//! once its own family gate would grant, see acquire.rs).
//!
//! # Refusals
//! `Sched::refusal` is the single place that decides whether a gate may be waited on at all: engine shutting
//! down, an OA host that is skipped / walled / blocked, an OA API that was switched off, an institutional route
//! that is halted, an institutional family that is blocked. The acquire futures call it on EVERY poll, so a halt
//! that starts while items are queued refuses them at their next wake-up; `Cx::halt_inst` (acquire.rs) wakes
//! the queued waiters so that wake-up happens at once.
//!
//! # Reconfiguration
//! A later `Configure` replaces `item_concurrency`, the PDF gap and the caps, and applies the new `Items`
//! capacity to the live limiter (waiters and holders stay; the head is woken in case room appeared). `persisted`
//! (ledger, halts, EZproxy / Alma caches) is read only by the first `Configure` of an instance.
//!
//! # What the run manager / engine must call (integration points)
//! * `Engine::step` Configure: `Sched::configure(&cfg, persisted, seed, wall)` (already wired).
//! * `Command::UserAction` -> `login::on_user_action(ctl, run, kind)`; `Command::Shutdown` -> `login::on_shutdown(ctl)`
//!   (also marks the scheduler as shutting down: every later acquire / reserve resolves `Refused::ShuttingDown`);
//!   a finished or cancelled run -> `LoginGate::on_run_gone(run)`.
//! * `RunReport.ledger` = `Sched::usage(wall)`; `RunReport.halted` = `Halts::inst_halted(wall)`; Shutdown persists
//!   `Sched::persist_all(true)`; `DumpState` can embed `Sched::dump(wall)` and `LoginGate::dump()`.
//! * Host walls (`HostBook`) are in memory only and expire after 30 minutes (`policy::HOST_WALL_TTL_MS`); halts and
//!   blocks (`Halts`) are persisted by the `Cx` methods that set them.
use crate::proto::*;
use fulltext_core::model::*;
use std::collections::BTreeMap;
use std::task::Waker;

pub(crate) mod acquire;
pub(crate) mod gate;
pub(crate) mod hosts;
pub(crate) mod ledger;
pub(crate) mod policy;

pub(crate) use acquire::{ItemPermit, LedgerTicket, Permit};
pub(crate) use gate::{Limiter, Prio, Rng, Spec};
pub(crate) use hosts::{Halts, HostBook, InstKey};
pub(crate) use ledger::{CapHit, Caps, Ledger};
// Only the unit tests in this module name these through `super::*`.
#[cfg(test)]
pub(crate) use {gate::GatePoll, hosts::HaltReason};

/// Names a limiter inside `Sched::limiters`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum GateKey {
    Items,
    Api(Source),
    OaHost(String),
    InstHost(InstKey),
    InstGlobal,
    Browser,
    Save,
    #[allow(dead_code)] // reserved: never constructed yet
    Bytes,
}

/// Page vs PDF decides the gap stamped on release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchKind {
    Page,
    Pdf,
}

/// Why an `acquire_*` / `reserve_licence` call failed instead of returning a permit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Institutional route halted (429, suspend, challenge loop, block signals) until `until_wall`.
    InstHalted { reason: String, until_wall: u64 },
    /// The host is skipped / walled for this engine lifetime.
    HostSkipped { host: String },
    /// A licence cap would be exceeded.
    Cap(CapHit),
    /// The engine is shutting down.
    ShuttingDown,
}

/// The `snake_case` wire name of a source (`"pmc_s3"`), for refusal messages and dumps.
pub(crate) fn source_name(src: Source) -> String {
    serde_json::to_value(src)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The scheduler state.
pub(crate) struct Sched {
    pub(crate) limiters: BTreeMap<GateKey, Limiter>,
    pub(crate) ledger: Ledger,
    pub(crate) halts: Halts,
    pub(crate) hosts: HostBook,
    pub(crate) rng: Rng,
    item_concurrency: u64,
    /// `max(INST_PDF_GAP_MS, InstPrefs.min_gap_sec * 1000)`: prefs may only RAISE the gap.
    inst_pdf_gap_ms: u32,
    configured: bool,
    shutting_down: bool,
}

impl Sched {
    /// Unconfigured scheduler (before the first `Configure`): default caps, empty ledger.
    pub(crate) fn new() -> Self {
        Self {
            limiters: BTreeMap::new(),
            ledger: Ledger::new(LedgerState::default(), Caps::clamp(0, 0, 0)),
            halts: Halts::default(),
            hosts: HostBook::new(),
            rng: Rng::new(0),
            item_concurrency: u64::from(policy::ITEM_CONCURRENCY_DEFAULT),
            inst_pdf_gap_ms: policy::INST_PDF_GAP_MS,
            configured: false,
            shutting_down: false,
        }
    }

    /// Apply a `Configure`. The FIRST call also loads `persisted` (ledger, halts, caches) and seeds the RNG;
    /// later calls only update caps, `item_concurrency` and the PDF gap (in-memory state is newer than the
    /// persisted copy). Existing limiters keep their waiters; changed capacities apply from the next grant.
    pub(crate) fn configure(
        &mut self,
        cfg: &EngineConfig,
        persisted: Persisted,
        seed: u64,
        wall: u64,
    ) {
        let caps = Caps::clamp(
            cfg.inst.hourly_cap,
            cfg.inst.daily_cap,
            cfg.inst.mb_per_hour,
        );
        self.item_concurrency = policy::clamp_item_concurrency(cfg.run.item_concurrency);
        self.inst_pdf_gap_ms = policy::pdf_gap_ms(cfg.inst.min_gap_sec);
        let items = self.spec_for(&GateKey::Items);
        if let Some(l) = self.limiters.get_mut(&GateKey::Items) {
            l.set_spec(items).into_iter().for_each(Waker::wake);
        }
        if self.configured {
            self.ledger.set_caps(caps);
            return;
        }
        self.configured = true;
        self.rng = Rng::new(seed);
        self.ledger = Ledger::new(persisted.ledger, caps);
        self.ledger.load(wall);
        self.halts = Halts::new(persisted.halts);
        self.halts.prune(wall);
        self.hosts.ezproxy = persisted.ezproxy;
        self.hosts.alma = persisted.alma;
        self.hosts.prune(wall);
    }

    /// Static spec for a gate (capacity, gap, rate) from `policy`.
    pub(crate) fn spec_for(&self, key: &GateKey) -> Spec {
        match key {
            GateKey::Items => Spec {
                capacity: self.item_concurrency,
                min_gap_ms: 0,
                rate: None,
            },
            GateKey::Api(src) => {
                let l = policy::api_limits(*src);
                Spec {
                    capacity: l.concurrency,
                    min_gap_ms: 0,
                    rate: Some(gate::Rate {
                        burst: l.burst,
                        refill_ms: l.refill_ms,
                    }),
                }
            }
            GateKey::OaHost(_) => Spec {
                capacity: policy::OA_HOST_CONCURRENCY,
                min_gap_ms: policy::OA_HOST_GAP_MS,
                rate: None,
            },
            // The floor after any release is the page gap; PDF releases stamp the longer gap explicitly.
            GateKey::InstHost(_) => Spec {
                capacity: policy::INST_HOST_CONCURRENCY,
                min_gap_ms: policy::INST_PAGE_GAP_MS,
                rate: None,
            },
            GateKey::InstGlobal => Spec {
                capacity: policy::INST_GLOBAL_CONCURRENCY,
                min_gap_ms: 0,
                rate: None,
            },
            GateKey::Browser => Spec {
                capacity: policy::BROWSER_SLOTS,
                min_gap_ms: 0,
                rate: None,
            },
            GateKey::Save => Spec {
                capacity: policy::SAVE_SLOTS,
                min_gap_ms: 0,
                rate: None,
            },
            GateKey::Bytes => Spec {
                capacity: policy::BYTES_BUDGET,
                min_gap_ms: 0,
                rate: None,
            },
        }
    }

    /// The limiter for `key`, created from `spec_for` on first use.
    pub(crate) fn limiter_mut(&mut self, key: &GateKey) -> &mut Limiter {
        if !self.limiters.contains_key(key) {
            let spec = self.spec_for(key);
            self.limiters.insert(key.clone(), Limiter::new(spec));
        }
        self.limiters.entry(key.clone()).or_insert_with(|| {
            Limiter::new(Spec {
                capacity: 0,
                min_gap_ms: 0,
                rate: None,
            })
        })
    }

    /// Gap stamped on an institutional host gate when a fetch of `kind` finishes: page 2 s; PDF = the PDF gap
    /// (floor 5 s, raised by prefs) plus 0-3 s of jitter. Consumes RNG state for PDFs.
    pub(crate) fn inst_gap_ms(&mut self, kind: FetchKind) -> u32 {
        match kind {
            FetchKind::Page => policy::INST_PAGE_GAP_MS,
            FetchKind::Pdf => self
                .inst_pdf_gap_ms
                .saturating_add(self.rng.jitter(policy::INST_PDF_JITTER_MS)),
        }
    }

    /// The configured PDF gap without jitter.
    #[cfg(test)]
    pub(crate) fn inst_pdf_gap_ms(&self) -> u32 {
        self.inst_pdf_gap_ms
    }

    /// Why waiting on `key` is pointless right now, if it is (see the module docs). `now` is the monotonic
    /// clock (API switch-offs), `wall` the epoch clock (halts, blocks).
    pub(crate) fn refusal(&self, key: &GateKey, now: u64, wall: u64) -> Option<Refused> {
        if self.shutting_down {
            return Some(Refused::ShuttingDown);
        }
        let halted = || {
            self.halts
                .inst_halted(wall)
                .map(|(reason, until_wall)| Refused::InstHalted { reason, until_wall })
        };
        match key {
            GateKey::OaHost(host) => (self.hosts.is_skipped_at(host, now)
                || self.halts.host_blocked(host, wall))
            .then(|| Refused::HostSkipped { host: host.clone() }),
            GateKey::Api(src) => self
                .hosts
                .api_disabled(*src, now)
                .then(|| Refused::HostSkipped {
                    host: source_name(*src),
                }),
            GateKey::InstHost(family) => halted().or_else(|| {
                self.halts
                    .host_blocked(&family.0, wall)
                    .then(|| Refused::HostSkipped {
                        host: family.0.clone(),
                    })
            }),
            GateKey::InstGlobal => halted(),
            GateKey::Items | GateKey::Browser | GateKey::Save | GateKey::Bytes => None,
        }
    }

    /// The engine is going away: every later `acquire_*` / `reserve_licence` refuses with `ShuttingDown`.
    pub(crate) fn begin_shutdown(&mut self) {
        self.shutting_down = true;
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// Wakers of everything queued on `key` (so each waiter re-runs `refusal` at once).
    pub(crate) fn gate_wakers(&self, key: &GateKey) -> Vec<Waker> {
        self.limiters
            .get(key)
            .map(Limiter::all_wakers)
            .unwrap_or_default()
    }

    /// Wakers of everything queued on any institutional gate (a halt or block just started).
    pub(crate) fn inst_wakers(&self) -> Vec<Waker> {
        self.limiters
            .iter()
            .filter(|(k, _)| matches!(k, GateKey::InstHost(_) | GateKey::InstGlobal))
            .flat_map(|(_, l)| l.all_wakers())
            .collect()
    }

    /// Ledger usage for `RunReport.ledger`.
    pub(crate) fn usage(&self, wall: u64) -> LedgerUsage {
        self.ledger.usage(wall)
    }

    /// `Persist` effect for the halt record.
    pub(crate) fn halts_effect(&self, flush: bool) -> Effect {
        Effect::Persist {
            key: PersistKey::Halts,
            value: serde_json::to_value(&self.halts.state).unwrap_or(serde_json::Value::Null),
            flush,
        }
    }

    /// `Persist` effects for every key (used on `Shutdown`, with `flush = true`).
    pub(crate) fn persist_all(&self, flush: bool) -> Vec<Effect> {
        vec![
            self.ledger.persist_effect(flush),
            self.halts_effect(flush),
            Effect::Persist {
                key: PersistKey::Ezproxy,
                value: serde_json::to_value(&self.hosts.ezproxy).unwrap_or(serde_json::Value::Null),
                flush,
            },
            Effect::Persist {
                key: PersistKey::Alma,
                value: serde_json::to_value(&self.hosts.alma).unwrap_or(serde_json::Value::Null),
                flush,
            },
        ]
    }

    /// Compact JSON of every gate, the ledger and the halt record, for `DumpState`.
    pub(crate) fn dump(&self, wall: u64) -> serde_json::Value {
        let gates: Vec<serde_json::Value> = self
            .limiters
            .iter()
            .map(|(key, l)| {
                serde_json::json!({
                    "gate": format!("{key:?}"),
                    "capacity": l.spec.capacity,
                    "in_use": l.in_use,
                    "waiting": l.waiting(),
                    "next_at": l.next_at,
                })
            })
            .collect();
        serde_json::json!({
            "gates": gates,
            "ledger": self.ledger.usage(wall),
            "halted": self.halts.inst_halted(wall).map(|(reason, until)| serde_json::json!({ "reason": reason, "until": until })),
            "shutting_down": self.shutting_down,
        })
    }
}

/// Test helpers shared by the sched and login tests: a counting waker and a tiny manual executor. The real
/// `Executor` is not needed (and not used): tests poll futures by hand and complete ops through the reactor.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use crate::cx::{Cx, Owner, Shared};
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Wake};

    pub(crate) const WALL: u64 = 1_792_496_400_000;

    /// A waker target that counts wakes.
    #[derive(Default)]
    pub(crate) struct Flag {
        wakes: AtomicUsize,
        seen: AtomicUsize,
    }

    impl Flag {
        /// Total wakes so far.
        pub(crate) fn count(&self) -> usize {
            self.wakes.load(Ordering::SeqCst)
        }

        /// True when woken since the last `take` (the executor polls only then).
        pub(crate) fn take(&self) -> bool {
            let now = self.count();
            self.seen.swap(now, Ordering::SeqCst) != now
        }
    }

    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub(crate) fn flag_waker() -> (Arc<Flag>, Waker) {
        let f = Arc::new(Flag::default());
        (Arc::clone(&f), Waker::from(f))
    }

    /// KCL-like config, all routes on (same values as `sim::fixtures::test_config`; copied so these tests do not
    /// depend on another package's file).
    pub(crate) fn test_config() -> EngineConfig {
        EngineConfig {
            oa: OaConfig {
                openalex: true,
                openalex_key: String::new(),
                europepmc: true,
                biorxiv: true,
                pmc_s3: true,
                europepmc_get_pdf: true,
                medrxiv_early: true,
                arxiv: true,
                zotero_oa: true,
                unpaywall_email: String::new(),
                semantic_scholar_key: String::new(),
                core_key: String::new(),
                version_order: vec![Version::Published, Version::Accepted, Version::Submitted],
            },
            inst: InstPrefs {
                enabled: true,
                try_direct: true,
                use_openathens: true,
                use_ezproxy: true,
                check_alma: true,
                login_mode: LoginMode::Window,
                auto_open_login: true,
                login_timeout_sec: 300,
                group_libraries: GroupPolicy::Ask,
                confirm_above: 10,
                batch_max: 20,
                hourly_cap: 30,
                daily_cap: 80,
                mb_per_hour: 300,
                min_gap_sec: 5,
                challenge_windows: true,
            },
            run: RunPrefs {
                item_concurrency: 4,
                hedge: true,
                pdf_max_mb: 100,
            },
            institution: Institution {
                name: "King's College London".into(),
                openathens_domain: "kcl.ac.uk".into(),
                ezproxy: Some(Ezproxy {
                    login_host: "login.kcl.idm.oclc.org".into(),
                    suffix: "kcl.idm.oclc.org".into(),
                }),
                ezproxy_hosts: vec!["doi.org".into(), "www.bmj.com".into()],
                alma: Some(Alma {
                    base: "https://librarysearch.kcl.ac.uk".into(),
                    inst: "44KCL_INST".into(),
                }),
                libkey_library_id: Some(286),
                idp_hosts: vec!["login.microsoftonline.com".into()],
                entra_tenant: None,
                passthrough_hosts: vec![],
                warmup_url: String::new(),
            },
            endpoint_map: Vec::new(),
            trace: false,
        }
    }

    /// Tiny deterministic generator for seeded stress tests (no dependency).
    pub(crate) struct Lcg(pub(crate) u64);

    impl Lcg {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        /// Uniform-ish in `0..n` (`n == 0` counts as 1).
        pub(crate) fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    struct TestTask {
        fut: Pin<Box<dyn Future<Output = ()>>>,
        flag: Arc<Flag>,
        waker: Waker,
        done: bool,
    }

    /// A configured `Shared` plus a manual executor for the futures a test spawns (and for `SpawnReq`s the code
    /// under test pushes, e.g. the login window driver).
    pub(crate) struct World {
        pub(crate) shared: Rc<Shared>,
        tasks: Vec<TestTask>,
    }

    impl World {
        /// Configured with `test_config()`, seed 1, `now = 1000`, `wall = WALL`.
        pub(crate) fn new() -> Self {
            Self::with_config(test_config())
        }

        pub(crate) fn with_config(cfg: EngineConfig) -> Self {
            let w = Self {
                shared: Shared::new(),
                tasks: Vec::new(),
            };
            let _ = w.shared.with(|s| {
                s.clock.advance(1_000, WALL);
            });
            w.configure(cfg);
            w
        }

        /// Like the engine's `Configure` handling: store the config and call `Sched::configure` (the first call
        /// loads `Persisted::default()`).
        pub(crate) fn configure(&self, cfg: EngineConfig) {
            self.configure_persisted(cfg, Persisted::default());
        }

        pub(crate) fn configure_persisted(&self, cfg: EngineConfig, persisted: Persisted) {
            let _ = self.shared.with(|s| {
                let wall = s.clock.wall;
                s.sched.configure(&cfg, persisted, 1, wall);
                s.cfg = Some(Rc::new(cfg));
            });
        }

        /// Mutate the stored config (used by login tests that flip `auto_open_login`).
        pub(crate) fn edit_config(&self, f: impl FnOnce(&mut EngineConfig)) {
            let _ = self.shared.with(|s| {
                if let Some(rc) = s.cfg.take() {
                    let mut cfg = (*rc).clone();
                    f(&mut cfg);
                    s.cfg = Some(Rc::new(cfg));
                }
            });
        }

        pub(crate) fn cx(&self) -> Cx {
            Cx::new(Rc::clone(&self.shared), Owner::System)
        }

        /// A context labelled as an item of a run (effects carry these ids).
        pub(crate) fn item_cx(&self, run: RunId, item: ItemId) -> Cx {
            Cx::new(Rc::clone(&self.shared), Owner::Item { run, item })
        }

        pub(crate) fn now(&self) -> u64 {
            self.shared.with(|s| s.clock.now).unwrap_or_default()
        }

        pub(crate) fn wall(&self) -> u64 {
            self.shared.with(|s| s.clock.wall).unwrap_or_default()
        }

        /// Add a task; it is polled by the next `run`. Returns its index (for `kill`).
        pub(crate) fn spawn(&mut self, fut: impl Future<Output = ()> + 'static) -> usize {
            self.add(Box::pin(fut))
        }

        fn add(&mut self, fut: Pin<Box<dyn Future<Output = ()>>>) -> usize {
            let (flag, waker) = flag_waker();
            waker.wake_by_ref(); // first poll
            self.tasks.push(TestTask {
                fut,
                flag,
                waker,
                done: false,
            });
            self.tasks.len() - 1
        }

        /// Drop a task's future now (cancellation: runs the `Drop`s of whatever it was awaiting).
        pub(crate) fn kill(&mut self, id: usize) {
            if let Some(t) = self.tasks.get_mut(id) {
                t.fut = Box::pin(std::future::ready(()));
                t.done = true;
            }
        }

        pub(crate) fn finished(&self, id: usize) -> bool {
            self.tasks.get(id).is_some_and(|t| t.done)
        }

        /// Poll woken tasks (and spawn `SpawnReq`s) until nothing is ready.
        pub(crate) fn run(&mut self) {
            for _ in 0..10_000 {
                let spawned = self
                    .shared
                    .with(|s| std::mem::take(&mut s.spawns))
                    .unwrap_or_default();
                let new_tasks = !spawned.is_empty();
                for req in spawned {
                    self.add(req.fut);
                }
                let mut polled = false;
                for t in &mut self.tasks {
                    if t.done || !t.flag.take() {
                        continue;
                    }
                    polled = true;
                    let mut cx = Context::from_waker(&t.waker);
                    if t.fut.as_mut().poll(&mut cx).is_ready() {
                        t.done = true;
                    }
                }
                if !polled && !new_tasks {
                    return;
                }
            }
            panic!("test executor did not settle within 10000 rounds");
        }

        /// Move both clocks forward and wake every timer that is now due.
        pub(crate) fn advance(&self, ms: u64) {
            let due = self
                .shared
                .with(|s| {
                    let (now, wall) = (s.clock.now + ms, s.clock.wall + ms);
                    s.clock.advance(now, wall);
                    s.reactor.due_timers(now)
                })
                .unwrap_or_default();
            due.into_iter().for_each(Waker::wake);
        }

        /// Deliver a successful completion.
        pub(crate) fn complete_ok(&self, op: OpId, value: serde_json::Value) {
            self.complete(CompletionMsg {
                op,
                ok: Some(value),
                err: None,
            });
        }

        pub(crate) fn complete_err(&self, op: OpId, kind: OpErrKind) {
            self.complete(CompletionMsg {
                op,
                ok: None,
                err: Some(OpErr {
                    kind,
                    message: String::new(),
                    after_ms: None,
                    status: None,
                }),
            });
        }

        fn complete(&self, msg: CompletionMsg) {
            let w = self.shared.with(|s| s.reactor.complete(msg)).flatten();
            if let Some(w) = w {
                w.wake();
            }
        }

        /// Take (and clear) the effects emitted so far.
        pub(crate) fn effects(&self) -> Vec<Effect> {
            self.shared
                .with(|s| std::mem::take(&mut s.outbox))
                .unwrap_or_default()
        }

        /// Number of virtual timers that are armed.
        pub(crate) fn timers(&self) -> usize {
            self.shared
                .with(|s| s.reactor.timer_count())
                .unwrap_or_default()
        }

        /// Earliest armed deadline.
        pub(crate) fn next_deadline(&self) -> Option<u64> {
            self.shared.with(|s| s.reactor.next_deadline()).flatten()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use crate::cx::Shared;

    #[test]
    fn spec_table_matches_the_policy() {
        let w = World::new();
        let spec = |k: GateKey| w.shared.with(|s| s.sched.spec_for(&k)).expect("state");
        assert_eq!(
            spec(GateKey::Items),
            Spec {
                capacity: 4,
                min_gap_ms: 0,
                rate: None
            }
        );
        assert_eq!(spec(GateKey::Browser).capacity, 2);
        assert_eq!(spec(GateKey::Save).capacity, 1);
        assert_eq!(spec(GateKey::Bytes).capacity, 192 * 1024 * 1024);
        assert_eq!(
            spec(GateKey::InstGlobal),
            Spec {
                capacity: 2,
                min_gap_ms: 0,
                rate: None
            }
        );
        let host = spec(GateKey::InstHost(InstKey("wiley".into())));
        assert_eq!(
            (host.capacity, host.min_gap_ms),
            (1, policy::INST_PAGE_GAP_MS)
        );
        let oa = spec(GateKey::OaHost("europepmc.org".into()));
        assert_eq!((oa.capacity, oa.min_gap_ms), (1, policy::OA_HOST_GAP_MS));
        let api = spec(GateKey::Api(Source::Biorxiv));
        assert_eq!(
            api,
            Spec {
                capacity: 1,
                min_gap_ms: 0,
                rate: Some(gate::Rate {
                    burst: 1,
                    refill_ms: 1_000
                })
            }
        );
        let api = spec(GateKey::Api(Source::Openalex));
        assert_eq!(api.capacity, 4);
        assert!(api.rate.is_some());
    }

    #[test]
    fn items_capacity_follows_the_pref_and_is_clamped() {
        for (pref, want) in [(1, 1), (4, 4), (6, 6), (0, 1), (50, 6)] {
            let mut cfg = test_config();
            cfg.run.item_concurrency = pref;
            let w = World::with_config(cfg);
            let cap = w
                .shared
                .with(|s| s.sched.spec_for(&GateKey::Items).capacity)
                .expect("state");
            assert_eq!(cap, want, "pref {pref}");
        }
    }

    #[test]
    fn pdf_gap_only_rises_with_the_pref() {
        for (sec, want) in [(0, 5_000), (3, 5_000), (5, 5_000), (9, 9_000)] {
            let mut cfg = test_config();
            cfg.inst.min_gap_sec = sec;
            let w = World::with_config(cfg);
            assert_eq!(
                w.shared.with(|s| s.sched.inst_pdf_gap_ms()).expect("state"),
                want,
                "min_gap_sec {sec}"
            );
        }
    }

    #[test]
    fn inst_gap_is_page_gap_or_pdf_gap_plus_bounded_jitter() {
        let w = World::new();
        let gaps: Vec<u32> = w
            .shared
            .with(|s| {
                (0..200)
                    .map(|_| s.sched.inst_gap_ms(FetchKind::Pdf))
                    .collect()
            })
            .expect("state");
        assert!(gaps.iter().all(|g| (5_000..=8_000).contains(g)), "{gaps:?}");
        assert!(gaps.iter().any(|g| *g != 5_000), "jitter must vary");
        assert_eq!(
            w.shared
                .with(|s| s.sched.inst_gap_ms(FetchKind::Page))
                .expect("state"),
            2_000
        );
    }

    #[test]
    fn configure_loads_persisted_state_once_and_clamps_caps() {
        let w = World::with_config({
            let mut c = test_config();
            c.inst.hourly_cap = 500;
            c.inst.daily_cap = 500;
            c.inst.mb_per_hour = 5_000;
            c
        });
        let caps = w.shared.with(|s| s.sched.ledger.caps()).expect("state");
        assert_eq!(
            caps,
            Caps {
                hourly: 30,
                daily: 80,
                mb_per_hour: 300
            }
        );

        // first Configure carries persisted state
        let persisted = Persisted {
            ledger: LedgerState {
                committed: vec![(WALL - 1_000, 5), (WALL - 3 * policy::DAY_MS, 7)],
                reserved: vec![],
            },
            halts: HaltState {
                inst_until: WALL + 60_000,
                reason: Some("rate_limited".into()),
                hosts: BTreeMap::new(),
            },
            ezproxy: BTreeMap::from([(
                "www.bmj.com".to_owned(),
                HostConf {
                    configured: true,
                    at: WALL - 1_000,
                },
            )]),
            alma: BTreeMap::from([(
                "10.1/x".to_owned(),
                AlmaEntry {
                    held: true,
                    at: WALL - 2 * policy::DAY_MS,
                },
            )]),
        };
        let fresh = Shared::new();
        let _ = fresh.with(|s| s.clock.advance(1_000, WALL));
        let _ = fresh.with(|s| s.sched.configure(&test_config(), persisted, 9, WALL));
        let (usage, halted, ez, alma) = fresh
            .with(|s| {
                (
                    s.sched.usage(WALL),
                    s.sched.halts.inst_halted(WALL),
                    s.sched.hosts.ezproxy_known("www.bmj.com", WALL),
                    s.sched.hosts.alma_known("10.1/x", WALL),
                )
            })
            .expect("state");
        assert_eq!(usage.hour, 1);
        assert_eq!(usage.day, 1, "the 3-day-old entry was pruned");
        assert_eq!(halted, Some(("rate_limited".into(), WALL + 60_000)));
        assert_eq!(ez, Some(true));
        assert_eq!(alma, None, "expired Alma answers are dropped");

        // a second Configure keeps the in-memory state and only updates caps / concurrency
        let mut c2 = test_config();
        c2.inst.hourly_cap = 7;
        c2.run.item_concurrency = 2;
        let _ = fresh.with(|s| s.sched.configure(&c2, Persisted::default(), 1234, WALL + 5));
        let (usage, halted, cap, items) = fresh
            .with(|s| {
                (
                    s.sched.usage(WALL),
                    s.sched.halts.inst_halted(WALL),
                    s.sched.ledger.caps().hourly,
                    s.sched.spec_for(&GateKey::Items).capacity,
                )
            })
            .expect("state");
        assert_eq!((usage.hour, usage.day), (1, 1));
        assert!(halted.is_some());
        assert_eq!((cap, items), (7, 2));
    }

    #[test]
    fn a_later_configure_resizes_the_live_items_limiter_and_wakes_the_head() {
        let w = World::new();
        let (flag, waker) = flag_waker();
        let woken = w
            .shared
            .with(|s| {
                let l = s.sched.limiter_mut(&GateKey::Items);
                let holders: Vec<_> = (0..4).map(|_| l.enqueue(Prio::New)).collect();
                for t in holders {
                    assert_eq!(l.poll(t, 1_000, 1, &waker), GatePoll::Granted);
                }
                let waiting = l.enqueue(Prio::New);
                assert_eq!(
                    l.poll(waiting, 1_000, 1, &waker),
                    GatePoll::Wait { retry_at: None }
                );
                waiting
            })
            .expect("state");
        let before = flag.count();
        let mut c = test_config();
        c.run.item_concurrency = 6;
        w.configure(c);
        assert_eq!(flag.count(), before + 1, "the head was woken");
        let again = w
            .shared
            .with(|s| {
                s.sched
                    .limiter_mut(&GateKey::Items)
                    .poll(woken, 1_000, 1, &waker)
            })
            .expect("state");
        assert_eq!(again, GatePoll::Granted, "room appeared");
    }

    #[test]
    fn refusals_per_gate() {
        let w = World::new();
        let wall = WALL;
        let r = |k: GateKey| {
            w.shared
                .with(|s| s.sched.refusal(&k, 1_000, wall))
                .expect("state")
        };
        assert_eq!(r(GateKey::Items), None);
        assert_eq!(r(GateKey::OaHost("a.org".into())), None);
        // walled OA host
        let _ = w.shared.with(|s| s.sched.hosts.wall("a.org", "failures"));
        assert_eq!(
            r(GateKey::OaHost("a.org".into())),
            Some(Refused::HostSkipped {
                host: "a.org".into()
            })
        );
        // blocked (persisted) OA host
        let _ = w
            .shared
            .with(|s| s.sched.halts.block_host("b.org", wall + 10));
        assert_eq!(
            r(GateKey::OaHost("b.org".into())),
            Some(Refused::HostSkipped {
                host: "b.org".into()
            })
        );
        // disabled API until `now = 5000`
        let _ = w
            .shared
            .with(|s| s.sched.hosts.disable_api(Source::Openalex, 5_000));
        assert_eq!(
            r(GateKey::Api(Source::Openalex)),
            Some(Refused::HostSkipped {
                host: "openalex".into()
            })
        );
        assert_eq!(
            w.shared
                .with(|s| s
                    .sched
                    .refusal(&GateKey::Api(Source::Openalex), 5_000, wall))
                .expect("state"),
            None
        );
        assert_eq!(r(GateKey::Api(Source::PmcS3)), None);
        // blocked institutional family
        let _ = w
            .shared
            .with(|s| s.sched.halts.block_host("wiley", wall + 10));
        let wiley = GateKey::InstHost(InstKey("wiley".into()));
        assert_eq!(
            r(wiley.clone()),
            Some(Refused::HostSkipped {
                host: "wiley".into()
            })
        );
        assert_eq!(r(GateKey::InstHost(InstKey("lww".into()))), None);
        assert_eq!(r(GateKey::InstGlobal), None);
        // a route halt refuses both institutional gates but never OA or local gates
        let _ = w
            .shared
            .with(|s| s.sched.halts.halt_inst(HaltReason::RateLimited, None, wall));
        let want = Some(Refused::InstHalted {
            reason: "rate_limited".into(),
            until_wall: wall + policy::HALT_MIN_MS,
        });
        assert_eq!(r(GateKey::InstGlobal), want);
        assert_eq!(r(GateKey::InstHost(InstKey("lww".into()))), want);
        assert_eq!(r(wiley), want, "a halt outranks a family block");
        assert_eq!(r(GateKey::Items), None);
        assert_eq!(r(GateKey::Browser), None);
        // shutdown refuses everything
        let _ = w.shared.with(|s| s.sched.begin_shutdown());
        for k in [
            GateKey::Items,
            GateKey::Save,
            GateKey::Bytes,
            GateKey::Browser,
            GateKey::OaHost("z.org".into()),
        ] {
            assert_eq!(r(k), Some(Refused::ShuttingDown));
        }
        assert!(
            w.shared
                .with(|s| s.sched.is_shutting_down())
                .expect("state")
        );
    }

    #[test]
    fn halt_expiry_lifts_the_refusal() {
        let w = World::new();
        let _ = w
            .shared
            .with(|s| s.sched.halts.halt_inst(HaltReason::RateLimited, None, WALL));
        let at = |wall: u64| {
            w.shared
                .with(|s| s.sched.refusal(&GateKey::InstGlobal, 1_000, wall))
                .expect("state")
        };
        assert!(at(WALL + policy::HALT_MIN_MS - 1).is_some());
        assert_eq!(at(WALL + policy::HALT_MIN_MS), None);
    }

    #[test]
    fn persist_all_covers_every_key_in_a_stable_order() {
        let w = World::new();
        let keys: Vec<(PersistKey, bool)> = w
            .shared
            .with(|s| s.sched.persist_all(true))
            .expect("state")
            .into_iter()
            .filter_map(|e| match e {
                Effect::Persist { key, flush, .. } => Some((key, flush)),
                _ => None,
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                (PersistKey::Ledger, true),
                (PersistKey::Halts, true),
                (PersistKey::Ezproxy, true),
                (PersistKey::Alma, true)
            ]
        );
    }

    #[test]
    fn dump_lists_the_gates_that_exist() {
        let w = World::new();
        let _ = w.shared.with(|s| {
            s.sched.limiter_mut(&GateKey::Save);
            s.sched.limiter_mut(&GateKey::Browser);
        });
        let dump = w.shared.with(|s| s.sched.dump(WALL)).expect("state");
        let gates = dump
            .get("gates")
            .and_then(|g| g.as_array())
            .expect("gates array");
        assert_eq!(gates.len(), 2);
        assert_eq!(dump.get("halted"), Some(&serde_json::Value::Null));
        assert_eq!(
            dump.pointer("/ledger/hour_cap"),
            Some(&serde_json::json!(30))
        );
    }

    #[test]
    fn source_names_are_wire_names() {
        assert_eq!(source_name(Source::PmcS3), "pmc_s3");
        assert_eq!(source_name(Source::SemanticScholar), "semantic_scholar");
        assert_eq!(source_name(Source::ZoteroOa), "zotero_oa");
    }

    #[test]
    fn the_manual_executor_runs_tasks_timers_and_cancellation() {
        let mut w = World::new();
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let cx = w.cx();
        let l = std::rc::Rc::clone(&log);
        let id = w.spawn(async move {
            cx.sleep(500).await;
            l.borrow_mut().push("slept");
        });
        w.run();
        assert!(log.borrow().is_empty());
        assert_eq!(w.timers(), 1);
        assert_eq!(w.next_deadline(), Some(1_500));
        w.advance(499);
        w.run();
        assert!(log.borrow().is_empty());
        w.advance(1);
        w.run();
        assert_eq!(*log.borrow(), vec!["slept"]);
        assert!(w.finished(id));
        assert_eq!(w.timers(), 0);

        // killing a task drops its sleep, which removes the timer
        let cx = w.cx();
        let id = w.spawn(async move { cx.sleep(10_000).await });
        w.run();
        assert_eq!(w.timers(), 1);
        w.kill(id);
        assert_eq!(w.timers(), 0);
    }
}

/// The same machinery through the real `Engine` (real executor, real `step` phases, stall detector).
#[cfg(test)]
mod engine_tests {
    use super::testkit::{WALL, test_config};
    use super::*;
    use crate::cx::{Cx, LocalFuture, Owner};
    use crate::engine::Engine;
    use crate::login::LoginRequest;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn configured(cfg: EngineConfig) -> Engine {
        let mut e = Engine::new();
        let cmd = Command::Configure {
            protocol: PROTOCOL,
            config: Box::new(cfg),
            persisted: Persisted::default(),
            seed: 7,
        };
        e.step(StepIn {
            now: 1_000,
            wall: WALL,
            commands: vec![cmd],
            completions: Vec::new(),
        })
        .expect("configure");
        e
    }

    fn step(e: &mut Engine, now: u64, completions: Vec<CompletionMsg>) -> StepOut {
        e.step(StepIn {
            now,
            wall: WALL + now,
            commands: Vec::new(),
            completions,
        })
        .expect("step")
    }

    fn boxed(f: impl Future<Output = ()> + 'static) -> LocalFuture {
        Box::pin(f)
    }

    use std::future::Future;

    fn login_req(seen_epoch: u32) -> LoginRequest {
        LoginRequest {
            entry_url: "https://go.openathens.net/x".into(),
            item_title: "t".into(),
            seen_epoch,
            capture_pdf: true,
        }
    }

    #[test]
    fn host_gate_serialises_tasks_through_the_real_executor() {
        let mut e = configured(test_config());
        let log = Rc::new(RefCell::new(Vec::new()));
        for i in 0..3u32 {
            let log = Rc::clone(&log);
            e.spawn_test(Owner::System, move |cx: Cx| {
                boxed(async move {
                    let p = cx.acquire_oa_host("h.org").await.expect("granted");
                    log.borrow_mut().push((i, cx.now()));
                    cx.sleep(500).await;
                    drop(p);
                })
            })
            .expect("spawn");
        }
        let out = step(&mut e, 1_000, Vec::new());
        assert_eq!(*log.borrow(), vec![(0, 1_000)]);
        assert_eq!(
            out.next_timer,
            Some(1_500),
            "task 0 is sleeping; the others wait for the release"
        );
        let out = step(&mut e, 1_500, Vec::new());
        assert_eq!(
            out.next_timer,
            Some(2_500),
            "the 1 s host gap is a timer in the same heap"
        );
        assert_eq!(out.stats.tasks, 2);
        let out = step(&mut e, 2_500, Vec::new());
        assert_eq!(*log.borrow(), vec![(0, 1_000), (1, 2_500)]);
        assert_eq!(out.next_timer, Some(3_000));
        step(&mut e, 3_000, Vec::new());
        let out = step(&mut e, 4_000, Vec::new());
        assert_eq!(*log.borrow(), vec![(0, 1_000), (1, 2_500), (2, 4_000)]);
        let out2 = step(&mut e, 4_500, Vec::new());
        assert_eq!((out.stats.tasks, out2.stats.tasks), (1, 0));
        assert!(!out.stats.stalled && !out2.stats.stalled);
        assert_eq!(
            (
                out2.stats.lost_wakes,
                out2.stats.timers,
                out2.stats.ops_inflight
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn five_items_share_one_login_window_through_the_real_engine() {
        let mut e = configured(test_config());
        let log = Rc::new(RefCell::new(Vec::new()));
        for item in 1..=5u32 {
            let log = Rc::clone(&log);
            e.spawn_test(Owner::Item { run: 1, item }, move |cx: Cx| {
                boxed(async move {
                    let v = cx.session_login(Realm::Openathens, login_req(0)).await;
                    log.borrow_mut().push(format!("{item}:{v:?}"));
                })
            })
            .expect("spawn");
        }
        let out = step(&mut e, 1_000, Vec::new());
        let windows: Vec<(OpId, u32)> = out
            .effects
            .iter()
            .filter_map(|ef| match ef {
                Effect::LoginWindow { op, req, .. } => Some((*op, req.waiting)),
                _ => None,
            })
            .collect();
        assert_eq!(windows.len(), 1, "one LoginWindow for five items");
        assert_eq!(
            windows[0].1, 5,
            "all five were already waiting when the driver opened the window"
        );
        assert_eq!(
            (
                out.stats.waiting_user,
                out.stats.ops_inflight,
                out.stats.stalled
            ),
            (5, 1, false)
        );
        // time passes with nothing to do: no stall, no lost wake
        let out = step(&mut e, 5_000, Vec::new());
        assert!(!out.stats.stalled && out.stats.lost_wakes == 0 && out.effects.is_empty());
        // the user signs in
        let ok = json!({ "result": "landed", "final_url": "https://x.org/", "inspect": { "expect": "none" } });
        let out = step(
            &mut e,
            6_000,
            vec![CompletionMsg {
                op: windows[0].0,
                ok: Some(ok),
                err: None,
            }],
        );
        let mut got = log.borrow().clone();
        got.sort();
        assert_eq!(
            got,
            (1..=5)
                .map(|i| format!("{i}:Retry {{ epoch: 1 }}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            (out.stats.tasks, out.stats.waiting_user, out.stats.stalled),
            (0, 0, false)
        );
        assert_eq!(out.stats.timers, 0, "the driver's deadline timer is gone");
    }

    #[test]
    fn waiting_for_the_user_is_not_a_stall() {
        let mut cfg = test_config();
        cfg.inst.auto_open_login = false;
        let mut e = configured(cfg);
        for item in 1..=2u32 {
            e.spawn_test(Owner::Item { run: 1, item }, move |cx: Cx| {
                boxed(async move {
                    let _ = cx.session_login(Realm::Openathens, login_req(0)).await;
                })
            })
            .expect("spawn");
        }
        let out = step(&mut e, 1_000, Vec::new());
        // no op, no timer: only the user can move things on
        assert_eq!(
            (
                out.stats.ops_inflight,
                out.stats.timers,
                out.stats.waiting_user
            ),
            (0, 0, 2)
        );
        assert!(!out.stats.stalled);
        let rows = out.effects.iter().filter(|ef| matches!(ef, Effect::Progress { row, .. } if row.actions == vec![RowAction::SignIn])).count();
        assert_eq!(rows, 2);
        let out = step(&mut e, 2_000, Vec::new());
        assert!(!out.stats.stalled && out.stats.lost_wakes == 0);
    }

    #[test]
    fn a_halt_refuses_the_queue_and_next_timer_covers_gate_waits() {
        let mut e = configured(test_config());
        let log = Rc::new(RefCell::new(Vec::new()));
        // task 0 holds the wiley family for 1 s; 1 and 2 wait; task 3 halts the route at the same instant (t = 2000)
        for i in 0..3u32 {
            let log = Rc::clone(&log);
            e.spawn_test(Owner::System, move |cx: Cx| {
                boxed(async move {
                    match cx
                        .acquire_inst(&InstKey("wiley".into()), FetchKind::Pdf)
                        .await
                    {
                        Ok(p) => {
                            log.borrow_mut().push(format!("{i} granted"));
                            cx.sleep(1_000).await;
                            drop(p);
                        }
                        Err(r) => log
                            .borrow_mut()
                            .push(format!("{i} {}", matches!(r, Refused::InstHalted { .. }))),
                    }
                })
            })
            .expect("spawn");
        }
        e.spawn_test(Owner::System, |cx: Cx| {
            boxed(async move {
                cx.sleep(1_000).await;
                cx.halt_inst(HaltReason::RateLimited, Some(60));
            })
        })
        .expect("spawn");
        let out = step(&mut e, 1_000, Vec::new());
        assert_eq!(*log.borrow(), vec!["0 granted"]);
        assert_eq!(out.next_timer, Some(2_000));
        // task 0 releases at 2000 (PDF gap 5 s + jitter), task 3 halts at the same instant: 1 and 2 are refused
        let out = step(&mut e, 2_000, Vec::new());
        let got = log.borrow().clone();
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(
            got.contains(&"1 true".to_owned()) && got.contains(&"2 true".to_owned()),
            "{got:?}"
        );
        assert!(out.effects.iter().any(|ef| matches!(
            ef,
            Effect::Persist {
                key: PersistKey::Halts,
                flush: true,
                ..
            }
        )));
        assert_eq!(out.stats.tasks, 0);
        assert!(!out.stats.stalled);
    }
}
