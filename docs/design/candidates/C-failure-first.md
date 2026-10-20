# zotero-fulltext v1 design: failure-first, Rust sans-IO engine with a JS effect executor

**Concurrency:** Three layers. (1) A deterministic, synchronous sans-IO Rust engine (wasm instance A, main thread) decides what may run now: per-item state machines, a per-realm institutional session machine, a global single-flight interactive gate (at most one login or challenge window), per-host queues with token buckets, global caps (4 active items, 16 admitted, 10 HTTP in flight, 2 institutional in flight, 1 per publisher host, at least 5 s plus seeded jitter between institutional PDFs to a host, 2 HiddenBrowsers), persisted licence budgets, retry, hedge and timeout policy, and wake-up timers. It takes time and randomness only from events, so replays are byte-identical. (2) A JS async effect executor (main thread) runs the engine's effects as independent promises: Zotero.HTTP.request/download with its own first-byte, idle and total timers and byte caps, HiddenBrowser loads with one multiplexed PDF MIME handler, login and challenge windows, DB saves, and translators. It owns a hierarchical CancelScope tree (run > item > effect > canceller) plus a generation counter for shutdown and disable, dedupes effect ids, and enforces defence-in-depth hard caps. (3) A ChromeWorker hosting wasm instance B analyses all untrusted bytes (classify, extract, OA parsers, PDF check) behind a 3 s watchdog with terminate and respawn. Within an item, OA lookups fan out in parallel with a 10 s deadline and an early start on a tier-A candidate. Attempts are sequential first-success-wins, with one hedge for OA only (never for institutional). Items parked on a host queue or on the login gate release their active slot, so a slow publisher or an absent user cannot stall the batch. OA work never waits on the login gate. User cancel takes effect in JS immediately and in the engine on the next step. An engine trap is recovered by checkpoint plus journal replay that fails only the offending item.

**Rust share:** About 70-75% of non-test code by lines (roughly 8-9k Rust against about 3.3k JS) and about 95% of the decision logic. Rust owns identifier normalisation, every OA response parser, page classification, the landing-page PDF extractor and publisher rules, EZproxy and OpenAthens URL algorithms and URL storage cleaning, PDF validation, candidate ranking, both state machines, the login gate, scheduling, rate limits, licence budgets, and retry, hedge and timeout policy. JS is limited to what only JS can do inside Zotero: Zotero and Gecko I/O (HTTP, HiddenBrowser, PageData, MIME capture, windows, item DB reads and saves, IOUtils), translators, UI, lifecycle and cancellation plumbing. Putting the policy in Rust as a sans-IO engine is the failure-first choice: every failure path becomes a natively testable state transition under a virtual clock, and it serves the user's "Rust wherever possible" requirement without asking Rust to do I/O it cannot do from a zero-import wasm module.

**Self-identified weaknesses:**
- The sans-IO event/effect protocol is a large boundary surface. Bugs in the JS executor or schema mismatches are not caught by cargo tests, and the Node fake-Zotero mocks may diverge from real Zotero behaviour.
- Checkpoint plus journal replay depends on strict engine determinism (BTreeMap, seeded PRNG, no clock reads). A single hidden source of nondeterminism silently breaks trap recovery. The replay property test reduces this risk but does not prove it absent.
- Login-success detection (URL leaves IdP hosts, page is stable, Rust classify) is unproven until the user's B1 run. The manual 'I've finished signing in' button is a crutch.
- Several Zotero internals are load-bearing and could change in 10.x point releases: HiddenBrowser.mjs, the PageData actor, MIMETypeHandler, getFileFromDocument and createURLAttachmentFromTemporaryStorageDirectory. Each has a fallback, but the fallbacks are weaker.
- It is unverified whether Zotero.HTTP.download (fetch) honours a custom User-Agent header, and whether a WebAssembly.Module can be posted to a ChromeWorker. Both have fallbacks that cost memory or startup time.
- Translators and DOMParser still run on the main thread and can issue their own requests outside the Rust rate limiter. They are mitigated by caps and restricted use, not eliminated.
- The licence budget numbers (30/h, 80/day, 300 MB/h, 5 s spacing) are conservative inferences from OCLC defaults and Jisc licence text, not KCL-confirmed. They are persisted per profile and are not a security control.
- The one-prompt-per-realm-per-run policy can frustrate a user who declined early and later wants institutional access in the same run. Recovery requires 'Retry with sign-in'.
- ProgressQueue has no per-item cancel and limited status richness, so the separate report window adds custom UI code to maintain.
- The XMP DOI identity check only catches wrong-article PDFs that carry XMP metadata, so a wrong PDF without XMP can still be attached.
- Title-only matching through Crossref is strict to avoid false positives. It will miss papers with differing titles or missing years, and OpenAlex search needs a user key.
- Hedging and the ChromeWorker analyser add complexity. A watchdog restart drops and requeues in-flight analysis for unrelated items, which can cost a few seconds.

---

# zotero-fulltext v1: failure-first design

Status: proposal (phase "design"). Author angle: start from what breaks in practice and design inward.
Evidence base: `research/SUMMARY.md` (cited as [S §x]) and briefs `01`..`08` (cited as [0N §x]). Zotero 10.0.6 source checked where marked "src".

---

## 0. Decisions in one screen

| # | Decision | Why (failure-first) |
|---|---|---|
| D1 | **All decisions live in a deterministic, sans-IO Rust engine** (per-item state machines, the institutional session machine, the single-flight login gate, scheduler, rate limits, licence budgets, retry/hedge policy, ranking). JS is a thin async **effect executor**. | Every failure in section 1 becomes a pure state transition that `cargo test` can drive with a virtual clock and scripted responses. JS inside Zotero is the hardest place to test; keep it dumb. |
| D2 | **Untrusted bytes are analysed in a separate wasm instance inside a ChromeWorker** (classify, extract, OA parsers, PDF check). The engine instance runs on the main thread and only ever sees our own structured events. | A parser panic or infinite loop on hostile HTML cannot freeze Zotero's UI (worker has a watchdog and is `terminate()`d) and cannot destroy engine state. |
| D3 | **Engine state is recoverable**: JS keeps a checkpoint plus an event journal; after a wasm trap it re-instantiates, imports the checkpoint, replays the journal tail, and injects `EngineFault` for the poisoning event. Only the item that triggered the bug fails. | "wasm trap mid-batch" is survivable instead of losing the batch [07 §4, S (b)]. |
| D4 | **One global engine per Zotero session**, shared by all runs. | Rate limits, licence budgets, the login gate and duplicate-item detection must be global, not per click. |
| D5 | **Institutional route = OpenAthens redirector in a HiddenBrowser (plain Firefox UA)**, then HTTP with the same jar and UA; EZproxy only for confirmed-configured hosts; campus/direct first. | KCL EZproxy covers ~19 hosts and none of the big medical publishers [04 §2.4]; SAML auto-POST needs a browser [04 §4.2]; Cloudflare at Wiley SSO needs the plain UA [08 §0.3]. |
| D6 | **Single-flight login gate**: at most one visible interactive window (login or challenge) at a time; items that hit login park *without holding an item slot*; OA work never waits on the gate; at most one prompt per realm per run unless the session demonstrably expired after a successful login. | One prompt serialises many waiters; the user is never shown a stack of windows; an absent user does not stall the batch. |
| D7 | **Degrade, never dead-end**: custom login window -> silent HiddenBrowser reuse -> "Open in browser" hand-off (redirector, LibKey) -> "Attach downloaded PDF". Every non-success has an actionable terminal state. | B1 (embedded Entra SSO) is still unproven [S (h) B1, 08 §0.1]. |
| D8 | **Own save path**: PDFs stream to a fresh `storage/tmp-XXXXXX` dir via `Zotero.HTTP.download`, are validated, then become the attachment via `createURLAttachmentFromTemporaryStorageDirectory` with an un-proxied URL (src `attachments.js:682-736`, `:2853`). | Zero-copy, low memory, no proxied/signed URLs stored [S C4, 02 §7]; temp dirs are swept if Zotero dies mid-run. |
| D9 | Licence limits are **enforced in Rust and persisted** (rolling hour/day counters in `state.json`). | Limits must survive restarts and cannot depend on JS discipline [04 §8.2]. |

---

## 1. Failure catalogue (the design driver)

Each row names the detection signal, the response, who owns it, and the native scenario test (`S##`, section 7) or harness test (`H##`).

