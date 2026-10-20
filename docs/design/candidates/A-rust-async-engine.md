# zotero-fulltext v1 design: a Rust async engine (sans-IO executor in zero-import WASM) with a thin JS effect runner

**Concurrency:** The orchestration is real async Rust running inside the wasm engine instance, which has zero imports. A custom single-threaded executor drives it. The design is sans-IO: Rust emits effects and JS performs them concurrently.

- Executor: one task per item, stored in a slab with generation-checked TaskKeys. Wakers are safe Arc<impl Wake>. Each waker pushes into a thread_local ready queue and dedupes with an AtomicBool, which lowers to plain loads and stores on wasm32 without threads. A LOST_WAKE flag triggers a full sweep, so a wake is never lost.
- Leaf ops: the only leaf future is Op. On first poll it emits an effect with an OpId into a reactor outbox. The completion delivered through fx_step stores the result and calls the stored waker after the reactor borrow is released. Dropping a waiting Op emits a cancel effect, and JS then aborts the XHR, destroys the HiddenBrowser or closes the window. Body handles release themselves on Drop.
- Timers: virtual. JS passes in monotonic now on every step and arms one setTimeout for the next_timer that Rust returns.
- fx_step: each call applies commands, delivers completions, fires due timers, and polls ready tasks under a poll budget (setting more=true if the budget runs out). Because the module has no imports, no JS can run during a call, so re-entrancy is structurally impossible. JS coalesces completions into one drain per turn.
- Concurrency inside an item: futures-util join!, select_biased! and FuturesUnordered. Phase 1 OA lookups run in parallel, each with a 9 s timeout and a token bucket. The PMC S3 lookup is chained into the same set once a PMCID is known. Discovery stops early once a strong candidate has been present for 1.5 s, and stragglers are cancelled.
- Candidate attempts: sequential by default. On OA routes only, attempts on different hosts are hedged at 3 s (first verified PDF wins). Institutional attempts are never hedged.
- Policy in Rust: item concurrency (4); per-host gates (1 in flight); global institutional semaphore (2); spacing of at least 5 s plus jitter between institutional PDFs; a persisted reserve-then-commit licence ledger (30/h, 80/24h, 300 MB/h); batch cap of 20 with confirmation above 10; per-item fetch budget of 4; stop signals that halt the route; browser slots (2); one interactive window at a time; a LoginGate that gives one sign-in per session epoch shared by all items; a bytes semaphore for retained PDF bodies; phase timeouts.
- Cancellation: user cancel, item deletion and plugin disable all become commands. The executor drops the matching futures inside the step, so the cancel and release effects come back in the same StepOut. On APP_SHUTDOWN, wasm is skipped.
- Parser isolation: a second, stateless 'pure' instance of the same module parses all untrusted HTML and JSON. A parser trap costs one page. An engine trap triggers journal-based restart-once recovery.

**Rust share:** About 80% of code lines and about 95% of decision logic are in Rust. The estimate is about 7,500 Rust lines (core parsers, extractor, classifier, proxy and ranking ~4,500; engine executor, reactor, scheduler, pipeline and protocol ~2,750; ABI ~250) against about 1,700 JS lines plus about 400 XHTML/FTL lines. Every branch that depends on page content, API results, ordering, limits, timing, retries, tier escalation, the URL to store, or the outcome is decided in Rust. That covers the pure instance for untrusted parsing and the engine for orchestration and policy. JS keeps only what can only be done in Gecko/Zotero JS: calling Zotero.HTTP.request, HiddenBrowser/PageData, the MIME capture dispatcher, custom XUL windows, translators, Zotero OA, Notifier observers, prefs I/O, attachment saving, and UI rendering. It also owns the lifetimes of those objects (abort/destroy on cancel effects) and keeps a few tripwire caps as defence in depth, not policy. PDF bytes stay in JS by design; Rust sees only handles plus a 1 KiB head and tail verdict.

**Self-identified weaknesses:**
- The whole institutional pipeline rests on B1 (embedded Entra/OpenAthens SSO completing inside Zotero), which is still unproven. If it fails, much of the Rust institutional machinery (LoginGate, redirector tiers) reduces to producing hand-off links.
- A custom executor plus a sans-IO JSON protocol adds a layer of indirection that a JS async/await implementation would not need. Async state inside wasm is harder to debug, and a trap gives no Rust stack trace, only the panic-hook message.
- An engine trap loses all in-flight run state. Recovery restarts unfinished items once, which can re-issue publisher requests. The ledger is protected by reserve-before-fetch, but the raw request count can exceed what a JS implementation would issue.
- The Waker design relies on single-threaded wasm (thread_local ready queue, Arc atomics lowered to plain ops). Enabling wasm threads or atomics later would silently break the wake path unless it is redesigned.
- Untrusted-input parsing (html5gum, up to 2 MiB) still runs on Zotero's main thread in the pure instance. A worst-case page can stall the UI for tens of milliseconds until the pure instance moves to a ChromeWorker, which is deferred to v2.
- Zotero translators make their own HTTP requests outside the Rust rate limiter. The mitigation (run them only as a fallback on institutional hosts and count them against the budget) is approximate.
- It depends on several Zotero internals that can change: HiddenBrowser (including _browser identity for MIME capture), the PageData actor, MIMETypeHandler, getFileFromDocument and createURLAttachmentFromTemporaryStorageDirectory. Feature detection degrades the tiers but cannot replace them.
- The global MIME handler sends any unhandled PDF in other Zotero browsers to the external download dialog while it is registered. Reference-counted registration narrows the window but does not remove it.
- The licence limits (30/h, 80/24h, 300 MB/h, batch 20) come from OCLC defaults and Jisc model-licence text, not from KCL. They may be too strict or not strict enough.
- Two instances of a roughly 420 KB module and the futures-util dependency increase memory and binary size. Neither the size budget nor the zero-import guarantee with futures-util has been measured yet; both are only gated in xtask.
- Policy caps are duplicated as JS tripwires. They are documented as non-policy, but they are a second place limits are encoded and could drift.
- UA policy (plain Firefox UA kept per host) and tier escalation depend on Cloudflare's risk-based decisions, which vary from run to run [08 §3.2]. Outcomes for the same item may differ between runs.

---

# zotero-fulltext: Rust-maximal async engine design

Status: design proposal (phase "architecture"). Author angle: **put the orchestration itself in Rust**.
Evidence base: `research/SUMMARY.md` (cited as [S §x]), briefs `research/0N-*.md` (cited as [0N §x]),
fixtures in `fixtures/oa` and `fixtures/html`, and the prototypes in `spikes/wasm-spike`, `tools/zotero-harness`
and `tools/sso-spike`. Zotero 10.0.6 source in `.ref/omni-app` was re-checked where an API is named.

---

## 0. Decisions in one page

| # | Decision | Why (evidence) |
|---|---|---|
| D1 | The per-item pipeline is **real async Rust** (`async fn`, `join!`, `select_biased!`, `FuturesUnordered`), run by a **custom single-threaded executor inside the wasm module**. | User asks for Rust wherever possible and for concurrency. Wasm has zero imports [S §b, 07 §3], so `wasm-bindgen-futures` (needs JS imports) is out, and JS Promise Integration is off in Zotero (`wasm_js_promise_integration=false`, [03 §3]). A hand executor is the only way to run Rust futures under those constraints. |
| D2 | **Sans-IO**: Rust never does I/O. It emits **effects** (HTTP, hidden-browser load, login window, translators, save, timers are internal). JS performs effects concurrently and returns **completions**; each completion wakes exactly one leaf future through a real `Waker`. | Zero-import ABI is proven in every Zotero context [03 §3-4]. Sans-IO also makes the whole concurrency model testable natively with virtual time (section 6). |
| D3 | **One export drives the engine**: `fx_step(StepIn) -> StepOut` (JSON). Each call: apply commands, deliver completions, fire due timers, poll ready tasks under a poll budget, return new effects + next timer deadline. | One wasm call per event-loop turn; JSON round trips cost ~0.05 ms [03 §3]. |
| D4 | The **same `.wasm` is instantiated twice**: an **engine** instance (stateful, long-lived, never sees raw HTML) and a **pure** instance (stateless parsers/extractor/classifier for untrusted bytes, recycled freely). JS pipes every response through `pure.inspect` before delivering it to the engine. | A panic traps the instance [07 §4]. Untrusted-HTML parsing is the likeliest panic source; isolating it means a parser trap costs one page, not the whole batch. Engine memory stays small (wasm memory never shrinks [07 §3.1]). |
| D5 | **Rust owns all policy**: scheduler, per-host/global gates, token buckets, licence ledger, hedging, timeouts, cancellation, retries, tier escalation, login gate, outcome mapping, URL to store. JS keeps only Zotero/Gecko API calls, object lifetimes and UI. JS has hard tripwire caps only (never policy). | Licence limits must live in one place [04 §8.2]. |
| D6 | **PDF bytes stay in JS** (BodyStore handles). Rust sees `{handle, len}` plus a 1 KiB head and 1 KiB tail inspection. Rust decides; JS writes. | Task requirement; avoids copying multi-MB PDFs into linear memory. |
| D7 | Own orchestrator, not `addFileFromURLs`; Zotero translators run *alongside* the Rust extractor on the same fetched page; own save via `createTemporaryStorageDirectory` + `IOUtils.write` + `createURLAttachmentFromTemporaryStorageDirectory` (verified at `xpcom/attachments.js:682-731, 2853`). | [S C4], [02 §7]. |
| D8 | Institutional route = campus/direct, then OpenAthens redirector in a HiddenBrowser, then **one** custom login window (plain Firefox UA) shared by all items through a Rust `LoginGate`, then hand-off fallbacks. EZproxy only for configured hosts. | [S §c], [08 §0, §5-6]. B1 is unproven, so every institutional state has a hand-off terminal outcome. |

