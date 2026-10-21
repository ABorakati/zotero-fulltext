# Engine design (crates/fulltext-engine)

Status: skeleton merged (compiles, clippy-clean, 12 unit tests). Behaviour is filled in by the work packages in
section 10. Contracts: `docs/design/contracts/{model,proto}.rs` (frozen; `src/proto.rs` is a verbatim copy with the
import path fixed). Background: `candidates/A-rust-async-engine.md` (the base), `judgements.json` (grafts),
`candidates/C-failure-first.md` (failure catalogue F1-F32).

## 1. What the engine is

A sans-IO, single-threaded, deterministic state machine that runs **one async task per item**. It never performs
I/O, never reads a clock, never spawns a thread. JS calls `Engine::step(StepIn { now, wall, commands, completions })`
and gets `StepOut { effects, next_timer, more, ignored, stats }`. Every leaf `.await` in the pipeline is an
effect (HTTP, hidden-browser load, login window, translators, Zotero OA, save, confirm) that JS performs
concurrently as a Promise; its completion wakes exactly the awaiting future through a real `Waker`.
Policy (limits, spacing, ledger, halts, tiers, login gate, outcomes) is Rust; Gecko/Zotero calls are JS.

Reads like ordinary async Rust: `let r = cx.timeout(9_000, cx.http(..)).await;` and `FuturesUnordered` for the
parallel OA lookups. Dropping a future cancels its effect (`Effect::Cancel`) and releases its bodies
(`Effect::Release`) in the same `StepOut`.

## 2. Module map and ownership

| file | role | work package |
|---|---|---|
| `proto.rs`, `error.rs`, `lib.rs`, `Cargo.toml` | contract copy, `EngineError`, crate root | executor-ops |
| `cx.rs` | `State` + `Shared` cell, `Cx` (task context), `ItemCx`, `Ctl`, `Owner`, `ItemDone` | executor-ops |
| `executor.rs` | task table, `Waker`, poll budget, cancel-by-owner | executor-ops |
| `ops.rs` | `Reactor` (op slots, virtual timers), `OpFuture`, `Sleep`, `Timeout`, `Body`, typed op wrappers on `Cx` | executor-ops |
| `engine.rs` | `Engine::new` / `step` (the 9 phases) | executor-ops |
| `sim/mod.rs`, `sim/fixtures.rs` | `SimHost` driver, virtual clock, `Responder`, fixture builders | executor-ops |
| `sched/{mod,gate,ledger,hosts,policy,acquire}.rs` | limiters, token buckets, ledger, halts, host knowledge, numbers, acquire futures + permits | sched-login |
| `login.rs` | session gate with epochs, interactive window gate, window driver task | sched-login |
| `pipeline/{mod,item,precheck,pool,requests,oa,attempt,landing,save,pure}.rs`, `l10n.rs` | item pipeline, OA path, save, Fluent ids | pipeline |
| `pipeline/{inst,handoff}.rs` | institutional route, hand-off links | inst |
| `run.rs` | run manager (StartRun, cancel, confirm, reports) | run-sim |
| `sim/{invariants,scenarios/*}.rs` | invariants + failure-catalogue scenarios + property tests | run-sim |

Pure logic (ids, URL builders, ranking, page/API parsing, `inspect`) lives in `fulltext-core`; only
`pipeline/pure.rs` talks to core's URL/id modules (a rename in core touches one file).

## 3. Execution model

**Shared state and the borrow rule.** All mutable state is one `State` in `Shared` (`Rc<RefCell<State>>`),
reachable only through `Shared::with(|st| ..)`, whose closure sees `&mut State` and nothing else. Rules (cx.rs
header): no borrow across `.await`; never drop a `Drop`-with-effects value (`OpFuture`, `Permit`, `Body`,
`LedgerTicket`, executor tasks) inside `with`; waking inside `with` is fine (it only pushes onto the ready
queue); `Executor::cancel_where` is called outside `with`. `with` uses `try_borrow_mut`: a failure returns `None`,
counts a re-entry and `step` returns `EngineError(reentered)`, so a bug fails the run instead of trapping the
instance. This replaces candidate A's try_borrow/deferred-cancel layers: nothing needs to be deferred because the
rules make the situation unreachable, and `step` verifies it.

