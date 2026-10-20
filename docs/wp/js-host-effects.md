# WP js-host-effects: wasm host, body store, effect runner

Read first: `docs/design/plugin-js.md` sections 1, 3, 5, 6, 9, 10, 12; `crates/fulltext-wasm/src/lib.rs` (ABI table);
`docs/design/contracts/proto.rs` (Effect, StepIn/StepOut, CompletionMsg, OpErr); the header comment of each file you own.
Reference prototype: `spikes/wasm-spike/js/fulltext-core.js` (poison/reset pattern; its ABI differs: here the CALLEE frees inputs).

## Files you own
`plugin/content/{wasm-host,inspect-worker,bodies,effects}.js`, tests `{wasm-host,bodies,effects,effects-failure}.test.mjs`.
Other modules (net, browser, login, save, ...) are replaced by fakes in your tests; you only call their declared signatures.

## 1. wasm-host.js
Implement `WasmInstance`, `Host`, `load`, `get`, `shutdown` as in the file header.
* **Marshalling** (all copies, views re-created AFTER every alloc/call because memory may grow): `callJson(name, obj)`: encode JSON, `fx_alloc`, copy,
  call `x[name](ptr, len)` -> BigInt `packed`; `ptr = Number(packed >> 32n)`, `len = Number(packed & 0xffffffffn)`; decode text, `fx_free(ptr, len)`,
  parse `{ok}`/`{err:{code,message}}` (`err` -> throw `EngineError`). The callee frees the inputs: never free them yourself. `callMetaBody` does the same with two
  buffers (meta JSON + raw bytes); `callTexts(name, a, b)` with two UTF-8 text buffers (`fx_login_state`). Zero-length buffers: pass ptr 0, len 0 (no alloc).
* **Trap handling**: wrap every call; a `WebAssembly.RuntimeError` (or any throw from an export) sets `poisoned`, reads `lastPanic()` (`fx_last_panic` JSON
  `{"code":"panic","message":...}` or len 0; do not `fx_free` a panic buffer) and rethrows `EngineTrap(message, panic)`. A poisoned instance refuses calls until `init()`
  re-instantiates from the cached `WebAssembly.Module`. `Host.step` -> `EngineTrap` leaves the engine poisoned; `resetEngine()` re-instantiates it (and calls `fx_reset` when healthy).
* **load()**: bytes via `fetch(url).arrayBuffer()` (the `chrome://` URL; `opts.bytes`/`opts.module` for tests), `WebAssembly.compile`, check `WebAssembly.Module.imports(mod)`
  is empty and all `REQUIRED_EXPORTS` exist, `fx_abi_version() === ABI_VERSION`; error messages are specific (`wasm ABI mismatch: got N`). Two instances: `engine`, `pure`.
* **Host.inspect/loginState/urlClass**: never reject (see header). Placement: if `opts.inspectInWorker !== false` and `typeof ChromeWorker !== 'undefined'` try the worker
  (below), else main thread. Main-thread path = a promise chain (one inspection at a time) with `await ZFT.util.idle()` before each call whose body is > 64 KiB.
  `urlClass`: cache (Map, 256 entries, LRU by re-insertion); the `go.openathens.net` + `/redirector/` rule runs BEFORE the Probe; mapping table in the header.
* **Worker** (`inspect-worker.js`): protocol in its header. Main side keeps `pending: Map<id, {resolve, timer}>`; the watchdog (`INSPECT_WATCHDOG_MS`) terminates and respawns the worker,
  requeues the request ONCE, then answers `{expect:'failed', reason:'inspect_timeout'}`. `{trap}` replies -> `Inspection::Failed{reason:'pure_trap'}`; after 3 traps (or 2 respawns in a row
  without a ready message) switch permanently to the main-thread path and log once. Bytes are transferred (`postMessage(msg, [buffer])`): copy first if the caller still needs them.
  Create with `new ChromeWorker(ZFT.CONTENT_URL + 'inspect-worker.js')`; the worker itself `fetch`es the wasm URL (posting a Module to a ChromeWorker is unverified).
  If the worker cannot be created in the Node tests, `Host` accepts `opts.workerFactory` (returns an object with `postMessage/terminate/onmessage`) so you can unit test the whole protocol with an in-process fake worker that wraps a real or fake instance.
