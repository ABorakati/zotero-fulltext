# WP js-shell: plugin shell, prefs, menu, strings, harness hook

Read first: `docs/design/plugin-js.md` (sections 1, 2, 4, 8, 11, 12) and the header comment of every file you own. You may read
anything; edit only the files below. Use `CARGO_TARGET_DIR` only if you run cargo (you should not need to).

## Files you own
`plugin/manifest.json`, `plugin/bootstrap.js`, `plugin/prefs.js`, `plugin/locale/en-US/zotero-fulltext.ftl`,
`plugin/content/{namespace,util,config,plugin,menu,harness}.js`, `plugin/content/prefs.xhtml`, `plugin/content/prefs-pane.js`,
`plugin/content/icons/*`, `tests/js/helpers/*`, tests `{load,static,prefs,ftl,util,config,plugin,menu,prefs-pane,harness}.test.mjs`.
`namespace.js`, `util.js`, `bootstrap.js`, `manifest.json`, `prefs.js` and the FTL exist and are tested; extend, do not rewrite.
Other packages add lines to the FTL for ids they introduce only through you: if a package needs a new id it appends it under its
own `## ...` heading (additive); never rename or delete an id.

## Deliverables

### 1. `ZFT.config` (config.js)
* `buildEngineConfig(get?) -> EngineConfig` (sync). Shape = `docs/design/contracts/proto.rs` `EngineConfig`:
  `{oa:{openalex, openalex_key, europepmc, biorxiv, pmc_s3, europepmc_get_pdf, medrxiv_early, arxiv, zotero_oa, unpaywall_email,
  semantic_scholar_key, core_key, version_order:["published",...]}, inst:{enabled, try_direct, use_openathens, use_ezproxy, check_alma,
  login_mode:"window"|"handoff", auto_open_login, login_timeout_sec, group_libraries:"ask"|"never"|"allow", confirm_above, batch_max,
  hourly_cap, daily_cap, mb_per_hour, min_gap_sec, challenge_windows}, run:{item_concurrency, hedge, pdf_max_mb},
  institution:{name, openathens_domain, ezproxy:{login_host, suffix}|null, ezproxy_hosts:[...], alma:{base, inst}|null,
  libkey_library_id:number|null, idp_hosts:[...], entra_tenant:string|null, passthrough_hosts:[...], warmup_url:""}, endpoint_map:[], trace:bool}`.
  Rules: lists = comma strings, trimmed, empties dropped, hosts lower-cased and de-duplicated; `ezproxy` null when login host empty;
  `alma` null when base empty; `libkey_library_id` 0 -> null; `entra_tenant` '' -> null; clamp `item_concurrency` 1..8, `pdf_max_mb` 1..500,
  `login_timeout_sec` 30..1800, caps/limits >= 0, `batch_max` >= 1; `version_order` keeps only published/accepted/submitted (append missing ones
  in default order); unknown `login_mode`/`group_libraries` fall back to the default; `trace = debug.trace`; `endpoint_map` from `ZFT.harness.endpointMap()`.
  `debug.inspect_in_worker` is plugin-only (never in the output). Output must `JSON.stringify` to something serde accepts: write a
  test that checks it against the example `Configure` JSON in `proto.rs` tests (copy that JSON into the test, compare key sets recursively).
* `validate(config) -> string[]` of l10n ids: `zotero-fulltext-prefs-problem-openathens-domain` (inst.enabled with empty domain),
  `-login-timeout` (< 30 s), `-unpaywall-email` (non-empty without `@`).
* `PANE_LAYOUT`: fill it (groups oa, inst, limits, run, institution, debug; one row per DEFAULTS key except none may be omitted). Test: every
  DEFAULTS key appears in exactly one row; widget matches `typeOf`; every `label`/`heading`/option id exists in the FTL (add the missing
  `-prefs-*` messages; ids for options already exist for login mode and group policy).

### 2. `ZFT.plugin` (plugin.js) and `bootstrap.js`
Implement exactly the behaviour in the `plugin.js` header: idempotent `startup`/`shutdown`, `Zotero.ZoteroFullText = ZFT`, pane + menu
registration, window tracking with a `WeakSet`, FTL insertion/removal, preload after `PRELOAD_DELAY_MS` (use `setTimeout`, injectable for tests),
APP_SHUTDOWN fast path, per-step try/catch in shutdown. Pane registration args: `src: ZFT.CONTENT_URL + 'prefs.xhtml'`,
`scripts: [ZFT.CONTENT_URL + 'prefs-pane.js']`, `label` from `ZFT.l10n.format('zotero-fulltext-prefs-title')` (raw text), `image` the icon, `helpURL`
the repository URL. bootstrap.js is final unless a test shows a defect (report it).

