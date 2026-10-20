# WP engine-sched-login: scheduler, licence ledger, host knowledge, login gate

Owner of: `crates/fulltext-engine/src/sched/{mod,gate,ledger,hosts,policy,acquire}.rs` and `src/login.rs`. Touch
nothing else. Read first: `docs/design/engine.md` sections 5-6, the headers of every file you own,
`research/04-kcl-institutional-access.md` section 8.2 (licence), `docs/design/candidates/C-failure-first.md`
sections 3.2 and 4.3. Env: `CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\engine-sched-login`.

You need NO executor: test the futures with a manual-poll helper (a `Waker` built from an `Arc` with a wake counter,
`futures_util::task::{ArcWake, waker}`), a `Shared::new()`, and a `Cx::new(shared, Owner::System)`. Complete ops by
hand with `shared.with(|s| s.reactor.complete(msg))` and read effects from `state.outbox`.
The pipeline-facing signatures in `sched/acquire.rs` and `login.rs` are FROZEN for the other packages: you may
add items, never change or remove an existing signature (if you must, say so in the docs and keep a shim).

## 1. sched/gate.rs: `Limiter`
Implement `poll`, `release`, `forget` (and add pure `check`/`take` if you need a dry run for the institutional
all-or-nothing grant). Algorithm:
* waiters sorted by `(Prio, ticket)`; only the head may be granted (strict FIFO per limiter).
* `check(ticket, now, weight)`: not head -> `Wait { retry_at: None }`; capacity 0 -> never; weight fits
  (`in_use == 0 || in_use + weight <= capacity`) else `Wait { None }`; `now < next_at` -> `Wait { Some(next_at) }`;
  token bucket (refill `elapsed / refill_ms` tokens up to `burst`, fractional tokens kept) with `< 1` token ->
  `Wait { Some(time when 1 token exists) }`; else `Granted`.
* `take`: remove the waiter, `in_use += weight`, `tokens -= 1`.
* `release(weight, now, gap_ms)`: `in_use -= weight` (saturating), `next_at = max(next_at, now + gap_ms)`, return
  the head waiter's waker. `forget(ticket)`: remove; if it was the head return the new head's waker.
* Tests: FIFO + priority (`Resumed` before `New`), gap, token refill (burst then spaced), weighted capacity
  incl. oversize-when-idle, forget passes the baton, release wakes exactly the head, no grant to a non-head.

## 2. sched/ledger.rs
Implement per the header: windows (`wall - 1 h`, `wall - 24 h`), `reserve` (count reservations in all three caps;
`CapHit { kind, reset_at }` where `reset_at` is the wall time the binding window frees enough room; MB means
10^6 bytes; a cap of 0 is an immediate hit with `reset_at = wall + 24 h`), `commit` (actual bytes), `rollback`,
`load` (prune > 24 h; reservations older than `RESERVATION_STALE_MS` become committed at their `wall_ms`),
`usage` (hour/day counts, `mb_hour` rounded up). Tests: caps at 30/80/300 with clamping of larger prefs; reserve
fails closed at the cap and succeeds again after the window passes; rollback frees; commit with a larger real size
can push `mb_hour` over (and then blocks the next reserve); persisted state round trip through serde; stale
reservation conversion; many interleavings (seeded loop) never exceed caps.

## 3. sched/hosts.rs, policy.rs, mod.rs
* `HostBook::note_outcome` (consecutive failures; `OA_HOST_FAIL_LIMIT` => `wall(host, "failures")`),
  `note_challenge` (2 => wall), `Halts::halt_inst` (until = wall + max(retry_after_ms, `HALT_MIN_MS`), or
  `SUSPEND_COOLDOWN_MS` for `EzproxySuspended`; never shortens; returns changed), `block_host`.
* `Sched::spec_for(key)`: `Items` capacity = `item_concurrency`; `Api(src)` from `policy::api_limits` (a disabled API
  after a 429: `HostSkipped`); `OaHost` 1 / `OA_HOST_GAP_MS`; `InstHost` 1; `InstGlobal` 2; `Browser`; `Save`;
  `Bytes`. Add the PDF gap config: `max(INST_PDF_GAP_MS, InstPrefs.min_gap_sec * 1000)` (prefs may only RAISE gaps).
* Tests for the family tables (add hosts/prefixes seen in `fixtures/html`), `host_of` edge cases, halts, cache TTLs.

## 4. sched/acquire.rs
Implement `Acquire` / `AcquireInst` / permits / `LedgerTicket` per the header contract:
* first poll `enqueue`s (Items/Resumed priority from the arg), later polls `check`; on `Wait { Some(t) }` arm
  `Reactor::add_timer(t, waker)` (re-arm on every poll via `park_timer`, remove on drop); on `Wait { None }` the
  limiter keeps the waker. Refusal checks on every poll: halted route (`InstHalted`, for inst and for `Refused`
  mapping), `HostSkipped` (OA host skipped/walled, disabled API).