| # | Failure | Signal | Response | Owner | Test |
|---|---|---|---|---|---|
| F1 | SSO login needed mid-batch | HiddenBrowser on redirector settles on `IdpLogin` (Entra `/<tenant>/saml2`, OpenAthens `/auth/`) [04 §6.5] | Session -> `NeedsLogin`; item parks in `AwaitingLogin` (slot released); engine stops starting new redirector runs; one `OpenLoginWindow` at the first waiter's entry URL; other waiters join; OA continues | engine | S01 |
| F2 | Session expiry mid-batch (EZproxy idle 120 min, OpenAthens ~8 h [04 §3.4, §4.3]) | An attempt bounces to `IdpLogin`/`EzproxyNeedLogin` while session is `Active{verified}` | Session -> `Expired` -> `NeedsLogin`; prompt again (max 2 prompts per realm per run); item re-parks once; a second bounce for the same item -> hand-off | engine | S02 |
| F3 | Entra Conditional Access refusal | Login window page classified `IdpLogin{ca_error:"AADSTS53000"...}` or text "You can't get there from here" [08 §5] | `LoginRefused{code}`; all waiters -> `NeedsUserAction(ca_refused)`; report suggests opt-in Windows SSO pref, VPN, Open in browser | engine + login-window.js | S03 |
| F4 | OpenAthens "Please wait..." | `OpenAthensWait` [04 §4.3] | In HiddenBrowser: page JS polls, we wait (settle 3 s, cap 30 s). In HTTP tier: engine schedules HEAD every 2.5 s up to 30 s, accepts 201/302+Location | engine | S04 |
| F5 | EZproxy needhost / suspend | `EzproxyHostNotConfigured` (302 to login host same path, then 404) / `EzproxyError{suspend}` (200 from `/login` without `EZproxyForm`, body mentions suspend/limit) [04 §3.5, §6.5] | not-configured: learn host (7 d cache), skip EZproxy for it. suspend: **institutional stop for all items**, persisted cooldown 24 h, report explains | engine | S05 |
| F6 | Cloudflare/Akamai challenge, PMC proof of work, SD "Preparing your download", Thieme JS wall | `Challenge{vendor, interactive}` [06 §5.2] | T1 -> T2 HiddenBrowser (plain UA, PDF capture). Interactive -> challenge window via the same gate if `interactive.challengeWindow`. Never solve CAPTCHAs. 2 challenge outcomes per host per run -> host `Walled` | engine | S06 |
| F7 | Hard block (CF WAF "Sorry, you have been blocked", Akamai Access Denied) | `Blocked{vendor}` | Host walled for run; no retry; next candidate; institutional: counts as block signal (3 -> suspend institutional for run) | engine | S06 |
| F8 | `citation_pdf_url` returns HTML (Springer/LWW no entitlement, PMC PoW) [06 §2.2] | PDF attempt: sniff fails; body re-classified | Same-article landing -> `AccessDenied` for this host in this phase; challenge -> F6; never re-extract a page already visited (per-item page set) | engine + analyser | S07 |
| F9 | ePDF / eReader shells | `ReaderShell` (`/doi/epdf/`, `/doi/reader/`, `.pdf+html`) | One rewrite (Wiley `/pdfdirect/{doi}?download=true`, Atypon/SAGE `/doi/pdf/`), never download the reader | analyser | fixture tests |
| F10 | Slow publishers (Wiley abstract 25-60 s [08 §3.6]) | Host profile `slow` | first-byte 60 s, total 120 s; UI "waiting on onlinelibrary.wiley.com (slow)"; OA hedge to another host after 8 s | engine | S09 |
| F11 | HTTP 429 | status 429 / `Retry-After` | OA API: honour Retry-After once if <=60 s else drop source for run. Institutional: **stop institutional for run**, 30 min persisted cooldown, no retry [04 §8.2] | engine | S10 |
| F12 | Partial download | `%%EOF` missing in last 2 KiB, or bytes < Content-Length | Discard; one retry on the same URL if per-item fetch budget allows; signed URLs are never retried after 60 s, re-derive from landing | engine + analyser | S11 |
| F13 | Non-PDF bytes on a PDF URL | `fx_pdf_check.is_pdf=false` | Classify body (header-only if non-2xx) and route as landing/challenge/login | analyser | S12 |
| F14 | Wrong article (OpenAlex merge, supplement, other-DOI link) [05 §0.8, 06 §2.2] | Extractor rejects other-DOI/PII URLs; PDF XMP `prism:doi`/`dc:identifier` mismatch | Reject candidate; rank down merged-record locations | analyser | fixture + S14 |
| F15 | wasm trap in engine | `WebAssembly.RuntimeError` in `fx_engine_step` | Mark poisoned, re-instantiate, import checkpoint, replay tail minus poisoning event, inject `EngineFault` -> that item `Failed(internal)`; 3rd trap in a run -> fail the run cleanly | core-glue.js | N04, S15 |
| F16 | Panic or hang in analyser | worker error / 3 s watchdog | Terminate + respawn worker (re-instantiate from cached Module), requeue other pending requests once, return `AnalyserFault` -> candidate unusable | analyser-client.js | N05 |
| F17 | Zotero shutdown / plugin disable / upgrade mid-batch | `shutdown(reason)` | Generation counter bump; abort all requests, destroy browsers, close windows, terminate worker, remove MIME handler; no save after bump; temp dirs swept on next start | bootstrap.js + executor | H07 |
| F18 | Item already has a PDF | snapshot `numFileAttachmentsWithContentType('application/pdf')>0`; `getBestAttachmentState().exists===false` | `Skipped(HasPdf)` / `Skipped(PdfNotDownloaded)` before any network; **re-checked right before save** -> `Skipped(HasPdfLate)` and discard | engine + zotero-items.js | S16 |
| F19 | No DOI | ids empty after normalisation | PMID/PMCID -> Europe PMC; arXiv id -> arXiv; else Crossref `query.bibliographic` (free) with strict match (title ratio >=0.95, year +-1, first-author surname); OpenAlex search only with a user key (costs 10x [05 §2.3]); else `NotFound(NoIdentifiers)` | engine + analyser | S17 |
| F20 | Group library | library kind `group` | OA allowed. Institutional: `inst.groupLibraries=ask` -> one confirm per run (licence 4.1.4/4.1.6 [04 §8.2]); `never` -> hand-off. Read-only (`filesEditable=false`) -> `Skipped(ReadOnly)` | engine + UI | S18 |
| F21 | Duplicate concurrent runs on the same item | engine already owns item key | Second request is a no-op for that item (shared progress row shows the live state); Zotero's own Find Full Text racing us is caught by F18 re-check | engine | S19 |
| F22 | User cancel | ProgressQueue `cancel` | `Cancel{run}`: JS cancels its scopes immediately (does not wait for engine); engine emits `Abort` for in-flight effects, `Discard` for temp files; items in `Saving` finish atomically | engine + executor | S20, H06 |
| F23 | Offline / network drop | `BrowserOfflineException`, status 0 on many hosts | Engine `NetworkDown`: pause new fetches, probe every 15 s for 2 min, then fail remaining with `Failed(offline)` | engine | S21 |
| F24 | Login window ignored / user away | gate timeout 300 s | `LoginTimedOut`; waiters -> `NeedsUserAction(login_timeout)` with "Retry with sign-in" in report; OA results unaffected | engine | S01b |
| F25 | Login "succeeded" but cookies not used | first resumed attempt still bounces to IdP | Session `Ineffective` for run; no more prompts; waiters hand-off; report links troubleshooting | engine | S22 |
| F26 | Licence budget exhausted mid-batch | rolling counters (30/h, 80/24 h, 300 MB/h) | Remaining institutional items -> `NeedsUserAction(budget)` with reset time | engine | S23 |
| F27 | Parent item deleted/trashed, library made read-only, disk full | save pre-check / `saveTx` / `IOUtils` errors | `Cancelled(ParentGone)` / `Failed(ReadOnly|Io)`; temp dir removed | zotero-items.js | H08 |
| F28 | Translator hang or side requests | 15 s race | Drop translator result; translators run only for OA pages, and for institutional pages only when Rust found nothing; each run counts 1 against the host budget | translators.js + engine | H05 |
| F29 | MIME handler collision with Zotero's own BrowserRequest | handler sees a PDF from a browser that is not ours | One multiplexed handler, registered only while >=1 waiter exists, matches `topFrameElement`, returns false for foreign browsers [01 §7.1] | browser.js | H04 |
| F30 | Redirect / refresh loops | hop count, tried-URL set incl. query [05 §5.4] | XHR follows natively; meta refresh <=15 s, not in `<noscript>`, max 3 per page; per-item tried set | engine + analyser | fixture |
| F31 | Proxied/signed/SAML URL stored | `storable_url()` | Always store un-proxied; strip `X-Amz-*`/token params; signed-asset hosts store the landing/DOI URL | analyser | unit |
| F32 | UA mismatch kills `cf_clearance` | host marked Plain | Per-host sticky UA: once a host is Plain in this session, every request and browser load to it uses `getPlainFirefoxUA()` [08 §6.2]; never call `registerPlainUAHost` (global) | engine | S06 |

---

## 2. Architecture and the JS<->Rust boundary

### 2.1 Diagram