### 3. `ZFT.menu` (menu.js)
Per the header: two item-menu entries (`zotero-fulltext-menu-get` Mode full, `-get-oa` Mode oa_only) with `onShowing` using
`ZFT.items.menuEligible(context.items)` (synchronous!), `onCommand` -> `ZFT.runs.startRun({items: context.items, mode, interactive: true})` with
errors shown once via `Services.prompt.alert` (title from `zotero-fulltext-prefs-title`, text `zotero-fulltext-notice-start-failed` {reason}); a
Tools-menu group: `-signin` -> `ZFT.login.warmUp(ZFT.config.buildEngineConfig())`, `-progress` -> `ZFT.progress.show()`, `-prefs` ->
`Zotero.Utilities.Internal.openPreferences(paneID)` (paneID stored by plugin.js). `register()` is idempotent and returns the keys;
`unregister()` calls `Zotero.MenuManager.unregisterMenu` for each. Handlers never throw (menuManager only catches synchronous throws, and a thrown
`onCommand` would be silent).

### 4. Preferences pane (prefs.xhtml, prefs-pane.js)
Per the `prefs-pane.js` header: build the DOM from `PANE_LAYOUT`, bind controls directly to prefs (no PreferencesBindings), show validation
problems, call `ZFT.runs.reconfigure()` after changes, three action buttons. The pane script runs in the preferences window global: reach the
plugin through `Zotero.ZoteroFullText`. Make `init()` idempotent and `destroy()` remove listeners. Secret-ish prefs (`*_key`) use
`type=password`-style display (`<html:input type="password">`) with a "show" toggle. Number inputs clamp to the row's `min`/`max`.

### 5. Harness hook (harness.js)
Per the header. `maybeRun()` returns null (and does nothing) unless `extensions.zotero-harness.resultFile` is a non-empty string. The checks must be
individually try/caught so one failure still writes a result. Write with `IOUtils.writeUTF8(path, text, {tmpPath: path + '.tmp'})`; quit only if
`extensions.zotero-harness.quitWhenDone` is not false. `endpointMap()` parses `extensions.zotero-fulltext.harness.endpoint_map` (JSON array of
2-string arrays; anything else -> []).

## Tests to write (all with `tests/js/helpers`, no Zotero)
* `config.test.mjs`: defaults produce the expected EngineConfig (deep compare with plain()); every clamp; list parsing; null mappings; validate().
* `plugin.test.mjs`: startup twice = one registration; shutdown twice safe; APP_SHUTDOWN path only flushes; a throwing step does not stop later teardown
  steps; window added exactly once; `onMainWindowLoad` after startup does not double-insert FTL; preload scheduled and failure swallowed.
* `menu.test.mjs`: registration shape (menuID, pluginID, target, l10nIDs, hooks); `onShowing` calls `setVisible` with the eligibility result and does not
  await; `onCommand` calls `runs.startRun` with the right mode and swallows/reports errors.
* `prefs-pane.test.mjs`: needs a DOM: write a ~60-line fake `document` (createElement/appendChild/addEventListener/querySelector) in the test file,
  or test only the pure helpers you export from `prefs-pane.js` (`ZoteroFullTextPrefs.buildRow(row)` returning a plain descriptor). Either is fine; no jsdom.
* `harness.test.mjs`: dormant without the pref; with it, writes the JSON shape of the header and honours quitWhenDone=false.

## Edge cases
Pref read errors fall back to defaults (never throw from `getPref`); `Zotero.getMainWindows()` may contain windows without `MozXULElement`
(skip with a log); `Zotero.PreferencePanes.register` may reject: log and continue (menus still work); the FTL file name must stay `zotero-fulltext.ftl`.

## Acceptance (runnable)
```
node --test tests/js/config.test.mjs tests/js/plugin.test.mjs tests/js/menu.test.mjs tests/js/prefs-pane.test.mjs tests/js/harness.test.mjs tests/js/load.test.mjs tests/js/static.test.mjs tests/js/prefs.test.mjs tests/js/ftl.test.mjs tests/js/util.test.mjs
```
All pass, and `node --test tests/js/ftl.test.mjs` stays green when another package adds an l10n id (that is the gate working).
