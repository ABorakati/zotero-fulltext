# WP js-runs: persisted state, item snapshots, run controller

Read first: `docs/design/plugin-js.md` sections 3, 4, 5, 9, 10, 12; header comments of `state.js`, `items.js`, `runs.js`;
`docs/design/contracts/{model,proto}.rs` (ItemSnapshot, Persisted, Command, RunReport). `ZFT.effects.createRunner` and the service modules are being
written in parallel by other packages: code against their declared signatures and test with fakes.

## Files you own
`plugin/content/{state,items,runs}.js`, tests `{state,items,runs}.test.mjs`.

## 1. state.js (persisted runtime state)
Behaviour per the header. Concrete rules:
* `init({dir})`: default `dir = PathUtils.join(Zotero.Profile.dir, 'zotero-fulltext')`; `IOUtils.makeDirectory(dir, {createAncestors:true, ignoreExisting:true})` and `<dir>/tmp`; read `state.json` with
  `IOUtils.readJSON`; accept it only if `v === 1` and objects have the right types, filling missing keys with defaults (`persisted` = `{ledger:{committed:[],reserved:[]}, halts:{inst_until:0,reason:null,hosts:{}}, ezproxy:{}, alma:{}}`,
  `tmp_dirs: []`, `run_counter: 0`); on parse/shape failure rename to `state.json.bad` (overwrite) and use defaults. Second `init` with the same dir returns without re-reading.
* `set(key, value, {flush})`: reject unknown keys (log, ignore); deep-copy with `JSON.parse(JSON.stringify(value))`; mark dirty; `flush` -> `await flush()` else (re)arm one debounce timer
  (`FLUSH_DEBOUNCE_MS`, `setTimeout` global resolved at call time). `flush()` serialises writes (a second flush waits for the first), writes `IOUtils.writeJSON(path, obj, {tmpPath: path + '.tmp'})`, clears dirty
  only if no newer change arrived during the write. Write errors are logged and leave `dirty` true (retry on the next set/flush) but `flush()` resolves (never throws into the pump).
* `registerTmpDir(path)` persists immediately (flush) before returning; `unregisterTmpDir` is lazy (debounced). `sweepTmp({keep})`: remove each registered dir not in `keep` (`IOUtils.remove(p, {recursive:true,
  ignoreAbsent:true})`, errors logged), clear the list, also remove children of `<dir>/tmp`; returns the count removed. Never touch paths outside the registered list and `<dir>/tmp`.
* `nextRunId()`: `++run_counter`, marks dirty; ids are unique across restarts. `reset(keys)` restores defaults for those persisted keys and flushes. `dispose()` flushes, clears timers, forgets loaded state.
* Tests with `makeFakeFs` + `makeFakeTimers`: round trip, corrupt file -> `.bad`, debounce coalescing (3 sets -> 1 write after 2000 virtual ms), flush ordering, write failure retry, sweep list semantics, run ids across a fake restart.

## 2. items.js (ItemSnapshot)
Behaviour per the header. `snapshot(item)` returns exactly the `ItemSnapshot` JSON of `model.rs`:
`{item: number, key: "<libraryID>/<itemKey>", library: {id, kind: "user"|"group"|"feed", files_editable, name}, regular, feed, item_type, title, fields: {doi, pmid, pmcid, url, archive_id, extra},
pdf_attachments, best_attachment: {kind: "none"|"pdf"|"other", exists: boolean|null}}`.
* Field reads are guarded: `fieldOrEmpty(item, 'DOI')` returns '' when the field is invalid for the item type or throws. Only regular items get `pdf_attachments`/`best_attachment` queries
  (`await item.loadDataType('childItems')` first when `item._loaded && !item._loaded.childItems` or simply always inside try/catch); non-regular items get `0` and `{kind:'none', exists:null}`.
