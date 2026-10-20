# 03: Empirical probe of the Zotero 10.0.6 plugin runtime

**Date:** 2026-10-20. **Host:** Windows 11. **Zotero:** 10.0.6 (`appBuildID 20261007153950`),
Gecko/Firefox **140.17.0 ESR** (`platformBuildID 20261001165425`), x86_64-msvc.
**Method:** throwaway Zotero instances (fresh temp profile and temp data dir) running a probe
plugin. They were started by a new reusable harness, and the user's running Zotero was never
touched. **Raw data:** `research/03-probe-result.json` (primary run plus comparison runs) and
`.ref/harness-runs/<run>/`.

## TL;DR verdict

1. **Rust to WebAssembly works inside a Zotero 10 plugin, in every JavaScript context
   tested.** The contexts were the plugin bootstrap sandbox, the main window, the shared
   system ES-module global, ChromeWorker, plain Worker, and a null-principal sandbox. A real
   82 KB Rust `cdylib` (serde_json, hand-written ABI, zero imports) compiled and ran end to
   end in all of them. Sync compile took 2-6 ms, and 2000 JSON round trips took 45-137 ms.
   Every WebAssembly pref is at its default (`javascript.options.wasm=true`,
   `wasm_trustedprincipals=true`, baseline and optimizing JIT on). *Confidence: high
   (empirical).*
2. **wasm-bindgen output also works** (`--target web` imported with
   `ChromeUtils.importESModule`). There are two gotchas: cross-compartment init objects, and
   the bootstrap sandbox has no `console` (see section 6). *High.*
3. **The best way to load the `.wasm` from a packed XPI** is
   `fetch(rootURI + 'x.wasm')` followed by `WebAssembly.instantiateStreaming` or
   `arrayBuffer() + instantiate`. Both work for `jar:` (XPI) and `file:` (unpacked)
   rootURIs. `instantiateStreaming` over a plugin-registered **chrome://** URL fails, because
   chrome:// serves `application/wasm;charset=UTF-8`. `IOUtils.read` only works for unpacked
   plugins. *High.*
4. **Cookie-jar sharing, the core design assumption, holds.** A login page loaded in a
   Zotero viewer window (`Zotero.openInViewer`) set an HttpOnly session cookie. That cookie
   was stored unpartitioned in the default jar and was then sent automatically by
   `Zotero.HTTP.request`, `Zotero.Attachments.downloadFile`, `Zotero.Attachments.addFileFromURLs`,
   `Zotero.HTTP.processDocuments` and `fetch(…, {credentials:'include'})`. It was **not**
   sent by plain `fetch()` (default credentials) or by an XHR without credentials. All of
   this ran against a local `127.0.0.1` server inside the throwaway instance.
   *High (empirical, local simulation).*
5. **`onMainWindowLoad` never fires for the initial main window**, headless or GUI. The
   window is fully loaded before plugin `startup()` runs. It does fire, in headless and GUI
   alike, for main windows opened later. Plugins must initialise already-open windows from
   `startup()`. *High.*
6. `Zotero.BrowserDownload` **does not exist** in 10.0.6. The equivalent is
   `Zotero.BrowserRequest` (`downloadPDF`, `downloadPDFViaViewer`, `clearChallenge`, …).
   `Zotero.Attachments.addFileFromURLs` **forces every resolver URL to https**. *High.*

---

## 1. What ran

| Run | Install | Mode | Probe scope | Result after | Exit |
|---|---|---|---|---|---|
| run01-headless-proxy | unpacked dir, proxy file | headless | base probe | 26.3 s | clean (0) |
| run02-headless-proxy | unpacked dir, proxy file | headless | + 2nd window + cookie/attach sim | 51.1 s | clean |
| run03-headless-xpi | packed XPI | headless | no cookie sim | 17.7 s | clean |
| run04-gui-xpi | packed XPI | **GUI window** | no cookie sim | 21.3 s | clean |
| run05-timeout-kill | noop plugin | headless | never writes result | timeout 25 s | killed 7 own PIDs (exit 2) |
| **run06-headless-xpi-full** (primary) | packed XPI | headless | everything + wasm-bindgen | 51.7 s | clean |

Safety checks held in every run (`harness-summary.json`): the user's Zotero PIDs
`162400, 166424, 148420` were alive before and after, and `profiles.ini`/`installs.ini`
hashes were unchanged. No files under `%APPDATA%\Zotero`, `%LOCALAPPDATA%\Zotero` or
`~/Zotero` changed after the first run started. Only processes from each run were stopped,
and only in run05.

## 2. The harness (`tools/zotero-harness/`)

- `run.ps1 -PluginDir <dir|.xpi> -PluginId <id> [-ResultFile] [-TimeoutSec 120] [-Headless(:$false)] [-ExtraPrefs @{}]`.
  The full description is in `tools/zotero-harness/README.md`.