Plugin id `zotero-fulltext@aborakati`, `update_url https://raw.githubusercontent.com/ABorakati/zotero-fulltext/main/updates.json`,
`strict_min_version "10.0"`, `strict_max_version "10.*"`, MIT licence (Zotero behaviour re-implemented, never copied).

---

## 1. Architecture and the JS <-> Rust boundary

### 1.1 Diagram

```
 Zotero main thread (bootstrap sandbox, system principal)
 ┌──────────────────────────────────────────────────────────────────────────────────────┐
 │ bootstrap.js ─ menus (MenuManager), prefs pane, windows, shutdown                    │
 │      │ start_run / cancel / user_action / configure (commands)                        │
 │      ▼                                                                               │
 │ host.js  EffectRunner ──────────────────────────────────────────────┐                 │
 │   queue(completions, commands) ─► drain() ─► engine.fx_step(StepIn) │ StepOut         │
 │   one setTimeout for next_timer          ◄─ effects, next_timer ────┘                 │
 │      │ dispatch by effect.type (all concurrent, each a JS Promise)                    │
 │      ├─ net.js      Zotero.HTTP.request (successCodes:false, errorDelayMax:0, ...)    │
 │      ├─ browser.js  HiddenBrowser load/settle, PageData, PDF MIME capture dispatcher  │
 │      ├─ login.js    custom login.xhtml window (plain UA, TLS bar, popups pane)        │
 │      ├─ adapter.js  translators, Zotero OA, item snapshots, save attachment, prefs    │
 │      ├─ ui.js       progress dialog / ProgressWindow, report, confirm prompts         │
 │      └─ BodyStore   handle -> ArrayBuffer (PDF/HTML bytes never enter the engine)     │
 │            │ every response:  stage(head bytes) ─► pure.fx_call("inspect") ─┐         │
 │            │                                                               ▼         │
 │  ┌───────────────── wasm instance #1: ENGINE ─────────┐  ┌──── wasm instance #2: PURE ──┐
 │  │ fx_step: commands → completions → timers → poll   │  │ fx_call: inspect, login_state, │
 │  │ Executor (task table, Arc<Wake> wakers)           │  │ pdf_verdict, unproxify, ...    │
 │  │ Reactor (ops, timers, outbox)                     │  │ html5gum extractor, classifier │
 │  │ Scheduler (gates, buckets, ledger, LoginGate)     │  │ OA API parsers, rules table    │
 │  │ Pipeline (async fn per item)                      │  │ stateless; recycled on trap or │
 │  │ zero imports; no JS runs during a call            │  │ when memory > 64 MiB           │
 │  └───────────────────────────────────────────────────┘  └────────────────────────────────┘
 │          (both instances from ONE cached WebAssembly.Module, compiled once per session)  │
 └──────────────────────────────────────────────────────────────────────────────────────┘
```

Loading follows [S C3]: `aomStartup.registerChrome` → `chrome://zotero-fulltext/content/`, glue loaded with
`Services.scriptloader.loadSubScriptWithOptions(url, {target, ignoreCache:true})` as a classic IIFE (no top-level
`let/const/class`), wasm bytes via `fetch(chrome://…/fulltext.wasm).arrayBuffer()` → `WebAssembly.compile` → cached
`Module` → two `WebAssembly.instantiate(module, {})`. Never `instantiateStreaming` over chrome:// [03 §5].

### 1.2 ABI (exports; all `i32` = wasm32 `usize`; `u64` arrives in JS as BigInt [03 §4])

| Export | Signature | Used on | Contract |
|---|---|---|---|
| `fx_abi_version` | `() -> u32` | both | `= 2`; glue refuses mismatch. |
| `fx_init` | `(seed_lo: u32, seed_hi: u32)` | both | Installs the panic hook (message → static buffer) and seeds the PRNG used for jitter. JS passes `crypto.getRandomValues`. |
| `fx_alloc` / `fx_free` | `(len) -> ptr` / `(ptr, len)` | both | Exact-capacity `Vec<u8>` [07 §3.1]. |
| `fx_last_panic` | `() -> u64` | both | `(ptr<<32)|len` of static buffer; allocation-free, callable after a trap. Not freed by JS. |
| `fx_step` | `(ptr, len) -> u64` | engine | Input: UTF-8 JSON `StepIn` (borrowed). Output: leaked `Box<[u8]>` JSON envelope; JS frees with `fx_free`. |
| `fx_stage` | `(ptr, cap, len) -> u32` | pure | Ownership transfer: Rust adopts a buffer JS got from `fx_alloc(cap)` and filled with `len` bytes; returns blob id (>0) or 0 if refused (over the 2 MiB per blob / 4 MiB total cap). JS must NOT free a staged buffer. Blobs not referenced by the next `fx_call` are freed at the end of that call. |
| `fx_call` | `(ptr, len) -> u64` | pure | `{"fn": "...", "args": {...}, "blob": id?, "blob2": id?}` → envelope. |
| `fx_mem_pages` | `() -> u32` | both | For the recycle policy. |
| `fx_debug_panic` | `(u32) -> u32` | test builds only (`--features debug-panic`) | Trap injection for recovery tests. |

Memory rules (same as the proven spike, [07 §3.1]): JS writes only into buffers it got from `fx_alloc`; JS re-creates
`Uint8Array` views after **every** call (memory may grow); every JSON output is `{"ok": …}` or
`{"err": {"code", "message"}}`; errors are values, never traps. `xtask` asserts the import section is empty.

### 1.3 Engine protocol (JSON over `fx_step`)

`StepIn` (one per drain; JS coalesces everything that arrived since the last call):

```json
{
  "now": 52311.7,
  "wall": 1792496400123,
  "commands": [
    {"type": "start_run", "run": 3, "mode": "full",
     "items": [{
       "item": 1201, "key": "Q7ZK2M4P",
       "library": {"id": 1, "kind": "user", "files_editable": true},
       "regular": true, "feed": false, "item_type": "journalArticle",
       "title": "Cold-stored whole blood in trauma resuscitation",
       "fields": {"DOI": "10.1097/SLA.0000000000005603", "PMID": "", "PMCID": "", "url": "", "archiveID": "", "extra": ""},
       "pdf_attachments": 0, "best_attachment": {"type": "none", "exists": false}}]}
  ],
  "completions": [
    {"op": 118, "ok": {"kind": "http", "status": 200,
      "final_url": "https://api.openalex.org/works/doi:10.1371/journal.pmed.1000097",
      "headers": {"content-type": "application/json"}, "body": null,
      "inspect": {"class": "api", "api": {"source": "openalex", "status": "found",
        "ids": {"pmcid": "PMC2707599", "pmid": "19621072"},
        "candidates": [{"url": "https://journals.plos.org/plosmedicine/article?id=10.1371/journal.pmed.1000097",
          "kind": "landing", "version": "published", "license": "cc-by", "host_kind": "publisher",
          "record_doi": "10.1371/journal.pmed.1000097", "source": "openalex"}]}}}},
    {"op": 121, "err": {"kind": "timeout", "after_ms": 9000}}
  ]
}
```

`now` is monotonic ms (`Cu.now()`), used for all timers; `wall` is epoch ms, used only for the persisted licence ledger.

`StepOut`:

```json
{"ok": {
  "effects": [
    {"type": "progress", "run": 3, "item": 1201, "phase": "searching_oa",
     "l10n": "zotero-fulltext-status-searching-oa", "args": {}},
    {"type": "http", "op": 122, "run": 3, "item": 1201, "route": "oa",
     "req": {"method": "GET", "url": "https://pmc-oa-opendata.s3.amazonaws.com/metadata/PMC2707599.1.json",
             "headers": {"Accept": "application/json"}, "ua": "default", "cookies": "include",
             "timeout_ms": 11000, "follow_redirects": true, "max_bytes": 1048576},
     "stage": {"text_max": 2097152, "binary_head": 1024, "binary_tail": 1024},
     "retain": "none", "inspect": {"expect": "api", "source": "pmc_s3_meta"}},
    {"type": "cancel", "op": 117, "run": 3, "item": 1188}
  ],
  "next_timer": 53311.7,
  "more": false,
  "ignored": [],
  "stats": {"tasks": 4, "ops_inflight": 6, "timers": 3, "mem_pages": 24}
}}
```

- `more: true` means the poll budget ran out; JS schedules another drain with `setTimeout(0)` (keeps each call short).
- `ignored` lists completions for ops that were cancelled or unknown; JS releases any BodyStore handle they carried.
- `ua`: `"default"` (Zotero UA) or `"plain"` (`Zotero.VersionHeader.getPlainFirefoxUA()`, sent as a `User-Agent`
  header on `Zotero.HTTP.request`, or as `customUserAgent` on browsers [08 §6.2]). Rust picks it per host and keeps it
  sticky per host for the session (cf_clearance is UA-bound [S §g]).
- `retain`: `"none"` (drop body after staging), `"page"` (keep HTML text for translators), `"pdf"` (keep bytes for save).

**Commands** (JS → Rust): `configure` (prefs snapshot, institution JSON, persisted ledger + host state, protocol
version 1), `start_run`, `cancel_run {run}`, `cancel_item {run,item}`, `user_action {run, kind:
"sign_in_now"|"skip_sign_in"|"keep_waiting"}`, `dump_state` (debug), `shutdown`.

**Effects** (Rust → JS). Those with `op` expect exactly one completion; the rest are fire-and-forget.