* `AcquireInst` grants the family gate AND `InstGlobal` together (dry-run both heads, then take both). Release stamps
  the host gap by `FetchKind` (page `INST_PAGE_GAP_MS`; PDF = pdf gap + `rng.jitter(INST_PDF_JITTER_MS)`), global gap 0.
* `Permit`/`InstPermit` `Drop` release and wake outside nothing-held borrows; `Acquire`/`AcquireInst` `Drop` call
  `forget` and remove their timer. No permit is ever stored in `State`.
* `reserve_licence`, `halt_inst`, host/UA/cache helpers already delegate; add tests that the `Persist` effect is
  pushed before returning (barrier ordering), a sent-but-unresolved ticket commits on drop, an unsent one rolls back.
* Tests: 3 tasks on a cap-1 host proceed one at a time with the gap; two hosts + global cap 2 never run 3; dropping a
  waiting acquire passes the baton; halted route refuses waiting `AcquireInst` after the halt starts; token-bucket
  API limiter spaces requests; `Resumed` priority beats `New`.

## 5. login.rs
Implement the state machine in the file header:
* `session_admit`: `SignedIn` -> `Proceed { epoch }`; `SignedOut` -> `Probing` and `Proceed`; `Probing`/`LoginOpen`
  by someone else -> park (store the waker in `RealmState::waiters`) and re-evaluate on wake; blocked run
  (`blocked_runs`) / `Refused` / `Halted` -> `Blocked(verdict)` immediately.
* `session_report(realm, epoch, result)`: `LoggedIn` -> `SignedIn { verified: true }` (epoch unchanged), wake all;
  `NeedsLogin` with `verified == false` after a sign-in (first resumed attempt bounced) -> `Halted(Ineffective)`
  for the caller's run; `NeedsLogin` otherwise leaves the caller to call `session_login`; `Misconfigured`,
  `Suspended` -> `Halted`; `Failed` releases a probe lock.
* `session_login`: if `seen_epoch < current epoch` -> `Retry { epoch }` at once; if the run is blocked or has used
  `MAX_PROMPTS_PER_REALM` prompts -> `Declined`/`Ineffective` verdict; if `LoginOpen` -> `waiting += 1`, push
  `LoginUpdate` (fresh id from `Reactor::alloc_id`), park; else open: state `LoginOpen`, `prompts[run] += 1`, push a
  `SpawnReq { owner: System, fut: window_driver(..) }`, park. A dropped (cancelled) `SessionLogin` decrements
  `waiting` and pushes `LoginUpdate`.
* `window_driver`: wait for the interactive gate (login before challenge); when `auto_open_login` is false wait for
  `UserAction::SignInNow` first (rows get `RowAction::SignIn`); `cx.with_ids(first_waiter_run, item)` +
  `login_window(LoginReq { kind: Login, realm, entry_url, item_title, waiting, timeout_ms, capture_pdf })` raced
  against the engine-side timer `login_timeout_sec` (`KeepWaiting` re-arms; drop the op => `Cancel` closes the
  window); when `waiting` reaches 0 drop the window op; map `LoginResult`: `Landed|Pdf` -> epoch+1,
  `SignedIn { verified: false }`, waiters get `Retry { epoch }`; `ClosedByUser|Handoff` -> run-scoped `Declined`;
  `Timeout` (or engine timer) -> `TimedOut`; `CaBlocked` or `ca_error` -> `Refused { code }`; an op error ->
  `TimedOut`. Always release the interactive gate; wake all.
* `on_user_action`, `on_shutdown`, `LoginGate::on_run_gone`, `waiting_user` (counts items waiting for a probe,
  window or `SignInNow`).
* `challenge_window`: queue on the interactive gate (below login), emit `LoginWindow { kind: Challenge }`, return
  the `LoginResp`; `Declined` -> `OpErr { kind: Cancelled }`.
* Tests: 20 `session_login` callers -> exactly ONE `LoginWindow` effect and `LoginUpdate`s with growing `waiting`;
  `Landed` -> all 20 get `Retry { epoch: 1 }` and a second sign-in is not prompted; decline -> same-run later call
  `Declined` with no new window, a NEW run prompts again; timeout; KeepWaiting re-arm; Conditional Access ->
  `Refused { code }` for all runs; Ineffective path; prompt cap; all waiters cancelled -> window op dropped
  (`Cancel`); challenge waits behind a login window; epoch silent retry; `RetrySignIn` clears the block.

## Acceptance
```
cargo test -p fulltext-engine sched:: login::
cargo clippy -p fulltext-engine --all-targets -- -D warnings
cargo check -p fulltext-engine --target wasm32-unknown-unknown
```