* `diagnostics()`, `dispose()` (terminate worker, drop instances, clear caches).

## 2. bodies.js
Implement `BodyStore` per the header. `deps.fs` defaults to `IOUtils`; `deps.state` defaults to `ZFT.state`; `deps.attachments` (default `Zotero.Attachments`) supplies
`createTemporaryStorageDirectory()` (returns an nsIFile-like with `.path`). Rules:
* handles start at 1 and are never reused in a session; `refs` starts at 1; `release` at 0 frees (memory: subtract from `memBytes`; file: `fs.remove(dir, {recursive:true, ignoreAbsent:true})` then
  `state.unregisterTmpDir(dir)`); releasing an unknown handle is a no-op; `releaseAll(run)` frees every body whose `run` matches (all when undefined) regardless of refcount.
* `putMemory(bytes, {run, kind})` throws `OpError('too_large')` if `bytes.length > PAGE_MAX_BYTES` or the total would exceed `MEM_CAP_TOTAL`; stores a COPY unless `opts.adopt`.
* `allocPdfSlot(run)`: `dir = (await attachments.createTemporaryStorageDirectory()).path`, `await state.registerTmpDir(dir)` BEFORE returning `{dir, path: join(dir,'file.tmp')}`.
* `putBlobAsPdf(blob, run)`: slot, `fs.write(path, new Uint8Array(await blob.arrayBuffer()))`, `putFile`.
* `stageForInspect(handle)` / `stageBytes(bytes, kind)`: page -> first `INSPECT_TEXT_MAX` bytes, `truncated = len > INSPECT_TEXT_MAX`, `head_len: null`; pdf -> if
  `len <= PDF_HEAD + PDF_TAIL` the WHOLE file with `head_len: null` (the body is complete); otherwise `head = first PDF_HEAD`, `tail = last PDF_TAIL`,
  bytes = head ++ tail, `head_len = PDF_HEAD`, `body_len = len` (use `fs.read(path, {offset, maxBytes})`). `body_len` is always the full length.
* `readText(handle, {contentType})`: charset from the header (`charset=...`, via `TextDecoder`, fallback utf-8 on unknown label), BOM stripped.

## 3. effects.js
Implement `Runner` and `syntheticReport` exactly as the header and design section 5 specify. Details to get right:
* **Pump**: `send()` appends commands; `CancelRun`/`CancelItem` also cancel the corresponding scopes before queuing. `pump()` is single-flight (a flag + `dirty`); inside: build
  `StepIn {now: util.monoNow(), wall: util.wallNow(), commands, completions}` (swap the queues), `stepOut = host.step(stepIn)` in try/catch (failure policy), dispatch effects IN ORDER
  (each dispatch is synchronous up to the first await; handlers run via `TaskSet.spawn`), record `stepOut.ignored` (debug log), keep `lastStats`, then: `more` -> `await util.idle()` and loop
  (max `MAX_STEPS_PER_PUMP`, then yield and re-schedule); else arm/clear the single timer from `next_timer` (`delay = max(0, next_timer - monoNow())`).
  Completion arrival schedules a pump through the timers dependency (`deps.timers` defaults to the globals) after `COMPLETION_BATCH_MS`.
* **Op bookkeeping**: `ops: Map<OpId, {type, run, item, scope, engineCancelled}>`. Scopes: `runScope(run)` -> `itemScope(run, item)` -> op scope. Op results are posted exactly once
  (guard); a result for an op whose scope the ENGINE cancelled is dropped. `stats().ops` is the live count; `whenQuiet()` resolves when ops, pending completions and pump are idle.