| type | op? | JS action | completion `ok` payload |
|---|---|---|---|
| `http` | yes | `Zotero.HTTP.request` with `responseType:'arraybuffer'`, `successCodes:false`, `errorDelayMax:0`, `noRetryOnThrottle:true`, `timeout`, `followRedirects`, `cancellerReceiver`; `requestObserver` aborts above `max_bytes`; then stage + `pure.inspect` | `{status, final_url, headers(whitelist), body:{handle,len}|null, inspect}` |
| `browser_load` | yes | `new HiddenBrowser({customUserAgent})`, load, settle (location stable `settle_ms`), optional PDF capture, `getPageData(['documentHTML','channelInfo'])`, stage + inspect, destroy | `{final_url, status, pdf:{handle,len,url}|null, body, inspect}` |
| `login_window` / `challenge_window` | yes | open `login.xhtml`, poll URL every 500 ms, ask `pure.login_state` when stable, capture PDFs | `{result:"landed"|"pdf"|"closed_by_user"|"timeout"|"ca_blocked", final_url, body, pdf, inspect}` |
| `run_translators` | yes | `blobToHTMLDocument` (after stripping `<noscript>`) + `getFileFromDocument` [S §a] | `{url, mime, title}` or `null` |
| `zotero_oa` | yes | `Zotero.Utilities.Internal.getOpenAccessPDFURLs(doi)` | `[{url,pageURL,version}]` |
| `save_pdf` | yes | re-check eligibility, trim prefix, offset-0 `%PDF-` assert, temp dir + `IOUtils.write` + `createURLAttachmentFromTemporaryStorageDirectory` | `{attachment_id}` or err `already_has_pdf`/`not_editable` |
| `confirm` | yes | `Services.prompt.confirmEx` | `{accepted: bool}` |
| `cancel` | no | abort XHR / destroy HiddenBrowser / close window / drop translator result | - |
| `release` | no | free BodyStore handle | - |
| `progress`, `log`, `trace` | no | UI row update / `Zotero.debug` | - |
| `persist` | no | `Zotero.Prefs.set('extensions.zotero-fulltext.state.<key>', value, true)` | - |
| `run_finished` | no | switch progress UI to report | - |

Completion errors: `{"err": {"kind": "timeout"|"network"|"cancelled"|"too_large"|"tls"|"unsupported"|"internal", ...}}`.
`unsupported` is what JS returns when a Zotero internal (HiddenBrowser, PageData, MIMETypeHandler, translators) is
missing after a Zotero update; Rust degrades by skipping that tier [S §g].

### 1.4 Pure protocol (`fx_call` on the pure instance)

```json
{"fn": "inspect", "blob": 12,
 "args": {"expect": "page", "status": 200,
   "final_url": "https://journals.lww.com/annalsofsurgery/fulltext/2022/09000/cold_stored_whole_blood.4.aspx",
   "headers": {"content-type": "text/html; charset=utf-8", "server": "cloudflare"},
   "body_len": 231044, "truncated": false}}
```
→
```json
{"ok": {"class": "landing", "page_ids": {"doi": "10.1097/SLA.0000000000005603", "pii": null},
  "candidates": [{"url": "https://journals.lww.com/annalsofsurgery/_layouts/15/oaks.journals/downloadpdf.aspx?an=00000658-202209000-00004",
    "signal": "head_meta:wkhealth_pdf_url", "score": 90, "needs_browser": true}],
  "refresh": null, "access_hint": "unknown", "rule": "lww"}}
```

Functions: `inspect` (expect = `api:<source>` | `page` | `pdf` | `probe`; implements `classify()` [S §c] and
`extract_pdf_candidates` [06 §8] with html5gum [S C2]), `login_state` (URL + optional HTML vs `done_when`),
`pdf_verdict` (head+tail blob: `%PDF-` within 1 KiB, `%%EOF` in tail, prefix to trim), `normalise_ids`,
`unproxify`. For a binary body JS stages `head(1024)+tail(1024)`; for text it stages up to `text_max` (2 MiB cap
[S §b]); bytes are decoded as lossy UTF-8 in Rust.

---

## 2. Concurrency and async model

### 2.1 Executor (inside the engine instance)

**Task table.** One task per item (`Owner {run, item}`), stored in a slab: `slots: Vec<Option<Task>>`, a generation
counter per slot, and a free list. `TaskKey {idx, gen}`; a wake for a retired slot carries an old `gen` and is
ignored. Concurrency *inside* an item uses sub-futures (`join!`, `select_biased!`, `FuturesUnordered`), not tasks;
tasks never spawn tasks. Max 256 tasks (OA-only batch cap 200).

**Waker valid on wasm32 without threads.** `Arc<TaskWaker>` implementing `std::task::Wake` (no `unsafe`). On
`wasm32-unknown-unknown` without the `atomics` feature, `Arc`/`AtomicBool` lower to plain loads/stores. `wake` sets
`queued` (dedupe) and pushes the key into a `thread_local!` `READY: RefCell<VecDeque<TaskKey>>` (thread-locals are
plain statics on this target). If the queue is somehow borrowed, it sets `LOST_WAKE` and the executor re-queues every
live task (a correct, slower fallback; a wake is never lost). `FuturesUnordered` works because it wraps whatever
waker it is polled with: a leaf inside it stores FU's sub-waker; on completion FU marks that child ready and calls
our task waker.

**Leaf futures and completions.** `Op` is the only leaf that talks to JS. First poll: allocate an `OpId` (u64,
never reused, < 2^53), store `Slot::Waiting{waker, owner}` in the reactor, push the effect to the outbox (lazy:
constructing a future does no I/O). `fx_step` delivers `{op, ok|err}`: reactor swaps the slot to `Done(c)` and
returns the stored waker, which is called **after** the reactor borrow ends. The next poll takes `Done(c)`. `Drop`
of a waiting `Op` removes the slot and emits `cancel {op}`; a late completion then lands in `ignored`. Body handles
inside completions are wrapped in `BodyHandle`, whose `Drop` emits `release`, so cancellation cannot leak PDF bytes
in JS.

**Timers.** `Sleep` registers `(deadline, op)` in a `BinaryHeap`; `fx_step` fires due timers using the `now` JS
passed in; `StepOut.next_timer` is the earliest live deadline. JS keeps a single `setTimeout` for it. Dropping a
`Sleep` removes its slot; the heap entry is skipped lazily. `timeout(ms, fut)` = `select_biased!(fut, sleep)`, and
losing branches are dropped (→ cancel effects).

**Step algorithm** (`fx_step`): (1) set `now/wall`; (2) apply commands (spawn item tasks, or `cancel_where(owner)`,
which drops futures inside the call so their cancel effects come out in this same step); (3) deliver completions,
collecting wakers, then wake; (4) fire due timers, then wake; (5) `run(budget=256 polls)`; between polls, apply
exec-requests queued by futures (for example "halt institutional route for run 3"); (6) emit `run_finished` for runs
with no live tasks; (7) drain outbox → `StepOut`.

**Re-entrancy rules.**
1. Zero imports, so no JS can run during a wasm call; each export runs to completion. JS cannot re-enter.
2. JS serialises engine calls: completions/commands go into a queue; one `drain` (scheduled with
   `Promise.resolve().then`, since the sandbox has no `queueMicrotask` [03 §7]) calls `fx_step` once. Effect handlers
   that finish synchronously still enqueue their completion; they never call `fx_step` directly.
3. In Rust: no `RefCell` borrow of the reactor, scheduler or ready queue is held across `.await`, across a
   `wake()`, or while a future is dropped. All access goes through `try_borrow_mut` helpers that return `None`
   instead of panicking; `Drop` paths fall back to a deferred-cancel queue drained by the executor.
4. The two instances never call each other; JS mediates.

**Trap recovery.**
- *Pure instance trap:* glue marks it poisoned, reads `fx_last_panic`, re-instantiates from the cached `Module`
  (0.2-0.9 ms [07 §4]) and completes the op with `inspect: {"class": "unknown", "error": "inspect_failed"}`. The
  engine treats the page as unusable and continues with translators or the next candidate.
- *Engine trap:* all in-flight state is lost. JS (a) cancels every op in its in-flight registry, (b) releases all
  bodies, (c) re-instantiates (epoch+1; completions from the old epoch are dropped), (d) sends `configure` with the
  persisted ledger and host state, (e) re-submits unfinished items **once** as a new run with
  `resume: {item: {inst_fetches_used, restarted: true}}` from its journal. Saved items are not re-run (the JS journal
  records `save_pdf` successes). A second engine trap in the same run stops the run and shows `Failed(internal)`
  with the panic text in the report. The ledger cannot under-count after a trap: Rust **reserves** a ledger slot and
  emits `persist` *before* emitting an institutional PDF fetch (reserve, then commit or roll back).

**Memory bounds.**
- Engine: never receives raw bodies, only inspections (≤ 64 KiB each). `StepIn` is capped at 4 MiB (JS splits).
  ≤ 256 tasks, ≤ 16 concurrent ops per item by construction, outbox bounded by ops. Ledger ≤ 80 entries; host state
  LRU ≤ 500 hosts. Expected < 4 MiB; JS recycles the engine when idle and `fx_mem_pages` > 256 (16 MiB).
- Pure: blob ≤ 2 MiB, total staged ≤ 4 MiB; recycled after any trap, when memory > 64 MiB, or every 500 calls
  (a 6 MB page grew memory to 27 MB, which never shrinks [07 §3.1]).
- JS BodyStore: Rust holds a **bytes semaphore** (192 MiB). A PDF fetch acquires `max(content-length hint, 16 MiB)`
  before its effect is emitted and adjusts on completion; per-download `max_bytes` = 100 MiB.

### 2.2 Executor core (sample code)

