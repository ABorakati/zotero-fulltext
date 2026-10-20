# Plugin JS shell: design (area `js`)

Scope: everything under `plugin/**` (the XPI root, except the build output `plugin/content/fulltext.wasm`) and `tests/js/**`
(except `tests/js/abi.test.mjs`, owned by core). This document is the contract between the five JS work packages. The
per-file API, with signatures and behaviour, is in the **header comment of each file** in `plugin/content/`; this document
holds the decisions that cross files. Frozen inputs: `docs/design/contracts/{model,proto}.rs`, the wasm ABI in
`crates/fulltext-wasm/src/lib.rs`.

## 1. Principles

1. **JS does I/O, Zotero API calls and UI, nothing else.** It never inspects a URL, header or body to make a decision. The
   pure wasm inspector (`fx_inspect`, `fx_login_state`) and the engine (`fx_step`) decide. The only JS-side URL rule is the
   OpenAthens redirector host test in `Host.urlClass` (section 6).
2. **Everything is async and concurrent.** Each Effect with an `op` is one Promise started immediately; the step loop never
   awaits an effect. A step is a short synchronous wasm call. Heavy parsing runs in a ChromeWorker (section 6).
3. **Cancellation is explicit.** The plugin sandbox has no `AbortController`: every async operation takes a
   `ZFT.util.CancelScope`, registers teardown with `onCancel`, and checks between awaits. No fire-and-forget promise without
   `TaskSet.spawn` (mandatory catch), so there are no unhandled rejections.
4. **Failure-first.** Every network call has a timeout plus a JS backstop; every temp file is registered before use and swept;
   an engine trap fails the run cleanly (no replay in v1); a login window is single-flight per epoch.
5. **Classic scripts, one namespace.** Each `content/*.js` is an IIFE attaching one object to `ZFT` (no top-level
   `let/const/class`, no ES modules, platform globals resolved at call time). Node tests load the same files into a `vm`
   context with fakes (`tests/js/helpers/zft-env.mjs`). Scripts load only from `chrome://zotero-fulltext/content/`.

## 2. Files and owners

| File (under `plugin/`) | Namespace | Owner WP | Role |
|---|---|---|---|
| `manifest.json`, `bootstrap.js`, `prefs.js`, `locale/en-US/zotero-fulltext.ftl` | - | js-shell | plugin identity, thin loader, pref defaults, strings |
| `content/namespace.js` (done), `util.js` (done) | `ZFT`, `ZFT.util`, `ZFT.l10n` | js-shell | root object, `LOAD_ORDER`, `OpError`, `CancelScope`, `TaskSet`, clocks, logging |
| `content/config.js` | `ZFT.config` | js-shell | pref schema `DEFAULTS`, `PANE_LAYOUT`, `buildEngineConfig`, `validate` |
| `content/plugin.js`, `menu.js`, `harness.js` | `ZFT.plugin/menu/harness` | js-shell | lifecycle, MenuManager entries, harness self-check |
| `content/prefs.xhtml`, `prefs-pane.js` | window global | js-shell | preferences pane |
| `content/wasm-host.js`, `inspect-worker.js` | `ZFT.wasmHost` | js-host-effects | module load, two instances, trap handling, inspector placement |
| `content/bodies.js` | `ZFT.bodies` | js-host-effects | `BodyRef` handle store, PDF slots, inspect staging |
| `content/effects.js` | `ZFT.effects` | js-host-effects | step loop, effect dispatch, failure policy |
| `content/state.js`, `runs.js`, `items.js` | `ZFT.state/runs/items` | js-runs | `state.json`, run controller, `ItemSnapshot` |
| `content/net.js`, `pdf-capture.js`, `browser.js`, `translators.js`, `zotero-oa.js`, `save.js` | `ZFT.net` ... | js-zotero-io | Zotero API adapters |
| `content/progress.js`, `progress.xhtml`, `progress-window.js`, `confirm.js`, `login-controller.js`, `login.xhtml`, `login.js` | `ZFT.progress/confirm/login` | js-ui | batch progress, confirmations, sign-in window |

`ZFT.LOAD_ORDER` (in `namespace.js`) is the single list of loaded files; `bootstrap.js` and the test helper both read it.
Window scripts (`prefs-pane.js`, `progress-window.js`, `login.js`) and `inspect-worker.js` run in their own global and are
**not** in the list; they reach the plugin through `Zotero.ZoteroFullText` (= `ZFT`) or objects passed in `window.arguments`.

## 3. Data flow