- Launch: `zotero.exe -profile "<tmp>\profile" -no-remote -datadir "<tmp>\data" -ZoteroDebugText -headless`
  via `Start-Process -PassThru`, with stdout and stderr redirected. **`-ZoteroDebugText`
  output reaches the redirected stdout on Windows** (about 0.6-0.76 MB per run). `-datadir`
  is a Zotero flag (`omni-app/modules/BrowserContentHandler.sys.mjs:476`). When it is set,
  Zotero skips data-dir migration checks (`xpcom/zotero.js:372-379`), and a custom data dir
  that does not exist is a hard startup error (`xpcom/dataDirectory.js:142-158`), so the
  harness creates it first.
- Plugin install: a proxy file `profile/extensions/<id>` containing the absolute dir path,
  or `<id>.xpi` copied in. Both loaded on first start with `extensions.autoDisableScopes=0`.
  No restart was needed, and there was no launcher-process hand-off: the `Start-Process`
  PID is the main process.
- Prefs that matter for safety, all of which I verified in source:
  - `extensions.zotero.firstRun2=false`. Otherwise `zoteroPane.js:640-646` loads
    `ZOTERO_CONFIG.START_URL`, which opens a page in the user's real browser.
  - `extensions.zoteroWinWordIntegration.skipInstallation=true` and
    `extensions.zoteroOpenOfficeIntegration.skipInstallation=true`. Otherwise a fresh profile
    auto-installs `Zotero.dotm` into the Word STARTUP folder and the LibreOffice extension
    (`omni-app/resource/word-processor-plugin-installer.mjs:64-98`;
    `integration/word-for-windows/resource/installer.mjs`).
  - `extensions.zotero.httpServer.enabled=false`, so port 23119 stays with the user's Zotero
    (default `true`, `defaults/preferences/zotero.js:1742-1743`).
  - `app.update.auto=false` etc. This is safe because Zotero patched
    `PER_INSTALLATION_PREFS_PLATFORMS = []` (`omni-gre/modules/UpdateUtils.sys.mjs:16`), so
    `app.update.auto` is an ordinary profile pref and nothing install-wide is written. The
    update timer cannot fire during a short run anyway: on first registration it sets
    `lastUpdateTime = now` (`UpdateTimerManager.sys.mjs:367-378`).
- Remaining network traffic from Zotero itself at startup: one proxy check to
  `https://zoteroproxycheck.s3.amazonaws.com/test` (stdout log line 106 of run01).

## 3. WebAssembly per JS context (probe item 1 and 2)

Every context ran the same 13 steps from `probe-plugin/content/wasm-tests.js`: `eval`,
`new Function`, validate, sync `new WebAssembly.Module/Instance`, async `instantiate`, async
`compile`, foreign (cross-compartment) bytes passed directly, `Memory.grow`, and four Rust
steps. **All 13 passed in every context** (run06, `03-probe-result.json`).

| Context | How it was created | `typeof WebAssembly` | Rust sync compile | 2000 Rust calls |
|---|---|---|---|---|
| Bootstrap sandbox | Zotero's `Cu.Sandbox(systemPrincipal)` (`xpcom/plugins.js:138-185`) | object | 6 ms | 102 ms |
| Main window (`zoteroPane.xhtml`) | `Services.scriptloader.loadSubScript(url, win)` | object | 2 ms | 45 ms |
| Shared system ESM global | `ChromeUtils.importESModule('chrome://zfprobe/content/probe-module.mjs')` | object | 5 ms | 63 ms |
| ChromeWorker | `new ChromeWorker('chrome://zfprobe/content/probe-worker.js')` | object | 3 ms | 45 ms |
| Worker | `new Worker(<same chrome:// URL>)` | object | 3 ms | 137 ms |
| Null-principal sandbox | `Cu.Sandbox(null)` + `evalInSandbox` | object | n/a (tiny module `add(3,4)=7`) | n/a |

- WebAssembly namespace keys (all contexts): `CompileError, Exception, Global, Instance, JSTag, LinkError, Memory, Module, RuntimeError, Table, Tag, compile, compileStreaming, instantiate, instantiateStreaming, validate`.
- An `i64` export returns a JS **BigInt**. `Memory.grow` from 1 to 256 pages worked.
- `eval` and `new Function` work in every context, including the system-principal sandbox,
  even though `security.allow_eval_with_system_principal=false`. Any glue that needs eval
  would therefore run. Generated wasm-bindgen glue does not use eval anyway.
- Main window CSP: `document.csp.policyCount = 0`, so there is no `wasm-unsafe-eval` concern.

### WASM-related prefs (probe item 4; all at their defaults, none user-set)

