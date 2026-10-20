# WP engine-executor-ops: executor, reactor/ops, Engine::step, SimHost

Owner of: `crates/fulltext-engine/{Cargo.toml, src/lib.rs, src/error.rs, src/proto.rs, src/cx.rs, src/executor.rs,
src/ops.rs, src/engine.rs, src/sim/mod.rs, src/sim/fixtures.rs}`. Touch nothing else. proto.rs stays a verbatim copy
of `docs/design/contracts/proto.rs` (additive changes only, documented in docs/design/engine.md).
Read first: `docs/design/engine.md` sections 3-4, the headers of `cx.rs`, `executor.rs`, `ops.rs`, `engine.rs`,
`sim/mod.rs`. Env: `CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\engine-executor-ops`.

This package is on the critical path of every other one. Land in this order and push after each step:
(1) executor (2) ops hardening (3) `Engine::step` (4) `SimHost` + `ScriptResponder`.

## 1. executor.rs (skeleton has types, `ReadyQueue`, `TaskWaker`; implement the methods)
* `spawn(owner, fut)`: take a slot from `free` or push a new one (`ExecFull` when `slots.len() >= max_tasks`);
  `TaskKey { index, epoch: epochs[index] }`; build `Arc<TaskWaker { key, queued: false, ready }>`; store the task;
  schedule the first poll by `waker.wake_by_ref()`.
* `run(budget)`: loop while `polls < budget`: if `ready.take_sweep()` wake every live task; `pop` a key; ignore it
  if the slot is empty or `task.epoch != key.epoch`; set `queued = false` BEFORE polling (a wake during the poll
  re-queues); take `fut` out of the slot, poll with `Waker::from(Arc::clone(&task.waker))`; `Ready` -> drop the
  future and `retire(index)` (bump `epochs[index]`, push to `free`); `Pending` -> put `fut` back. Return
  `RunStats { polls, more: ready.len() > 0 }`.
* `cancel_where(pred)`: collect matching indexes, for each `retire` (drop the future; its leaf `Drop`s push `Cancel`
  / `Release` into the outbox), return the owners. Never called inside `Shared::with`.
* `wake_all()` sets the sweep flag; the sweep wakes every live task via its waker.
* Tests (`#[cfg(test)]` in executor.rs): FIFO order; a wake storm dedupes to one poll; stale `TaskKey` after slot
  reuse ignored; budget exhaustion sets `more`; a task that wakes itself during poll re-queues exactly once;
  `cancel_where` drops futures (use a drop-flag future) and returns owners; spawn past `max_tasks` is `ExecFull`;
  finished task frees its slot and the next spawn reuses it with a new epoch; `FuturesUnordered` inside a task
  works (children completing out of order wake the parent); sweep wakes parked tasks. No `unsafe`, no
  thread-locals, no `unwrap`.

## 2. ops.rs (a working draft is in the skeleton; verify and harden, do not redesign)
Tests (use a tiny local executor or `Engine::spawn_test` once step works; manual `Waker` counters are fine):
first poll emits exactly one effect with a fresh monotonic `OpId`; constructing without polling emits nothing;
completion -> next poll returns `Ok(value)`, `err` -> `Err(OpErr)`, neither -> `Ok(Null)`; a completion for an
unknown/duplicate op returns `None` and lands in `take_ignored`; dropping a waiting `OpFuture` pushes exactly one
`Cancel` and a late completion is ignored; dropping after completion pushes nothing; `Body` drop pushes `Release`
(also when the owning task is cancelled); `Sleep` fires at `deadline <= now`, drop removes the timer,
`due_timers` returns earliest-first, `next_deadline` reflects removals; `Timeout` lets a ready inner future win
ties and drops the loser (an op inside it produces `Cancel`); typed wrappers: a malformed payload becomes
`OpErrKind::Internal`, never a panic; `alloc_id` never collides with `submit` ids.

## 3. engine.rs: `Engine::step` (follow the 9 phases in the file header exactly)
* Configure: `protocol != PROTOCOL` -> `EngineError(protocol_mismatch)`. Else store `Rc<EngineConfig>` in
  `state.cfg`, `sched.configure(&cfg, persisted, seed, wall)`, `configured = true`.
