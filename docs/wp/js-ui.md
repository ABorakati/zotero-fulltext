# WP js-ui: progress window, confirmations, institutional sign-in window

Read first: `docs/design/plugin-js.md` sections 1, 5, 8, 9; header comments of every file you own; `tools/sso-spike/plugin/content/login.{xhtml,js}` (validated template) and
`research/08-gap-fill.md` (why a custom window; Cloudflare/UA findings; popups); `docs/design/contracts/proto.rs` (ProgressRow, RowAction, RunReport, LoginReq, LoginOk, ConfirmReq);
`crates/fulltext-engine/src/l10n.rs` (the ids and args the engine emits).

## Files you own
`plugin/content/{progress.js,progress.xhtml,progress-window.js,confirm.js,login-controller.js,login.xhtml,login.js}`, tests `{progress,confirm,login}.test.mjs`.
You may APPEND strings to the FTL under your own heading if you need new ids (ids must start `zotero-fulltext-`; `ftl.test.mjs` must stay green). Window titles are set from script
(`document.title = await document.l10n.formatValue(id)`), buttons are `<html:button>` with `data-l10n-id` VALUE messages (do not switch to XUL buttons).

## 1. progress.js + progress window
* `ProgressModel` (DOM-free): `applyRow(item, row)` keeps `{state, l10n, args, route, actions}` per item in selection order; unknown item -> log + ignore; a row after a terminal state (`done|attention|failed|skipped|cancelled`) is ignored
  unless it is a `retry` transition (state goes back to `working`/`waiting`); `counts()` returns `{queued, working, waiting, done, attention, failed, skipped, cancelled, total, finished}`;
  `setReport(report)` stores it and notifies; `groups()` buckets `report.items` by `outcome.kind`: `saved` -> attached; `needs_user|not_entitled|blocked|halted|deferred` -> needs_you;
  `not_found|no_identifier` -> not_found; `failed` -> failed; `skipped|cancelled` -> skipped. Each entry: `{item, key, title, outcome, row (last row), handoffActions}` where `handoffActions` converts
  `Outcome` hand-off URLs (`redirector_url`, `ezproxy_url`, `libkey_url`, `doi_url`) into `RowAction::OpenBrowser` entries `{kind:'open_browser', url, l10n}` with l10n ids `zotero-fulltext-action-open-redirector|-open-ezproxy|-open-libkey|-open-doi`
  (only `http(s)` URLs; others dropped). `diagnostics()` = redacted text: per item `key | outcome.kind | last row l10n id | trace steps (src tier class ms)`, URLs through `redactUrl`, plus the ledger line; contains no title text, no query strings, no keys.
* Controller: `open(run, snapshots, {interactive})` creates the model and opens the window with `Services.ww.openWindow(null, WINDOW_URL, '_blank', 'chrome,dialog=no,resizable,centerscreen' + (interactive ? '' : ',alwaysLowered'), argsHolder)`
  where `argsHolder = {wrappedJSObject: {model, onAction, onCancel, onClose, onCopy, format}}`; the window may be closed and reopened (`show(run)`) any number of times; closing never cancels the run. `finish(run, report)`
  after the window was closed reopens it only if the report has `needs_you` items; otherwise it stays closed (no extra notice; the Tools-menu entry reopens it).
  `handleAction` per the header; `open_browser` refuses non-http(s) URLs; `attach_file` -> `ZFT.save.attachFile`; `sign_in|retry` -> `ZFT.runs.userAction(run, 'sign_in_now'|'retry_sign_in')`; `cancel_item` -> `ZFT.runs.cancelItem`.
  `redactUrl('https://u:p@a.b/c/d?x=1#h')` -> `a.b/c/d`; garbage -> `(invalid url)`.
