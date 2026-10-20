# WP js-zotero-io: Zotero API adapters (HTTP, browser, PDF capture, translators, OA lookup, save)

Read first: `docs/design/plugin-js.md` sections 1, 6, 7, 9; the header comment of every file you own (they hold the verified Zotero 10.0.6 API notes, with file:line
references into `.ref/omni-app/chrome/content/zotero/`); `research/02-zotero-download-internals.md`, `research/03-zotero-runtime-probe.md`, `research/08-gap-fill.md`.
`ZFT.bodies`, `ZFT.wasmHost`, the effect runner and the UI are written in parallel: use their declared signatures; your tests inject fakes.

## Files you own
`plugin/content/{net,pdf-capture,browser,translators,zotero-oa,save}.js`, tests `{net,pdf-capture,browser,translators,zotero-oa,save}.test.mjs`.
Every function takes a `ctx.scope` (`ZFT.util.CancelScope`) and must stop its I/O when it is cancelled, then reject with `OpError('cancelled')`.
Never throw anything but `OpError` out of an adapter (wrap unknown errors as `internal`); never leave a timer, listener, HiddenBrowser or temp file behind on ANY path.

## 1. net.js
Contract: `request(req: HttpReq, {scope, sink: 'memory'|'file', path?}) -> RawResponse` (types in the header).
Memory sink: `Zotero.HTTP.request(req.method, req.url, {headers: buildHeaders(req), body: req.body, responseType: 'arraybuffer', timeout: req.timeout_ms, successCodes: false, errorDelayMax: 0,
noRetryOnThrottle: true, followRedirects: req.follow_redirects, cancellerReceiver: (cancel) => offCancel = scope.onCancel(cancel), requestObserver: (xhr) => {...progress cap...}})`.
Result: `status = xhr.status`, `final_url = xhr.responseURL || req.url`, headers from `xhr.getAllResponseHeaders()` filtered by `RESPONSE_HEADERS` (lower-case names, values cut at 512), `body = new Uint8Array(xhr.response)`.
Unfollowed redirects return a fake object (`{status, getResponseHeader}` only): detect it (`xhr.responseURL === undefined`), build headers from `getResponseHeader('location'|'retry-after'|...)`, resolve `location` against
`req.url` with `new URL`, `body = new Uint8Array(0)`, `final_url = req.url`.
File sink: `Zotero.HTTP.download(req.url, path, {headers, timeout: req.timeout_ms, successCodes: false, errorDelayMax: 0, noRetryOnThrottle: true, cancellerReceiver, onProgress(total, contentLength)})` ->
`Response`: `status`, `final_url = response.url`, `headers` from `response.headers`. Enforce `req.max_bytes` in `onProgress` (and from `contentLength` up front): record `abortReason = 'too_large'`, cancel, and map the resulting
`TimeoutException` to `OpError('too_large')`. Track the TRUE abort cause (`abortReason` variable, `scope.cancelled`, JS backstop) because download() reports every abort as a timeout.
Non-2xx on the file sink: nothing is written; read up to `INSPECT_TEXT_MAX` of `response.body` (reader loop, cancel the rest) into `errorBody`, `path: null`.
`buildHeaders`: drop `DROPPED_REQUEST_HEADERS` (case-insensitive); for `req.ua === 'plain'` set `User-Agent: Zotero.VersionHeader.getPlainFirefoxUA()`; keep everything else.
`classifyError(e, info)`: by `e.name`/instance: `TimeoutException` -> `timeout` unless `info.abortReason`/`info.scope.cancelled`; `CancelledException` -> `cancelled`; `BrowserOfflineException` -> `offline`;
`xhr.status === 0` with a security failure (`info.xhr.channel.securityInfo.errorCode` or message matching `/SSL|TLS|certificate/i`) -> `tls`; anything else with a network flavour -> `network`; else `internal`.
JS backstop: `setTimeout(timeout_ms + TIMEOUT_BACKSTOP_MS)` cancels the request and rejects `timeout`; always cleared in `finally`.
Tests (fake `Zotero.HTTP` object recording options and scripting outcomes): option set exactly as above; UA policy; header whitelist; fake redirect object; cap enforcement; abort-cause mapping (cancel vs cap vs timeout);
backstop with fake timers; scope cancelled before start rejects without calling HTTP; every error class mapping; file-sink non-2xx path.

## 2. pdf-capture.js
Implement the ref-counted dispatcher exactly as the header describes (one handler object, `addHandlers('application/pdf', handler, true)` when the first capture starts, `removeHandlers('application/pdf', handler)`
when the last ends; removal idempotent; `Zotero.MIMETypeHandler` resolved at call time). `onStartRequest(name, contentType, channel)`: find `topFrameElement` via
`channel.notificationCallbacks.getInterface(Ci.nsILoadContext).topFrameElement`, fallback `channel.loadGroup.notificationCallbacks...`; remember `channel -> browser` in a WeakMap.
`onContent(blob, name, contentType, channel)`: if the mapped browser belongs to a live capture, resolve that capture's promise with `blob` (first PDF only) and return `true`, else return `false`.
`capture(browser).promise` resolves `null` after `dispose()`; `disposeAll()` resolves all. Tests: fake MIMETypeHandler recording add/remove; two overlapping captures (handler added once, removed once, each gets only its own PDF);
dispose twice; unknown channel returns false; capture after disposeAll works again.