**Executor.** Slab of tasks with `TaskKey { index, epoch }` (stale wakes ignored). `TaskWaker` = `Arc` with a
`queued: AtomicBool` for dedupe and an `Arc<ReadyQueue>` (a `Mutex<VecDeque>`; valid on wasm32 without threads
because nothing holds the lock across user code). No thread-locals, no globals, no `unsafe`. `run(budget)` polls
FIFO; a wake during a poll re-queues. Owner-tagged tasks: `Item{run,item}`, `Run(run)`, `System`. Concurrency inside
an item is sub-futures (`FuturesUnordered`, `Timeout`, `select_biased!`), never tasks. Logic that needs a new task
(the login window driver, the batch confirm) pushes a `SpawnReq` into `State::spawns`; `step` spawns it after the
poll round.

**Step phases** (engine.rs header): clock -> commands -> completions -> due timers -> poll rounds (spawn, run,
`collect_done`) -> `finish_runs` -> stall check -> reentry check -> output. Effect order in `outbox` is emission
order and is part of the contract.

**Time.** `Clock { now, wall }` per step (`now` monotonic ms for timers/gaps; `wall` epoch ms for ledger, halts,
cache TTLs). Timers are virtual: `Sleep` registers `(deadline, seq) -> Waker`; `StepOut.next_timer` is the
earliest deadline (op timers and limiter retry instants are the same heap).

**Lost wakes and stalls.** A stall = live tasks, none ready, no op in flight, no timer, nobody waiting for the
user. `step` then calls `exec.wake_all()` once and polls again: progress => `Stats.lost_wakes += 1`; no progress
=> `Stats.stalled = true` (JS sends `DumpState`, fails the run). Sim tests assert both stay at zero.

**Determinism.** BTreeMap everywhere, `Rng` (xorshift64*) seeded from `Configure.seed`, no clock reads. Same
inputs => same effects, so every field bug is reproducible natively from a recorded `StepIn` log.

## 4. Ops, completions and protocol interpretations

Leaf futures (ops.rs): `OpFuture` emits its effect on the **first poll** (lazy), `Waiting` slot keyed by a fresh
`OpId` (starts at 1, never reused); a completion flips the slot to `Done` and returns the stored waker, woken
after the borrow ends; **dropping a waiting `OpFuture` emits `Cancel`**; unknown/duplicate completions go to
`StepOut.ignored`. `Body` wraps every `BodyRef`; **dropping it emits `Release`**. Typed wrappers on `Cx`:
`http`, `browser_load`, `login_window` (low level), `run_translators`, `zotero_oa`, `save_pdf`, `confirm`,
`sleep`, `timeout`. Op errors are `OpErr` (decode failures become `OpErrKind::Internal`).

The frozen proto leaves a few points open. The engine behaves as follows; the JS area must match (also listed in
the notes returned with this design):

1. `barrier: true` (Http/BrowserLoad on `Route::Inst` that consume licence): JS must perform every earlier effect of
   the same `StepOut` first, in order, and wait for `Persist { flush: true }` to be durable before starting it.
2. `Release { handle }` is idempotent and is emitted for **every** `BodyRef` the engine received, including after a
   `SavePdf` finished; JS treats unknown/consumed handles as a no-op. Completions listed in `StepOut.ignored` carry
   bodies JS must release itself.
3. `Cancel { op }`: HTTP/browser -> abort; `LoginWindow` -> close the window; `SavePdf` -> ignore if already
   running (the engine marks saving items non-cancellable, so this only happens at shutdown).
4. `LoginUpdate { op, waiting }`: `op` is a FRESH id (`Reactor::alloc_id`), never the window's; JS completes it
   immediately and that completion lands in `StepOut.ignored`. Sent whenever the number of waiting items changes.
5. `ZoteroOa` completion `ok` = JSON array of `ZoteroOaHit`. `RunTranslators` ok = `TranslatorsOk`.
6. `Configure` may be re-sent; each `StartRun` snapshots the latest config. `persisted` is applied only by the
   first `Configure` of an instance (in-memory state is newer afterwards).
7. `StartRun.resume[].inst_blocked` -> that item skips the institutional route (`NeedsUser(EngineRestarted)`);
   a non-empty `resume` sets `RunReport.engine_restarts = 1`. No journal replay in v1: an engine trap fails the run.
8. `retain`: `Page` for landing pages that translators/extraction need, `Pdf` for PDF fetches, else `None`.
9. `more = true`: poll budget (256 polls per step) exhausted; JS calls `fx_step` again soon with empty input.
10. `Stats.mem_hint_kb` is left 0 by the engine; the wasm crate may fill it.

## 5. Scheduler (sched/)