```
menu (ZFT.menu) -> ZFT.runs.startRun -> items.snapshotAll -> StartRun command
   -> ZFT.effects.Runner.send -> pump -> wasmHost.step(StepIn) -> StepOut{effects}
        http/browser_load/login/translators/zotero_oa/save/confirm ... each a Promise (own CancelScope)
        -> raw result -> bodies (stage) -> wasmHost.inspect(meta, bytes) -> CompletionMsg
   -> completions batched into the next step ... until RunFinished -> progress.finish -> RunHandle.done
```

## 4. Lifecycle (bootstrap and `ZFT.plugin`)

* `startup` is idempotent (the sandbox is reused on disable then enable). It registers the chrome package, loads
  `namespace.js` and `LOAD_ORDER` into the sandbox global, then `ZFT.plugin.startup`: sets `Zotero.ZoteroFullText = ZFT`,
  registers the preference pane and menus, adds every already-open main window (`onMainWindowLoad` never fires for the first
  window), and preloads the engine 2 s later (`ZFT.runs.init`: state, sweep, wasm compile, Configure). A failed preload only logs.
* `prefs.js` and `locale/*/*.ftl` are picked up by Zotero itself (`xpcom/plugins.js:507, :584`); bootstrap does not load them.
* `shutdown` skips teardown on `APP_SHUTDOWN` except flushing state; otherwise it tears down in a fixed order with a try/catch
  per step (order in the `plugin.js` header). Registered menus and panes are removed explicitly even though Zotero also does it.
* `ZFT.runs.shutdown`: `cancel_run` for active runs, `shutdown` command, one last pump, wait up to 2 s for quiet, dispose.

## 5. Engine protocol handling (`effects.js`)

Decisions where `proto.rs` leaves room; the engine area must agree (also listed in section 12):

* **Single-flight pump**, commands and completions folded into the next step; completions coalesced for one macrotask
  (`COMPLETION_BATCH_MS = 0`). `more` yields one macrotask (`util.idle`) and steps again, max 64 steps per pump. One timer is
  armed from `next_timer` (JS monotonic clock), re-armed after every step.
* **`barrier: true`** on `Http`/`BrowserLoad` is interpreted as *persist barrier*: before the op starts, JS awaits every
  earlier `Persist` write (`state.set` with `flush`), so the reserve-then-commit licence reservation is on disk before an
  institutional request leaves the machine. Other effects are never delayed.
* **Fire-and-forget effects** (no completion): `cancel`, `release`, `progress`, `persist`, `run_finished`, `log`, `trace`.
  `login_update` has an `op` but is completed immediately with `ok: {}` (harmless if the engine already forgot it).
* **`Cancel{op}` from the engine** cancels the op's scope and suppresses its completion. A JS-initiated cancel (CancelRun /
  CancelItem passing through `Runner.send`, shutdown, failure) cancels scopes immediately and the ops complete with
  `err.kind = cancelled`.
* **Completion payloads** (`ok`): `http` HttpOk, `browser_load` BrowserOk, `login_window` LoginOk, `run_translators`
  TranslatorsOk, `save_pdf` SaveOk, `confirm` ConfirmOk, **`zotero_oa` = JSON array of `ZoteroOaHit`** (empty = none; `version`
  passed through verbatim, e.g. `publishedVersion`). A rejected handler becomes `err` (`OpError.toOpErr()`, unknown throws =
  `internal`).
* **BrowserLoad inspect spec**: when a PDF was captured the runner inspects with `{expect:'pdf', requested, route}` taken from
  the effect's page spec; otherwise the effect's own spec is used.
* **Engine failure policy**: `EngineTrap`, `EngineError` from `step`, or a stall (nothing in flight, no timer, no `more`, no
  waiting user, unfinished runs, for 15 s; `DumpState` is sent once first) stops stepping, cancels all scopes, releases all
  bodies, builds a synthetic `RunReport` per unfinished run (items seen saved stay `Saved`; all others
  `NeedsUser{engine_restarted}`; `engine_restarts = 1`), calls `progress.finish`, resets the engine instance and sets
  `needsConfigure`. The next run re-sends `Configure` (persisted state included).

## 6. Concurrency, UI responsiveness, inspector placement