```
 Zotero main thread (bootstrap sandbox, system principal)                    ChromeWorker
+-----------------------------------------------------------------+   +---------------------------+
| bootstrap.js   lifecycle, generation counter, registerChrome    |   | analyser-worker.js        |
|     |                                                           |   |  wasm instance B          |
| main.js  ---> ui-menu.js / ui-progress.js / ui-report.js        |   |  (same compiled Module)   |
|     |                                                           |   |  fx_analyse, fx_pdf_check |
| executor.js  (async; runs effects, owns CancelScopes)           |   |  untrusted bytes only     |
|   +- net.js          Zotero.HTTP.request / HTTP.download        |   +-------------^-------------+
|   +- browser.js      HiddenBrowser pool (<=2), PageData,        |  postMessage  | transfer ArrayBuffer
|   |                  one multiplexed PDF MIME handler           |---------------+ watchdog 3 s ->
|   +- login-window.js login.xhtml / challenge window (gate = 1)  |                terminate + respawn
|   +- zotero-items.js snapshot, eligibility re-check, save, sweep|
|   +- translators.js  getFileFromDocument on fetched page (15 s) |
|   +- state-store.js  <profile>/zotero-fulltext/state.json       |
|     ^ events (JSON, journaled)          | effects (JSON)        |
|     |                                   v                       |
| engine-host (core-glue.js): checkpoint + journal + replay       |
|   wasm instance A: fx_engine_step  (sans-IO, deterministic)     |
|     item FSMs | session FSM | interactive gate | scheduler      |
|     rate limits | licence budgets | ranking | retry/hedge policy|
+-----------------------------------------------------------------+
```