```rust
//! crates/fulltext-engine/src/exec.rs: single-threaded executor for wasm32 (no imports, no threads)
use std::{cell::{Cell, RefCell}, collections::VecDeque, future::Future, pin::Pin,
          sync::{Arc, atomic::{AtomicBool, Ordering}}, task::{Context, Wake, Waker}};
use crate::reactor;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TaskKey { idx: u32, gen: u32 }
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
pub struct Owner { pub run: u32, pub item: u32 }
pub struct Full;
pub struct RunReport { pub polls: u32, pub more: bool }

thread_local! {
    static READY: RefCell<VecDeque<TaskKey>> = const { RefCell::new(VecDeque::new()) };
    static LOST_WAKE: Cell<bool> = const { Cell::new(false) };
    /// Set while a task is polled or dropped, so leaf ops tag their effects with run/item.
    pub(crate) static CURRENT: Cell<Option<Owner>> = const { Cell::new(None) };
}

struct TaskWaker { key: TaskKey, queued: AtomicBool }

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) { self.wake_by_ref() }
    fn wake_by_ref(self: &Arc<Self>) {
        if self.queued.swap(true, Ordering::AcqRel) { return; }            // already queued
        let pushed = READY.with(|q| q.try_borrow_mut().map(|mut q| q.push_back(self.key)).is_ok());
        if !pushed { LOST_WAKE.with(|f| f.set(true)); }                    // executor sweeps all tasks
    }
}

struct Task { gen: u32, owner: Owner, waker: Arc<TaskWaker>,
              fut: Option<Pin<Box<dyn Future<Output = ()>>>> }

pub struct Executor { slots: Vec<Option<Task>>, gens: Vec<u32>, free: Vec<u32>, max_tasks: u32 }

impl Executor {
    pub fn spawn(&mut self, owner: Owner, fut: impl Future<Output = ()> + 'static) -> Result<TaskKey, Full> {
        let idx = match self.free.pop() {
            Some(i) => i,
            None => {
                let i = u32::try_from(self.slots.len()).map_err(|_| Full)?;
                if i >= self.max_tasks { return Err(Full); }
                self.slots.push(None);
                self.gens.push(0);
                i
            }
        };
        let gen = self.gens.get(idx as usize).copied().unwrap_or_default();
        let key = TaskKey { idx, gen };
        let waker = Arc::new(TaskWaker { key, queued: AtomicBool::new(false) });
        *self.slots.get_mut(idx as usize).ok_or(Full)? =
            Some(Task { gen, owner, waker: Arc::clone(&waker), fut: Some(Box::pin(fut)) });
        waker.wake_by_ref();                                                 // schedule the first poll
        Ok(key)
    }

    /// Dropping the future runs leaf `Drop`s: `cancel` effects, permits released, bodies released.
    fn retire(&mut self, idx: u32) {
        let task = self.slots.get_mut(idx as usize).and_then(Option::take);
        if let Some(g) = self.gens.get_mut(idx as usize) { *g = g.wrapping_add(1); }
        if let Some(t) = task {
            self.free.push(idx);
            CURRENT.with(|c| c.set(Some(t.owner)));
            drop(t);
            CURRENT.with(|c| c.set(None));
        }
    }

    pub fn cancel_where(&mut self, pred: impl Fn(&Owner) -> bool) -> usize {
        let doomed: Vec<u32> = self.slots.iter().zip(0u32..)
            .filter_map(|(s, i)| s.as_ref().filter(|t| pred(&t.owner)).map(|_| i)).collect();
        for idx in &doomed { self.retire(*idx); }
        doomed.len()
    }

    pub fn run(&mut self, budget: u32) -> RunReport {
        let mut polls = 0;
        while polls < budget {
            if LOST_WAKE.with(|f| f.replace(false)) { self.requeue_all(); }
            let Some(key) = READY.with(|q| q.try_borrow_mut().ok().and_then(|mut q| q.pop_front())) else {
                return RunReport { polls, more: false };
            };
            let Some(Some(task)) = self.slots.get_mut(key.idx as usize) else { continue };
            if task.gen != key.gen { continue; }                             // stale wake, slot reused
            task.waker.queued.store(false, Ordering::Release);               // wakes during poll re-queue
            let (Some(mut fut), owner) = (task.fut.take(), task.owner) else { continue };
            let waker = Waker::from(Arc::clone(&task.waker));
            CURRENT.with(|c| c.set(Some(owner)));
            let ready = fut.as_mut().poll(&mut Context::from_waker(&waker)).is_ready();
            CURRENT.with(|c| c.set(None));
            polls += 1;
            if ready { drop(fut); self.retire(key.idx); }
            else if let Some(Some(t)) = self.slots.get_mut(key.idx as usize) { t.fut = Some(fut); }
            for req in reactor::take_exec_requests() { self.apply(req); }    // e.g. cancel item, halt route
        }
        RunReport { polls, more: true }
    }
}
```

Leaf op (excerpt of `reactor.rs`):

```rust
pub struct Op { id: Option<OpId>, body: Option<EffectBody> }        // Unpin

impl Future for Op {
    type Output = Completion;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Completion> {
        if let Some(body) = self.body.take() {                          // first poll: emit the effect
            return match with(|r| r.submit(body, cx.waker().clone())) {
                Some(id) => { self.id = Some(id); Poll::Pending }
                None => Poll::Ready(Completion::internal("reactor busy")),
            };
        }
        let Some(id) = self.id else { return Poll::Ready(Completion::internal("polled after completion")) };
        match with(|r| r.take_or_park(id, cx.waker())) {                // Done -> take; Waiting -> refresh waker
            Some(Some(c)) => { self.id = None; Poll::Ready(c) }
            Some(None) => Poll::Pending,
            None => Poll::Ready(Completion::internal("reactor busy")),
        }
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {                              // still waiting: tell JS to abort
            if with(|r| r.cancel(id)).is_none() { defer_cancel(id); }
        }
    }
}
```

`with()` is `R.with(|r| r.try_borrow_mut().ok().map(|mut r| f(&mut r)))`. Typed wrappers
(`http(req) -> Result<HttpResp, NetErr>`, `browser_load`, `login_window`, `translators`, `save_pdf`, `confirm`)
are thin `Op` adapters. Combinators come from `futures-util` (`default-features = false`, `alloc`,
`async-await-macro`); only `select_biased!` is used (deterministic, no RNG). Off-the-shelf executors were rejected:
`wasm-bindgen-futures` needs imports, and `futures-executor::LocalPool` has no owner-tagged cancellation, no poll
budget and no generation-checked keys.

### 2.3 Scheduler: where every limit lives

All of the following are Rust (`fulltext-engine::sched`), in one `Rc<RefCell<Sched>>` shared by item futures. Waits
are async: `gate.acquire(key, class).await -> Permit`; `Permit::drop` releases the slot, stamps `next_at`, and wakes
the FIFO head.

| Mechanism | Value (default; hard ceiling in Rust where licence-derived) | Source |
|---|---|---|
| Item concurrency (`items` semaphore) | 4 (pref, 1-6) | [05 §6] |
| OA API token buckets | OpenAlex 10/s; Europe PMC 5/s; NCBI/PMC S3 3/s and 5/s; bioRxiv 1/s; arXiv export 1 per 3 s, concurrency 1; Crossref 5/s, concurrency 1; Unpaywall 10/s; Zotero OA 2/s; Semantic Scholar 1/s (key only); CORE 10/min | [S §d], [05 §6] |
| Publisher host gate, OA route | concurrency 1 per host-key, ≥1 s gap; 5 consecutive failures → skip host for the run | [05 §6], [06 §5.2] |
| Institutional global semaphore | **2** in flight | [04 §8.2] |
| Institutional host gate | concurrency **1** per host; ≥2 s between page fetches, ≥**5 s + 0-3 s jitter** between PDFs (PRNG seeded from JS) | [04 §8.2] |
| Licence ledger (persisted) | ≤**30** PDFs/hour, ≤**80** per rolling 24 h, ≤**300 MB**/hour; reserve before fetch, commit on PDF, roll back on non-PDF; user prefs may only lower these | [04 §8.2], [S §c] |
| Batch cap | institutional route for ≤**20** items per run; `confirm` effect when >10 eligible items; items beyond 20 run OA-only (outcome `LicenceCap`) | [04 §8.2] |
| Per-item institutional fetch budget | ≤**4** fetches (landing, PDF link, PDF, +1); translators on institutional pages count as 1 | [04 §8.2] |
| Stop signals | 429, `EzproxyErrorPage`, suspension/"unusual activity" text, a second challenge on the same host after a browser tier → **halt institutional route for the run**, `blocked_until = now + max(Retry-After, 30 min)` for that host (persisted) | [04 §8.2] |
| No retry storms | at most one retry per OA lookup (429/5xx, after `Retry-After` capped at 10 s); no retries on institutional routes | [S §d], [04 §8.2] |
| Browser slots | ≤2 concurrent HiddenBrowsers (each costs a content process) | judgement |
| `InteractiveGate` | ≤1 visible login/challenge window at a time, engine-wide | [08 §5] |
| `LoginGate` | one interactive sign-in per session epoch, shared by all items and runs (2.6) | [08 §5] |
| Save semaphore | 1 (Zotero DB writes are serialised anyway) | - |
| Bytes semaphore | 192 MiB of retained bodies | 2.1 |
| Dedup | an item already in an active run → `InProgressElsewhere` | - |

**JS tripwires (defence in depth, not policy):** ≤24 concurrent XHRs, ≤3 HiddenBrowsers, ≤1 login window, ≤256 MiB
BodyStore. If Rust ever exceeds them, JS queues the effect and logs an invariant violation (a test failure in CI).

### 2.4 Item-level and intra-item concurrency

- **Across items:** all item tasks are spawned at `start_run` and immediately wait on the `items` semaphore (cheap:
  an unstarted async block). OA work of other items continues while one item waits for the institutional semaphore
  or the login gate, because those are separate gates.
- **Phase 1 OA lookups (per item, parallel):** a `FuturesUnordered` of source lookups, each
  `timeout(9 s, bucket.acquire() then http)`. When a PMCID becomes known (from OpenAlex `ids.pmcid`, Europe PMC, or
  the item field), the PMC S3 metadata lookup is pushed into the *same* set (chained concurrency). The set ends when
  (a) empty, (b) 12 s overall budget, or (c) a strong candidate exists (PMC S3 PDF, arXiv, publisher gold PDF) and
  1.5 s have passed since it appeared. Stragglers are dropped, which cancels their XHRs.
- **Phase 2 candidate attempts:** ranked by `core::rank` [S §d]. Default **sequential**, stopping at the first
  verified PDF. **Hedged** only on the OA route and only across *different* host-keys: if attempt *k* has not
  answered in 3 s, start *k+1*; at most 2 in flight per item; first verified PDF wins and the loser is dropped.
  **Never hedged on institutional routes** (licence).