* Any `StartRun` before a Configure -> `EngineError(not_configured)`. Unknown run/item in
  CancelRun/CancelItem/UserAction is NOT an error (the run manager logs and ignores).
* Dispatch to `run::{start_run, cancel_run, cancel_item, user_action, dump_state, shutdown}` building a
  `Ctl { shared, exec }`. `run.rs` is implemented by engine-run-sim; until it lands those are skeleton no-ops.
* Completions: `reactor.complete(msg)` for each inside ONE `with`, collect the wakers, wake them after the closure.
* Timers: `due_timers(now)`, wake after the closure.
* Poll rounds: total budget `policy::POLL_BUDGET` per step. Loop { `exec.run(remaining)`; drain `state.spawns`
  into the executor (`ExecFull` -> drop the future, push a `Log`); `run::collect_done(&mut ctl, runner)`; stop when
  nothing is ready or the budget is spent }. `more = ready_len() > 0` after the loop.
* `run::finish_runs`.
* Stall check (only when `!more`): `tasks > 0 && ops_inflight == 0 && timers == 0 && waiting_user == 0 &&
  ready_len == 0` -> `wake_all()`, one more round; progress (outbox grew, live tasks dropped, or ops/timers
  appeared) => `lost_wakes += 1` (cumulative counter on `Engine`), else `stalled = true` in this step's `Stats`.
* `shared.reentries() > 0` -> `EngineError(reentered)`.
* Output: `effects = take(outbox)`, `next_timer = reactor.next_deadline()`, `ignored = reactor.take_ignored()`,
  `Stats { tasks, ops_inflight, timers, waiting_user, polls (this step), lost_wakes (cumulative), stalled,
  mem_hint_kb: 0 }`.
* Tests via `Engine::spawn_test` + `step`: protocol mismatch; not_configured; a task awaiting `cx.sleep(100)`
  yields `next_timer == now+100` then finishes when stepped at that time; an op round trip through
  `StepIn.completions`; `ignored` reporting; budget -> `more` with a task that yields 1000 times; stall detection
  with a leaf future that never registers a wake (stalled after the sweep) and lost-wake counting with a leaf
  that only wakes on a sweep; nested `Shared::with` returns `None` and `step` reports `reentered`; determinism
  (same inputs twice => identical `StepOut` JSON); `Effect` order preserved (Persist before Http).

## 4. sim/mod.rs + sim/fixtures.rs
Implement `SimHost::step` / `run_until_idle` per the module docs: completions due now are sorted by
`(due, op)`; every effect with an op is registered in `pending` and the `Responder` is asked once
(`Reply::Never` => stays pending); `Effect::Cancel` removes the op from `pending` and `scheduled`;
`LoginUpdate` is answered immediately with `ok: {}` (this is what JS does; the engine reports it in `ignored`);
`Persist` fills `persisted`; `RunFinished` fills `reports`; invariants run after each step (failure =>
`Err(EngineError { code: "invariant", message: name + detail + the last 30 logged effects })`).
`run_until_idle(max_ms)` jumps the clock to `min(next scheduled reply, StepOut.next_timer)`, steps with empty
input while `more`, returns `SimEnd::Idle` when nothing is scheduled (pending `Never` ops stay in `pending` for
the scenario to assert on) or `TimeLimit`. Add to fixtures the builders other packages will need, same style as
the existing ones: `browser_ok`, `login_ok`, `translators_ok`, `api_found(source, candidates)`,
`page_inspection(class, candidates)`, `body(handle, len)`, `candidate(url, kind, source, tier)`. `ScriptResponder`,
`url_has`, `is_type`, `effect_url` already exist (keep their signatures).
Tests: `SimHost` smoke tests for virtual-time jumps, cancel removing pending, LoginUpdate auto-answer,
deterministic logs.

## Acceptance (all must pass)
```
cargo test -p fulltext-engine executor:: ops:: engine:: sim::
cargo clippy -p fulltext-engine --all-targets -- -D warnings
cargo check -p fulltext-engine --target wasm32-unknown-unknown
cargo test --workspace
```
Do not remove the crate-level `#![allow(dead_code, unused_imports)]` (engine-run-sim does at the end).