Loading follows the proven path [S C3, 03 §5, 07 §3]: `aomStartup.registerChrome` -> `loadSubScriptWithOptions('chrome://zotero-fulltext/content/core-glue.js', {target, ignoreCache:true})` -> `fetch('chrome://zotero-fulltext/content/fulltext_core.wasm').arrayBuffer()` -> `WebAssembly.compile` once -> cached `Module`. Instance A is created on the main thread; the Module is posted to the worker for instance B (fallback: the worker fetches the chrome:// URL itself, which works in workers [03 §5]).

### 2.2 ABI (hand-written, zero imports)

One `cdylib` (`fulltext-wasm`), `wasm32-unknown-unknown`, no imports, `ABI_VERSION = 2` (spike was 1). All pointers/lengths are wasm32 `u32`; results are packed `u64 = (ptr << 32) | len`, read in JS as BigInt [03 §4].

| Export | Signature | Instance | Notes |
|---|---|---|---|
| `memory` | | both | JS re-creates views after **every** call (`memory.grow` detaches) [07 §3.1] |
| `fx_abi_version` | `() -> u32` | both | glue refuses to run unless `== 2` |
| `fx_build_info` | `() -> u64` | both | `{"version","git","html":"html5gum"}` |
| `fx_init` | `()` | both | installs the panic hook writing `LAST_PANIC` |
| `fx_alloc` / `fx_free` | `(len) -> ptr` / `(ptr,len)` | both | exact-capacity `Vec<u8>` |
| `fx_last_panic` | `() -> u64` | both | static buffer, allocation-free, **never freed by JS** |
| `fx_engine_new` | `(ptr,len) -> u64` | A | `EngineInit` JSON (config + persisted state + PRNG seed) |
| `fx_engine_step` | `(ptr,len) -> u64` | A | `StepInput` -> `StepOutput` |
| `fx_engine_export` | `() -> u64` | A | full checkpoint (serde of `Engine`) |
| `fx_engine_import` | `(ptr,len) -> u64` | A | restore checkpoint |
| `fx_engine_view` | `() -> u64` | A | read-only diagnostics view (redacted) |
| `fx_analyse` | `(meta_ptr,meta_len, body_ptr,body_len) -> u64` | B | two-buffer raw call: JSON meta + raw body bytes (2-2.3x faster than JSON-embedded HTML [07 §3.1]) |
| `fx_pdf_check` | `(head_ptr,head_len, tail_ptr,tail_len) -> u64` | B | first 256 KiB + last 256 KiB of the file |
| `fx_sniff_pdf` | `(ptr,len) -> u32` | B | fast `%PDF-` within 1 KiB |

Memory ownership: JS allocates inputs with `fx_alloc`, Rust only borrows them for the call, JS frees them after the call. Rust allocates outputs (`Box<[u8]>` leaked); JS decodes a copy, then `fx_free(out_ptr, out_len)`. Input caps are checked in JS before `fx_alloc` and again in Rust: HTML 2 MiB, API JSON/XML 4 MiB, step input 1 MiB. After a huge input, instance B is recycled because wasm memory never shrinks [07 §3.1].

Envelope (every JSON export): `{"ok": <T>}` or `{"err": {"code": "bad_json|bad_utf8|too_large|bad_url|bad_state|unsupported", "message": "..."}}`. Errors are values, never traps. `bad_state` from the engine is treated by JS like a trap (F15). Release profile: `opt-level="z"`, `lto`, `codegen-units=1`, `panic="abort"`, `strip`; clippy denies `unwrap_used, expect_used, panic, unreachable, todo, unimplemented, string_slice` [07 §1.2, §4].

### 2.3 Message protocol (engine)

`StepInput = {"v":1, "now": <monotonic ms, Cu.now()>, "wall": <Date.now()>, "events": [Event...]}`. `StepOutput = {"fx_next": n, "effects": [Effect...]}`. The engine never reads a clock or randomness itself: time comes in events, jitter comes from a PRNG seeded in `StartRun`, and all maps are `BTreeMap`, so a replay of the same events yields byte-identical effects (property-tested, section 7).

Events (JS -> engine). Each has a `seq` assigned by the journal:

```json
{"seq":1,"t":"StartRun","run":"r1","opts":{"mode":"full","interactive":true,"seed":2718281,"selected":3},
 "items":[{"key":"1/ABCD1234","id":123,"lib":{"id":1,"kind":"user","files_editable":true,"name":"My Library"},
   "type":"journalArticle","title":"Outcomes after ...","date":"2022-09","creators":["Smith","Jones"],
   "fields":{"DOI":"https://doi.org/10.1097/SLA.0000000000005603","PMID":"","PMCID":"","archiveID":"",
             "url":"https://journals.lww.com/annalsofsurgery/...","extra":"PMID: 35815893"},
   "pdf":{"count":0,"best":"none","exists":null}}]}
{"seq":7,"t":"FetchDone","fx":2,"res":{"status":200,"final_url":"https://api.openalex.org/works/doi:10.1097/sla.0000000000005603",
   "ctype":"application/json","bytes":48211,"ms":640,"headers":{"x-ratelimit-remaining":"998"},
   "analysis":{"kind":"oa","source":"openalex","record_doi":"10.1097/sla.0000000000005603",
     "ids":{"pmid":"35815893","pmcid":null},"oa_status":"closed",
     "candidates":[{"landing":"https://doi.org/10.1097/sla.0000000000005603","pdf":null,"version":"published",
                    "license":null,"host_kind":"publisher","record_doi_match":true}]}}}
{"seq":9,"t":"FetchFailed","fx":3,"err":{"kind":"timeout_first_byte","ms":8000}}
{"seq":15,"t":"BrowserDone","fx":31,"res":{"final_url":"https://login.microsoftonline.com/8370cf14-.../saml2",
   "ms":6120,"analysis":{"class":{"IdpLogin":{"idp":"entra","ca_error":null}}}}}
{"seq":40,"t":"WindowDone","fx":32,"res":{"outcome":"landed","final_url":"https://journals.lww.com/annalsofsurgery/fulltext/...",
   "analysis":{"class":"Landing","candidates":[{"url":"https://journals.lww.com/annalsofsurgery/_layouts/15/oaks.journals/downloadpdf.aspx?an=00000658-202209000-00012",
   "signal":"HeadMeta:wkhealth_pdf_url","score":90}]}}}
{"seq":52,"t":"PdfChecked","fx":44,"res":{"is_pdf":true,"junk_prefix":0,"has_eof":true,"bytes":1834567,"xmp_doi":"10.1097/sla.0000000000005603"}}
{"seq":53,"t":"SaveDone","fx":45,"res":{"attachment":"1/QRST9876"}}
{"seq":54,"t":"SaveFailed","fx":46,"err":{"kind":"has_pdf_now"}}
{"seq":60,"t":"TimerFired","fx":12}
{"seq":61,"t":"Confirmed","fx":5,"answer":"oa_only"}
{"seq":70,"t":"Cancel","scope":{"run":"r1"}}
{"seq":71,"t":"EngineFault","about_seq":69,"item":"1/EFGH5678","panic":"panicked at crates/fulltext-core/src/rank.rs:212:9 ..."}
{"seq":72,"t":"AnalyserFault","fx":50,"why":"watchdog"}
```

Effects (engine -> JS). Every effect has an engine-assigned `fx` id; JS executes each id at most once:

```json
{"fx":2,"t":"Fetch","item":"1/ABCD1234","purpose":"oa:openalex","method":"GET",
 "url":"https://api.openalex.org/works/doi:10.1097/sla.0000000000005603","headers":{"Accept":"application/json"},
 "ua":"zotero","sink":"memory","max_bytes":4194304,"redirects":"follow",
 "timeouts":{"first_byte":8000,"idle":8000,"total":10000},"analyse":{"as":"oa:openalex","want":{"doi":"10.1097/sla.0000000000005603"}}}
{"fx":40,"t":"Fetch","item":"1/ABCD1234","purpose":"pdf","method":"GET","url":"https://journals.lww.com/.../downloadpdf.aspx?an=...",
 "headers":{"Referer":"https://journals.lww.com/annalsofsurgery/fulltext/..."},"ua":"plain","sink":"file",
 "max_bytes":157286400,"redirects":"follow","timeouts":{"first_byte":20000,"idle":30000,"total":180000},
 "analyse":{"as":"pdf","want":{"doi":"10.1097/sla.0000000000005603"}},"route":"institutional"}
{"fx":31,"t":"BrowserLoad","item":"1/ABCD1234","url":"https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1097%2FSLA.0000000000005603",
 "ua":"plain","capture_pdf":true,"settle_ms":3000,"timeout_ms":45000,"stop_on":["idp_login","challenge_interactive"]}
{"fx":32,"t":"OpenLoginWindow","realm":"openathens","entry_url":"https://go.openathens.net/redirector/kcl.ac.uk?url=...","waiting":3,"timeout_ms":300000}
{"fx":33,"t":"OpenChallengeWindow","item":"1/IJKL0001","url":"https://www.sciencedirect.com/science/article/pii/.../pdfft?...","ua":"plain","timeout_ms":120000}
{"fx":41,"t":"Abort","target":38,"reason":"hedge_lost"}
{"fx":12,"t":"SetTimer","after_ms":5300,"why":"host_gap:onlinelibrary-wiley-com"}
{"fx":44,"t":"CheckPdf","item":"1/ABCD1234","file":"ref:40"}
{"fx":45,"t":"Save","item":"1/ABCD1234","file":"ref:40","title_key":"attachment.fullText","junk_prefix":0,
 "url":"https://doi.org/10.1097/SLA.0000000000005603","route":"institutional","source":"lww"}
{"fx":46,"t":"Discard","file":"ref:38"}
{"fx":5,"t":"Confirm","kind":"institutional_batch","n":34,"cap":20}
{"fx":47,"t":"Ui","run":"r1","item":"1/ABCD1234","row":"processing","msg":["zotero-fulltext-status-trying",{"source":"journals.lww.com"}]}
{"fx":48,"t":"Persist","state":{"inst":{"pdf_log":[[1792500000000,1834567]],"suspended_until":0}}}
{"fx":99,"t":"RunFinished","run":"r1","summary":{"saved":7,"needs_you":2,"not_found":1,"skipped":3,"failed":0,"cancelled":0},
 "items":[{"key":"1/WXYZ5555","outcome":"NeedsUserAction","why":"login_declined",
   "handoffs":[{"kind":"openathens","url":"https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646"},
               {"kind":"libkey","url":"https://libkey.io/libraries/286/10.1002/bjs.11646"}],
   "trace":[{"src":"openalex","tier":"api","class":"ok","ms":512},{"src":"redirector","tier":"browser","class":"IdpLogin","ms":6120}]}]}
```

Analyser requests (main -> worker), with the body transferred, not copied:

```json
{"id":88,"op":"analyse","meta":{"as":"page","req_url":"https://doi.org/10.1002/bjs.11646","final_url":"https://academic.oup.com/bjs/article/...",
 "status":200,"headers":[["content-type","text/html; charset=utf-8"],["server","cloudflare"]],"want":{"doi":"10.1002/bjs.11646"},
 "config":{"ezproxy":{"login_host":"login.kcl.idm.oclc.org","suffix":"kcl.idm.oclc.org"}}},"body":"<ArrayBuffer>"}
```

The executor fills `analysis` in `FetchDone`/`BrowserDone`/`WindowDone` from the worker reply before the event enters the journal. So the journal holds compact derived data, never raw bodies, and replay never re-parses HTML.

### 2.4 Checkpoint, journal, replay (F15)

- `engine-host` calls `fx_engine_export()` at `StartRun`, at `RunFinished`, and every 64 events. It keeps that checkpoint plus the event journal since then.
- On a trap: `poisoned=true` -> `reset()` (0.2-0.9 ms [07 §4]) -> `fx_engine_import(checkpoint)` -> replay the journal minus the poisoning event -> append `EngineFault{about_seq, item}`. Effects re-emitted during replay have `fx <= highWater`; the executor drops them because they already ran. In-flight effects stay in flight and complete normally.
- If the poisoning event has no item (global), the run is failed with `Failed(internal)` and budgets are flushed from the last `Persist` effect, which JS mirrors.
- 3 traps in one run -> abort the run. `fx_last_panic` text goes to `Zotero.logError` and to the report's "Copy diagnostics".

---

## 3. Concurrency and async model

### 3.1 Layers

1. **Engine (Rust, synchronous, main thread).** Decides *what may run now*. It never blocks; each `step` is bounded (no unbounded loops: the scheduler walks finite queues). Typical step cost is well under 1 ms.
2. **Executor (JS, async, main thread).** Runs effects as independent promises. Concurrency is the fan-out of effects. It also enforces **defence-in-depth hard caps** (16 HTTP, 2 browsers, 1 window) that the engine should never hit; a hit is logged as an engine bug and answered with `FetchFailed{kind:"executor_cap"}`.
3. **Analyser (Rust in a ChromeWorker).** One worker, FIFO queue (<=32 pending), watchdog per request. Parallel to the main thread, so HTML parsing never janks the UI.

Why not async Rust: `wasm32-unknown-unknown` with zero imports has no threads and cannot call JS, so a Rust future could not await I/O. The sans-IO poll model (as in quinn-proto or h2) is the idiomatic Rust answer, and it is what makes virtual-time testing possible.

### 3.2 Limits (defaults; all in the engine unless noted)

| Resource | Limit | Basis |
|---|---|---|
| Active items (doing work) | 4 | [05 §6] |
| Admitted items (active + parked) | 16 | memory bound |
| Discovery lookups per item | all applicable in parallel (<=6) | [05 §6] |
| Global HTTP in flight | 10 | politeness + memory |
| api.openalex.org | 8 req/s, 4 concurrent | 10 req/s keyless [05 §2.3] |
| www.ebi.ac.uk (Europe PMC) | 4 req/s, 2 concurrent | [05 §6] |
| api.crossref.org | 4 req/s, 1 concurrent | [05 §6] |
| api.biorxiv.org | 1 req/s | [05 §6] |
| export.arxiv.org / arxiv.org | 1 per 3 s / 1 concurrent | arXiv ToU |
| pmc-oa-opendata.s3.amazonaws.com | 5 req/s | [05 §6] |
| services.zotero.org (via Zotero API) | 2 req/s, only if OpenAlex failed | private API |
| librarysearch.kcl.ac.uk (Alma CTO) | 1 concurrent, 1 req/s, 1 per item | [04 §4.5] |
| Publisher host, OA phase | 1 concurrent, >=1 s gap, 5 consecutive failures -> skip host | Zotero parity [02 §2.5] |
| **Institutional, global** | **2 concurrent**, >=2 s between page fetches | [04 §8.2] |
| **Institutional, per publisher host** | **1 concurrent; PDFs >=5 s + 0-3 s jitter apart** | [04 §8.2] |
| **Institutional budgets** | 20 items/run (confirm above 10), 30 PDFs/h, 80/24 h, 300 MB/h, <=4 fetches/item | [04 §8.2] |
| HiddenBrowsers | 2 | memory, content processes |
| Visible interactive windows | **1** (login or challenge) | gate D6 |
| Hedges per item | 1 extra attempt, OA only | 3.4 |

Fairness and back-pressure: items waiting on a host queue or on the login gate are **parked** and do not hold an active slot, so one slow publisher or an absent user cannot starve the batch (head-of-line blocking). Resumed items get priority over new ones. Engine order of service per step: (1) resumed-after-login waiters, FIFO; (2) active items round-robin; (3) admission of queued items. The engine coalesces wake-ups into one pending `SetTimer` (the earliest host gap or deadline). UI effects are coalesced by JS to <=10 Hz.

### 3.3 Timeouts (JS enforces; engine chooses per effect)

| Kind | first byte | idle | total | Notes |
|---|---|---|---|---|
| OA API | 8 s | 8 s | 10 s | discovery deadline 10 s per item |
| Landing page | 20 s | 15 s | 45 s | slow hosts: 60 / 30 / 120 s |
| PDF | 20 s | 30 s | 180 s | slow hosts first byte 60 s; cap 150 MB |
| HiddenBrowser load | settle 3 s quiet | | 45-60 s | Zotero's own pref default 60 s [08 §2.4] |
| Login window | | | 300 s | "Keep waiting" adds 300 s |
| Challenge window | | | 120 s | |
| Translator run | | | 15 s | cannot be cancelled; result ignored after |
| Analyser request | | | 3 s watchdog | terminate + respawn |
| Item active time | | | 6 min | parked time excluded |

`net.js` implements them itself: `Zotero.HTTP.request(..., {timeout:0, successCodes:false, errorDelayMax:0, noRetryOnThrottle:true, requestObserver, cancellerReceiver})`, where `requestObserver(xhr)` adds `readystatechange` (first byte) and `progress` (idle timer, byte cap) listeners. These options avoid Zotero's uncapped Retry-After loop (src `http.js:1595-1607`, [S (a)]). `HTTP.download` is used for `sink:"file"`; its `timeout` is connect + inactivity (src `http.js:669-742`), and `onProgress` calls our canceller at `max_bytes`. Because `download` reports every abort as `TimeoutException`, `net.js` records its own `abortReason` (`cancelled | byte_cap | total_timeout | shutdown`) so the engine gets the true cause.

### 3.4 Candidate attempts

- **Discovery** fans out all applicable lookups at once (OpenAlex singleton, Europe PMC, bioRxiv if `10.1101/`, Crossref when institutional is enabled, Unpaywall only with a user email, Zotero OA only if OpenAlex failed). PMC S3 metadata is chained when any source yields a PMCID. Discovery **settles** when every lookup is done, or the 10 s deadline fires, or a tier-A candidate exists (PMC S3 PDF, arXiv, medRxiv early path, Europe PMC `/api/getPdf`). Late lookups still merge into the ranked list; attempts not yet started are re-ordered.
- **Attempts are sequential per item** by default (politeness, licence). **Hedging (OA only):** if the current attempt has no first byte after 8 s, or its host is `slow`, and the next candidate is on a different host, start it too. The first validated PDF wins and the loser gets `Abort{hedge_lost}`. Institutional attempts are never hedged (that would double-count licence budget and break 1-per-host).
- **First-success-wins** inside an item; the remaining candidates are dropped with `Discard`.
- Per-item fetch budgets: OA phase 12 fetches (API lookups excluded); institutional phase 4 (a browser load counts as 1) [04 §8.2].

### 3.5 Cancellation

```
RunScope (per run) ── ItemScope (per item) ── EffectScope (per fx) ── canceller fns
                                                  (Zotero.HTTP cancellerReceiver,
                                                   HiddenBrowser.destroy, win.close)
GlobalScope (generation) ── all of the above
```

- `CancelScope` is a small JS class (`cancel(reason)`, `onCancel(fn)`, `child()`), because the bootstrap sandbox has no `AbortController` [03 §7].
- **User cancel:** JS cancels the `RunScope` at once, then sends `Cancel{run}`. The engine moves non-terminal items to `Cancelled`, emits `Abort`/`Discard`, and lets items already in `Saving` finish (the DB save is atomic and short). The login window closes only if no other run's items are waiting.
- **Shutdown/disable:** `generation++` first. Every async continuation captures `gen` and becomes a no-op when it changes. `zotero-items.js` checks `gen` immediately before `createURLAttachmentFromTemporaryStorageDirectory`. On `APP_SHUTDOWN`: cancel network only, skip DOM teardown [01 §2.1]. On disable/uninstall/upgrade: abort all scopes, destroy browsers, close windows, remove the MIME handler, terminate the worker, flush `state.json` (1 s cap), remove FTL links, `chromeHandle.destruct()`. Disable->enable reuses the sandbox, so `startup()` rebuilds everything idempotently [01 §2.1].
- **Temp dirs:** each `storage/tmp-XXXXXX` we create is recorded in `state.json.tmp_dirs` before use and removed from the list after save or discard. `startup()` sweeps leftovers (only names on our list).

---

## 4. Per-item pipeline and state machines

### 4.1 Pipeline

1. **Snapshot (JS)**: raw fields only (`DOI`, `extra`, `PMID`, `PMCID`, `archiveID`, `url`, title, date, creators, type, library kind and `filesEditable`, PDF count, `getBestAttachmentState()`).
2. **Eligibility (Rust)**: regular, non-feed, editable files, no PDF, not owned by another live run.
3. **Identifiers (Rust `normalise_identifiers`)**: DOI from the field or Extra (cleaned, lowercase-compared), PMID/PMCID, arXiv id (`10.48550/arXiv.X`, `archiveID`, arxiv.org URL), the `10.1101/` rxiv flag [S (d)]. With no DOI: PMID -> Europe PMC; title -> Crossref with strict match (F19).
4. **OA discovery** (3.4), status-first parsing because many errors are HTML/XML/text or 200-empty [05 §8].
5. **Ranking (Rust)**: published > accepted > submitted (pref), then tier A > B (publisher gold/hybrid `pdf_url` with record-DOI match, then repositories) > C (landing pages) > D (challenge-walled: PMC website, `europepmc ?pdf=render`). Down-weight merged OpenAlex records with unknown source. Dedupe including the query string [05 §5].
6. **Fetch tiers**: T1 HTTP (Zotero UA, or Plain if the host is sticky-plain) -> classify -> T2 HiddenBrowser with PDF capture (plain UA) -> T3 visible window via the gate (user acts, never automated) -> T4 hand-off.
7. **Landing extraction**: Rust extractor (publisher rules, `citation_pdf_url`, `link[rel=alternate]`, `wkhealth_pdf_url`, SD `__PRELOADED_STATE__`, scored anchors, frames, interstitials [06 §2-3]) runs in the worker. **In parallel**, JS runs Zotero translators on the same bytes (`blobToHTMLDocument` -> remove `<noscript>` -> `getFileFromDocument`, 15 s). Ranking: publisher rule > translator result > generic signals; agreement boosts.
8. **Institutional phase** (only if OA failed and every gate passes): run cap, budgets, not suspended, group-library consent, session not `Declined/Refused/Ineffective/Misconfigured`.
   - a. Alma CTO (cached 1 day per DOI): no `getFullTxt` -> `NotFound(not_held_by_kcl)` with a LibKey hand-off; Alma error -> continue.
   - b. Direct (campus/existing session): `doi.org` landing in T1 plain UA, then PDF candidate. Skipped when access hints say "no access" (SD `.accessContent`, missing Silverchair `a.article-pdfLink`) [06 §2.2].
   - c. OpenAthens redirector `https://go.openathens.net/redirector/kcl.ac.uk?url=<enc https://doi.org/DOI>` in T2. The settled page decides: publisher landing -> extract -> PDF via T1 plain UA, same jar (T2 capture if challenged); `IdpLogin` -> session gate; `OpenAthensUnknownResource` -> EZproxy if the host is configured, else hand-off; `OpenAthensBadOrg` -> session `Misconfigured`.
   - d. EZproxy (`login?qurl=` + full encoding) only for hosts in the seed list [04 §2.4] or confirmed by the no-follow probe (7-day cache).
9. **PDF validation (worker)**: `%PDF-` within 1 KiB (strip junk so the saved file has it at offset 0 [S C7]), `%%EOF` in the last 2 KiB, size > 1 KiB, Content-Length match, XMP DOI soft check (mismatch rejects; absence accepts).
10. **Save (JS)**: re-check the parent (exists, not trashed, still no PDF, library editable, `gen` unchanged) -> rename the temp file to `getFileBaseNameFromItem(item,{attachmentTitle})+'.pdf'` -> `createURLAttachmentFromTemporaryStorageDirectory({directory: tmpDirPath, parentItemID, filename, title, url: storableUrl, contentType:'application/pdf'})`. `url` is mandatory in that function (src `attachments.js:686`). Title comes from `Zotero.getString('attachment.fullText'|'attachment.acceptedVersion'|'attachment.submittedVersion')` [S (a)].

### 4.2 Item state machine

```
Queued --admit--> Checking --ineligible--> Skipped{HasPdf|PdfNotDownloaded|ReadOnly|NotRegular|Feed}
                     |
                     v
                 Resolving --no ids, lookups failed--> NotFound{NoIdentifiers}
                     |
                     v
                 Discovering --settled--> Attempting(cand_i, tier) <----------------------+
                                            |   |   |                                   |
           Landing -> Extracting -----------+   |   +-- NotPdf/Blocked/Error/Timeout ---+ (next cand)
           Challenge(auto) -> T2 ---------------+   +-- Challenge(interactive) -> WaitingWindow (gate)
                                            |
                                     PdfFetched -> Validating --bad--> (next cand / 1 retry if truncated)
                                            |          |
                         OA exhausted       |          +--ok--> Saving --> Saved
                                            v                       |--> Skipped{HasPdfLate}
                                   InstitutionalCheck               |--> Cancelled{ParentGone} / Failed{Io|ReadOnly}
                                     |  gates fail --> NeedsUserAction{why} | NotFound{not_held_by_kcl}
                                     v
                         Institutional{Direct|Redirector|Ezproxy, step}
                                     |  IdpLogin / EzproxyNeedLogin
                                     v
                         AwaitingLogin{realm} (parked, no slot)
                            | gate Active --> Institutional (resume; first waiter gets the window's landing page)
                            | gate Declined|TimedOut|Refused|Ineffective --> NeedsUserAction{why, handoffs}
Any non-terminal: Cancel -> Cancelled{user} (Saving finishes first); EngineFault -> Failed{internal};
                  ItemDeadline -> NotFound{timeout} or NeedsUserAction; NetworkDown -> paused, then Failed{offline}
```

Terminal outcomes: `Saved{source, version, route, url}`, `Skipped{reason}`, `NotFound{reason, tried}`, `NeedsUserAction{why, handoffs}`, `Cancelled{by}`, `Failed{error}`. Every terminal state carries a compact attempt trace for the report.

### 4.3 Institutional session machine (per realm: `openathens`, `ezproxy`) and the interactive gate

```
Unknown --attempt ok--> Active{verified:true}
Unknown|Active --IdpLogin on attempt--> NeedsLogin{first_waiter}
NeedsLogin --gate free--> LoginOpen{window_fx, waiters[], opened_at}      (effect OpenLoginWindow)
LoginOpen --more IdpLogin--> LoginOpen (waiter appended; no new window, no new redirector runs)
LoginOpen --WindowDone landed|user "I've signed in"--> Active{verified:false}  (resume waiters FIFO under inst limits)
Active{verified:false} --first resumed attempt ok--> Active{verified:true}
Active{verified:false} --first resumed attempt bounces--> Ineffective   (no more prompts this run)
Active{verified:true} --bounce--> Expired -> NeedsLogin                  (max 2 prompts/realm/run)
LoginOpen --user closed--> Declined   --timeout--> TimedOut   --CA page--> Refused{code}
any --EzproxyError{suspend} | inst 429 | 3 block signals--> Suspended{until} (persisted; global, all realms)
any --OpenAthensBadOrg--> Misconfigured (check the domain pref)
Declined|TimedOut|Refused|Ineffective --"Retry with sign-in" (new run)--> Unknown
```

**Interactive gate** (global, capacity 1): a priority queue of `{login (realm), challenge (item)}` requests; login has priority. While the gate holds a login window, the engine starts no new redirector loads (they would only reach the IdP form again). Direct and EZproxy-for-other-realm work and all OA work continue. When the window lands, its settled page belongs to the first waiter (saves one fetch). Other waiters resume FIFO at 2 concurrent / 1 per host / 5 s PDF spacing, so 12 waiters across 4 publishers take about a minute.

Login success detection (in `login-window.js`, decided by Rust `classify` on PageData `documentHTML`): the URL has left the IdP and SSO-hop host sets, the page has been stable for 2 s, and it classifies as `Landing|Pdf`, or the MIME handler captured a PDF. Entra error pages (AADSTS codes, "You can't get there from here") give `refused`. Cookie names are never used as the signal [S C6].

---

## 5. UI

### 5.1 Context menu (`Zotero.MenuManager.registerMenu`, target `main/library/item`)

| menuID | Fluent l10nID | Shown when (`onShowing`, synchronous, cheap) | Command |
|---|---|---|---|
| `zft-get` | `zotero-fulltext-menu-get` (.label "Get Full Text") | >=1 regular item selected | `StartRun{mode:"full"}` |
| `zft-get-oa` | `zotero-fulltext-menu-get-oa` (.label "Get Full Text (Open Access Only)") | same | `StartRun{mode:"oa"}` |
| `zft-attach` | `zotero-fulltext-menu-attach-downloaded` (.label "Attach Downloaded PDF...") | exactly 1 regular item | file picker -> `fx_pdf_check` (XMP DOI warn on mismatch) -> `Zotero.Attachments.importFromFile` |

Tools menu (`main/menubar/tools`): `zotero-fulltext-menu-show-progress`. The label is distinct from the built-in "Find Full Text" [S (a)]. The FTL is `locale/en-US/zotero-fulltext.ftl`, inserted per window with `insertFTLIfNeeded`, plus `Zotero.ftl.addResourceIds` so `Zotero.getString` resolves our ProgressQueue keys [01 §5].

### 5.2 Progress

- One shared `Zotero.ProgressQueues.create({id:"zotero-fulltext", title:"zotero-fulltext-progress-title", columns:["general.item","zotero-fulltext-col-status"]})`, so concurrent runs share rows like Zotero's own Find Full Text [01 §6.2].
- Row status uses `ROW_QUEUED/PROCESSING/SUCCEEDED/FAILED`. Messages are Fluent strings with args: `-status-queued`, `-status-discovering`, `-status-trying = Trying { $source }...`, `-status-slow = Waiting for { $host } (slow publisher)...`, `-status-waiting-login = Waiting for KCL sign-in`, `-status-waiting-host`, `-status-saving`, `-status-saved = Attached ({ $source }, { $version })`, `-status-skipped-has-pdf`, `-status-not-found`, `-status-needs-you`, `-status-cancelled`, `-status-failed`.
- The dialog status line carries run-level state: "Waiting for you to sign in to King's (7 items). Open-access downloads continue." The `cancel` listener sends `Cancel{run}` for every live run. Closing the dialog does not cancel; it can be reopened from Tools.
- Pre-run confirmations (`Services.prompt.confirmEx`): (a) institutional batch above 10 items: "Open access for all N; King's access for at most 20" with buttons [OA + King's] [Open access only] [Cancel]; (b) group library with institutional mode under `inst.groupLibraries=ask`.

### 5.3 End-of-run report

- If everything was `Saved`/`Skipped`: a `Zotero.ProgressWindow` summary that closes after 4 s.
- Otherwise `report.xhtml` opens, grouped as **Needs you** (buttons per item: *Open via King's* = redirector URL in the system browser through `Zotero.launchURL`; *LibKey* = `https://libkey.io/libraries/286/<doi>`; *Attach downloaded PDF...*), **Not found** (reason, e.g. "not held by King's" or "no open-access copy"), **Failed**, **Skipped**, **Attached**.
- Run-level actions: *Retry with sign-in* (new run over the `NeedsUserAction` items) and *Copy diagnostics* (redacted traces: URLs with only allow-listed query keys, no cookies, no page text).

### 5.4 Login window (`content/login.xhtml`, based on `tools/sso-spike/plugin/content/login.*`)

- A custom XUL window with one `<browser type="content" maychangeremoteness>` and **no** `messagemanagergroup`, so there is no ExternalLinkHandler sending links to the system browser [08 §0.7].
- An origin + TLS bar from `secureBrowserUI.state`, and a visible popup pane via `window.browserDOMWindow`.
- `customUserAgent = Zotero.VersionHeader.getPlainFirefoxUA()`; load with the system triggering principal.
- Banner: "Sign in to King's College London to continue N items. Open-access downloads continue in the background." Buttons: **Cancel** (= decline), **I've finished signing in** (manual completion override), **Open in my browser instead** (hand-off for all waiters), **Keep waiting** (+5 min).
- It never fills, reads or logs form fields; it reads only URL and PageData HTML for classification. It is opened only by the gate, focused once, and closed on success, timeout, cancel or shutdown. The challenge window reuses the same XHTML with a different banner plus PDF capture.

### 5.5 Preferences (`Zotero.PreferencePanes.register`; keys under `extensions.zotero-fulltext.`; text inputs bind to string prefs [01 §4.3])

| Key | Default | Pane group |
|---|---|---|
| `oa.openalex.enabled` | true | Open access |
| `oa.openalex.apiKey` | "" | Open access (enables title search via OpenAlex) |
| `oa.europepmc.enabled` / `oa.biorxiv.enabled` / `oa.pmcS3.enabled` | true / true / true | Open access |
| `oa.zoteroOA.enabled` | true (used only when OpenAlex fails; private API [05 §1.2]) | Open access |
| `oa.unpaywall.email` | "" (empty = Unpaywall off; format-validated; never prefilled from anything) | Open access |
| `oa.crossref.mailto` | "" | Open access |
| `oa.versions` | "published,accepted,submitted" | Open access |
| `inst.enabled` | true | King's access |
| `inst.openathens.domain` | "kcl.ac.uk" | King's access |
| `inst.ezproxy.loginHost` / `inst.ezproxy.suffix` | "login.kcl.idm.oclc.org" / "kcl.idm.oclc.org" | King's access |
| `inst.alma.base` / `inst.alma.inst` / `inst.alma.check` | "https://librarysearch.kcl.ac.uk" / "44KCL_INST" / true | King's access |
| `inst.libkeyLibraryId` | "286" | King's access |
| `inst.directFirst` | true | King's access |
| `inst.login` | "prompt" ("prompt" or "never" = hand-off only) | King's access |
| `inst.loginTimeoutSec` | "300" | King's access |
| `inst.groupLibraries` | "ask" ("ask", "never", "allow") | King's access |
| `inst.maxPerRun` / `inst.confirmAbove` | "20" / "10" | Limits (read-only usage line: "used: 12/30 this hour, 41/80 today") |
| `inst.maxPerHour` / `inst.maxPerDay` / `inst.maxMBPerHour` / `inst.minGapSec` | "30" / "80" / "300" / "5" | Limits (UI can lower, never raise above defaults) |
| `interactive.challengeWindow` | true | Advanced |
| `inst.windowsSSO` | false (opt-in; sets Gecko `network.http.windows-sso.enabled`, restores the old value on disable/uninstall; warning text) [08 §2.5] | Advanced |
| `run.activeItems` | "4" | Advanced |
| `net.timeoutSec` / `net.slowTimeoutSec` / `pdf.maxMB` | "20" / "60" / "150" | Advanced |
| `debug.verbose` | false | Advanced |
| `testMode` / `test.endpoints` | false / "{}" (honoured only when `testMode=true`; harness only) | hidden |

Runtime state is **not** in prefs: `<profileDir>/zotero-fulltext/state.json` (atomic `IOUtils.writeJSON` with `tmpPath`, debounced 2 s) holds the institutional PDF log `[wall_ms, bytes][]`, `suspended_until`/reason, the EZproxy configured-host cache, the Alma cache, `tmp_dirs`, and `windows_sso_set_by_us`.

---

## 6. Code layout

### 6.1 Rust workspace

```
Cargo.toml                  workspace; lints; release profile (2.2)
crates/fulltext-core/       rlib: all logic + native tests (no cdylib here: dual type drops LTO [07 §1.3])
  src/lib.rs
  src/ids.rs                ItemIds, normalise_identifiers, clean_doi, arxiv/rxiv, title_key, title_match
  src/oa/{openalex,europepmc,biorxiv,pmc_s3,crossref,unpaywall,zotero_oa,alma}.rs  request builders + status-first parsers
  src/rank.rs               Candidate, Version, Tier, rank(), dedupe_key()
  src/page/html.rs          html5gum 0.8.4 wrapper (naively_switch_states) -> PageFacts
  src/page/classify.rs      classify(): S (c) rules incl. header-only mode
  src/page/extract.rs       extract_pdf_candidates(), page_ids(), access_hints()
  src/page/rules.rs         data-driven publisher table keyed by host_key (hyphenated) [06 §4.1]
  src/proxy.rs              Ezproxy, login_url, proxify_host, unproxify, HostMap, redirector_url, storable_url
  src/pdf.rs                PdfCheck, check_pdf, xmp_doi
  src/net.rs                HostKey, HostClass, UaPolicy, TimeoutProfile, parse_retry_after
  src/engine/mod.rs         Engine::new/step/export/import/view
  src/engine/{event,effect}.rs   serde enums (the protocol in 2.3)
  src/engine/item.rs        ItemState, ItemCtx, transitions
  src/engine/session.rs     SessionState per realm, Gate
  src/engine/sched.rs       HostQueue, token buckets, global limits, wake-up coalescing
  src/engine/budget.rs      rolling windows, persistence model
  src/engine/report.rs      RunSummary, ItemTrace, Handoff
  src/engine/rng.rs         seeded xorshift (jitter)
crates/fulltext-wasm/       cdylib: abi.rs (alloc/free/panic hook/envelope) + extern "C" exports only
crates/fulltext-sim/        dev-only: virtual clock + scripted fake hosts for scenario tests
fuzz/                       cargo-fuzz targets: classify, extract, oa parsers, engine event sequences
xtask/                      build, stage, dist, harness, dev-profile
```

Key signatures:

```rust
pub fn normalise_identifiers(raw: &RawItem) -> ItemIds;
pub fn classify(meta: &RespMeta<'_>, body_head: Option<&[u8]>, cfg: &InstConfig) -> PageClass;
pub fn extract_pdf_candidates(html: &str, page_url: &Url, want: &ItemIds) -> Extraction; // {candidates, page_ids, access_hint}
pub fn parse_source(src: OaSource, status: u16, ctype: Option<&str>, body: &[u8], want: &ItemIds) -> Result<OaParsed, ParseError>;
pub fn rank(cands: Vec<Candidate>, prefs: &RankPrefs, want: &ItemIds) -> Vec<Candidate>;
pub fn check_pdf(head: &[u8], tail: &[u8], want: &ItemIds) -> PdfCheck;
pub fn login_url(cfg: &Ezproxy, target: &str) -> String;
pub fn proxify_host(cfg: &Ezproxy, target: &Url) -> Option<Url>;
pub fn unproxify(cfg: &Ezproxy, u: &Url, known: &HostMap) -> Unproxied;
pub fn storable_url(final_url: &Url, page_url: Option<&Url>, ids: &ItemIds, cfg: &InstConfig) -> String;

pub enum PageClass { Pdf{junk_prefix:u16}, Landing, ReaderShell{rewrite:Option<String>}, Redirect{next:String},
  Challenge{vendor:Vendor, interactive:bool}, Blocked{vendor:Vendor}, RateLimited{retry_after_s:Option<u32>},
  EzproxyNeedLogin, EzproxyHostNotConfigured, EzproxyError{suspend:bool}, IdpLogin{idp:Idp, ca_error:Option<String>},
  SamlAutoPost, OpenAthensWait, OpenAthensUnknownResource, OpenAthensBadOrg, PublisherSsoHop, AccessDenied, Html }

pub struct Engine { cfg: EngineConfig, items: BTreeMap<ItemKey, ItemCtx>, runs: BTreeMap<RunId, Run>,
  sessions: BTreeMap<Realm, SessionState>, gate: Gate, sched: Scheduler, budget: Budget, rng: Rng, fx_next: u32 }
impl Engine {
  pub fn new(init: EngineInit) -> Result<Self, ErrBody>;
  pub fn step(&mut self, input: StepInput) -> Result<StepOutput, ErrBody>; // bounded work per call
  pub fn export(&self) -> Checkpoint;  pub fn import(cp: Checkpoint) -> Result<Self, ErrBody>;
}
```

### 6.2 JS / XPI layout

```
manifest.json   bootstrap.js   prefs.js   locale/en-US/zotero-fulltext.ftl
content/
  core-glue.js        IIFE, no top-level let/const/class [07 §3.1]; FulltextCore (ABI) + EngineHost (checkpoint/journal/replay)
  analyser-worker.js  ChromeWorker: importScripts(core-glue.js); instance B; request loop
  analyser-client.js  ids, transfer, 3 s watchdog, terminate/respawn, requeue-once
  main.js             Zotero.ZoteroFullText namespace; wiring; generation counter
  executor.js         effect dispatch table, fx high-water dedupe, CancelScopes, hard caps
  net.js              HTTP wrapper (timers, byte caps, UA header, abortReason)
  browser.js          HiddenBrowser pool (feature-detected import of chrome://zotero/content/HiddenBrowser.mjs), settle, PageData, PDF MIME multiplexer
  login-window.js     gate windows driver (login + challenge)
  zotero-items.js     snapshot, re-check, save, tmp-dir ledger + sweep, attach-downloaded
  translators.js      blobToHTMLDocument + noscript strip + getFileFromDocument (15 s)
  state-store.js      state.json
  ui-menu.js  ui-progress.js  ui-report.js  report.xhtml  report.js
  login.xhtml  login.js  preferences.xhtml  preferences.js
  fulltext_core.wasm  icons/
```

Feature detection with explicit degradation: missing `HiddenBrowser` -> T2 via `Zotero.BrowserRequest.downloadPDF` if present, else hand-off. Missing `getFileFromDocument` -> Rust extractor only. Missing `createURLAttachmentFromTemporaryStorageDirectory` -> `importFromFile` + `setField('url')`. Missing `MenuManager` -> refuse to start, with a clear error.

---

## 7. Testing strategy

### 7.1 Native `cargo test` (no Zotero, no network)

- **Fixtures**: `fixtures/html/expected.json` (31 pages: `kind`, `expected_pdf_url`, `alternates`, `must_not_select` [06 §6]). Status/headers for CF and Akamai fixtures come from the README. `fixtures/oa/manifest.json` (81 bodies with their real status and content type: status-first parsing, HTML 404s, 200-empty).
- **Unit vectors**: proxify/unproxify from [04 §6.4] (thieme-connect hyphen case, `qurl` lowercase hex, `url=` verbatim tail); DOI cleaning; OpenAlex merges (arXiv -> `10.65215/...`, medRxiv -> PLOS ONE); synthetic PDFs (junk prefix, no `%%EOF`, XMP DOI match/mismatch); `Retry-After` seconds and HTTP-date.
- **Scenario tests** on `fulltext-sim` (virtual clock, scripted hosts), one per failure row:

| ID | Scenario | Asserts |
|---|---|---|
| S01 | 12 items, 6 need institutional; IdP appears at item 3; login lands after 90 s virtual | exactly 1 `OpenLoginWindow`; OA items finish during login; waiters resume FIFO; first waiter consumes the landing page |
| S01b | same, user never answers | timeout at 300 s; waiters `NeedsUserAction(login_timeout)`; no second prompt |
| S02 | session expires after 30 PDFs | one re-prompt; an item bouncing twice -> hand-off; never more than 2 prompts |
| S03 | login window shows AADSTS53000 | `Refused`; no more redirector loads; report suggests Windows SSO |
| S04 | redirector "Please wait" then 201 | HEAD polls at 2.5 s, stops at 30 s |
| S05 | EZproxy suspend page | institutional stops globally; `suspended_until` persisted; next run refuses institutional |
| S06 | CF challenge on T1, clears in T2; second host blocked | plain UA sticky for the host; blocked host never retried |
| S07/S12 | `citation_pdf_url` -> HTML landing | no loop; `AccessDenied` recorded; next candidate |
| S09 | Wiley slow (50 s first byte) | slow profile used; OA hedge started at 8 s; loser aborted |
| S10 | 429 from OpenAlex / from a publisher in institutional phase | OA: 1 retry within 60 s; institutional: stop, no retry |
| S11 | truncated PDF | one retry; signed URL not retried after 60 s |
| S14 | XMP DOI mismatch | candidate rejected |
| S15 | `EngineFault` injected | only that item fails; others complete |
| S16-S23 | has-PDF-late, no DOI, group library ask/never, duplicate run, cancel mid-flight, offline, ineffective login, budget exhausted | as in section 1 |

- **Property tests (proptest) over random host behaviours and event interleavings**: never more than 2 institutional in flight; never more than 1 per publisher host; institutional PDF spacing >= 5 s per host; at most 1 interactive window; every item reaches a terminal state within bounded virtual time; no effect other than `Abort`/`Discard`/`Ui` for a cancelled item; budgets never exceeded across a simulated restart (export -> import); **replay determinism** (run -> record events -> replay from any checkpoint -> identical effects).
- **Fuzz** (native x86_64): extractor, classify, OA parsers, engine with arbitrary event sequences. Requirement: no panic, only error envelopes.

### 7.2 Node tests (real wasm, no Zotero)

- N01: ABI round trips, envelopes, UTF-8, no leak over 300 calls (spike `js/test-node.cjs` pattern [07 §3.1]).
- N02: bootstrap-sandbox simulation (`js/sim-bootstrap.cjs` pattern) loads glue as a classic script.
- N03: the executor against a fake Zotero (mock `HTTP.request/download`, HiddenBrowser, windows, DB) with a real async event loop: cancel mid-flight, generation bump mid-save, fx dedupe during replay, hard caps.
- N04: trap recovery with a `debug-panic` build: poison -> reset -> checkpoint import -> replay -> `EngineFault`.
- N05: analyser in a Node `Worker`: watchdog terminate/respawn, requeue-once.

### 7.3 Throwaway-Zotero harness (`tools/zotero-harness/run.ps1`; never the user's Zotero; no login)

- H01 lifecycle: packed XPI and proxy-file installs; menu appears without `onMainWindowLoad` [03 §9]; disable/enable idempotent.
- H02 local fakes with `httpd.sys.mjs` on 127.0.0.1 via `testMode` endpoints: OA APIs serving fixtures; a publisher landing + PDF; a JS challenge page that sets a cookie and reloads (T2); OpenAthens "please wait" (201 HEAD); a **fake IdP form page** (our own page, no real credentials) to drive the login gate end to end, including "user closes window"; EZproxy needhost/suspend pages; 429; slow-drip (idle timeout); truncated PDF; HTML-as-PDF.
- H03 live unauthenticated OA smoke on the fixture DOIs (gold, green, arXiv, medRxiv, paywalled, nonexistent). It also answers N1 (Gecko vs curl Cloudflare), N3 (`<noscript>` refresh) and N11 (compile/parse time).
- H04 MIME multiplexer while Zotero's own Find Full Text runs concurrently.
- H05 translators timeout and side requests.
- H06 cancel mid-batch: no attachments after cancel except completed saves.
- H07 shutdown mid-batch (`eAttemptQuit`) and disable mid-batch: no orphan windows; `tmp-*` dirs swept on next start.
- H08 parent trashed mid-run, read-only library (simulated), disk errors (read-only temp dir).
- Verifications to add here: `User-Agent` header honoured by `HTTP.download` (fallback: `request` with arraybuffer); `WebAssembly.Module` postMessage to a ChromeWorker; HiddenBrowser `customUserAgent` persisting across redirects.

### 7.4 Needs the user (cannot be automated under the no-login rule)

1. B1: `pwsh -NoProfile -File tools\sso-spike\run-manual.ps1` (20-30 min). It decides custom window vs hand-off, T1p vs T2p download path, and whether silent re-auth works (T4) [08 §5].
2. Dogfood run in the GUI harness after B1 passes: 5 entitled DOIs (Wiley, Lancet via DOI, BJS/OUP, Annals of Surgery/LWW, BMJ via EZproxy); a next-day run for session expiry; the Windows SSO toggle only if CA refused.
3. Confirm with KCL Library (optional): acceptable volume limits; whether Annals of Surgery is on LWW or Ovid (Alma answers this per DOI).

---

## 8. Build, packaging, versioning, dev install

- `cargo xtask wasm`: `cargo rustc -p fulltext-wasm --release --target wasm32-unknown-unknown --crate-type cdylib` with `CARGO_ENCODED_RUSTFLAGS=--remap-path-prefix=<USERPROFILE>=~` (otherwise `C:\Users\abora\...` ships inside the wasm [07 §6]). It finds output via `cargo metadata` because of the global `CARGO_TARGET_DIR`, and runs with `RUSTC_WRAPPER=` (kache off) for releases [07 §1.1]. `idna_adapter` pinned to 1.0.0 in `Cargo.lock` [07 §1.4]. Size budget: about 310 KB raw / 110 KB gzip; CI fails above 400 KB raw.
- `cargo xtask stage`: assembles `build/plugin/` (manifest, bootstrap, prefs, locale, content, wasm). The manifest version is taken from the workspace version.
- `cargo xtask dist`: zip 9 + `deflate-flate2-zlib-rs`, sorted entries, fixed timestamps -> reproducible `dist/zotero-fulltext-<ver>.xpi`; writes `updates.json` with `update_link` (GitHub release asset) and `update_hash: sha256:...`.
- `cargo xtask test`: native tests + clippy (deny list) + Node tests. `cargo xtask harness [--gui] [--suite H02]` wraps `run.ps1`.
- `cargo xtask dev-profile`: creates `.dev/profile` and `.dev/data`, writes the proxy file `.dev/profile/extensions/zotero-fulltext@aborakati` containing the absolute path of `build/plugin/`, applies the harness safety prefs [03 §2], and launches `zotero.exe -profile .dev/profile -no-remote -datadir .dev/data -ZoteroDebugText`. It **refuses** any profile path under `%APPDATA%\Zotero`. Edit loop: `stage` -> disable/enable in the dev instance (subscripts reload with `ignoreCache`; the wasm is re-fetched on `startup()`); edits to `bootstrap.js` need a restart.
- Manifest: `manifest_version: 2`; `applications.zotero = {id: "zotero-fulltext@aborakati", update_url: "https://raw.githubusercontent.com/ABorakati/zotero-fulltext/main/updates.json", strict_min_version: "10.0", strict_max_version: "10.*"}` [S (a)].
- Versioning: semver `0.y.z` until B1 passes and the dogfood run is clean. `ABI_VERSION` and the event schema `v` are bumped independently, and the glue checks both. Licence MIT; Zotero behaviour is re-implemented, never copied (Zotero is AGPL) [02 §14].

---

## 9. Risks, mitigations, and what v1 leaves out

### 9.1 Risks

| Risk | Mitigation |
|---|---|
| Embedded Entra SSO fails (B1) | Full degrade chain (D7); `inst.login=never` makes the plugin OA + direct + hand-off only; every institutional item still ends with actionable links |
| Effect-protocol bugs at the JS/Rust boundary | One schema (serde) with generated JSON samples checked into tests; Node executor tests; fx dedupe; JS hard caps log engine bugs |
| Hidden nondeterminism breaks replay | `BTreeMap` only, no floats in decisions, seeded PRNG, the replay property test |
| Zotero internals change (HiddenBrowser, PageData, MIMETypeHandler, getFileFromDocument, createURL...) | Feature detection plus a documented fallback for each (6.2); harness suite runs on every Zotero update |
| Cloudflare decisions vary run to run [08 §0.3] | Sticky plain UA, T2 browser tier, user-facing challenge window, host walling after 2 challenges |
| Licence breach | Rust-enforced, persisted budgets; user-initiated only; no retries on block signals; global suspend on suspend/429 |
| Wrong PDF attached | Other-DOI rejection, record-DOI checks, XMP DOI check (soft) |
| Translators bypass our limiter | Restricted use, 15 s cap, counted against the host budget |
| Main-thread jank from DOMParser + translators on 2 MB pages | 2 MiB cap; translators skipped when Rust already has a rule-based candidate on institutional pages |
| `HTTP.download` may ignore a custom `User-Agent` | Harness check; fallback to `request` with an arraybuffer for plain-UA hosts |

### 9.2 Deliberately out of v1

EPUB; LibKey API (needs a KCL token); OpenAlex paid content API; Semantic Scholar/CORE; NHS OpenAthens and multi-institution UI (the config model supports it); hand-built WAYFless links when the redirector is down; Ovid route for LWW titles; Downloads-folder watcher (v1 has the manual "Attach Downloaded PDF..."); per-item cancel; writing inferred DOIs back to items; any automatic or background fetching (licence); syncing budgets across machines; full-text identity checks beyond XMP; localisations other than en-US; Zotero 11.