- **Translators alongside the extractor:** on a landing page the engine has the Rust inspection immediately and emits
  `run_translators` concurrently. A Rust *publisher rule* for a known Zotero gap (LWW `wkhealth_pdf_url`, ScienceDirect
  JSON state, NEJM/T&F/SAGE `/doi/pdf/`, Wiley `pdfdirect`, linkinghub) ranks first; otherwise the translator result
  ranks first, then Rust generic signals [06 §0, §2.1]. Wait for translators ≤ 8 s, then proceed with Rust candidates
  and append a late translator URL if it differs. Dedupe by URL including the query [05 §5].

### 2.5 Cancellation and timeouts

| Trigger | Mechanism |
|---|---|
| User "Cancel all" (progress dialog) | `cancel_run` → `cancel_where(run)` drops item futures inside the step → `cancel`/`release` effects in the same `StepOut` → JS aborts XHRs (`cancellerReceiver`), destroys HiddenBrowsers, closes windows. |
| Per-item ✕ | `cancel_item`. |
| Item trashed/deleted mid-run | JS `Zotero.Notifier` observer → `cancel_item`. |
| Plugin disable/uninstall/upgrade | `shutdown()` → `fx_step({commands:[{type:"shutdown"}]})` → execute returned cancels synchronously → remove MIME dispatcher → drop instances → `chromeHandle.destruct()`. |
| Zotero quit (`APP_SHUTDOWN`) | skip wasm; JS aborts its in-flight registry best-effort; no teardown [S §a]. |
| Engine-internal (timeouts, hedge losers, halted route) | drop the losing future; same effect path. |

Timeouts are owned by Rust (`timeout()` combinator on virtual time); JS adds a hard backstop slightly longer.

| Operation | Rust timeout | JS backstop |
|---|---|---|
| OA API GET | 9 s | XHR `timeout` 11 s |
| Landing page GET | 20 s | 22 s |
| PDF GET | 90 s, `max_bytes` 100 MiB | 95 s / `requestObserver` abort |
| No-follow probe | 8 s | 10 s |
| HiddenBrowser load + settle | 60 s (settle = URL stable 3 s) | 65 s then `destroy()` |
| Translators | 8 s | none (late result ignored) |
| Login window | 5 min (user can press "Keep waiting") | closes on `cancel` |
| Item, excluding time parked in the LoginGate | 6 min | - |

### 2.6 LoginGate (Rust)

```
state: Idle | Prompting{epoch, owner} | Declined{run}           epoch: u32 (bumps on each successful sign-in)

ensure_session(entry_url, seen_epoch) -> SessionOutcome
  Idle, epoch > seen_epoch  -> Retry                 // someone signed in since this item tried: re-run hidden
  Idle                      -> Prompting; acquire InteractiveGate; emit login_window(entry_url, plain UA)
                               landed|pdf -> epoch += 1; Idle; wake all parked; return Landed(result)
                               closed_by_user -> Declined{run}; wake all; return Declined
                               timeout -> Idle; return TimedOut
                               ca_blocked -> Declined{run}; return ConditionalAccess
  Prompting{..}             -> park (waker list) until state changes, then loop
  Declined{run == mine}     -> Declined (no more windows in this run)
```

At most one window per sign-in attempt; each item retries its hidden redirector at most once after an epoch bump;
landing on the IdP again → `NeedsLogin` (hand-off). The pref `institution.autoOpenLogin=false` makes the gate wait
for `user_action sign_in_now` (the "Sign in" button in the progress dialog) instead of opening the window itself.

### 2.7 Placement summary: what lives where, and why

| Concern | Rust | JS | Why |
|---|---|---|---|
| Identifier normalisation, Extra parsing | ✓ | | pure, fixture-tested |
| OA API parsing, ranking, dedupe | ✓ (pure) | | untrusted input, native tests |
| HTML classify/extract, publisher rules | ✓ (pure) | | [S C2], fixtures |
| Proxify/unproxify, redirector URLs | ✓ (pure) | | [04 §6] test vectors |
| Pipeline order, tier escalation, outcome mapping | ✓ (engine) | | the product logic |
| Concurrency, gates, buckets, hedging, ledger, timeouts, cancellation | ✓ (engine) | tripwires | one source of truth; deterministic sim tests |
| Time | consumes `now`/`wall` | supplies `Cu.now()`/`Date.now()` | wasm has no clock without imports |
| HTTP, HiddenBrowser, PageData, MIME capture, windows | | ✓ | Gecko/Zotero APIs |
| Translators, Zotero OA, save, Notifier, prefs I/O | | ✓ | Zotero APIs |
| PDF bytes | handle + head/tail verdict | storage + write | requirement D6 |
| UI (menus, dialog, report, prefs pane, prompts) | emits `progress`/`run_finished` | renders | XUL/Fluent |

---

## 3. Per-item pipeline

### 3.1 Stages

1. **Eligibility (Rust, from the JS snapshot):** regular, non-feed item; library `files_editable`;
   `pdf_attachments == 0`; skip if `best_attachment.type == "pdf" && !exists` (synced but not downloaded)
   [S §a, 02 §8]. JS re-checks at save time (race with the user adding a PDF).
2. **Identifiers:** DOI from the field or Extra (cleaned, compared case-insensitively), PMID, PMCID, arXiv id
   (`10.48550/arXiv.X`, `archiveID`, arxiv.org URL), bioRxiv/medRxiv from `10.1101/` [S §d].
3. **OA discovery** (2.4): OpenAlex `/works/doi:` singleton (keyless primary), Europe PMC REST, bioRxiv details,
   optional Zotero OA, Unpaywall only with a user email, Semantic Scholar/CORE only with keys; then PMC S3 metadata.
4. **Ranking:** published > accepted > submitted (prefs); rank every `locations[]`; down-weight other-DOI records
   from unknown sources (OpenAlex merges); "gold" is not a PDF; PMC S3 > publisher > arXiv/medRxiv early > repository
   [S §d, 05 §5].
5. **OA fetch tiers** for each candidate, in order [S §d]: PMC S3 PDF; `europepmc.org/api/getPdf?pmcid=` (flaky,
   toggle) [S C1]; `arxiv.org/pdf/{id}`; medRxiv early path; direct `pdf_url`s; landing pages (Rust extractor +
   translators); challenge-walled copies (PMC website) only via the browser tier. Tier escalation per URL:
   `http` → `inspect.class == challenge` → `browser_load(capture_pdf)` hidden with the plain UA → for OA, stop there
   (no visible window on OA routes in v1).
6. **Institutional route** (if enabled, OA exhausted, and the run still allows it):
   a. Alma CTO entitlement check (`svc_dat=CTO`): no `getFullTxt` → `NotEntitled` (with hand-off links) [S §c].
   b. **Direct**: `https://doi.org/<doi>` (campus IP or existing publisher session), plain UA for Cloudflare-fronted
      hosts → landing → candidates → PDF.
   c. **OpenAthens redirector** in a HiddenBrowser: `https://go.openathens.net/redirector/kcl.ac.uk?url=` +
      `encodeURIComponent("https://doi.org/" + doi)` (never `thelancet.com` URLs) [S §c].
      `OpenAthensWait` is handled by the browser itself; `IdpLogin`/`SamlAutoPost` stuck → LoginGate;
      `OpenAthensUnknownResource`/`BadOrg` → EZproxy if applicable, else `NeedsLogin`.
   d. **Login**: custom window opened **at the entry URL**, not the IdP URL [S §c]. After `Landed`, the hidden
      redirector is retried once (session reuse, test T4 in [08 §4.1]).
   e. **Extract** from the landed page (`documentHTML` via PageData → `pure.inspect` + translators).
   f. **PDF fetch**, same jar, same UA: `http` → if HTML/challenge → `browser_load(candidate, capture_pdf)` → if still
      nothing and the gate allows → `challenge_window` (the user acts; never solve CAPTCHAs) → else hand-off.
   g. **EZproxy** only for hosts confirmed configured (static list of about 19 hosts + learned 7-day cache from no-follow
      probes) or doi.org entries: `https://login.kcl.idm.oclc.org/login?qurl=` + full percent-encoding [S §c].
7. **PDF validation** (`pure.pdf_verdict` on head+tail): `%PDF-` within the first 1 KiB, `%%EOF` in the last 1 KiB
   (warn only), length > 1 KiB, not HTML. The stored file must start with `%PDF-` at offset 0: Rust returns
   `trim_prefix`, JS trims and re-asserts the 5 bytes before writing [S C7].
8. **Save** (`save_pdf`): filename `getFileBaseNameFromItem(parent, {attachmentTitle}) + '.pdf'`; title
   `attachment.fullText` / `attachment.acceptedVersion` / `attachment.submittedVersion`; URL = **un-proxied,
   un-signed** URL chosen by Rust (`unproxify`; signed hosts such as `pdf.sciencedirectassets.com` or token queries
   → store the landing or `https://doi.org/<doi>` URL instead); never `login?`, redirector or SAML URLs [S §c].
   A group library triggers a one-time `confirm` per run before any institutional save [04 §8.2].

### 3.2 State machine

```
 Queued ──permit──► Checking ──ineligible──────────────────────────────► Skipped{reason}
                       │ ok
                       ▼
                   Identifying ──no DOI/PMID/PMCID/arXiv/URL──────────────► NoIdentifier
                       ▼
                   DiscoveringOa ──► Ranking ──► AttemptingOa{k, tier} ──verified PDF──► Validating ─► Saving ─► Saved{route:oa}
                                                     │ exhausted                            │bad      │err
                                                     ▼                                      └─►(next k)└─► Failed{save}
                         institution off / OA-only run / cap reached ───────────────────────► NotFound | LicenceCap
                                                     │ allowed
                                                     ▼
                   InstEntitlement ──not held──► NotEntitled{handoff}
                       ▼ held/unknown
                   InstDirect ──PDF──► Validating ─► Saving ─► Saved{route:direct}
                       ▼ login/SSO/challenge/no access
                   InstRedirector(hidden) ──landed on publisher──► InstExtract ──► InstPdf{tier} ──PDF──► Validating ─► Saving ─► Saved{route:openathens|ezproxy}
                       │ IdP page                                     │ no candidate / tiers exhausted ──► NeedsUser{handoff}
                       ▼                                              │ block/429/suspend ──► Blocked{host} (+ halt run's inst route)
                   AwaitLogin(LoginGate) ──Retry──► InstRedirector (once)
                       │ Landed ─────────────────► InstExtract
                       │ Declined/ConditionalAccess ──► LoginDeclined{handoff}
                       └ TimedOut ────────────────────► NeedsLogin{handoff}
 any state ──cancel──► Cancelled        any state ──phase timeout──► TimedOut        engine trap twice ──► Failed{internal}
```