* `title`: `item.getDisplayTitle()` fallback `getField('title')`, trimmed, max 300 chars.
* `snapshotAll`: sequential reads are fine (cheap), but yield with `util.idle()` every 25 items; a throwing item becomes `{...minimal, regular:false}` and is logged; never rejects.
* `menuEligible(items)`: synchronous; true if any item is `isRegularItem()` and not a feed item; tolerate non-item objects (false).
* `watchDeletions(onGone)`: `Zotero.Notifier.registerObserver({notify(event, type, ids) {...}}, ['item'], 'zotero-fulltext')`; call `onGone(id)` for `delete` and `trash` events (trash: ids of the trashed items);
  returns an unregister function calling `Zotero.Notifier.unregisterObserver(observerID)`. Observer exceptions are caught and logged.
* Tests with plain-object fake items (methods + fields), covering each mapping above, each guard, and observer registration/unregistration with a fake `Zotero.Notifier`.

## 3. runs.js (controller)
Behaviour per the header. Concrete rules:
* `init(overrides)`: memoised promise (`initPromise`); on failure clear it so the next call retries, and rethrow an `Error` whose `message` is the l10n id
  (`zotero-fulltext-notice-wasm-load-failed` when `wasmHost.get()` fails). Order: `state.init`, `state.sweepTmp`, host, bodies store (`ZFT.bodies.store = new BodyStore({state})`), runner, Configure.
  `overrides` replaces any dependency (host, state, bodies, services, timers, clock) so tests never need wasm or Zotero.
* Configure: `{type:'configure', protocol: ZFT.PROTOCOL, config: ZFT.config.buildEngineConfig(), persisted: state.getPersisted(), seed}`; `seed` from
  `crypto.getRandomValues(new Uint32Array(2))` combined to an integer < 2^53. Track `configHash = JSON.stringify(config)`; `reconfigure()` re-sends only when it changed and no run is active,
  otherwise sets `pendingReconfigure` and re-evaluates when the last run finishes. After `runner.needsConfigure` (engine failure) the next `startRun` re-sends Configure first (same step).
* `startRun`: `await init()`; if `items` is empty throw `Error('zotero-fulltext-notice-no-eligible')`; `snapshots = await items.snapshotAll(items)`; allocate id; `progress.open(run, snapshots, {interactive})`;
  register the deletion watcher on first active run (unregister when none); `runner.send([startRun])`; return `{run, done, cancel}`. `done` resolves with the report when `onRunFinished` fires for that run
  (also for synthetic reports) and never rejects.
* `onRunFinished(report)`: store `reports.set(run, report)` (keep the last 20), drop the active entry, resolve `done`, apply a pending reconfigure, `state.flush()`.
* `onEngineFailure(err)`: log; show ONE notice per failure via `Services.prompt.alert` (`zotero-fulltext-notice-engine-failed`); never throws.
* `shutdown()`: as the header; each step in its own try/catch; idempotent (second call resolves immediately); after it `runner()` is null and a later `init()` rebuilds everything.
* Guard rails: batch limits (`inst.batch_max`, confirmations) are the engine's business; JS applies none. The only JS-side guard is a sanity cap of 5000 items per run (throw
  `Error('zotero-fulltext-notice-start-failed')` with the reason in `error.reason`).

## Tests (fake runner/host/services; no wasm, no Zotero)
`runs.test.mjs`: init order and memoisation (concurrent callers share one init; failure then retry works); startRun sends exactly one `start_run` with snapshots in selection order and opens progress;
Configure content and the re-send rules; `done` resolves with the report from `onRunFinished` including a synthetic one; cancelRun/cancelItem/userAction send the right commands; item deletion -> `cancel_item`
for the right runs only; shutdown ordering (assert call order on recorders) and idempotence; sweep happens before the first Configure.

## Acceptance (runnable)
```
node --test tests/js/state.test.mjs tests/js/items.test.mjs tests/js/runs.test.mjs
node --test tests/js/load.test.mjs tests/js/static.test.mjs
```