One generic `Limiter` (weighted capacity + min gap + optional token bucket + FIFO waiters with `Prio`) behind
`GateKey`: `Items`, `Api(Source)`, `OaHost`, `InstHost(family)`, `InstGlobal`, `Browser`, `Save`, `Bytes`. All
numbers live in `sched/policy.rs`. Highlights: items 4 (1-6); OA hosts 1 in flight, 1 s gap, skipped after 5
consecutive failures; institutional 1 per publisher **family**, 2 global, >= 2 s after a page, >= 5 s + 0-3 s
jitter after a PDF, **host and global granted together or not at all** (no deadlock, no head-of-line blocking);
hidden browsers 2; save 1; retained bodies 192 MiB; the licence ceilings 30/h, 80/24 h, 300 MB/h and 20
institutional items per run are hard constants that prefs may only lower.

*Publisher families* (`InstKey`): hosts and DOI registrant prefixes of a known publisher map to one key
(`wiley`, `elsevier`, ...). The entry hop (doi.org / redirector / EZproxy) is gated by `InstKey::for_doi`, later
page and PDF fetches by `InstKey::for_host`; both land on the same key, which fixes the judges' "real host only
known after redirects" flaw.

*Licence ledger* (reserve-then-commit, persisted): `reserve_licence` counts against all caps immediately and pushes
`Persist { Ledger, flush }` before the barrier fetch; `commit(bytes)` on a verified PDF; `rollback()` on a non-PDF
or before sending; a dropped ticket that was marked sent **commits** its estimate (never under-count); on load,
stale reservations become committed entries. A cap hit is `Refused::Cap` -> `Outcome::Deferred { cap }`.

*Halts*: 429, EZproxy suspend page, licence-stop text, 3 block signals, challenge loop -> `halt_inst` for
`max(Retry-After, 30 min)` (24 h for suspend), persisted via `Persist { Halts }`, reported in `RunReport.halted`.

*Timeouts* are chosen by the engine (`policy::timeout_ms`, with the slow-host profile for Wiley-class hosts:
landing 60 s, PDF 120 s, later hedge) and enforced twice: virtual-time `Timeout` in Rust, a slightly longer JS
backstop.

*Host knowledge* (`HostBook`): sticky Plain UA per host (cf_clearance is UA-bound), challenge counts (2 -> host
walled), EZproxy "host configured" cache (7 days) and Alma holdings cache (1 day), both persisted.

## 6. Login gate (login.rs)

Single-flight per realm (`Openathens`, `Ezproxy`, `Saml`) with epochs; states `SignedOut / Probing / LoginOpen / SignedIn / Refused / Halted`
(diagram in the file header). Twenty items needing sign-in produce **one** `LoginWindow` effect: the first
`session_login` call spawns a `System` window-driver task, later callers only increment `waiting` and trigger
`LoginUpdate`. `session_admit` makes the first item probe alone so N parallel hidden loads do not all bounce.
After a sign-in the epoch bumps and waiters retry silently once (`Retry { epoch }`); a bounce on the first
resumed attempt means `Ineffective` (no more prompts this run); `Declined` is per run; Entra Conditional Access
(`ca_error` / `CaBlocked`) is `Refused { code }`; max 2 prompts per realm per run. Interactive gate: capacity 1,
login before challenge. Parked items **release their item slot** and re-acquire with `Prio::Resumed`
(`pipeline::parked`), fixing the judges' batch-stall flaw. The driver owns the engine-side login timeout
(`login_timeout_sec`; `UserAction::KeepWaiting` re-arms it) and closes the window when no waiter is left.

## 7. Run manager (run.rs)

`StartRun` snapshots config, claims items (duplicate in another live run -> `Skipped(InProgressElsewhere)`),
emits `Queued` rows, spawns one task per item, and (interactive, Full mode, inst enabled, more than `confirm_above`
candidates) emits one `Confirm { LargeBatch }` before any item starts. Items enter the institutional phase through
`claim_inst_slot` (mode, halt, batch cap 20 -> `Deferred { Batch }`, group-library policy with one `Confirm
{ GroupLibrary }` per run, resume hints). `CancelRun/CancelItem/Shutdown` drop tasks first (cancels and releases
come out in the same step), then record `Cancelled` outcomes; saving items finish. Finished tasks arrive through
`State::done`; `collect_done` records the outcome, emits the terminal row (`l10n::outcome_row`), `finish_runs`
emits exactly one `RunFinished` with counts, ledger usage, halted reason. `DumpState` -> one `Log` effect.

## 8. Pipeline (pipeline/)