| Pref | Value |
|---|---|
| `javascript.options.wasm` | **true** |
| `javascript.options.wasm_trustedprincipals` | **true** (wasm allowed for system principal) |
| `javascript.options.wasm_baselinejit` | **true** |
| `javascript.options.wasm_optimizingjit` | **true** |
| `wasm_verbose` / `wasm_gc` / `wasm_tail_calls` / `wasm_relaxed_simd` / `wasm_js_promise_integration` | false |
| `wasm_exnref`, `wasm_memory64`, `wasm_multi_memory`, `wasm_js_string_builtins`, `wasm_caching`, `wasm_lazy_tiering`, `wasm_simd_avx` | true |
| `javascript.options.shared_memory` | true |
| `network.cookie.cookieBehavior` | **5** (`BEHAVIOR_REJECT_TRACKER_AND_PARTITION_FOREIGN`, default; `defaults/preferences/zotero.js:943`) |
| `privacy.partition.network_state` | true |
| `security.allow_eval_with_system_principal` / `..._in_parent_process` | false / false |
| `extensions.zotero.findPDFs.resolvers` | `"[]"` |

The source defaults are `omni-gre/greprefs.js:391-394` (wasm, trustedprincipals, verbose,
baselinejit). The full `getChildList('javascript.options.wasm')` (31 prefs) is in the JSON
under `prefs.wasmChildList`.

## 4. The real Rust module (probe item 3)

- Crate: `tools/zotero-harness/probe-wasm/` (`cdylib`, `serde_json = "1"`, release profile
  `opt-level="s"`, `lto=true`, `codegen-units=1`, `panic="abort"`, `strip=true`). Built with
  `cargo build --release --target wasm32-unknown-unknown`, with no wasm-opt. The result is
  **82,402 bytes** with 0 imports. Exports: `memory`, `zf_abi_version`, `zf_add`,
  `zf_alloc`, `zf_dealloc`, `zf_process_json`. The initial memory is 1,179,648 B (18 pages).
- Hand-written ABI (`probe-wasm/src/lib.rs`):

```rust
#[no_mangle] pub extern "C" fn zf_alloc(len: usize) -> *mut u8;              // align 1
#[no_mangle] pub unsafe extern "C" fn zf_dealloc(ptr: *mut u8, len: usize);
#[no_mangle] pub unsafe extern "C" fn zf_process_json(ptr: *const u8, len: usize) -> u64;
// returns out_ptr | (out_len << 32); caller frees with zf_dealloc(out_ptr, out_len)
```

```js
// JS side (worked unchanged in all 5 contexts)
const p = e.zf_alloc(input.length);
new Uint8Array(e.memory.buffer, p, input.length).set(input);
const r = BigInt.asUintN(64, e.zf_process_json(p, input.length)); e.zf_dealloc(p, input.length);
const op = Number(r & 0xffffffffn), ol = Number(r >> 32n);
const out = JSON.parse(new TextDecoder().decode(new Uint8Array(e.memory.buffer, op, ol).slice()));
e.zf_dealloc(op, ol);
```

Output observed:
`{"abi":1,"doi_normalized":"10.1000/abc.def","echo_keys":["doi","numbers","text"],"engine":"rust-wasm32-unknown-unknown","ok":true,"sum":6.5,"text_chars":11,"text_upper":"HÉLLO WÖRLD"}`.
The error path also worked: `{"ok":false,"error":"invalid JSON: key must be a string at line 1 column 2"}`.
Memory did not grow over 2000 calls, so nothing leaked.

## 5. Ways to load the `.wasm` bytes (bootstrap scope)