Terminal outcomes (`Outcome` enum): `Saved{route, source, version, url}`, `SkippedHasPdf`, `SkippedNotDownloaded`,
`SkippedNotEditable`, `SkippedNotRegular`, `NoIdentifier`, `InProgressElsewhere`, `NotFound`, `NotEntitled`,
`NeedsLogin`, `LoginDeclined`, `NeedsUser` (challenge or no PDF link), `Blocked{host}`, `LicenceCap{which}`,
`InstitutionalHalted`, `Cancelled`, `TimedOut`, `Failed{code}`. Every institutional failure carries
`Handoff {redirector_url, libkey_url: "https://libkey.io/libraries/286/<doi>", doi_url}` [08 §5].

### 3.3 One pipeline stage in Rust: OA discovery

```rust
//! crates/fulltext-engine/src/pipeline/oa.rs
use futures_util::{future::{Fuse, FusedFuture, FutureExt}, select_biased, stream::{FuturesUnordered, StreamExt}};

const LOOKUP_TIMEOUT_MS: u64 = 9_000;
const DISCOVERY_BUDGET_MS: u64 = 12_000;
const STRONG_SETTLE_MS: u64 = 1_500;

pub async fn discover_oa(cx: &ItemCx, ids: &Ids) -> CandidatePool {
    let mut pool = CandidatePool::new(ids);
    if let Some(a) = &ids.arxiv { pool.add(Candidate::arxiv_pdf(a)); }      // deterministic URL, no lookup

    let mut lookups = FuturesUnordered::new();
    if let Some(doi) = &ids.doi {
        let o = &cx.cfg.oa;
        if o.openalex  { lookups.push(lookup(cx, Source::OpenAlex,  api::openalex_work(doi, o)).boxed_local()); }
        if o.europepmc { lookups.push(lookup(cx, Source::EuropePmc, api::epmc_by_doi(doi)).boxed_local()); }
        if o.biorxiv && ids.is_rxiv() { lookups.push(lookup(cx, Source::BioRxiv, api::rxiv_details(doi)).boxed_local()); }
        if o.zotero_oa { lookups.push(zotero_oa(cx, doi).boxed_local()); }
        if let Some(email) = &o.unpaywall_email { lookups.push(lookup(cx, Source::Unpaywall, api::unpaywall(doi, email)).boxed_local()); }
    }
    let mut pmc_started = false;
    let mut overall = cx.sleep(DISCOVERY_BUDGET_MS).fuse();
    let mut settle = Fuse::terminated();                                     // armed when a strong candidate appears

    loop {
        if !pmc_started && cx.cfg.oa.pmc_s3 {
            if let Some(pmcid) = pool.pmcid() {                              // chain PMC S3 into the same set
                pmc_started = true;
                lookups.push(lookup(cx, Source::PmcS3, api::pmc_s3_meta(&pmcid)).boxed_local());
            }
        }
        if lookups.is_empty() { break; }
        select_biased! {
            res = lookups.select_next_some() => match res {
                Ok(found) => {
                    pool.merge(found);                                       // candidates + learned ids
                    if settle.is_terminated() && pool.has_strong() { settle = cx.sleep(STRONG_SETTLE_MS).fuse(); }
                }
                Err(e) => cx.trace(TraceEvent::LookupFailed(e)),
            },
            _ = settle => break,                                             // good enough: start fetching
            _ = overall => break,
        }
    }
    drop(lookups);                                                           // stragglers -> `cancel` effects
    pool
}

async fn lookup(cx: &ItemCx, src: Source, req: HttpReq) -> Result<Found, LookupError> {
    let _permit = cx.sched.api(src).acquire().await?;                       // token bucket + concurrency
    let resp = cx.timeout(LOOKUP_TIMEOUT_MS, cx.http(req.expect_api(src))).await
        .ok_or(LookupError::Timeout(src))??;
    match resp.inspect.api {                                                 // parsed by the PURE instance
        Some(api) if api.status == ApiStatus::Found => Ok(Found::from(api)),
        Some(api) if api.status == ApiStatus::NotFound => Ok(Found::empty(src)),
        _ if resp.status == 429 || resp.status >= 500 => Err(LookupError::Throttled(src, resp.retry_after())),
        _ => Err(LookupError::Unparseable(src, resp.status)),
    }
}
```

`ItemCx` holds `Rc` handles to config (a snapshot taken at `start_run`, so pref changes do not alter a running batch),
the scheduler, and the run state. `cx.http`, `cx.sleep` and `cx.timeout` are the typed leaf wrappers from 2.2.
The item entry point is a plain `async fn run_item(cx) -> Outcome` that acquires the `items` permit, then awaits
precheck, `discover_oa`, `attempt_oa`, and `institutional`, each wrapped in its phase timeout; the task body emits the
terminal `progress` effect from the returned `Outcome`.

---

## 4. UI

### 4.1 Context menu (MenuManager, target `main/library/item`, l10nID only [S §a])

| menuID | Fluent ID (`.label`) | Visible / enabled |
|---|---|---|
| `zotero-fulltext-get` | `zotero-fulltext-menu-get` = "Get Full Text" | visible if any regular item is selected; enabled if any is eligible |
| `zotero-fulltext-get-oa` | `zotero-fulltext-menu-get-oa` = "Get Full Text (Open Access Only)" | same |
| `zotero-fulltext-attach` | `zotero-fulltext-menu-attach` = "Attach Downloaded PDF…" | exactly one regular item, no PDF |

Labels avoid the built-in "Find Full Text" [S §a]. `onShowing` and `onCommand` never throw synchronously.
`onCommand` builds item snapshots in JS (cheap field reads) and sends `start_run`. FTL: one file
`locale/en-US/zotero-fulltext.ftl`, `insertFTLIfNeeded` for every main window, including windows already open in
`startup()` (`onMainWindowLoad` does not fire for the first window [S §a]); `Zotero.ftl.addResourceIds` for strings used
through `Zotero.getString`.

### 4.2 Progress and report

- **Single item:** `Zotero.ProgressWindow` (`ItemProgress` with the item type icon; on success
  `setItemTypeAndIcon('attachmentPDF')`, `setProgress(100)`, close timer 4 s). On a non-saved outcome it stays open
  with one line and an `addDescription` link "Details…" that opens the report window.
- **Batch (≥2 items) or any item needing the user:** a non-modal custom window `content/progress.xhtml`
  (`Services.ww.openWindow`, `dialog=no`). Columns: Item | Status | Source | Action. Buttons: **Cancel all**,
  **Sign in** (shown while the LoginGate waits and `autoOpenLogin` is off), **Close** (hides; the run continues).
  Each row has ✕ (cancel item). Status strings come from `progress.l10n` + args (for example
  `zotero-fulltext-status-waiting-host { $host } { $seconds }`), so users can see the licence pacing.