## 3. browser.js
Implement `load(req, {scope, host})` per the header (HiddenBrowser quirks and the settle algorithm). Pseudocode:
acquire a browser slot (`MAX_BROWSERS`, FIFO, released in `finally`); `const {HiddenBrowser} = ChromeUtils.importESModule('chrome://zotero/content/HiddenBrowser.mjs')` (call time; missing -> `OpError('unsupported')`);
`hb = new HiddenBrowser(req.ua === 'plain' ? {customUserAgent: Zotero.VersionHeader.getPlainFirefoxUA()} : {})`; `await hb._createdPromise`; optional `cap = ZFT.pdfCapture.capture(hb._browser)`; add a
`nsIWebProgressListener` (`ChromeUtils.generateQI` available in tests via a fake) with `onLocationChange(progress, request, location, flags)` ignoring non-top-level, `LOCATION_CHANGE_SAME_DOCUMENT` and `about:blank`,
recording `lastChange = now` and `trail` (`trailEntry`, de-duplicated consecutive, max `URL_TRAIL_MAX`); `hb.load(req.url).catch(() => {})` WITHOUT awaiting its meaning; loop every `POLL_MS`: PDF captured -> done;
`now - lastChange >= req.settle_ms` and `await host.urlClass(hb.currentURI.spec)` not in `req.hold_while` -> settled; deadline `req.timeout_ms` -> settled if any navigation happened else `OpError('timeout')`.
After settle (no PDF): if `req.want_page`, `await hb.getPageData(['documentHTML','channelInfo'], {timeout: PAGE_DATA_TIMEOUT_MS})`; failures here leave `page_html: null`. `finally`: `cap?.dispose()`, remove the listener, `hb.destroy()`, release the slot.
Tests with a fake HiddenBrowser module installed on the context (`ChromeUtils.importESModule` fake) driven by a script of `{atMs, url}` location events using fake timers: settle after quiet period; hold_while defers settle; PDF capture
ends early; timeout with and without navigation; `load()` rejecting/returning false is ignored; cancel destroys everything; slot queue (4th browser waits); `trailEntry` examples (`https://a.b/c/d?x=1#h` -> `a.b/c/d`).

## 4. translators.js and zotero-oa.js
Per the headers. translators: concurrency limit `MAX_CONCURRENT` (queue), `TRANSLATE_TIMEOUT_MS` race (fake timers), `parsePage` removes `<noscript>` (test with a minimal fake DOMParser/document exposing `querySelectorAll('noscript')`
and `remove()`), any failure -> `{found: null, ms}`, hit mapping `{url, mime: mimeType, title}`. zotero-oa: maps `{url, pageURL, version}` to `{url, page_url, version}`, drops empty entries, passes `timeout: LOOKUP_TIMEOUT_MS`,
throws `OpError('unsupported')` when the API is missing, maps thrown errors through `ZFT.net.classifyError`, honours cancel (reject `cancelled`, ignore the late result).

## 5. save.js
Per the header. Order of operations in `savePdf`: validate `req` and `isStorableUrl(req.url_to_store)`; `item = ZFT.items.getItem(req.item)` else `parent_gone`; library editable else `not_editable`; parent still has no PDF
(`await item.loadDataType('childItems')`, `numFileAttachmentsWithContentType('application/pdf') === 0`) else `already_has_pdf`; resolve the staged file from `ctx.bodies.get(req.body.handle)` (must be a file-backed pdf body
with `dir`/`path`); trim prefix if `req.trim_prefix > 0`; `title = titleForVersion(req.version)`; `base = Zotero.Attachments.getFileBaseNameFromItem(item, {attachmentTitle: title})`; `filename = await Zotero.File.rename(path, base + '.pdf')`
(returns the final name); `attachment = await Zotero.Attachments.createURLAttachmentFromTemporaryStorageDirectory({directory: dir, libraryID: item.libraryID, parentItemID: item.id, filename, title, url: req.url_to_store, contentType:
'application/pdf'})`; on success `ctx.bodies` entry is marked consumed (`release` is the engine's job, but the directory no longer exists: make `BodyStore.release` tolerate a missing dir) and the dir is unregistered from `ZFT.state`;
return `{attachment_key: item.libraryID + '/' + attachment.key, filename}`. On any failure remove the dir and rethrow as `OpError` (`io` for filesystem errors). `isStorableUrl` rejects `login?qurl=`, `/redirector/`, `SAMLRequest`, the EZproxy suffix pattern
`*.kcl.idm.oclc.org` generalised as a host whose first label contains `-` and whose suffix equals `ZFT.config.getPref('institution.ezproxy_suffix')`. `attachFile(itemID, win)`: `Zotero.Utilities.Internal` FilePicker via
`Cc['@mozilla.org/filepicker;1']` (mode open, filter PDF), then `Zotero.Attachments.importFromFile({file, libraryID, parentItemID})`.
Tests with fake Zotero.Attachments/File/Items/IOUtils: happy path argument shapes; every error kind; trim_prefix rewriting (`%PDF-` offset 7 -> file starts with `%PDF-`); cleanup on failure; title mapping; url guard table.

## Acceptance (runnable)
```
node --test tests/js/net.test.mjs tests/js/pdf-capture.test.mjs tests/js/browser.test.mjs tests/js/translators.test.mjs tests/js/zotero-oa.test.mjs tests/js/save.test.mjs
node --test tests/js/load.test.mjs tests/js/static.test.mjs
```
Real-Zotero verification (HiddenBrowser, MIME handler, `HTTP.download`) belongs to the harness integration step; note in your PR which assumptions only the harness can confirm.
