# WP engine-run-sim: run manager, invariants, failure-catalogue scenarios

Owner of: `crates/fulltext-engine/src/run.rs`, `src/sim/invariants.rs`, `src/sim/scenarios/**`,
`crates/fulltext-engine/tests/golden/**`, and (last step) the crate-level `#![allow(dead_code, unused_imports)]` in
`lib.rs` (the only line of lib.rs you may touch). Everything else is read-only for you.
Read first: `docs/design/engine.md` sections 7, 9; headers of `run.rs`, `sim/mod.rs`, `sim/invariants.rs`,
`sim/scenarios/mod.rs`; `docs/design/candidates/C-failure-first.md` section 1 (the catalogue) and A section 6 item 2
(`docs/design/candidates/A-rust-async-engine.md`, the sim invariants).
Depends on: `run.rs` needs only the skeleton plus engine-executor-ops' `Executor` to test; the scenarios need all of
engine-executor-ops, engine-sched-login, engine-pipeline and engine-inst. Do `run.rs` and `invariants.rs` first.
Env: `CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\engine-run-sim`.

## 1. run.rs (contract in the file header; signatures FROZEN)
* `start_run`: `State::cfg` must exist (else `EngineError(not_configured)`); `Rc<EngineConfig>` snapshot into the
  run; per item: duplicate in another live run (`claimed`) -> immediate `ItemDone { Skipped(InProgressElsewhere) }`;
  else `ItemRec { phase: Queued }`, row `QUEUED`, `ItemCx::new(..)` and `exec.spawn(Owner::Item, runner(cx))`; when
  the table is full the item stays `Queued` and is spawned by `collect_done` as slots free. Admission: interactive
  Full-mode runs with `inst.enabled` and more than `confirm_above` items -> spawn an `Owner::Run` task that emits
  `Confirm { LargeBatch, n, cap }`; rows show `WAITING_CONFIRM`; `Yes` -> `Go { inst_allowed: true }` with quota
  `min(batch_max, CEIL_BATCH_MAX)`; `OaOnly` -> `Go { inst_allowed: false }`; `Cancel`/error -> `cancel_run`.
  Otherwise admission is immediate (`inst_allowed = mode == Full && inst.enabled`). Wake `waiters` when decided.
* `claim_inst_slot` (on `ItemCx`): not allowed -> `NotAllowed`; `resume.inst_blocked` -> `ResumeBlocked`; quota 0 ->
  `BatchCap`; `cx.inst_halted()` -> `Halted`; group library: policy `Never` -> `GroupDenied`; `Ask` + interactive ->
  one `Confirm { GroupLibrary }` per run (`group_consent`), answer cached in `RunState::group_consent`, non-interactive
  `Ask` -> `GroupDenied`; else consume one quota unit -> `Granted`. Implement `WaitAdmitted` and `GroupConsent` futures
  with stored wakers (never hold the borrow across a poll).
* `cancel_run`/`cancel_item`: `exec.cancel_where` FIRST (outside `with`), then for each dropped `Owner::Item` record
  `Outcome::Cancelled { by }`, terminal row `CANCELLED`, release the claim; items in `ItemPhase::Saving` are skipped;
  `login.on_run_gone(run)`; cancelling a run also drops its `Owner::Run` helper tasks. Unknown run/item: `Log` + ignore.
* `user_action`: forward to `login::on_user_action`; `SkipSignIn`/`RetrySignIn` additionally clear `SignIn` actions.
* `collect_done`: drain `State::done`, record outcome + trace into `ItemRec`, mark `Done`, emit
  `Progress { row: l10n::outcome_row(&outcome) }`, free the claim, spawn queued items while the executor has room.
* `finish_runs`: all items `Done` and no live helper task -> one `RunFinished` (counts from outcomes: Saved ->
  `saved`; Skipped -> `skipped`; NotFound/NoIdentifier -> `not_found`; NeedsUser/NotEntitled -> `needs_user`;
  Blocked -> `blocked`; Deferred -> `deferred`; Cancelled -> `cancelled`; Failed -> `failed`; Halted -> `needs_user`),
  `items` in snapshot order, `ledger: sched.usage(wall)`, `halted`, `engine_restarts`); then forget the run.
* `shutdown`: `cancel_run(.., Shutdown)` for all, `login::on_shutdown`, push `sched.persist_all(true)`, finish runs.
* `dump_state`: one `Log { Info }` with JSON of tasks/owners, waiting ops, timers, limiter in_use/waiting, login
  states, run table (item phases). Deterministic key order.
* Unit tests with `Engine::with_runner(test_runner)` (a runner that sleeps / awaits an op / returns a fixed
  outcome): counts and report order, duplicate claim, cancel emits `Cancel` in the same step, saving item survives
  `CancelRun`, shutdown flushes `Persist`, queued items spawn when the table has room, confirm flows (Yes / OaOnly /
  Cancel), resume hints, non-interactive large batch, exactly one `RunFinished`.

## 2. sim/invariants.rs
Implement every invariant listed in the file header as a struct with `Invariant`; `standard()` returns them all.
They run in EVERY scenario (the scenario helper `run_scenario` builds `SimHost::new(responder).with_invariants(
standard())`). Failure message: invariant name, the violating step time, the last 30 effects.

## 3. sim/scenarios/*.rs
One `#[test]` per catalogue row marked for you in `docs/design/engine.md` section 9 (names `fNN_*`), plus the
scheduler scenarios listed there and the property tests (`property.rs`): seeded (use `Rng`), 20-item batches with
random completion latencies (0-30 s) and random reply kinds (ok/timeout/network/challenge/429); assertions: no panic,
no invariant violation, run finishes, `stalled == false`, `lost_wakes == 0`, same seed => identical effect log,
outcome set stable across latency permutations for deterministic replies. Golden traces: for 3 canonical scenarios
(OA happy path, 20-item login batch, 429 halt) store the effect log as JSON in `tests/golden/*.json` and assert
equality (regenerate with an env var `ZFT_UPDATE_GOLDEN=1`, never silently).
Use `ScriptResponder` + the fixture builders; canned `Inspection` values (parsing is core's job).

## 4. Last step: make the crate clean
Remove `#![allow(dead_code, unused_imports, ...)]` from lib.rs. Delete genuinely unused code, or give it a test or a
caller; keep a narrowly scoped `#[allow(dead_code)]` with a reason only where an item is part of a documented
frozen API that no caller needs yet. Fix whatever clippy then reports.

## Acceptance
```
cargo test -p fulltext-engine
cargo clippy --workspace --all-targets -- -D warnings
cargo check -p fulltext-engine --target wasm32-unknown-unknown
cargo test --workspace
```