- **End-of-run report** (same window): counts by outcome, the licence ledger ("Institutional PDFs: 7 in the last hour
  (limit 30), 12 today (limit 80)"), and per-row actions by outcome: `NeedsLogin`/`LoginDeclined`/`NeedsUser`/`Blocked`
  → **Open in browser** (redirector or LibKey via `Zotero.launchURL`) and **Attach PDF…**; `NotFound` → **Attach PDF…**;
  any failure → **Retry** (new user-initiated run). **Copy report** puts a text summary (no cookies or page text) on the clipboard.

FTL IDs (en-US only): `zotero-fulltext-progress-title`, `-status-{queued,checking,searching-oa,downloading,
waiting-host,waiting-login,signing-in,institutional,saving,saved-oa,saved-inst,skipped-has-pdf,skipped-not-downloaded,
not-editable,no-identifier,not-found,not-entitled,needs-login,login-declined,needs-user,blocked,cap-reached,halted,
cancelled,timed-out,failed}`, `-action-{open-browser,attach-file,sign-in,retry,cancel,cancel-all,close,copy-report}`,
`-confirm-large-batch` (`$count`), `-confirm-group-library`, `-login-{title,hint,done,use-browser,keep-waiting,cancel}`,
`-pref-*`.

### 4.3 Login window (`content/login.xhtml` + `login.js`, from `tools/sso-spike/plugin/content/`)

- XUL window with one `<browser type="content" maychangeremoteness="true" disableglobalhistory="true">` and **no**
  `messagemanagergroup` (no ExternalLinkHandler), an origin + TLS bar from `browsingContext.secureBrowserUI.state`,
  and a visible popup pane via `window.browserDOMWindow` [08 §6].
- Plain Firefox UA as `customUserAgent`; load with the system triggering principal; default cookie jar.
- Title: "Sign in to King's College London (for: <item title>)". Hint text: "If you see 'Just a moment…', wait or
  tick the box. Zotero Full Text never sees your password."
- Buttons: **I have finished signing in** (forces a `login_state` check), **Open in my browser instead** (hand-off;
  completes as `closed_by_user` and marks the run Declined), **Cancel**.
- Completion detection: JS polls `browser.currentURI` every 500 ms; when the URL has been stable for 1.5 s it reads
  `documentHTML` through the PageData actor and calls `pure.login_state(url, html, done_when)`. Terminal states are
  `landed` (host in target hosts, not IdP/OpenAthens/EZproxy login), `pdf` (captured through the MIME dispatcher),
  and `ca_blocked` (Entra error page). Nothing is recorded except the URL host and the class.
- The same window type in "challenge" mode is used for `challenge_window`: hint "Complete the check if asked; the PDF
  will be captured automatically."

PDF capture: one plugin-wide `Zotero.MIMETypeHandler.addHandlers('application/pdf', dispatcher, true)`, registered
by reference count only while capture ops are in flight. Its `onContent(blob, name, type, channel)` resolves
`channel…topFrameElement` **per channel** and routes the blob to the op that owns that `<browser>`; it returns `false`
for anything else. This avoids the shared `isOurPDF` state in Zotero's per-browser handler pattern
(`browserRequest.js:525-559`), which can mis-attribute when two captures overlap.

### 4.4 Preferences pane (`Zotero.PreferencePanes.register`, `preference="extensions.zotero-fulltext.<key>"`)

All keys live under `extensions.zotero-fulltext.`. Text inputs bind to string prefs [01 §4.3].

| Key | Type | Default | UI |
|---|---|---|---|
| `institution.enabled` | bool | true | "Use my institution's access" |
| `institution.config` | string (JSON) | KCL blob [04 §6.1] (OpenAthens `kcl.ac.uk`, EZproxy login host + suffix, Entra tenant, Alma `44KCL_INST`, LibKey 286) | menulist "King's College London" / "Custom (JSON)" |
| `institution.route.direct` / `.openathens` / `.ezproxy` | bool | true / true / true | checkboxes |
| `institution.checkHoldings` | bool | true | "Check library holdings (Alma) first" |
| `institution.autoOpenLogin` | bool | true | "Open the sign-in window automatically" |
| `institution.windowsSSO` | bool | **false** | "Use Windows sign-in for device-based Conditional Access" (sets `network.http.windows-sso.enabled`, remembers the previous value in `institution.windowsSSO.prev`; security note) [08 §2.5] |
| `oa.openalex` / `oa.openalex.apiKey` | bool / string | true / "" | key is optional (Bearer) |
| `oa.europepmc`, `oa.pmcS3`, `oa.biorxiv`, `oa.europepmcGetPdf`, `oa.medrxivEarly`, `oa.zoteroOA` | bool | true | checkboxes; the undocumented endpoints are labelled as such |
| `oa.unpaywall.email` | string | "" (**no default; Unpaywall is off while empty**) | validated |
| `oa.semanticScholar.apiKey`, `oa.core.apiKey`, `oa.crossref.mailto` | string | "" (off) | optional |
| `versions.published` / `.accepted` / `.submitted`, `versions.preferPublished` | bool | true | ranking |
| `run.itemConcurrency` | int | 4 | 1-6 |
| `limits.batchMax` / `limits.confirmAbove` | int | 20 / 10 | can only be lowered |
| `limits.hourly` / `limits.daily` / `limits.mbPerHour` | int | 30 / 80 / 300 | can only be lowered; Rust clamps to these ceilings |
| `warn.groupLibrary` | bool | true | |
| `debug.trace` | bool | false | emits `trace` effects to `Zotero.debug` |
| `state.ledger`, `state.hosts` | string (JSON) | "" | internal (not shown); "Reset learned host data" button |

The pane also shows a privacy note: the redirector and EZproxy see target URLs; nothing else leaves the machine
except API queries [S §g].

---

## 5. Code layout

### 5.1 Rust workspace (`Cargo.toml`, `resolver = "3"`, Rust 1.96, `idna_adapter` pinned to 1.0.0 [S §f])

```
crates/
  fulltext-core/        rlib, PURE, no async, native-tested
    ids.rs        pub fn normalise(fields: &ItemFields) -> Ids; pub fn clean_doi(&str) -> Option<Doi>
    oa/{openalex,europepmc,biorxiv,pmc_s3,unpaywall,zotero_oa,s2,core,crossref,alma}.rs
                  pub fn parse(status: u16, body: &[u8], requested: &Ids) -> ApiInspection
    rank.rs       pub fn rank(pool: CandidatePool, prefs: &RankPrefs) -> Vec<Candidate>
    html/{gum.rs, extract.rs, rules.rs}
                  pub fn extract_pdf_candidates(html: &str, page_url: &Url) -> Vec<PdfCandidate>
                  pub fn page_ids(html: &str, page_url: &Url) -> PageIds
    classify.rs   pub fn classify(meta: &RespMeta, body_head: &[u8]) -> PageClass   // [S §c] rule order
    proxy.rs      login_url, proxify_host, unproxify, learn_from_redirect, redirector_url  // [04 §6.2-6.4]
    login.rs      pub fn login_state(url: &Url, html: Option<&str>, done: &DoneWhen) -> LoginState
    pdf.rs        pub fn pdf_verdict(head: &[u8], tail: &[u8], len: u64) -> PdfVerdict
    inspect.rs    pub fn inspect(args: InspectArgs, body: &[u8]) -> Inspection          // fx_call "inspect"
    hosts.rs      static rules: Cloudflare-fronted, signed-URL hosts, EZproxy static list, UA policy
  fulltext-engine/      rlib, async, depends on core + futures-util
    exec.rs       Executor, TaskKey, Owner, TaskWaker, RunReport
    reactor.rs    Reactor, Op, Sleep, OpId, Completion, BodyHandle, with(), defer_cancel()
    proto.rs      StepIn, StepOut, Command, Effect, EffectBody, CompletionPayload (serde)
    sched/{gate.rs, bucket.rs, ledger.rs, login_gate.rs, interactive.rs, bytes.rs}
                  pub async fn Gate::acquire(&self, key: HostKey, class: Class) -> Result<Permit, Refused>
                  pub fn Ledger::reserve(&mut self, wall: u64, est_bytes: u64) -> Result<Reservation, CapHit>
    pipeline/{item.rs, precheck.rs, oa.rs, attempt.rs, institutional.rs, login.rs, save.rs, outcome.rs}
                  pub async fn run_item(cx: ItemCx) -> Outcome
    engine.rs     pub struct Engine; pub fn step(&mut self, input: StepIn) -> StepOut
    sim.rs        (cfg(test) + feature "sim") SimHost: virtual clock, scripted responses, trace recorder
  fulltext-wasm/        cdylib only (keeps LTO [07 §1.3]); abi.rs: all fx_* exports, panic hook, blob staging
xtask/                  build, check, dist, dev, harness
plugin/                 static plugin files (below)
tests/js/               Node tests for glue + host
```

Lints (workspace): deny `unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`, `unimplemented`,
`string_slice`; warn `indexing_slicing` [07 §4]. Release profile: `opt-level="z"`, `lto=true`, `codegen-units=1`,
`panic="abort"`, `strip=true` [S §f]. Size budget: about 420 KB raw / 150 KB gzip (core 308 KB [07 §1.4] + engine +
futures-util; to be measured).

### 5.2 JS files (`plugin/`; classic scripts, IIFE, no top-level `let/const/class`)

| File | Responsibility | Est. lines |
|---|---|---|
| `manifest.json` (generated) | MV2, `applications.zotero` | - |
| `bootstrap.js` | startup/shutdown idempotent (sandbox reused on re-enable), `registerChrome`, load subscripts, init existing windows, menus, prefs pane, pref observers, Notifier observer, `shutdown` command | 150 |
| `prefs.js` | defaults (bool/string/int only) | 40 |
| `content/core-glue.js` | compile Module once; `Engine` (`step`) and `Pure` (`call`, `stage`) wrappers; poison/reset; epoch; memory recycle | 170 |
| `content/host.js` | EffectRunner: queue, drain, timer, dispatch table, in-flight registry (op → abort fn), tripwires, journal for trap restart, stall watchdog | 220 |
| `content/net.js` | `http` effect; BodyStore; staging policy; header whitelist; UA header | 160 |
| `content/browser.js` | `browser_load`; settle logic (re-implemented); PageData reads; MIME capture dispatcher | 200 |
| `content/login.js`, `login.xhtml` | login/challenge window and polling loop | 180 |
| `content/adapter.js` | item snapshots, eligibility re-check, translators, Zotero OA, `save_pdf`, attach-downloaded flow, prefs/state persistence | 220 |
| `content/ui.js`, `progress.xhtml` | ProgressWindow, progress/report window, confirm prompts | 260 |
| `content/preferences.xhtml`, `preferences-pane.js` | prefs pane, Windows SSO toggle, reset button | 150 |
| `locale/en-US/zotero-fulltext.ftl` | all strings | 120 |

The effect runner is the whole of the "orchestration" left in JS:

```js
drain() {
  this.scheduled = false;
  const out = this.engine.step({ now: Cu.now(), wall: Date.now(),
    commands: this.commands.splice(0), completions: this.completions.splice(0) });
  for (const op of out.ignored) Bodies.releaseForOp(op);
  for (const e of out.effects) this.run(e);
  this.armTimer(out.next_timer);
  if (out.more) setTimeout(() => this.kick(), 0);
}
run(e) {
  const handler = RUNNERS[e.type];
  if (e.op == null) { try { handler?.(e); } catch (x) { Zotero.logError(x); } return; }
  if (!handler) return this.complete(e.op, { err: { kind: 'unsupported' } });
  const ctl = Inflight.add(e);                                   // abort hooks, journal, tripwires
  Promise.resolve().then(() => handler(e, ctl))
    .then(ok => this.complete(e.op, { ok }), err => this.complete(e.op, { err: toErr(err) }))
    .finally(() => Inflight.remove(e.op));
}
```

Stall watchdog: if `stats.tasks > 0`, `ops_inflight == 0` and `next_timer == null`, the engine has a lost wakeup.
JS sends `dump_state`, logs it, cancels the run and reports `Failed{stall}`. Simulation tests assert this never happens.

---

## 6. Testing strategy

1. **Native `cargo test` on fixtures (`fulltext-core`):**
   - `fixtures/html/expected.json`: `classify` kind (403 + headers from the README for bot-wall fixtures), top
     candidate ∈ {expected, alternates}, no `must_not_select` [06 §6].
   - `fixtures/oa/manifest.json`: every parser fed the recorded status + body (HTML/XML/text error bodies, 200 "not
     found" shapes, OpenAlex merges, PMC S3 `s3://` rewrite, AAM without PDF) [05 §8].
   - Proxy vectors from [04 §6.4]; DOI/Extra/arXiv/rxiv normalisation; ranking cases (medRxiv → PLOS ONE merge).
   - `cargo fuzz` (x86_64) on `inspect`, `extract_pdf_candidates` and the parsers; clippy on both targets.
2. **Deterministic simulation tests of the engine (native, the main payoff of sans-IO):** `SimHost` plays JS with a
   virtual clock and scripted responses built from the fixtures. It records every effect and asserts invariants after
   every step:
   - concurrency per host ≤ 1 and global institutional ≤ 2; PDF spacing ≥ 5 s per host; ledger never exceeds
     30/h, 80/24 h, 300 MB/h, including under engine "restart" with a persisted ledger;
   - every op is completed or cancelled exactly once; every `BodyHandle` is released or saved (leak check);
     no stall (live tasks with nothing pending);
   - `cancel_run` mid-flight emits `cancel` for every in-flight op in the same step;
   - hedging starts the second attempt at 3 s, the loser is cancelled, same-host candidates are never hedged;
   - LoginGate: 20 items needing login → exactly one `login_window`; declined → no further windows in that run; T4-style
     silent retry after `Landed`;
   - property tests: random completion orders and latencies (seeded) for 20-item batches give identical outcomes and
     never panic. Golden effect traces live as JSON files in `tests/golden/`.
3. **Node tests (`tests/js`, no Zotero):** build the real `.wasm`; check zero imports and the export list; alloc/free
   pairing; `fx_stage` ownership; envelopes; poison + reset via `fx_debug_panic`; engine-trap recovery through the
   real `core-glue.js` + `host.js` with Zotero APIs stubbed (fake `Zotero.HTTP.request` serving fixtures); memory
   recycle thresholds; a 20-item OA batch end to end against fakes.
4. **Throwaway-Zotero e2e (`tools/zotero-harness/run.ps1`, headless, packed XPI) [03 §2]. No login is needed for any of:**
   - load/unload: menus registered, FTL resolves, prefs pane registers, idempotent disable → enable, no leaked
     windows, HiddenBrowsers or MIME handlers after `shutdown`;
   - local fakes with `httpd.sys.mjs` on 127.0.0.1: OA APIs (fixture bodies), publisher landing pages and PDFs,
     a Cloudflare-style 403 challenge, a fake OpenAthens redirector that 302s to a fake IdP page, a fake EZproxy
     `login?qurl=` page. Endpoint base URLs are redirected through `extensions.zotero-fulltext.test.endpointMap`,
     honoured only when the harness pref `extensions.zotero-harness.resultFile` is set. Our pipeline uses
     `Zotero.HTTP.request`, so plain http works (no forced https, unlike `addFileFromURLs` [03 §10]);
   - save path: attachment `linkMode`, `contentType`, title, un-proxied `url`, file starts `%PDF-` at offset 0;
   - cancellation mid-run, item deletion mid-run, plugin disable mid-run, engine trap injection (test build);
   - an IdP-landing hidden load gives `NeedsLogin` plus a single login window (auto-closed by the test as
     `closed_by_user`);
   - opt-in live OA smoke run (3 fixture DOIs, OA sources only, polite rates, no institutional route).
   - Unauthenticated probes for N1/N2 [S §h]: how often `Zotero.HTTP.request` meets Cloudflare on the major hosts, and
     HiddenBrowser capture on unregistered hosts.
5. **Needs the user:**
   - **B1** `tools/sso-spike/run-manual.ps1` (20-30 min) before `institution.enabled` ships as default true. If B1
     fails, flip the default to false and keep hand-off only [08 §5].
   - A first live institutional run on three Alma-confirmed entitled DOIs (Wiley, Elsevier, BMJ via EZproxy), including
     MFA and session reuse across items; and the Windows SSO toggle if Entra shows a Conditional Access error.
   - Licence confirmation from KCL Library (limits and acceptability of the redirector), optional [04 §9].

---

## 7. Build, packaging, versioning, dev install

- **`cargo xtask dist`** (Rust only, no Node) [07 §6]:
  1. `cargo rustc -p fulltext-wasm --release --target wasm32-unknown-unknown` with
     `--remap-path-prefix` for `USERPROFILE`, `CARGO_HOME` and `RUSTUP_HOME`; locate the output via `cargo metadata`
     (`target_directory`), because the user's global config sets `CARGO_TARGET_DIR` and the `kache` wrapper [S §f].
  2. Verify: import section empty; export set == ABI manifest; no `C:\Users` or `\.cargo\` strings; size ≤ budget.
  3. Render `manifest.json` from `plugin/manifest.template.json` with `[workspace.package] version`.
  4. Stage `plugin/**` + `fulltext.wasm`; deterministic zip (sorted entries, fixed timestamps, deflate 9) →
     `dist/zotero-fulltext-<ver>.xpi` (byte-identical on rebuild [07 §6]).
  5. Emit `dist/updates.json` entry: `addons["zotero-fulltext@aborakati"].updates[] = {version, update_link
     (GitHub release asset), update_hash: "sha256:…", applications.zotero.{strict_min_version:"10.0",
     strict_max_version:"10.*"}}`.
- **`cargo xtask check`**: fmt, clippy (native + wasm32), `cargo test`, sim tests, build wasm, Node tests.
- **`cargo xtask harness [--gui]`**: build, then `run.ps1` with the unpacked `plugin/` dir.
- **Versioning:** SemVer for the plugin (workspace version = manifest version). `ABI_VERSION` (u32, `fx_abi_version`)
  and protocol version (in `configure`) are separate and bumped on incompatible change; the glue refuses mismatches.
  The XPI ships glue and wasm together, so a mismatch indicates a packaging bug.
- **Dev install via proxy file (never the user's real profile):** `cargo xtask dev` builds the wasm into `plugin/content/`
  (gitignored) and renders `plugin/manifest.json`. Create a dedicated dev profile and data dir, write the file
  `<devprofile>/extensions/zotero-fulltext@aborakati` whose content is the absolute path
  `C:\Users\abora\zotero-fulltext\plugin`, set `extensions.autoDisableScopes=0`, then run
  `zotero.exe -profile <devprofile> -no-remote -datadir <devdata> -ZoteroDebugText -jsconsole` [01 §8.1, 03 §2].
  Reload by disable/enable (subscripts use `ignoreCache:true`; the wasm is fetched with `?v=<build-id>`).

---

## 8. Risks, mitigations, and what v1 leaves out

| Risk | Mitigation |
|---|---|
| **B1 unproven:** embedded Entra/OpenAthens SSO may fail (Conditional Access, MFA UI, SAML POST-back) [S §h] | Every institutional state has a hand-off outcome; Windows SSO opt-in; ship institutional enabled only after the user's B1 run; campus/VPN direct route still works. |
| Custom executor bugs (lost wakeups, leaks, stale keys) | ~300 lines, no `unsafe`; generation-checked keys; `LOST_WAKE` sweep; stall watchdog in JS; deterministic sim + property tests with leak and invariant checks; `dump_state` command. |
| Engine trap loses all in-flight run state | Parser isolation in the pure instance; reserve-before-fetch ledger; JS journal plus restart-once; second trap stops the run with a clear report. |
| Async state inside wasm is opaque to debugging | `trace` effects (debug pref) with owner, op and state transitions; `dump_state`; panic text via `fx_last_panic`. |
| Main-thread parsing of large pages (html5gum, up to ~45 ms for 2 MiB) | 2 MiB cap; pure instance is stateless, so it can move to a ChromeWorker later without engine changes [03 §11]. |
| Zotero translators make their own requests outside the Rust limiter | On institutional hosts run translators only when Rust found no candidate, and count them against the per-item budget. |
| Zotero internals (HiddenBrowser, PageData, MIMETypeHandler, `getFileFromDocument`, `createURLAttachmentFromTemporaryStorageDirectory`) change | Feature-detect in JS; `unsupported` completions make Rust skip that tier [S §g]. |
| MIME handler is global: while registered, unhandled PDFs in other Zotero browsers go to the download dialog | Register by reference count only while captures are in flight; one dispatcher keyed by channel. |
| Cloudflare decisions vary; UA consistency | Plain UA for login windows and Cloudflare-fronted institutional hosts, sticky per host; never `registerPlainUAHost` [08 §6.2]. |
| Licence limits are research-derived, not KCL-confirmed | Conservative ceilings in Rust that prefs can only lower; persisted ledger; stop signals halt the route; ask KCL Library. |
| Wrong document attached (OpenAlex merges, supplements, other-DOI links) | Record-DOI check, reject other-DOI/PII URLs and supplements [S §e]; un-proxied URL stored for audit. |
| Endpoint drift (Europe PMC `getPdf`, medRxiv early path, PMC proof of work) | Each source toggleable; assert structure in fixture tests [S §g]. |
| `futures-util` adds size or an import | xtask import/size gate; fall back to hand-written `join`/`select` (about 150 lines) if needed. |
| JSON protocol drift between JS and Rust | `proto.rs` is the single schema; Node tests round-trip every effect type; versioned `configure`. |
| AGPL contamination | Re-implement settle/capture/translator glue from behaviour; never copy Zotero code [S §g]. |

**Deliberately left out of v1:** ChromeWorker placement of the pure instance; cross-item request coalescing (Europe
PMC OR-batches, NCBI idconv batches); OpenAlex paid content API; Semantic Scholar and CORE beyond a key field (off
by default); LibKey API (needs a KCL token); automatic triggers of any kind (the licence requires user-initiated use);
visible challenge windows on OA routes; PDF-title identity verification; EPUB; institutions other than KCL beyond the
JSON config (no setup wizard); Zotero 7-9 support; locales other than en-US; a Tools-menu "Sign in" entry.