* **Network**: `Zotero.HTTP.request` for memory bodies (JSON/HTML), `Zotero.HTTP.download` (fetch + stream to disk) for PDFs.
  Verified options in `net.js`. Timeouts come from the engine per request (`timeout_ms`; Wiley-class hosts get 25-60 s from
  the engine's slow-host profile); JS adds a 2 s backstop. No JS retries.
* **Pure instance**: default a **ChromeWorker** (`inspect-worker.js`), 3 s watchdog, terminate and respawn, requeue once, permanent
  fallback to the main thread if the worker cannot start (pref `debug.inspect_in_worker`). The main-thread fallback serialises
  inspections and yields between them, and the text input is capped at 2 MiB (html5gum is about 107 ms per 2 MiB). PDFs are
  never parsed: the inspector gets head 4 KiB + tail 2 KiB (`RespMeta.head_len`).
* **Engine instance** stays on the main thread (its steps are microseconds to a few milliseconds).
* **HiddenBrowsers**: at most 3 live; the PDF MIME handler is one ref-counted dispatcher keyed by `topFrameElement`
  (`pdf-capture.js`), never one handler per load. `HiddenBrowser.load()` is called and its result ignored (it fails after 5 s
  on slow pages and always for PDF responses); settle detection uses our own progress listener.
* **Memory**: PDFs are file-backed; page bodies are capped at 8 MiB each and 64 MiB total; `retain` decides what survives.
* **Login**: one `LoginWindow` effect per sign-in epoch for all waiting items (engine side); the controller refuses a second
  concurrent window for the same realm. The window's own timer is authoritative (one "Keep waiting" extension).

## 7. Zotero 10.0.6 APIs relied on (all read in `.ref/omni-app/chrome/content/zotero/`)

| API | Where | Used by |
|---|---|---|
| `Zotero.MenuManager.registerMenu/unregisterMenu`, hooks `onShowing(event, ctx)`, `onCommand(event, ctx)`, `ctx.items`, `setVisible` | `xpcom/pluginAPI/menuManager.js:580-704, 824-833` | menu.js |
| `Zotero.PreferencePanes.register({pluginID, src, scripts, label, image, helpURL})` (src is an XHTML fragment) | `xpcom/preferencePanes.js:134` | plugin.js |
| `Zotero.HTTP.request` options (`successCodes:false`, `errorDelayMax:0`, `noRetryOnThrottle`, `cancellerReceiver`, `requestObserver`, `responseType`, `timeout`, `followRedirects`); fake xhr for unfollowed 3xx | `xpcom/http.js:222-582` | net.js |
| `Zotero.HTTP.download(url, path, {headers, timeout, successCodes, errorDelayMax, cancellerReceiver, onProgress})` returns the fetch `Response`; every abort throws `TimeoutException` | `xpcom/http.js:636-809` | net.js |
| `HiddenBrowser` (`customUserAgent`, `webProgress`, `currentURI`, `getPageData`, `destroy`; `load()` quirks) | `HiddenBrowser.mjs:48-346` | browser.js |
| `Zotero.MIMETypeHandler.addHandlers/removeHandlers` (delete-all-on-last-removal hazard) and the topFrameElement pattern | `xpcom/mimeTypeHandler.js:105-141, 154-203`; `xpcom/browserRequest.js:525-559` | pdf-capture.js |
| `Zotero.Utilities.Internal.getOpenAccessPDFURLs`, `blobToHTMLDocument`, `getFileFromDocument` | `xpcom/utilities_internal.js:966, 1438, 1475` | zotero-oa.js, translators.js |
| `Zotero.Attachments.createTemporaryStorageDirectory`, `createURLAttachmentFromTemporaryStorageDirectory`, `getFileBaseNameFromItem`, `importFromFile` | `xpcom/attachments.js:2853, 682, 2380, 63` | bodies.js, save.js |
| `Zotero.VersionHeader.getPlainFirefoxUA` (UA without `Zotero/` is left alone) | `xpcom/zotero.js:2238-2331` | net.js, browser.js, login |
| `Item.getBestAttachmentState`, `numFileAttachmentsWithContentType`, `isRegularItem`, `getField`, `Library.filesEditable` | `xpcom/data/item.js:2447, 2857, 4347`; `library.js` | items.js, save.js |
| Plugin `prefs.js` (`pref(name, value)` shim), FTL auto-registration, `<link rel="localization" href="zotero-fulltext.ftl">` | `xpcom/plugins.js:507, 584` | prefs.js, windows |

Not usable / avoided: `Zotero.openInViewer` for credentials, wasm-bindgen, `instantiateStreaming` over `chrome://`, loading scripts from
`jar:` or `file:` URLs, `Zotero.ProgressWindow` for per-item rows.

## 8. Localization ids

* Message ids are a **cross-area contract**. The engine emits `ProgressRow.l10n` / `RowAction.l10n` ids and args
  (`crates/fulltext-engine/src/l10n.rs`); the FTL (`plugin/locale/en-US/zotero-fulltext.ftl`) defines them; JS formats them with
  `document.l10n.setAttributes` (windows) or `ZFT.l10n.format(id, args)` (`Localization([ftl], true).formatValueSync`).
  `tests/js/ftl.test.mjs` fails if any quoted `"zotero-fulltext-..."` literal under `plugin/` or `crates/**/*.rs` has no message.
* **Engine-emitted rows** (status, args in brackets): `queued`, `waiting-confirm`, `checking`, `searching-oa`, `downloading` [host],
  `waiting-host` [host, seconds], `waiting-login`, `signing-in`, `institutional`, `challenge`, `saving`, `network-down`; terminal rows
  `saved-oa`, `saved-inst`, `skipped-has-pdf`, `skipped-not-downloaded`, `not-editable`, `skipped-not-regular`, `skipped-in-progress`,
  `no-identifier`, `not-found`, `not-found-oa-only`, `not-entitled`, `needs-login`, `login-declined`, `login-timeout`,
  `login-refused` [code], `login-ineffective`, `misconfigured`, `needs-user`, `no-pdf-link`, `group-library`, `engine-restarted`,
  `blocked` [host], `cap-reached` [cap: hourly|daily|mb_per_hour|batch], `halted`, `cancelled`, `timed-out`, `failed` [code]
  (all prefixed `zotero-fulltext-status-`). Actions: `zotero-fulltext-action-open-browser`, `-open-libkey`, plus JS/engine extras
  `-open-redirector`, `-open-ezproxy`, `-open-doi`, `-attach-file`, `-sign-in`, `-retry`, `-cancel-item`.
* **JS-owned ids**: `menu-*` (`.label`), `progress-*`, `report-*`, `login-*`, `confirm-*`, `notice-*`, `prefs-*`. Args are plain strings
  or numbers; no message interpolates untrusted text except `{ $host }` and `{ $code }`.
* A new engine id requires an FTL line in the same change; the gate makes that mechanical.

## 9. Failure handling owned by JS (acceptance matrix for the JS packages)

| Condition | JS behaviour |
|---|---|
| offline / DNS / connect failure | `OpError` `offline` / `network`; no JS retry; engine pauses and probes (NetworkDown) |
| HTTP timeout, slow host | `timeout` after `timeout_ms` (engine-chosen per host class) + 2 s backstop; scope cancelled, socket aborted |
| TLS failure | `tls` (distinct from `network`, so the engine can say "certificate problem") |
| 429 / 503 with Retry-After | returned as a normal HttpOk (status + `retry-after` header); never retried by `Zotero.HTTP` |
| body larger than `max_bytes` / `pdf_max_mb` | abort mid-stream, `too_large`, partial file removed |
| non-2xx on the PDF path (403 Cloudflare, 404) | `HttpOk` with the error page staged for the inspector; no file kept |
| bot wall (Cloudflare, Akamai, proof of work) | never solved by JS; page returned and classified; user-operated window only (`LoginReq.kind = challenge`) |
| HiddenBrowser never navigates | `timeout`; navigated but unsettled at timeout -> settled result, inspector decides |
| PDF arrives while another capture is live | routed by `topFrameElement`; unknown channels untouched |
| wasm cannot load / ABI mismatch | `zotero-fulltext-notice-wasm-load-failed` once; menu entries stay but fail fast |
| pure instance trap | `Inspection::Failed`, re-instantiate, 3 traps disable the worker |
| engine trap / stall | section 5 failure policy |
| item deleted or trashed mid-run | `cancel_item`; `SavePdf` re-checks the parent (`parent_gone`) |
| PDF added meanwhile (sync, user) | `SavePdf` -> `already_has_pdf` (nothing written) |
| library not file-editable | `not_editable` |
| login window closed / timed out / Conditional Access | `closed_by_user` / `timeout` / `ca_blocked` + `ca_error` |
| Zotero quits or plugin disabled mid-run | `runs.shutdown` cancels and flushes; temp dirs swept on next start |
| crash with temp dirs left behind | `state.sweepTmp()` at the first `runs.init` |
| `state.json` corrupt | renamed `state.json.bad`, defaults used, a log line |
| prefs changed mid-run | `reconfigure()` waits until no run is active |

## 10. Persistence and temp files

`<profile>/zotero-fulltext/state.json` = `{v, persisted: {ledger, halts, ezproxy, alma}, tmp_dirs, run_counter}` (written atomically with
`IOUtils.writeJSON(..., {tmpPath})`, debounced 2 s, immediate for `flush: true`). PDFs are staged in Zotero storage temp
directories (`createTemporaryStorageDirectory`, registered in `tmp_dirs` before use) so a save is a rename; they are removed on
release, on failure, and by the startup/shutdown sweep.

## 11. Testing

* `node --test "tests/js/*.test.mjs"` (Node 24, no npm). **Node 24 does not accept a bare directory**: `node --test tests/js` fails with
  `MODULE_NOT_FOUND`; the xtask must pass the glob (see section 12).
* `tests/js/helpers/`: `zft-env.mjs` (vm loader, `plain()`), `fakes.mjs` (in-memory `IOUtils`/`PathUtils`, `makeFakeTimers`,
  `makeFakeZotero` seeded from `prefs.js`, `makeFakeServices`), `api-manifest.mjs` (declared API per module), `xml-check.mjs`.
  Packages may append fakes; never change existing signatures.
* Skeleton gates already green: `load`, `static`, `prefs`, `ftl`, `util` tests (26 tests).
* The real wasm (`plugin/content/fulltext.wasm`, `cargo xtask build`) is used by `wasm-host` tests via `wasmBuilt()` and skipped otherwise.
* Engine behaviour is tested with a **scripted fake host** (a `step()` that returns canned `StepOut`s) so the runner never needs wasm.
* Harness (`cargo xtask harness`, throwaway profile, about 18 s per run): `ZFT.harness.maybeRun` smoke check only; end-to-end
  scenarios (H-series) are a later integration task once all packages are merged.

## 12. Contract notes for other areas (additive, none breaks a frozen file)

1. **xtask**: use `node --test "tests/js/*.test.mjs"`; `tests/js/abi.test.mjs` needs the built wasm.
2. **engine**: `barrier` = persist barrier (section 5); `ZoteroOa` completion = `ZoteroOaHit[]`; `LoginUpdate` is completed immediately;
   `Configure` may arrive more than once per engine instance (after a prefs change or a trap): replace config, keep ledger;
   `Persist.value` is opaque JSON written under `persisted[key]`; `RowAction::OpenBrowser.url` must be http(s) (JS refuses others).
3. **core**: `fx_inspect` with `InspectSpec::Probe`, empty body, `RespMeta{status:200, final_url, body_len:0}` must classify IdP,
   EZproxy-login and publisher-SSO URLs from the URL alone (used for settle detection); PDF verdict must work from head 4096 B +
   tail 2048 B with `RespMeta.head_len`; text bodies arrive truncated to 2 MiB with `truncated:true` and the real `body_len`.
4. **engine**: UA policy is per request (`Ua::Plain` only where needed); JS keeps no UA state.

## 13. Work packages

| Id | Owns | Depends on | Acceptance (runnable) |
|---|---|---|---|
| js-shell | manifest, bootstrap, prefs.js, ftl, config.js, plugin.js, menu.js, harness.js, prefs.xhtml, prefs-pane.js, icons, namespace.js, util.js, tests/js/helpers/*, `{load,static,prefs,ftl,util,config,plugin,menu,prefs-pane,harness}.test.mjs` | - | `node --test tests/js/config.test.mjs tests/js/plugin.test.mjs tests/js/menu.test.mjs tests/js/prefs-pane.test.mjs tests/js/harness.test.mjs tests/js/load.test.mjs tests/js/static.test.mjs tests/js/prefs.test.mjs tests/js/ftl.test.mjs tests/js/util.test.mjs` |
| js-host-effects | wasm-host.js, inspect-worker.js, bodies.js, effects.js + `{wasm-host,bodies,effects,effects-failure}.test.mjs` | - | `node --test tests/js/wasm-host.test.mjs tests/js/bodies.test.mjs tests/js/effects.test.mjs tests/js/effects-failure.test.mjs` |
| js-runs | state.js, runs.js, items.js + `{state,runs,items}.test.mjs` | - | `node --test tests/js/state.test.mjs tests/js/runs.test.mjs tests/js/items.test.mjs` |
| js-zotero-io | net.js, pdf-capture.js, browser.js, translators.js, zotero-oa.js, save.js + `{net,pdf-capture,browser,translators,zotero-oa,save}.test.mjs` | - | `node --test tests/js/net.test.mjs tests/js/pdf-capture.test.mjs tests/js/browser.test.mjs tests/js/translators.test.mjs tests/js/zotero-oa.test.mjs tests/js/save.test.mjs` |
| js-ui | progress.js, progress.xhtml, progress-window.js, confirm.js, login-controller.js, login.xhtml, login.js + `{progress,confirm,login}.test.mjs` | - | `node --test tests/js/progress.test.mjs tests/js/confirm.test.mjs tests/js/login.test.mjs` |

Specs: `docs/wp/js-*.md`. Packages code against the declared signatures and fakes, so they can run in parallel; the full-stack check
(`cargo xtask build && node --test "tests/js/*.test.mjs" && cargo xtask xpi`) belongs to the integration step after merge.