`item_runner(cx)` = `wait_admitted` -> item slot -> `run_item` -> `cx.finish(outcome)`. `run_item` (item.rs header has
the full outcome map): precheck (skip rules) -> ids (`pure::normalise_ids`) -> `discover_oa` (parallel
`FuturesUnordered` of OpenAlex / Europe PMC / bioRxiv / Zotero OA / optional Unpaywall, S2, CORE; PMC S3 chained
once a PMCID is known; settle 1.5 s after a strong candidate; 12 s budget; stragglers cancelled by drop) ->
`attempt_oa` -> institutional route -> `save_pdf_blob` -> `Outcome`.

*Fetch tiers* (attempt.rs): HTTP (sticky UA, host gate, slow-host timeouts) -> classify the attached `Inspection`
-> one rewrite for reader shells -> hidden browser with PDF capture on `Challenge` -> visible challenge window only
when allowed (never solving CAPTCHAs) -> next candidate. **Hedging**: OA only, second attempt after 3 s (8 s on
slow hosts) on a different host key, at most 2 in flight, loser dropped; institutional attempts never hedge.
Candidate URLs dedupe by core's `canonical_key` (query included); a page already visited is never re-extracted.

*Landing* (landing.rs): Rust candidates (from `Inspection::Page`) merged with Zotero translators (`RunTranslators`,
8 s); on institutional pages translators run only when Rust found nothing and count against the fetch budget.

*Institutional route* (inst.rs header has the steps): configuration check -> Alma entitlement -> direct (`doi.org`) ->
OpenAthens redirector in a hidden browser (plain UA, `barrier`) -> publisher SAML / Shibboleth (WAYFless link) ->
login gate -> extract -> PDF -> EZproxy -> hand-off links on every failure. Limits: 4 fetches per item, no retries
except the single post-login retry, stop signals halt the route.

*Route order and the new institutions* (SAML / EZproxy-for-every-host work). Direct/campus -> OpenAthens (`use_openathens`
and a non-blank `openathens_domain`; a blank domain never builds a redirector URL: `pure::redirector_url` returns
`None`) -> **SAML WAYFless** (`use_saml`, a well-formed `Institution.saml_entity_id`) -> EZproxy -> hand-off.
* The SAML entry is `pure::wayfless_url`: the first page the item reached, else its own URL, else the article its DOI
  stands for (`fulltext_core::wayfless`, core.md section 3.4); an unsupported publisher skips the step with
  `Miss::NotKnown`. It runs only while the best explanation so far is "not known" or weaker: if OpenAthens reached the
  publisher and was refused, found no PDF link or met a bot check, the same identity provider would say the same.