* `progress-window.js`: render on every model change (coalesce with `requestAnimationFrame`); rows as `<html:div>` with title (CSS truncation, `title` attribute), status span with `document.l10n.setAttributes(span, row.l10n, row.args)`,
  buttons for `row.actions` (label via `data-l10n-id` of the action's l10n id or the kind default `zotero-fulltext-action-<kind>`); header counts via `zotero-fulltext-progress-header` with `{done,total,working,waiting}`;
  after the report: `zotero-fulltext-report-summary` + grouped sections (`report-group-*`) + ledger line `zotero-fulltext-report-ledger`; Cancel hidden after completion; Copy diagnostics copies `args.onCopy()` text with
  `Cc['@mozilla.org/widget/clipboardhelper;1']`. Rows are keyed by item id and patched in place (no full re-render of thousands of rows), no leaks on re-render.

## 2. confirm.js
Per the header: `Services.prompt.confirmEx` with three buttons. Use `BUTTON_POS_0 | BUTTON_POS_1 | BUTTON_POS_2` with `BUTTON_TITLE_IS_STRING` and strings from `ZFT.l10n.format`: large_batch -> [`confirm-yes`, `confirm-oa-only`,
`confirm-cancel`]; group_library -> [`confirm-save-anyway`, `confirm-oa-only`, `confirm-cancel`]. Map the returned index 0/1/2 -> yes/oa_only/cancel (closing the dialog or any other value -> cancel). If `Services.prompt` throws, resolve `cancel`.
Title/body ids `zotero-fulltext-confirm-large-batch-title/-body` ({n, cap}) and `-group-title/-body` ({library}). Tests with a fake `Services.prompt` returning each index.

## 3. login-controller.js + login window
* `decide(s)`: pure, exactly the priority in the header: pdfCaptured -> `pdf`; windowClosed or userAction 'skip' -> `closed_by_user`; userAction 'handoff' -> `handoff`; state.refused -> `ca_blocked` with `caError = state.code`;
  state.landed -> `landed`; `elapsedMs >= timeoutMs` -> if not yet `extended` and userAction 'keep_waiting' -> `{done: null, extend: true}`, else `timeout`; `challenge_shown` -> `{done: null, showChallengeHint: true}`;
  `elapsedMs >= KEEP_WAITING_AT * timeoutMs && !extended` -> `offerKeepWaiting: true`. Table-test every branch and the priority collisions.
* `open(eff, ctx)`: as the header (steps 1-6). Open the window with `Services.ww.openWindow(null, WINDOW_URL, '_blank', 'chrome,centerscreen,resizable,dialog=no', holder)`, wait for `load`, `Zotero.Utilities.Internal.activate(win)`,
  set `browser.browsingContext.customUserAgent = Zotero.VersionHeader.getPlainFirefoxUA()`, `browser.loadURI(Services.io.newURI(entry_url), {triggeringPrincipal: Services.scriptSecurityManager.getSystemPrincipal()})`
  (the spike proved this works; the sign-in itself is the user's). Poll loop via `ZFT.util.sleep(POLL_MS, scope)`; html via `browser.browsingContext.currentWindowGlobal.getActor('PageData').sendQuery('documentHTML')` wrapped in a 2 s race and capped at 256 KiB.
  Build LoginOk: `landed` -> `page` BodyRef from `ctx.bodies.putMemory(utf8(html), {kind:'page'})` and `inspect = await ctx.host.inspect({spec:{expect:'page', requested:{}, route:'inst'}, resp:{status:200, final_url, headers:{'content-type':'text/html'}, body_len}}, bytes)`;
  `pdf` -> `ctx.bodies.putBlobAsPdf(blob)` and `inspect = Pdf verdict` via `stageForInspect`; all other results use `{expect:'none'}` as `inspect`. Never read cookies or form fields; never log URLs with query strings (use `ZFT.progress.redactUrl`).
* `update(op, waiting)`: look up the window by op, call `win.zftLogin.setLabels({waiting})`; unknown op -> no-op.
* `warmUp(config)`: same window, no capture, `waiting = 0` hides the label; resolves when the window closes or `decide` says landed (then closes the window after 1.5 s so the user sees the result).
* `closeAll()`: close windows, settle every open promise with `closed_by_user`, clear timers.
* `login.js`: port of the spike script (origin/TLS bar, popup pane with `nsIBrowserDOMWindow`), ids renamed, `window.zftLogin.setLabels`, buttons wired to `args.onEvent`; set the title `Sign in: <origin>` from `ZFT.l10n`-equivalent message `zotero-fulltext-login-title` + origin; show
  `login-ca-blocked`/`login-challenge-hint` text through `setLabels({hint})`; secure/not-secure text via `login-secure`/`login-not-secure`.
* Tests (`login.test.mjs`): `decide` table; `open` with a fake `Services.ww.openWindow` returning a scripted fake window (fake browser with `currentURI`, fake `loadURI`, event listeners) and a scripted fake `host.loginState`: landed, pdf, skip, handoff (asserts `Zotero.launchURL` called with the entry URL only for http(s)),
  timeout and keep-waiting extension (fake timers), refused with code, challenge hint, scope cancellation closes the window, second concurrent open for the same realm rejected, `update` changes the label, `warmUp`.

## Edge cases
Window closed by the user during `load`; `loadURI` throws (close the window, finish `closed_by_user`, log); page actor not ready (treat html as ''); multiple popups; main window absent (no parent for prompts: pass null); the progress window opened while Zotero is quitting
(`closeAll` must beat it); very long titles; RTL not required.

## Acceptance (runnable)
```
node --test tests/js/progress.test.mjs tests/js/confirm.test.mjs tests/js/login.test.mjs
node --test tests/js/load.test.mjs tests/js/static.test.mjs tests/js/ftl.test.mjs
```
The XHTML files stay well-formed (`static.test.mjs` checks) and every element id used by the window scripts exists in the markup.