* **Barrier**: `persistQueue` = promise chain of `state.set` calls (errors logged, never break the chain). `barrier:true` effects `await persistQueue` first.
* **Handlers** per the header table. Build `RespMeta` as `{status, final_url, headers, body_len, head_len, truncated}` (omit nulls); on `net.request` failure post `err`. For `http` with
  `retain:'pdf'` the file slot is kept (BodyRef to the file) and the staged head/tail is what the inspector sees; with `retain:'none'` or any failure the slot dir is removed.
  A response with `status` outside 2xx on the file sink has `raw.errorBody`: stage THAT (kind page) and delete the slot. Inspect never throws (Host contract), but wrap anyway.
* **Failure policy**: implement `_fail(err)` once; it is idempotent. `syntheticReport(run, items, saved)` returns a model.rs `RunReport`:
  `{run, counts:{saved,skipped:0,not_found:0,needs_user,blocked:0,deferred:0,cancelled:0,failed:0}, items:[{item,key,outcome,trace:[]}], ledger:{all 0}, halted:null, engine_restarts:1}`.
  Saved outcome object = what `save_pdf` success remembered: `{kind:'saved', route, source, version, url, attachment_key}`.
* **Stall watchdog**: a timer (started with the first run, `opts.stallMs` default `STALL_MS`) checks `lastStats`: unfinished runs and `ops==0 && !more && next_timer==null && stats.waiting_user==0`
  for two consecutive checks -> send `{type:'dump_state'}` once, log the reply, and if still stalled at the next check -> `_fail(new Error('engine stalled'))`.
* **Trace**: `opts.trace` -> ring buffer of `TRACE_RING` `{run,item,ev}`; also log `Debug`-level `Log` effects only when trace is on.

## Tests (scripted fake host + fake services; use `makeFakeTimers`, `makeFakeFs`)
Fake host: `step(stepIn)` returns the next canned `StepOut` from a queue and records every `stepIn`. Required tests (name them like these):
1. three `http` effects in one StepOut all START before any completes (concurrency), completions arriving together are folded into ONE next step.
2. `more:true` loops; `next_timer` arms exactly one timer and re-arms; `pump` re-entrancy does not run two steps at once.
3. `barrier` effect waits for a slow persist; non-barrier effects do not.
4. `cancel{op}` aborts the handler's scope and drops its completion; `send([cancel_run])` aborts all ops of the run immediately and still forwards the command.
5. every effect type reaches its service with the documented arguments and produces the documented completion shape (table-driven; compare with `plain()`).
6. http file sink: slot allocated, `net.request` gets `{sink:'file', path}`, staged head+tail passed to `host.inspect`, `body` BodyRef present only for `retain:'pdf'`, slot removed otherwise.
7. engine failure: `EngineTrap` from `step` -> all scopes cancelled, bodies released, synthetic reports, `onEngineFailure`, `needsConfigure`, further `send` is ignored until reset.
8. stall detection with fake timers. 9. service rejection -> `err` completion with the right kind; a non-OpError throw -> `internal`.
10. `wasm-host.test.mjs`: (a) a hand-built minimal wasm module (build the bytes in the test: exports memory, fx_* stubs) exercising marshalling, trap -> poison -> reinstantiate, `lastPanic`; (b) if `wasmBuilt()`, the real module:
`abi_version`, `inspect` of a tiny HTML page returns an `Inspection` with `expect`, `step` accepts a `Configure` command built from `ZFT.config.DEFAULTS`-like JSON, `loginState('https://login.microsoftonline.com/x')`
is a valid LoginState; 1000 inspect calls do not grow `memory.buffer.byteLength` unboundedly; (c) worker protocol with the in-process fake worker incl. watchdog (fake timers) and fallback.
11. `bodies.test.mjs`: refcount/release/releaseAll, caps, slot registration order (register before return), staging math for short/long/empty files, charset decoding.

## Acceptance (runnable)
```
node --test tests/js/wasm-host.test.mjs tests/js/bodies.test.mjs tests/js/effects.test.mjs tests/js/effects-failure.test.mjs
node --test tests/js/load.test.mjs tests/js/static.test.mjs
```
All pass (the real-wasm cases skip with a clear message when `plugin/content/fulltext.wasm` is absent; run `cargo xtask build` first to include them).