| Method | Unpacked dir (`rootURI = file:///…/probe-plugin/`) | **Packed XPI** (`rootURI = jar:file:///…/<id>.xpi!/`) |
|---|---|---|
| `fetch(rootURI + 'probe_core.wasm')` | OK, `content-type: application/wasm` | **OK, `application/wasm`** |
| `fetch('chrome://zfprobe/content/probe_core.wasm')` | OK, `application/wasm;charset=UTF-8` | OK (same) |
| `WebAssembly.instantiateStreaming(fetch(rootURI…))` | **OK** | **OK** |
| `WebAssembly.instantiateStreaming(fetch(chrome://…))` | **FAIL** `TypeError: WebAssembly: Response has unsupported MIME type 'application/wasm;charset=utf-8' expected 'application/wasm'` | FAIL (same) |
| `compileStreaming(new Response(buf, {headers:{'Content-Type':'application/wasm'}}))` | OK | OK |
| `IOUtils.read(<path from rootURI>)` | OK | **FAIL by construction**: a `jar:` URI has no file path, and IOUtils cannot read inside the zip |
| `XMLHttpRequest` `responseType='arraybuffer'` (chrome:// or rootURI) | OK | OK |
| `Zotero.HTTP.request('GET', chrome://…, {responseType:'arraybuffer'})` | OK | OK |
| `NetUtil.newChannel({uri, loadUsingSystemPrincipal:true}).open()` (sync) | OK | OK |
| Base64 inside a `.js` (`loadSubScript` + `atob`) | OK (SHA-256 matched) | OK |

The same chrome:// streaming failure also happened inside ChromeWorker and Worker. `fetch`
of the chrome:// URL itself works in workers and in the ESM global.
**Recommendation:** use `fetch(rootURI + 'core.wasm')` and then `instantiateStreaming`.
Alternatively use `arrayBuffer()` and `WebAssembly.instantiate(bytes, {})`, which is
MIME-agnostic. The first `fetch` took about 214-231 ms (cold, during startup) and later
fetches took about 10-20 ms.

## 6. wasm-bindgen 0.2.126 glue (extra; relevant if the Rust core uses bindgen)

Crate `tools/zotero-harness/probe-wasm-bindgen/` (`wasm-bindgen = "=0.2.126"`, which must
match the CLI). The `.wasm` is 73,201 bytes and the `--target web` glue is about 8.5 KB.

| Test (run06, packed XPI) | Result |
|---|---|
| `ChromeUtils.importESModule('chrome://zfprobe/content/wbg-web/probe_wbg.js')` then `await m.default()` (fetches `probe_wbg_bg.wasm` via `import.meta.url`) | **OK**. `add`, `process_json` and `Result<_, JsValue>` errors (thrown as JS `"input was empty"`) all worked. The glue logged its own `console.warn` (instantiateStreaming MIME fallback, from the chrome:// charset) and fell back to `instantiate`. The extension `.js` is accepted by `importESModule`. |
| Same glue as `.mjs` (a separate module instance), `m.initSync({module: bytes})` with the object created in the plugin sandbox | **FAIL** `TypeError: first argument must be an ArrayBuffer or typed array object @probe_wbg.mjs:246`. The glue checks `Object.getPrototypeOf(arg) === Object.prototype`, which is false for an object from another compartment, so it treats the whole object as the module. |
| `m.initSync(Cu.cloneInto({module: bytes}, Cu.getGlobalForObject(m)))` | **OK** |
| `--target no-modules` via `loadSubScript(url, obj)` | `obj.wasm_bindgen` is **undefined**, because the top-level `let wasm_bindgen` stays lexical |
| `--target no-modules` via `new Function(src + '\nreturn wasm_bindgen;')()` then `initSync({module})` (in the sandbox) | **OK** |

Gotcha: the **bootstrap sandbox has no `console`**, while the ESM global, windows and
workers do. Any glue path that calls `console.*` would throw a ReferenceError if evaluated in
the sandbox. Prefer the ESM route.

## 7. Globals in each context (probe item 5)

Only rows that differ are shown. Everything else (`WebAssembly`, `fetch`, `Response`,
`Request`, `Headers`, `TextEncoder`, `TextDecoder`, `URL`, `URLSearchParams`, `crypto`
(+ `crypto.subtle`), `XMLHttpRequest`, `Blob`, `File`, `FileReader`, `atob`/`btoa`,
`setTimeout`, `Atomics`, `BigInt`, `IOUtils`, `PathUtils`, `ChromeUtils`, `Worker`, `dump`)
is present in all five contexts.

| Global | Bootstrap sandbox | ESM global | Main window | ChromeWorker | Worker |
|---|---|---|---|---|---|
| `AbortController` / `AbortSignal` | **undefined** | function | function | function | function |
| `structuredClone` | undefined | function | function | function | function |
| `queueMicrotask` | undefined | undefined | function | function | function |
| `ReadableStream`/`WritableStream` | undefined | function | function | function | function |
| `console` | **undefined** | object | object | object | object |
| `DOMParser` / `XMLSerializer` | function | function | function | undefined | undefined |
| `SharedArrayBuffer` | function | function | undefined | function | undefined |
| `Services`/`Components`/`Cc`/`Ci`/`Cu`/`Zotero`/`Localization` | present | present | present | absent | absent |
| `ChromeWorker` | function | function | function | function | undefined |

The bootstrap sandbox's list matches Zotero's `wantGlobalProperties` (`atob, btoa, Blob,
crypto, CSS, ChromeUtils, DOMParser, fetch, File, FileReader, TextDecoder, TextEncoder, URL,
URLSearchParams, XMLHttpRequest`) plus injected `Zotero, ChromeWorker, IOUtils, Localization,
PathUtils, Services, Worker, XMLSerializer, setTimeout…` (`xpcom/plugins.js:141-180`).
`Cc`/`Ci`/`Cu`/`Components` exist because the sandbox is system-principal.
`ChromeUtils.import` is shimmed to warn and redirect to `importESModule`
(`plugins.js:186-197`).

These functional checks passed in the bootstrap sandbox: `DOMParser` (parsed
`<meta name="citation_pdf_url">`), `TextEncoder/Decoder` round trip, `URL` parsing an
EZproxy-style `login?url=` query, `crypto.subtle.digest('SHA-256')`, `fetch('data:…')`,
`new Response(bytes, {headers})`, `IOUtils.writeUTF8` and `PathUtils.tempDir`.
`new AbortController()` threw `ReferenceError` in the sandbox, but
`new (Zotero.getMainWindow().AbortController)()` works, and so does the ESM global's
`AbortController`. `Zotero.HTTP.request` has its own `cancellerReceiver` option
(`xpcom/http.js:197-198`).

## 8. Zotero API shapes (probe item 5)

Every row was captured live (run06 `apis.*`, which has `Function.length` and the first 300
chars of `toString()`) and cross-checked against the local source.

| API | Exists | `length` | Signature (from `toString`) |
|---|---|---|---|
| `Zotero.version` | `"10.0.6"` | | |
| `Zotero.MenuManager` | yes, keys `registerMenu, unregisterMenu, updateMenuPopup, _menuManager` | | `registerMenu(options)` returns the menu key (string) or `false`; `unregisterMenu(id)` returns a boolean |
| `Zotero.PreferencePanes.register` | yes (+`unregister`, `pluginPanes`, `builtInPanes`) | 1 | `async function (options)`, which throws unless `pluginID` and `src` are set |
| `Zotero.ProgressWindow` | yes, constructor | 0 | `function (options = {})`; instance methods `ItemProgress, Translation, addDescription, addLines, changeHeadline, close, show, startCloseTimer` |
| `Zotero.HTTP.request` | yes | 2 | `async function (method, url, options = {})` wraps `_requestInternal` with retry on server error |
| `Zotero.HTTP.download` | yes | 2 | `async function (uri, path, options = {})`; HTTP(S) only |
| `Zotero.HTTP.processDocuments` | yes | 2 | `async function (urls, processor, options = {})` |
| `Zotero.Attachments.addFileFromURLs` | yes | 2 | `async function (item, urlResolvers, options = {})` |
| `Zotero.Attachments.downloadFirstAvailableFile` | yes | 3 | `async function (urlResolvers, path, options)` |
| `Zotero.Attachments.addAvailableFile` | yes | 1 | `async function (item, options = {})`, which equals `addFileFromURLs(item, getFileResolvers(item, options.methods))` |
| `Zotero.Attachments.addAvailableFiles` | yes | 1 | `async function (items, options = {})` (per-domain queue, `sameDomainRequestDelay`) |
| `Zotero.Attachments.getFileResolvers` | yes | 3 | `function (item, methods, automatic)`; methods default `['doi','url','oa','custom']` |
| `Zotero.Attachments.importFromURL` | yes | 1 | `async function (options)` |
| `Zotero.Attachments.downloadFile` | yes | 2 | `async function (url, path, options = {})` |
| `Zotero.Attachments.canFindFileForItem` | yes | 1 | `function (item)` |
| `addAvailablePDF` / `getPDFResolvers` / `cleanAttachmentURI` | deprecated shims | | warn and forward |
| `Zotero.BrowserDownload` | **undefined** | | |
| `Zotero.BrowserRequest` | **yes** | | keys `CHALLENGE_URLS, PLAIN_UA_HOSTS, clearChallenge, clearChallengeInViewer, downloadPDF, downloadPDFViaViewer, getEntryForURL, …`; `downloadPDF(url, path, options = {})` uses a `HiddenBrowser` (`xpcom/browserRequest.js:28,114,567,616`) |
| `Zotero.Proxies` | yes | | keys `autoRecognize, disableByDomain, getPotentialProxies, hosts, init, properToProxy(url, onlyReturnIfProxied), proxyToProper(url, onlyReturnIfProxied), proxies, save(proxy), transparent, …`; `Zotero.Proxy(row)` constructor |
| `Zotero.openInViewer` | yes | 2 | `function (uri, options)`; options include `onLoad` and `userContextId` (`xpcom/zotero.js`, basicViewer `standalone/basicViewer.js:38-59`) |
| `Zotero.getMainWindow` / `getMainWindows` / `openMainWindow` | yes | 0 | `xpcom/zotero.js:77-90,1122-1128` |
| `Zotero.Utilities.Internal.getOpenAccessPDFURLs` | yes | 1 | `async function (doi, options = {})` |
| `Zotero.Utilities.Internal.getFileFromDocument` | yes | 1 | `async function (doc)` runs web translators on a Document |
| `Zotero.ItemTreeManager` / `ItemPaneManager` / `Plugins` | yes | | see JSON for key lists |

**Live MenuManager test:** `registerMenu({menuID:'zfprobe-test-menu', pluginID, target:'main/library/item', menus:[{menuType:'menuitem', l10nID:'…', onCommand(){}}]})`
returned `"probe\@zotero-fulltext\.local-zfprobe-test-menu"`, and `unregisterMenu(thatId)`
returned `true`. Valid targets include `main/library/item` and `main/library/collection`
(both grouped into a submenu when crowded), `main/menubar/tools`, and others
(`xpcom/pluginAPI/menuManager.js:28-57`). Labels come only from Fluent `l10nID`/`l10nArgs`;
there is no plain `label` option (`menuManager.js:113-180`, `440-470`).

## 9. `onMainWindowLoad`, headless, and startup timing (probe item 6)

Milliseconds from process start (run06 headless; run04 GUI in brackets):
`createTopLevelWindow` 489 [416], `firstLoadURI` 1802 [2722], **plugin `startup()` 2997 [4370]**,
`Zotero.uiReadyPromise` resolved about 1.2 s [0.7 s] after `startup()`.

- When `startup()` ran, `Zotero.getMainWindow()` already returned
  `chrome://zotero/content/zoteroPane.xhtml` with `readyState: "complete"`. This was true in
  **all runs, headless and GUI**.
- `onMainWindowLoad` **did not fire for that initial window** in any run. The probe waited
  10 s after `uiReadyPromise`.
- `Zotero.openMainWindow()` called after startup made **`onMainWindowLoad` fire** (about
  3 s, in both headless and GUI). Closing that second window left the original one.
- Reason in code: `Zotero.Plugins.init` awaits every plugin's `startup()` and only then
  calls `Services.wm.addListener(mainWindowListener)`, whose `onOpenWindow` handles only
  windows opened afterwards (`xpcom/plugins.js:65-124`). `Plugins.init` itself runs from
  `Zotero.initComplete` (`xpcom/zotero.js:424-444`). `uiReadyPromise` is resolved by
  `ZoteroPane.itemsView.waitForLoad().then(() => Zotero.uiIsReady())`
  (`zoteroPane.js:1620`), independently of plugin startup, so awaiting it inside `startup()`
  cannot deadlock. It does delay other plugins.
- Headless works fully: the window is 1000x600, `ZoteroPane`, `itemsView` and
  `#zotero-itemmenu` exist, and viewer windows with remote `<browser>`s load pages. The only
  noise is `RenderCompositorSWGL failed mapping default framebuffer` on stdout.

Pattern to use:

```js
const inited = new WeakSet();
function initWindow(win) { if (inited.has(win)) return; inited.add(win); /* add UI */ }
async function startup({ id, version, rootURI }) {
  // ... registerChrome, MenuManager.registerMenu, load wasm (do not block on UI) ...
  Zotero.uiReadyPromise.then(() => Zotero.getMainWindows().forEach(initWindow));
}
function onMainWindowLoad({ window }) { initWindow(window); }
```

## 10. Cookie jar and attachment pipeline (local simulation, beyond the brief)

The probe started `HttpServer` from `chrome://remote/content/server/httpd.sys.mjs` on
`127.0.0.1:<random>` inside the throwaway process. No external hosts were involved. It
simulated an EZproxy-like flow: `/protected.pdf` returns 302 to `/login-form` unless the
request carries the `sess=ok` cookie, and `/login` sets `Set-Cookie: sess=ok; Path=/; HttpOnly`.
Results from run06 (run02 was identical):

| # | Step | Result |
|---|---|---|
| 01 | `Zotero.HTTP.request` to `/set-cookie?name=zh` | 200; cookie `zh` stored, `partitionKey ""`, `userContextId 0` |
| 02 | `fetch(/echo)` default credentials | **no cookie sent** |
| 03 | `fetch(/echo, {credentials:'include'})` | `zh=1` sent |
| 04 | `Zotero.HTTP.request(/echo)` | `zh=1` sent |
| 05 | sandbox `XMLHttpRequest` (no `withCredentials`) | **no cookie sent** |
| 06-07 | cookie set by `fetch(…include)` then read by `Zotero.HTTP.request` | `zh=1; fe=1` (one shared jar) |
| 08 | `Attachments.downloadFile(protected.pdf, {enforceFileType:true})` before login | throws `Downloaded file was not a supported type (PDF or EPUB)` (got the login-form HTML) |
| 09 | `Zotero.openInViewer(base + '/login')` | page loaded in a **remote** `<browser>` (`isRemoteBrowser: true`, no `usercontextid`); viewer title "simulated SSO done"; HttpOnly `sess=ok` stored with `partitionKey ""` |
| 10 | `downloadFile(protected.pdf)` after login | **OK**, 193-byte PDF (`%PDF-1.4`); request carried `zh=1; fe=1; sess=ok` |
| 11 / 12 | `fetch(protected.pdf)` with `include` / default | 200 `application/pdf` / redirected to login-form (no cookie) |
| 13-14 | `new Zotero.Item('journalArticle')` with DOI and URL, then `getFileResolvers(item)` | `[{pageURL:'https://doi.org/10.5555/zfprobe.1', accessMethod:'doi'}, {pageURL:'<item url>', accessMethod:'url'}, [async function (oa)]]` |
| 15 | `addFileFromURLs(item, [{url: …/protected.pdf, accessMethod:'zfprobe-direct'}])` | **attachment created**: `linkMode 1` (imported_url), `contentType application/pdf`, title "Full Text", file in `data/storage/<KEY>/zfprobe test article.pdf` (auto-renamed) |
| 16 | `addFileFromURLs(item2, [{pageURL: …/landing}])` where the landing page has `<meta name="citation_pdf_url">` | **attachment created** from the `citation_pdf_url` (title "Full Text PDF"); the PDF request carried `Referer: <landing URL>` and the session cookie |
| 17 | `addFileFromURLs` on plain `http://127.0.0.1…` **without** `Zotero.test` | `false` after about 30 s; the URL was rewritten to `https://` and never reached the http server |
| 18 | `Zotero.HTTP.processDocuments(/doc, doc => doc.title)` | `["processDocuments page"]`; its `Set-Cookie` was stored in the same jar |

Steps 15 and 16 set `Zotero.test = true` only around the call. This works around the forced
https rewrite in `downloadFirstAvailableFile` (`xpcom/attachments.js:2060-2064`, "Force URLs
to HTTPS"), which the local http server would otherwise fail.

The User-Agent for all mechanisms was
`Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0 Zotero/10.0.6`.

The source explains both observations. `Zotero.HTTP.request` uses the global Firefox cookie
jar unless `{anon:true}` (`xpcom/http.js:276-283`), sets
`channel.forceAllowThirdPartyCookie = true` (`http.js:330-331`), and supports isolated jars
through `{userContextId}` from `Zotero.HTTP.newCookieContext()` (`http.js:159-175`,
`307-310`). `downloadFile` calls `Zotero.HTTP.download`, and on InvalidPDF or 403 it falls
back to `Zotero.BrowserRequest.downloadPDF` only for URLs matching `CHALLENGE_URLS`
(`attachments.js:1176-1218`).

## 11. Implications for zotero-fulltext

1. **Architecture confirmed: a thin JS shell plus a Rust/WASM core.** Keep all network I/O in
   JS (`Zotero.HTTP.request` or `fetch` with `credentials:'include'`) and keep the Rust core
   pure, with no imports (`wasm32-unknown-unknown`).
2. **Ship the wasm inside the XPI** and load it with `fetch(rootURI + 'core.wasm')`, then
   `WebAssembly.instantiateStreaming(...)`. Fall back to `arrayBuffer()` +
   `WebAssembly.instantiate`. Do not use streaming over chrome:// URLs, and do not use
   `IOUtils.read(rootURI)`.
3. **Where to run the core:** the bootstrap sandbox is fine for small synchronous calls (sync
   compile of 80 KB took about 6 ms). For HTML parsing of large landing pages, a ChromeWorker
   keeps the UI responsive. `DOMParser` is not available in workers, so the Rust side must
   parse HTML itself (it already must for landing-page PDF-link extraction). If the core uses
   wasm-bindgen, load it as an ES module (`ChromeUtils.importESModule`). Pass init objects
   with `Cu.cloneInto(obj, Cu.getGlobalForObject(module))`, or call the default init with
   the URL.
4. **The KCL login flow works with Zotero's own primitives.** Open the EZproxy login URL with
   `Zotero.openInViewer(url)` (default container). Once the user completes SSO, the session
   cookie sits in the default jar and every later Zotero download call carries it. Detect
   "logged in" by polling `Services.cookies` for the proxy host's cookie, or by watching the
   viewer's `<browser>` URL.
5. **Prefer `Zotero.Attachments.addFileFromURLs(item, resolvers)`** for the final attach. It
   handles the temp download, PDF/EPUB sniffing (`enforceFileType`), the pageURL-to-translator
   PDF discovery, attachment creation and renaming. Supply our own resolvers as
   `{url, pageURL, accessMethod, referrer}` objects or async functions returning arrays.
   **All resolver URLs are forced to https**, which is fine because EZproxy and OA URLs are
   https.
6. Menu: `Zotero.MenuManager.registerMenu({target:'main/library/item', …})` works from
   `startup()` before any window exists. Labels need a Fluent FTL in the plugin's `locale/`.
7. Initialise window UI from `startup()` (section 9), not only from `onMainWindowLoad`.

## 12. What failed, and why

| Failure | Cause | Matters? |
|---|---|---|
| `instantiateStreaming(fetch(chrome://…))` in sandbox, workers, and wasm-bindgen default init | chrome:// responses are `application/wasm;charset=UTF-8`; the spec requires exactly `application/wasm` | Avoid it; use rootURI or `instantiate` |
| `IOUtils.read` in the XPI run | `jar:` URI, no filesystem path | Avoid for XPI |
| `new AbortController()` in sandbox | not in Zotero's `wantGlobalProperties` | Borrow it from the window or ESM, or use `cancellerReceiver` |
| wasm-bindgen `initSync({module})` with a sandbox-created object | prototype-identity check across compartments | Use `Cu.cloneInto` |
| no-modules glue `obj.wasm_bindgen` undefined | top-level `let` in the subscript | Use `new Function(…)` or the ESM target |
| `addFileFromURLs` with an http URL (step 17) | forced https (`attachments.js:2060-2064`) | Fine for real use; costs about 30 s on a dead https endpoint |
| `onMainWindowLoad` not firing for the first window | lifecycle order (section 9) | Handle it in `startup()` |
| Sandbox XHR/`fetch` without credentials sent no cookies | credentials mode `same-origin` from the system principal | Use `Zotero.HTTP.request` or `credentials:'include'` |

## 13. Reproduce

```powershell
pwsh -NoProfile -File C:\Users\abora\zotero-fulltext\tools\zotero-harness\build-probe.ps1 -Offline
pwsh -NoProfile -File C:\Users\abora\zotero-fulltext\tools\zotero-harness\run.ps1 `
  -PluginDir "C:\Users\abora\zotero-fulltext\tools\zotero-harness\out\probe@zotero-fulltext.local.xpi" `
  -PluginId probe@zotero-fulltext.local -TimeoutSec 240
```

## Sources

Local extracted Zotero 10.0.6 (`.ref/omni-app`, `.ref/omni-gre`) is the source of truth. It
matches GitHub tag `10.0.6` of zotero/zotero (the tags list includes `10.0.6`, and a `10.0`
branch exists). Paths below are relative to `.ref/omni-app/chrome/content/zotero/` unless
they start with `omni-app/` or `omni-gre/`.

- Plugin loader: `xpcom/plugins.js:65-124` (init, startup loop, wm listener),
  `133-215` (sandbox and scope), `223-290` (`_callMethod`, `func.call(scope, params, reason)`).
- Startup order: `xpcom/zotero.js:198-224`, `263-264`, `372-379`, `424-444`;
  `zoteroPane.js:640-646` (firstRun2), `1620` (uiIsReady).
- Main windows: `xpcom/zotero.js:77-90`, `1122-1128`.
- HTTP: `xpcom/http.js:159-175`, `180-219`, `222`, `276-283`, `307-310`, `330-331`, `636`.
- Attachments: `xpcom/attachments.js:1176-1218`, `1250-1255`, `1275-1490`, `1512`, `1847`,
  `1872`, `1978-2328` (https forcing at `2060-2064`, pageURL to `getFileFromDocument` at
  `2287-2289`).
- BrowserRequest: `xpcom/browserRequest.js:28`, `84`, `114`, `322`, `567`, `616`.
- MenuManager: `xpcom/pluginAPI/menuManager.js:28-57`, `77-107`, `113-232`, `816-845`.
- Debug and CLI: `xpcom/debug.js:53-75`;
  `omni-app/modules/BrowserContentHandler.sys.mjs:462-476` (`-ZoteroDebugText`, `-datadir`).
- Data dir: `xpcom/dataDirectory.js:56-81`, `142-158`.
- Prefs: `omni-app/defaults/preferences/zotero.js` lines 338-340 (eval), 943
  (cookieBehavior 5), 1483 (`xpinstall.signatures.required=false`), 1587, 1592-1593, 1742-1744,
  1841-1848; `omni-gre/greprefs.js:391-394` (wasm).
- Updates: `omni-gre/modules/UpdateUtils.sys.mjs:16`, `225-227`;
  `omni-gre/modules/UpdateTimerManager.sys.mjs:367-378`.
- Word/LibreOffice installer: `omni-app/resource/word-processor-plugin-installer.mjs:64-98`.
- Viewer: `standalone/basicViewer.js:38-59`; `standalone/basicViewer.xhtml:247-254`.
- Reference plugin (manifest and bootstrap pattern, `aomStartup.registerChrome`):
  `.ref/xpis/zotmoov@wileyy.com/bootstrap.js`, `manifest.json` (`strict_max_version "10.*"`).
- Empirical: `research/03-probe-result.json`;
  `.ref/harness-runs/run0{1..6}-*/{result.json,harness-summary.json,logs/zotero-stdout.log}`.