* Same machinery as OpenAthens: a silent hidden load (plain UA, `barrier`, `hold_while [PublisherSsoHop]` so the
  publisher's auto-posting SSO start page is not mistaken for a settled page), the single-flight session gate, a visible
  window (`LoginReq.realm = saml`, entry = the WAYFless link) only when the load ends on an interactive page (`IdpLogin`,
  which core now classifies for any Shibboleth / SAML IdP). `Realm::Saml` is a realm of its own (own epoch, own
  prompts-per-run budget); windows of different realms are still one at a time (the interactive gate is global).
  Saves record `source = "saml"`.
* `Institution.ezproxy_all_hosts`: EZproxy is tried for any publisher host (a DOI with no known host starts from
  `doi.org`). Order of knowledge for a host: the persisted cache (7 days; what the proxy said last) -> `ezproxy_hosts` ->
  the no-follow probe of the proxified host; an unanswered probe means "try" instead of "skip". A host is learnt
  `false` from the probe, from the `EzproxyHostNotConfigured` page and from the proxy's own needhost page
  (`EzproxyError { suspend: false }` on the EZproxy realm), and learnt `true` when a page came back through the proxy.
* A configuration with no usable route (no direct access, and no switched-on route with its settings: OpenAthens
  domain, SAML entityID, EZproxy login host and suffix) ends the item as `NeedsUser { Misconfigured }` before any
  request (row `zotero-fulltext-status-misconfigured`).
* Hand-off: `Handoff.wayfless_url` (row action `zotero-fulltext-action-open-wayfless`, offered after the redirector link).

*Save* (save.rs): `validate(&PdfVerdict)`, trim prefix, `mark_saving` (no longer cancellable), save slot,
`SavePdf` with the un-proxied URL from `pure::storable_url`; `AlreadyHasPdf` -> `Skipped(PdfAddedMeanwhile)`.

## 9. Failure catalogue -> acceptance matrix

Each row of C's catalogue is a sim scenario (`sim/scenarios/*.rs`, name `fNN_*`) or a unit test; the invariants
in `sim/invariants.rs` run in every scenario. Owners: P pipeline, I inst, S sched-login, R run-sim, X executor-ops.

| F | scenario | owner | F | scenario | owner |
|---|---|---|---|---|---|
| F1 login mid-batch: ONE window, others join, OA continues | f01 | S+I | F17 shutdown/disable mid-batch | f17 | R |
| F2 session expiry: re-prompt <= 2/run, 2nd bounce hands off | f02 | S+I | F18 item has PDF / not downloaded / PDF added late | f18 | P+R |
| F3 Conditional Access refusal -> Refused{code} | f03 | S | F19 no DOI: PMID/arXiv ids, else NoIdentifier | f19 | P |
| F4 OpenAthens "Please wait" poll | f04 | I | F20 group library: Ask -> one Confirm, Never -> hand-off | f20 | R+I |
| F5 EZproxy not-configured learned / suspend halts | f05 | I+S | F21 duplicate concurrent runs | f21 | R |
| F6 challenge -> browser tier; 2 challenges wall host | f06 | P | F22 user cancel: Cancel for every op, same step | f22 | R+X |
| F7 hard block: wall host, next candidate | f07 | P | F23 offline: pause, probe, Failed(offline) | f23 | R+P |
| F8 PDF URL returns HTML landing/denied | f08 | P | F24 login window ignored: TimedOut, KeepWaiting | f24 | S |
| F9 reader shell: one rewrite | f09 | P | F25 login ineffective: no more prompts | f25 | S+I |
| F10 slow host: 60/120 s profile, later hedge | f10 | P+S | F26 licence budget exhausted: Deferred{cap}, reset time | f26 | S+I |
| F11 429: OA retry once <= 10 s; inst halts route | f11 | P+I | F27 parent gone / read-only / disk full at save | f27 | P+R |
| F12 partial download: one retry if budget allows | f12 | P | F28 translator hang: dropped after 8 s | f28 | P |
| F13 non-PDF bytes on PDF URL: re-classify | f13 | P | F29 (JS-side MIME handler; engine: no effect) | - | - |
| F14 wrong article: other-DOI/supplement rejected | f14 | P | F30 redirect/refresh loops: tried-set, max 3 | f30 | P |
| F15 engine trap: run fails cleanly (wasm area) | - | - | F31 stored URL un-proxied/unsigned | unit | P |
| F16 analyser fault (wasm area) | - | - | F32 sticky Plain UA per host | f32 | P+S |

Scheduler-only scenarios (R): one inst request per family, global 2, PDF spacing, ledger never above caps
including after an engine restart with a persisted ledger, reserve-then-commit ordering (Persist before barrier),
cancelled-after-sent ticket still counted, item slots released while parked, no stall under random completion
orders (seeded property tests over 20-item batches).

## 10. Work packages (parallel; disjoint files; specs in docs/wp/)

| id | owns | depends on |
|---|---|---|
| engine-executor-ops | Cargo.toml, lib/error/proto/cx/executor/ops/engine, sim/mod, sim/fixtures | - |
| engine-sched-login | sched/**, login.rs | - |
| engine-pipeline | pipeline/{mod,item,precheck,pool,requests,oa,attempt,landing,save,pure}.rs, l10n.rs | executor-ops, sched-login |
| engine-inst | pipeline/{inst,handoff}.rs | executor-ops, sched-login |
| engine-run-sim | run.rs, sim/invariants.rs, sim/scenarios/** | all four above |

engine-sched-login needs no executor: its futures are tested with a manual-poll helper. engine-run-sim's `run.rs`
can start immediately (it only needs the skeleton); its scenarios need everything. The last package to land
removes the crate-level `#![allow(dead_code, unused_imports)]` in lib.rs and fixes what clippy then reports.

## 11. Risks and open items

* The executor/reactor are the largest bug surface (lost wakes are silent): mitigations are the borrow rules, the
  stall detector, `DumpState`, and the invariants run in every sim scenario.
* `RefCell` re-entry = `EngineError(reentered)`, never a trap; but a logic panic still traps (panic = abort). Lints
  deny unwrap/expect/panic/indexing in non-test code; `clippy::await_holding_refcell_ref` is warn-by-default and must
  stay clean.
* Engine trap = run failure (no replay); the ledger stays safe because reserve is persisted before the fetch.
* Hedging and translators issue requests outside a strict per-host sequence; both are limited (OA only / counted).
* Licence numbers are research-derived, not KCL-confirmed; prefs can only lower them.
* Main-thread parse cost is a core/host concern (pure instance); the engine never sees raw bodies.
