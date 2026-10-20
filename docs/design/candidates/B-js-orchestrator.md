# zotero-fulltext v1 design: Pragmatic Split (JS orchestrates, Rust decides)

**Concurrency:** Single Zotero event loop. JS owns all asynchrony, Rust owns the admission policy.
- Cancellation: the bootstrap sandbox has no AbortController ([03 §7]), so a structured CancelScope tree (plugin → run → item → phase → attempt → request) cancels everything. It bridges to Zotero.HTTP.request cancellerReceiver (late-receiver safe), HiddenBrowser.destroy() and window.close(). Deadlines are scope timers, so no timer outlives its scope.
- Item pool: bounded at 4 (pref 1-6). Items waiting for a login or user action release their slot and re-acquire it with priority, so OA work for other items continues. A second invocation joins the active run, and duplicate items are dropped.
- Per item:
  - OA lookups (OpenAlex, Europe PMC, bioRxiv, optional Unpaywall; Zotero OA as fallback) run in parallel. Promise.allSettled feeds a Rust-ranked CandidateQueue, and an early start fires as soon as a trusted direct PDF appears.
  - Candidate attempts are sequential and the first verified PDF wins. A bounded hedge starts the next candidate on a different host after 8 s, on the OA route only, at most 2 in flight; the loser is cancelled as "superseded".
  - The institutional route is never hedged.
- Every network request needs a lease from a sans-IO Rust Scheduler, which lives in its own wasm instance (S) so a parser panic cannot wipe licence state. It is driven by a single JS pump (submit / poll(now) / complete / cancel, one armed timer for next_wake_ms).
  - Rust enforces global inflight 8, per-host token buckets and concurrency, and the 5-failure host skip.
  - Compiled licence ceilings: institutional 2 global / 1 per host, PDFs ≥5 s + seeded jitter, pages ≥2 s, ≤30 PDFs/h, ≤80 per rolling 24 h, ≤300 MB/h, ≤20 items per run, ≤4 publisher fetches per item.
  - Halt-on-429, suspension and challenge loops is delivered as typed effects. JS persists the ledger and cancels institutional work run-wide.
  - Time and randomness are inputs, so these limits are proven by deterministic native simulation tests.
- JS-runtime resources stay in JS: 2 hidden browsers, 1 login window per realm (all waiting items join it), 1 translator run.
- Timeouts:
  - XHR timeouts: API 10 s, page 30 s, PDF 120 s.
  - Browser session 45 s; login 300 s.
  - Phase deadlines exclude parked time.
- Shutdown and disable: cancel the root scope, settle for ≤2 s, then force-close windows, destroy browsers, unregister the single PDF MIME dispatcher, delete tracked temp dirs, persist the ledger, and guard late continuations with a generation counter. APP_SHUTDOWN cancels synchronously without awaiting.
- No unhandled rejections: every promise is awaited, passed to allSettled, or spawned via a TaskSet with a mandatory catch. Saving is a short non-cancellable critical section.

**Rust share:** About 60% of shipped source lines are Rust (roughly 5k Rust vs 3.3k JS, excluding tests and fixtures), and about 85-90% of branching decision logic. That covers:
- identifier normalisation;
- OA request construction and status-first parsing;
- candidate ranking, merging and dedupe;
- URL and page classification, including login, challenge and licence-stop detection;
- HTML → PDF-candidate extraction with publisher rules;
- proxify / unproxify / redirector algebra;
- the institutional route plan and Alma parsing;
- PDF inspection;
- attachment-URL cleaning and redaction;
- the complete rate-limit / licence scheduler with its ledger.

None of the I/O, timers, cancellation, browser and window control, Zotero DB writes or UI is in Rust. The per-item state machine and hedging/parking sequencing are also JS (they switch only on Rust-returned enums).

The reason is that all effects must call Gecko/Zotero JS APIs (Zotero.HTTP.request, HiddenBrowser, PageData, MIMETypeHandler, saveTx, windows), which a zero-import wasm module cannot reach. wasm-bindgen futures/imports were measured as fragile in the plugin sandbox (SUMMARY C3/C9). Making Rust drive them would turn every effect into a message protocol without moving any real logic into Rust. The table-driven FSM keeps a migration path open: it could later move to Rust as an ItemMachine without changing ports.

**Self-identified weaknesses:**
- The per-item state machine, hedging and parking logic live in JS and are verified only by Node tests with fake ports, not by cargo; fidelity of the fakes to real Zotero behaviour (XHR redirects, HiddenBrowser settle timing, PageData) is a gap that only the harness E2E tests partially close.
- The policy/sequencing split can drift: the 'JS never inspects URLs or bodies' rule is enforced by convention plus an ast-grep pattern, which can be evaded by indirection.
- Navigation hops inside HiddenBrowser and the login window (redirector, IdP, SAML POSTs, Cloudflare challenge XHRs) are not individually scheduled; licence accounting treats a browser session as one fetch, an approximation.
- Zotero translators may issue their own network requests outside the scheduler; mitigated (fallback-only on the institutional route, lease held) but not eliminated.
- The two-instance design (stateless decider plus stateful scheduler) adds glue complexity; after a scheduler trap the rebuilt scheduler does not know about in-flight requests, so global concurrency can briefly overshoot (institutional route is halted, so the licence side fails closed).
- The whole institutional login design rests on the unproven B1 assumption; IdP error-page classification (Entra Conditional Access codes such as AADSTS53000) is low-confidence and untested.
- Whole PDFs are held in memory via XHR arraybuffer (capped at 200 MB); streaming via Zotero.HTTP.download is deferred to v1.1.
- Page analysis runs on the main thread; with the 2 MiB cap the worst case is an estimated ~110 ms jank (extrapolated, not measured in Gecko) until the ChromeWorker offload exists.
- The bounded hedge adds complexity for a modest latency gain and could be dropped if E2E data shows little benefit.
- The custom progress window means more UI code to maintain than reusing Zotero.ProgressQueue.
- The persisted licence ledger lives in a user-editable pref, so it is a self-limiter, not tamper-proof.
- v1 has no PDF-content verification (title/DOI match), so a wrong-but-valid PDF from a merged OpenAlex record or a mislabelled link can still be attached.
- Several proposed harness features (-KeepProfile, extra test-build endpoint overrides, launch-dev.ps1) do not exist yet and need building in a later phase.

---

# zotero-fulltext v1: "Pragmatic Split" design

**Status:** proposal, 2026-10-20. **Target:** Zotero 10.0.6 / Gecko 140 ESR, Windows 11 first.
**Evidence base:** `research/SUMMARY.md` and briefs `01`–`08`. Citations look like `[03 §7]` or `[SUMMARY C3]`. Zotero source is cited as `file:line` under `.ref/omni-app/chrome/content/zotero/`.
**Angle:** JavaScript owns every *effect*: network, browsers, windows, timers, cancellation, the Zotero DB and the UI. Rust owns every *decision* through pure synchronous functions plus one stateful scheduler built in the sans-IO style (it has no I/O of its own).

---

## 0. Decision record

| # | Decision | Evidence |
|---|---|---|
| D1 | JS runs all I/O and control flow. Rust (wasm32, **zero imports**) decides: identifiers, OA request specs and response parsing, candidate ranking, page and URL classification, PDF-link extraction, proxy URL algebra, the institutional route plan, PDF inspection, attachment-URL cleaning, and the rate/licence scheduler. | Zotero APIs are JS-only [03 §8]. wasm-bindgen was rejected [SUMMARY C3]. |
| D2 | **One compiled `WebAssembly.Module`, two instances.** **D** (decider) is stateless and reset freely after a trap. **S** (scheduler) holds the rate-limit and licence state. A panic in the HTML tokenizer can never wipe licence counters. | A panic traps and poisons the whole instance [07 §4]. Re-instantiation costs 0.2–0.9 ms [SUMMARY b]. |
| D3 | Hand-written ABI v2: a JSON dispatcher `fx_call`, plus raw `fx_page` (HTML bytes) and `fx_pdf_inspect`. Glue is a classic IIFE subscript loaded from `chrome://zotero-fulltext/content/`. Bytes come from `fetch(chrome://…).arrayBuffer()` and are compiled with `WebAssembly.compile`, and the `Module` is cached. | [03 §5], [07 §3.1], [SUMMARY C3] |
| D4 | **No `AbortController`.** It is `undefined` in the bootstrap sandbox [03 §7]. We ship our own `CancelScope` tree and bridge it to `cancellerReceiver` (`http.js:426-435`), `HiddenBrowser.destroy()` and `window.close()`. | [03 §7, §12] |
| D5 | We run our own orchestrator, not `addFileFromURLs`. Calls are `Zotero.HTTP.request(..., {responseType:'arraybuffer', successCodes:false, errorDelayMax:0, noRetryOnThrottle:true, timeout})`. Zotero translators run on the pages we fetched. We save through `createTemporaryStorageDirectory()` → `IOUtils.write` → `createURLAttachmentFromTemporaryStorageDirectory({directory: <path string>, libraryID, parentItemID, filename, title, url, contentType})`, verified at `attachments.js:682-736, 2853-2860`, and pass the **un-proxied** URL. | [SUMMARY C4], [02 §7] |
| D6 | The institutional route is the OpenAthens redirector, run in a HiddenBrowser with the plain Firefox UA. If it lands on the IdP, a custom login window opens. Once the session exists it is reused in hidden browsers. EZproxy is used only for configured hosts. If all of that fails, the fallback chain is "Open in browser", then "Attach a PDF I downloaded". | [04 §4, §7], [08 §0, §6] |
| D7 | The licence limits are **compiled into Rust as ceilings**. Prefs can tighten them but never loosen them. | [04 §8.2] |
| D8 | UA policy: every institutional request and every browser context uses `Zotero.VersionHeader.getPlainFirefoxUA()`, which stays fixed per host for the session. OA API calls keep Zotero's UA. We never call `registerPlainUAHost`. | [08 §0.3, §2.3, §6.2] |

---

## 1. Architecture and the JS↔Rust boundary

### 1.1 Diagram

```
 Zotero 10 main process (Gecko 140, single JS event loop)
 ┌───────────────────────────────────────────────────────────────────────────────────────────┐
 │ bootstrap.js  (Cu.Sandbox, system principal; no window/console/AbortController [03 §7])  │
 │   startup(): registerChrome ─► loadSubScript(chrome://…/lib/*.js) ─► CoreHost.load(wasm)  │
 │              ─► MenuManager.registerMenu ─► PreferencePanes.register ─► initWindows()     │
 │                                                                                           │
 │  UI (JS)                 ORCHESTRATION (JS)                        PORTS (JS → Zotero)     │
 │  progress.xhtml ◄─events─ RunManager ─► ItemPool(4) ─► ItemTask FSM ─► HttpPort ──► Zotero.HTTP.request
 │  login.xhtml    ◄──────── LoginCoordinator ◄──────────────┤         BrowserPort ─► HiddenBrowser,
 │  prefs pane               SchedulerPump ◄── lease/complete ┘                         PageData actor,
 │                              │  ▲                                                    MIMETypeHandler
 │                 submit/poll/ │  │ grants / denials / effects     TranslatorPort ─► getFileFromDocument
 │                 complete     ▼  │                                SavePort ──────► Attachments.*, IOUtils
 │  ┌──────────────────── WebAssembly: ONE compiled Module (fulltext_core.wasm) ────────────┐│
 │  │ Instance S "scheduler" (stateful)      │ Instance D "decider" (stateless, resettable)  ││
 │  │  sched.*  Scheduler{queue,hosts,       │  ids.* oa.* cand.* url.* inst.* proxy.*       ││
 │  │           ledger,rng}                  │  fx_page (html5gum) fx_pdf_inspect            ││
 │  └──────────────────────────────────────────────────────────────────────────────────────┘│
 └───────────────────────────────────────────────────────────────────────────────────────────┘
       │ default cookie jar (userContextId 0) shared by HTTP.request, HiddenBrowser, login window [08 §3.5]
       ▼
  OA APIs (OpenAlex, Europe PMC, bioRxiv, PMC S3, arXiv, [Unpaywall])   go.openathens.net → Entra → publisher
  publisher hosts (Cloudflare/Akamai-fronted [06 §5.1])                 login.kcl.idm.oclc.org (≈19 hosts) [04 §2.4]
```

**Rule of the boundary:** JS **never inspects response content or URL strings to make a decision.** It switches only on enums that Rust returns: `class.kind`, `plan.steps[i].kind`, `grant | wait | deny`, `effects[i].t`. CI enforces this with an `ast-grep` rule. The rule fails if `content/lib/{item-task,oa,inst,fetch}.js` contains `.includes(`, `.match(`, `RegExp`, `startsWith(` or `endsWith(` applied to a URL, body or header variable (allow-list: `ports/` and `ui/`).

### 1.2 Why this split beats a Rust-driven async engine, and where it loses

**The alternative.** Rust owns an `Engine` (the per-item FSM plus scheduling). It emits `Effect::{Http, BrowserLoad, OpenLogin, Save, Sleep}` and consumes `Event`s, with JS as a dumb executor. A variant uses wasm-bindgen-futures to `await` JS promises.

**Where the pragmatic split wins:**
1. **The effects are inherently JS.** That means `Zotero.HTTP.request`, `HiddenBrowser`, the `PageData` actor, `MIMETypeHandler`, `saveTx`, `Services.ww`, prompts and windows. The futures variant needs wasm imports, ESM loading and cross-compartment init, and the research measured all three as fragile in the sandbox [03 §6, SUMMARY C3/C9]. The sans-IO variant would turn every browser, login and UI interaction into a message protocol. That doubles the boundary surface without moving any real logic into Rust, because the logic *is already* in Rust as decision functions.
2. **It is debuggable inside Zotero.** JS stack traces reach `Zotero.debug`, the Browser Toolbox can set breakpoints on JS, and every Rust call is one logged `op` with JSON in and out. A stripped wasm engine gives opaque `unreachable` traps. One panic in an engine-held FSM would poison every in-flight item, whereas here the decider is stateless and a trap costs one call (reset < 1 ms).
3. **Fewer moving parts cross the boundary.** Every call is synchronous request→response. There are no callbacks from Rust into JS, no re-entrancy, no wakers, no JS objects held by Rust, and no promise registries on both sides.
4. **Cancellation lives next to the resources it cancels:** XHR cancellers, browser destroy, window close.
5. **Determinism is still in Rust where it matters.** The scheduler is a pure state machine that is given time and a random seed, so licence limits are proven by native simulation tests (§6.1).

**Where it loses (honestly):**
1. **The user asked for "Rust wherever possible".** Rust ends up with about 60% of shipped lines and about 85–90% of branching decisions, but the *sequencing* (the item FSM, hedging, parking) is JS. That part is tested by Node fakes, not by `cargo test`.
2. **The logic is split.** Policy (Rust) and sequencing (JS) can drift. Escalation rules could also creep into JS. Mitigations are the boundary rule plus the ast-grep CI rule above, and an FSM that is table-driven and switches on Rust enums. That keeps a migration path open: `ItemMachine` can later move to Rust without changing any port.
3. **JS has no compiler-checked structured concurrency.** Correctness comes from discipline (CancelScope trees, a single pump, a `TaskSet` with mandatory `.catch`) plus tests that trap unhandled rejections and leaked timers.
4. **Two toolchains for contributors**, cargo plus Node for tests. There is still no Node *build* step [SUMMARY f].

### 1.3 ABI v2 (hand-written, zero imports)

| Export | Signature (wasm32) | Notes |
|---|---|---|
| `memory` | — | The glue re-creates every view after **every** call, because `memory.grow` detaches buffers [07 §3.1]. |
| `fx_abi_version` | `() -> u32` | Returns `2`. The glue refuses any other value. |
| `fx_init` | `() -> ()` | Installs the panic hook that writes into a static buffer. Idempotent. |
| `fx_alloc` | `(len: u32) -> u32` | `Vec::with_capacity(len)` forgotten. `len == 0` returns a dangling non-null pointer. |
| `fx_free` | `(ptr: u32, len: u32) -> ()` | Frees exactly one allocation, with the **same** `len`. |
| `fx_last_panic` | `() -> u64` | Packed pointer and length into a static buffer. **Never freed.** Readable after a trap. |
| `fx_call` | `(ptr: u32, len: u32) -> u64` | UTF-8 JSON `{"v":1,"op":…,"args":…}` in. Packed `{"ok":…}` or `{"err":{code,message}}` out. |
| `fx_page` | `(ctx_ptr, ctx_len, html_ptr, html_len) -> u64` | `ctx` is JSON. `html` is **raw response bytes**, decoded in Rust with `from_utf8_lossy`. The URLs and metas we extract are ASCII, so a charset mismatch cannot corrupt them. Output is the same envelope as `fx_call`. |
| `fx_pdf_inspect` | `(head_ptr, head_len, tail_ptr, tail_len) -> u32` | Bit flags: `1` = `%PDF-` found, `2` = at offset 0, `4` = `%%EOF` in the tail, `8` = `/Encrypt` seen, `16` = looks like HTML. Bits 16–25 hold the header offset (≤ 1023). |

**Memory ownership** (normative; the glue enforces it and Node tests check it):
1. JS allocates every input with `fx_alloc`, writes it, calls the export, then frees it with `fx_free(ptr, len)`. Rust only borrows inputs.
2. Every packed `u64` result is `(ptr << 32) | len` of a leaked `Box<[u8]>`. JS copies it out with `new Uint8Array(mem, ptr, len).slice()`, then calls `fx_free(ptr, len)` **exactly once**.
3. The only borrowed output is `fx_last_panic`, which points into a static buffer.
4. Input caps, enforced in the glue: JSON ≤ 4 MiB, HTML ≤ 2 MiB (truncated, and flagged `truncated:true` in ctx), PDF head and tail 1 KiB each.
5. After a `WebAssembly.RuntimeError` the instance is poisoned. The glue reads `fx_last_panic`, discards the instance, and re-instantiates from the cached `Module`.
   - **D** then replays its configuration ops (`cfg.set`), so D holds only *replayable* configuration.
   - **S** is never silently reset (§2.8).
6. Hygiene reset: D is also reset when `memory.buffer.byteLength > 32 MiB` after a call, because wasm memory never shrinks [07 §3.1].
7. The release build must have **0 imports** and no `C:\Users` strings in data sections. `cargo xtask check-wasm` asserts both [07 §6].

### 1.4 Op catalogue

| Op (instance) | Args → result (summary) |
|---|---|
| `meta` (D,S) | → `{abi, build, version, ops[]}`. The build stamp is compared with `manifest.json` to detect a stale wasm in dev. |
| `cfg.set` (D) | `{institution, endpoints?, rules_override?}` → `{}`. Replayed after a reset. |
| `ids.normalise` (D) | Item fields → `Ids{doi, pmid, pmcid, arxiv, preprint, url, sources}` |
| `oa.requests` (D) | `{ids, enabled[], unpaywall_email?}` → `RequestSpec[]`, the exact URLs. JS adds auth headers from prefs, so secrets never enter Rust. |
| `oa.parse` (D) | `{src, status, content_type, body, ids}` → `{candidates[], learned{pmcid…}, followups: RequestSpec[]}`. Parses **by status first** [05 §8]. |
| `cand.rank` (D) | `{cands[], attempted[], prefs, ids}` → `{order: RankedCandidate[], early_start}` (merge, dedupe including the query string [05 §5]) |
| `cand.from_translator` (D) | `{translator_hit, page, ids}` → a candidate after the reject rules (supplement, other DOI, …) |
| `fx_page` (D, raw) | ctx `{status, final_url, content_type, headers, expected_ids, route}` + bytes → `PageAnalysis` |
| `url.classify` (D) | `{url}` → `UrlClass` (`idp_login`, `openathens_wait`, `ezproxy_login`, `publisher_sso_hop`, `publisher`, `pdf_host`, …). This is cheap, so the browser poll loop can call it every 500 ms. |
| `inst.alma_parse` (D) | Alma CTO XML (tokenised with html5gum, no extra XML crate) → `Entitlement{known, services[]}` |
| `inst.plan` (D) | `{ids, item_url, entitlement, knowledge, mode}` → `InstPlan{steps[], handoff[]}` |
| `proxy.*` (D) | `login_url`, `proxify_host`, `unproxify`, `redirector_url`, `unwrap_redirector`, `learn_from_redirect` [04 §6.3-6.4] |
| `attach.clean_url` (D) | `{final_url, landing_url, ids}` → the URL to store. It is never a `login?`, redirector, SAML or signed `X-Amz-*`/`token=` URL; those are replaced by the landing page or `https://doi.org/<doi>`. |
| `redact.url` (D) | → host + path, keeping only allow-listed query keys (for logs and reports) |
| `sched.new / submit / poll / complete / cancel / cancel_item / halt / snapshot / drop` (S) | §2.8 |

### 1.5 JSON examples

```json
// ids.normalise
{"v":1,"op":"ids.normalise","args":{"item_type":"journalArticle","doi":"https://doi.org/10.1371/JOURNAL.PMED.1000097",
 "extra":"PMID: 19621072","pmid":"","pmcid":"","archive_id":"","url":"https://journals.plos.org/plosmedicine/article?id=10.1371/journal.pmed.1000097"}}
{"ok":{"doi":"10.1371/journal.pmed.1000097","pmid":"19621072","pmcid":null,"arxiv":null,"preprint":null,
 "url":"https://journals.plos.org/plosmedicine/article?id=10.1371/journal.pmed.1000097","sources":{"doi":"field","pmid":"extra"}}}

// oa.requests
{"ok":[{"src":"openalex","method":"GET","url":"https://api.openalex.org/works/doi:10.1371/journal.pmed.1000097",
        "host":"api.openalex.org","accept":"application/json","timeout_ms":10000,"auth":"openalex_key?"},
       {"src":"europepmc","method":"GET","host":"www.ebi.ac.uk","timeout_ms":10000,"auth":null,
        "url":"https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=DOI%3A%2210.1371%2Fjournal.pmed.1000097%22&resultType=core&format=json"},
       {"src":"zotero_oa","method":"zotero_api","when":"fallback_if_failed:openalex"}]}

// oa.parse (OpenAlex 404 with an HTML body is not an error: it means "no record")
{"v":1,"op":"oa.parse","args":{"src":"openalex","status":404,"content_type":"text/html","body":"<html>…","ids":{"doi":"10.9999/zft.nonexistent.000001"}}}
{"ok":{"candidates":[],"learned":{},"followups":[],"note":"not_found"}}

// oa.parse (Europe PMC hit → PMCID learned → PMC S3 follow-up)
{"ok":{"candidates":[{"key":"europepmc.org/api/getpdf?pmcid=pmc2707599","src":"europepmc","pdf_url":"https://europepmc.org/api/getPdf?pmcid=PMC2707599",
   "landing_url":"https://europepmc.org/article/PMC/PMC2707599","version":"published","license":"cc-by","host_kind":"pmc","record_doi":"10.1371/journal.pmed.1000097"}],
  "learned":{"pmcid":"PMC2707599"},
  "followups":[{"src":"pmc_s3","method":"GET","host":"pmc-oa-opendata.s3.amazonaws.com","timeout_ms":10000,
                "url":"https://pmc-oa-opendata.s3.amazonaws.com/metadata/PMC2707599.1.json"}]}}

// cand.rank
{"ok":{"early_start":true,"order":[
  {"key":"pmc-oa-opendata.s3.amazonaws.com/pmc2707599.1/pmc2707599.1.pdf","score":960,"why":["pmc_s3","published","cc-by","doi_match"],
   "fetch":{"url":"https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf","kind":"pdf","host":"pmc-oa-opendata.s3.amazonaws.com","route":"oa"}},
  {"key":"europepmc.org/api/getpdf?pmcid=pmc2707599","score":930,"why":["epmc_getpdf","published"],
   "fetch":{"url":"https://europepmc.org/api/getPdf?pmcid=PMC2707599","kind":"pdf","host":"europepmc.org","route":"oa"}}]}}

// fx_page ctx + result (fixture springer__article-paywalled.html, institutional route)
{"status":200,"final_url":"https://link.springer.com/article/10.1007/s00464-025-11552-4","content_type":"text/html; charset=UTF-8",
 "headers":{"server":"envoy"},"expected_ids":{"doi":"10.1007/s00464-025-11552-4"},"route":"inst","truncated":false}
{"ok":{"class":{"kind":"landing"},"page_ids":{"doi":"10.1007/s00464-025-11552-4"},"doi_match":true,"access":"no_access_hint",
  "candidates":[{"url":"https://link.springer.com/content/pdf/10.1007/s00464-025-11552-4.pdf","signal":"citation_pdf_url","score":880,"needs_browser":false}],
  "rejected":[{"url":"https://media.springernature.com/original/springer-static/esm/art%3A10.1007%2Fs00464-025-11552-4/MediaObjects/464_2025_11552_MOESM1_ESM.doc","why":"supplement"}],
  "translators":"fallback_only"}}

// fx_page on a Cloudflare managed challenge (status 403, cf-mitigated: challenge)
{"ok":{"class":{"kind":"challenge","vendor":"cloudflare","interactive":false},"candidates":[],"rejected":[],"translators":"skip"}}

// url.classify while polling a hidden browser
{"v":1,"op":"url.classify","args":{"url":"https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/saml2?SAMLRequest=…"}}
{"ok":{"kind":"idp_login","realm":"kcl"}}

// inst.plan (Annals of Surgery; Alma says entitled; direct access already failed for this host in this run)
{"v":1,"op":"inst.plan","args":{"ids":{"doi":"10.1097/SLA.0000000000005603"},"entitlement":{"known":true,"services":[{"package":"Ovid Journals","host":"ovidsp.ovid.com","is_free":false}]},
 "knowledge":{"hosts":{"journals.lww.com":{"direct":"no_access"}}},"mode":"window"}}
{"ok":{"steps":[],"skipped":[{"kind":"direct","why":"host_direct_no_access_this_run"},{"kind":"redirector","why":"redirector_passthrough_host"}],
  "handoff":[{"l10n":"zotero-fulltext-handoff-redirector","url":"https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1097%2FSLA.0000000000005603"},
             {"l10n":"zotero-fulltext-handoff-libkey","url":"https://libkey.io/libraries/286/10.1097/SLA.0000000000005603"}]}}

// errors are values, never traps
{"err":{"code":"bad_args","message":"missing field `ids` at line 1 column 31"}}
```

---

## 2. Concurrency and async model

### 2.1 Execution facts we design around
- Everything runs on Zotero's main-process event loop. The sandbox has `setTimeout`/`clearTimeout` and `requestIdleCallback` (injected at `plugins.js:176-181`), and `Promise.any` and `Promise.allSettled` (language built-ins). It has **no** `AbortController`, `queueMicrotask`, `structuredClone` or `console` [03 §7].
- JS is single-threaded, so every scheduler call (`submit`, `poll`, `complete`, `cancel`) runs to completion atomically. Ticket-state races are impossible by construction, and no locks are needed.
- The only synchronous blocking work is the Rust calls. These take < 1 ms for JSON ops [03 §4]. A page analysis on a typical page takes 1–5 ms, and about 110 ms worst case for the 2 MiB cap (extrapolated from 320 ms for 6 MB [07 §2.2]). Page analyses run after `await idle(scope)` (requestIdleCallback with a 50 ms timeout). The D facade is **async-shaped** (`await core.decide(op, args)`), so it can move into a ChromeWorker later without touching call sites; the Rust side already parses HTML itself [SUMMARY b]. S stays synchronous on the main thread.

### 2.2 Primitives (all in `lib/async.js`, Node-tested)

```js
// CancelScope: a structured cancellation tree. Reasons: "user" | "shutdown" | "timeout" |
// "superseded" | "licence-halt" | "parent".
class CancelScope {
  constructor(parent = null, { label = "", deadlineMs = 0 } = {})  // child of parent; optional deadline
  get reason()                  // null while live
  cancel(reason)                // idempotent; cascades to children first, then runs callbacks LIFO;
                                //   callback errors are caught and logged, never rethrown
  onCancel(fn) -> dispose       // already cancelled → fn runs on the next tick
  child(opts) -> CancelScope
  dispose()                     // detach from parent, clear the deadline timer
  throwIfCancelled()            // throws CancelledError(reason)
  sleep(ms) -> Promise          // timer cleared on cancel; rejects with CancelledError
  race(p) -> Promise            // settles with p or rejects on cancel (does NOT stop p's work)
}
// Bridge to Zotero.HTTP.request: cancel even when the receiver arrives late.
function httpOpts(scope, o) {
  let cancel = null; const dispose = scope.onCancel(() => cancel?.());
  return [{ ...o, cancellerReceiver: f => { if (scope.reason) f(); else cancel = f; } }, dispose];
}
class Semaphore { constructor(n); acquire(scope, {priority=0}) -> Promise<Permit>; }  // priority FIFO
class TaskSet   { spawn(scope, label, fn) -> Promise; settleAll(ms) }   // every task gets .catch(logUnlessCancelled)
async function firstSuccess(scope, thunks, { hedgeAfterMs })            // Promise.any semantics + loser cancellation
```

Rules (checked in code review and by tests):
- Every promise is awaited, returned, passed to `allSettled`, or spawned through a `TaskSet`. No bare `.then` without a `.catch`.
- Every `setTimeout` goes through `scope.sleep` or a scope deadline, so no timer outlives its scope.
- `CancelledError` is the only error that crosses scope boundaries silently. Everything else becomes an item outcome `Failed{error_id}`, and the details go to `Zotero.debug` with redacted URLs.

### 2.3 Scope hierarchy

```
pluginScope (startup → shutdown)
 └─ runScope (one per "Get Full Text" invocation; cancelled by the Cancel button)
     └─ itemScope (one per item)
         ├─ oaScope        deadline 120 s    ├─ lookupScope × N (10 s XHR timeout each)
         │                                   └─ attemptScope × ≤2 (hedge)
         └─ instScope      deadline 240 s, paused while parked (parking happens in itemScope, outside instScope)
             └─ browserScope deadline 45 s (hidden) / loginScope deadline 300 s (visible window)
```

### 2.4 Item-level concurrency (JS `ItemPool`)
- A semaphore of `net.maxConcurrentItems` slots (default **4**, range 1–6 [05 §6]). Items are admitted in selection order.
- **Parking.** When an item must wait for the user (`AwaitingLogin`, `AwaitingUser`) it **releases its slot** and awaits the `LoginCoordinator`. When it resumes it re-acquires a slot with `priority=1`, ahead of new admissions. Users waiting on a login window therefore never stall OA work for other items.
- The same item cannot run twice. A plugin-wide `Map<itemID, ItemTask>` makes a second invocation **join** the active run: its items are appended to the queue, and duplicates are dropped. This mirrors Zotero's `_findFileQueue` [02 §2.5]. The batch caps apply to the run total.
- Other resource semaphores, owned by JS because they are JS-runtime resources:
  - `browserSlots` = 2 hidden browsers;
  - `translatorSlots` = 1, because translators are heavy and run on the main thread;
  - one login window per realm (`LoginCoordinator`).

### 2.5 Per-item parallel OA lookups
1. Rust `oa.requests(ids)` returns the lookups: OpenAlex, Europe PMC, bioRxiv (only for `10.1101/`), Unpaywall (only if the user set an email), and Zotero OA (only as `fallback_if_failed:openalex`).
2. JS launches all of them concurrently, each in a `lookupScope` and each through the scheduler (§2.8). They are collected with `Promise.allSettled`, wrapped so that a lookup's completion feeds a **`CandidateQueue`**:
   - each `oa.parse` result is merged by `cand.rank`, re-ranked, and its `followups` (PMC S3 metadata, ListObjectsV2) are spawned as further lookups.
3. **Early start.** Phase 2 starts when every lookup has settled, **or** when `cand.rank` returns `early_start:true`. That happens when a trusted direct PDF appears (a PMC S3 PDF, arXiv, or a publisher `pdf_url` whose `record_doi` matches). Lookups still running keep feeding the queue.
4. API errors (429/5xx) get **one** backoff, and only if Rust's `complete` effect says `retry_after_ms ≤ 30000` [SUMMARY d]. Otherwise that source is dropped for this item.

### 2.6 Candidate attempts
- **Sequential, first verified success wins.** The attempt loop pulls `await queue.next(oaScope)`. The queue resolves with the best candidate not yet attempted, or with `done` once all lookups have settled and the queue is empty.
- **Bounded hedge (OA route only).** If attempt *k* is still pending after `hedgeAfterMs` (8 s), the next candidate is on a **different host**, and the scheduler would grant it now, then attempt *k+1* starts. The first attempt that **validates** (§3.7) wins. The loser's `attemptScope` is cancelled with `"superseded"`, which aborts its XHR or destroys its browser, and its ticket completes as `cancelled`. At most 2 attempts per item are in flight. **Hedging is never used on the institutional route** (licence) or for browser tiers.
- A landing page can push child candidates (extracted PDF links) into the **front** of the queue. Depth is limited to 1 on the OA route and 2 on the institutional route (landing → PDF link → reader-shell rewrite), and within the per-item budget.

### 2.7 Limits, and where each one lives

| Limit | Value | Lives in | Why there |
|---|---|---|---|
| Items in flight | 4 (pref, 1–6) | JS `ItemPool` | Work, memory and UI pacing; slots released while parked |
| Lookups per item | all enabled (≤ 6) in parallel | JS `allSettled` | Latency; each is still gated by S |
| Global requests in flight | 8 | **Rust S** | Policy |
| Per-host concurrency and rate | table below | **Rust S** (token bucket + concurrency + min spacing) | Policy; deterministic tests |
| Unknown publisher host (OA) | 1 concurrent, ≥ 1 s spacing, 5 consecutive failures → host skipped for the run | **Rust S** | Mirrors `attachments.js:1513-1514` [06 §1] |
| **Institutional** in flight | **2 global, 1 per host** (ceiling) | **Rust S** | [04 §8.2] |
| Institutional spacing | pages ≥ 2 s; PDFs ≥ 5 s + U[0, 3 s) jitter (seeded xorshift) | **Rust S** | [04 §8.2] |
| Institutional volume | ≤ 30 PDFs/h, ≤ 80 PDFs per rolling 24 h, ≤ 300 MB/h (ceilings) | **Rust S** ledger; JS persists it to the `inst.ledger` pref after every PDF | [04 §8.2]; survives restarts |
| Institutional batch | confirm > 10 items; ≤ 20 distinct items enter the inst route per run (ceiling) | Confirm in JS UI; **count in Rust S** (`deny: batch_cap`) | |
| Per-item institutional budget | ≤ 4 publisher-host fetches + 1 browser session + 1 login | **Rust S** (per-item ticket accounting) | No spidering |
| Halt conditions | 429, `LicenceStop` page, `EzproxyError`, ≥ 2 consecutive challenges on inst, `Blocked` | **Rust S** emits a `halt` effect; JS cancels inst work run-wide | Fail closed |
| Hidden browsers | 2 | JS semaphore | Gecko process and memory resource |
| Login windows | 1 per realm | JS `LoginCoordinator` | User-interaction resource |
| Translator runs | 1 | JS semaphore | Main-thread cost; translators may make their own requests |

Default host table (Rust `sched/hosts.rs`, overridable only downwards):

| Host | conc | rate | Host | conc | rate |
|---|---|---|---|---|---|
| api.openalex.org | 3 | 8/s | api.crossref.org | 1 | 5/s [05 §6] |
| www.ebi.ac.uk | 2 | 4/s | api.unpaywall.org | 2 | 5/s |
| europepmc.org (`/api/getPdf`) | 1 | 1/s | services.zotero.org (via Zotero API) | 1 | 2/s |
| pmc-oa-opendata.s3.amazonaws.com | 2 | 5/s | librarysearch.kcl.ac.uk (Alma) | 1 | 1/s |
| api.biorxiv.org, www.{bio,med}rxiv.org | 1 | 1/s | go.openathens.net | 1 | 1/s |
| arxiv.org (PDF) / export.arxiv.org | 1 / 1 | 1/s / 1 per 3 s | any other host | 1 | 1/s (OA); inst rules above |

### 2.8 The scheduler protocol (Rust S, sans-IO)

```
JS SchedulerPump                                   Rust Scheduler (instance S)
acquire(scope, req) ── sched.submit{req,now} ─────► enqueue Ticket{id, item_seq, kind, host, route}
  (await deferred)    ◄── {ticket} ──────────────
_poll() (coalesced:   ── sched.poll{now} ─────────► scan queue in (item_seq, submit_seq) order; skip tickets whose
 one per microtask)                                  host is busy (no head-of-line blocking); check global/inst caps,
                      ◄── {granted[{ticket,ua}],     buckets, spacing, ledger, halts → grant / deny / keep waiting
                           denied[{ticket,reason}],
                           next_wake_ms} ──────────  JS arms ONE setTimeout for next_wake_ms
lease.complete(out)   ── sched.complete{ticket,outcome,now} ─► account bytes/PDF; update host health & UA stickiness;
                      ◄── {effects[]} ────────────  ledger / halt / host_cooldown / retry_after effects
scope cancelled       ── sched.cancel{ticket,now} ─► drop a queued ticket, or release an in-flight one as cancelled
```

- **Time and randomness are inputs.** `now` (from `Date.now()`) is passed on every call. The jitter PRNG is seeded at `sched.new` from `crypto.getRandomValues`. Native tests replay the same sequence deterministically.
- **The UA decision comes with the grant.** A grant carries `ua: "zotero" | "plain"`. The rule is: `plain` for the inst route; `plain` for any host that was challenged or loaded in a plain-UA browser this session; otherwise `zotero`. The choice is sticky per host [08 §6.2].
- **Effects are typed:**
  - `{"t":"ledger","ledger":[[t_ms,bytes],…]}`: JS writes it to the pref immediately (crash-safe).
  - `{"t":"halt","route":"inst","reason":"http_429","until_ms":…}`: JS cancels every `instScope` with `"licence-halt"`, items end as `Blocked`/`Deferred`, and the UI shows a banner.
  - `{"t":"retry","ticket_kind":"api","after_ms":2000}`
  - `{"t":"host_skip","host":…,"why":"5_failures"}`
- **Fault policy for S (fail closed).** If S traps:
  1. Every pending ticket is rejected with `SchedulerFault`.
  2. A new S is created from the **persisted ledger**.
  3. The inst route is **halted for the rest of the run**.
  4. OA continues. During the switch, global concurrency may overshoot by at most the requests already in flight; their late `complete`s are ignored (unknown ticket → `err`, dropped).
- **Example:**

```json
{"v":1,"op":"sched.new","args":{"now":1760968800000,"seed":"9f2c41a07be3d511",
  "cfg":{"max_inflight":8,"inst":{"enabled":true,"hourly_cap":30,"daily_cap":80,"mb_per_hour":300,"batch_items":20}},
  "ledger":[[1760960000000,1843221]]}}
{"ok":{"handle":1,"effective":{"inst":{"hourly_cap":30,"daily_cap":80,"remaining_today":79,"remaining_hour":30}}}}
{"v":1,"op":"sched.submit","args":{"h":1,"now":1760968801000,"item":3,"host":"www.ebi.ac.uk","kind":"api","route":"oa"}}
{"ok":{"ticket":17}}
{"v":1,"op":"sched.poll","args":{"h":1,"now":1760968801001}}
{"ok":{"granted":[{"ticket":17,"ua":"zotero"}],"denied":[{"ticket":12,"reason":"daily_cap"}],"next_wake_ms":1760968806412}}
{"v":1,"op":"sched.complete","args":{"h":1,"now":1760968809000,"ticket":21,"outcome":{"t":"http","status":429,"retry_after_s":600}}}
{"ok":{"effects":[{"t":"halt","route":"inst","reason":"http_429","until_ms":1760970609000}]}}
```

### 2.9 Timeouts
| What | Value | Mechanism |
|---|---|---|
| OA API XHR | 10 s | `Zotero.HTTP.request` `timeout` |
| Landing page XHR | 30 s (Wiley abstract pages took 25–37 s [08 §3.6]) | same |
| PDF XHR | 120 s total | same |
| Hidden-browser session | 45 s; "settled" = URL stable 3 s and not an `openathens_wait` URL | browserScope deadline + 500 ms poll |
| Login window | 300 s (pref) | loginScope deadline; the window shows a countdown |
| Translator run | 8 s | `scope.race` (the run cannot be aborted, so its result is ignored) |
| Item OA phase / inst phase | 120 s / 240 s, excluding parked time | Phase scopes |
| Waiting for a scheduler slot | no hard limit, but if the wait exceeds 60 s the UI shows "waiting for <host> (rate limit)" | Pump reports `next_wake_ms` |

### 2.10 Cancellation sources and shutdown
- **User Cancel** (progress window) → `runScope.cancel("user")`. In-flight XHRs abort, browsers are destroyed, login windows close, and parked items reject. Items end as `Cancelled`, except those already in **Saving**. `saveTx` is a short **non-cancellable critical section**, and scope checks happen just before it, never inside it.
- **Login window closed** → that realm's waiters receive `LoginDeclined`, and their items end as `NeedsBrowser`. The window is not reopened automatically in the same run; the progress UI offers "Retry sign-in".
- **Plugin disable / upgrade / uninstall** → `shutdown(reason)`:
  1. `pluginScope.cancel("shutdown")`.
  2. Await `runManager.settle(2000)`.
  3. Force-close windows.
  4. Destroy every tracked `HiddenBrowser`.
  5. Unregister our **single** PDF MIME dispatcher.
  6. Remove tracked temp dirs (`IOUtils.remove(dir, {recursive:true, ignoreAbsent:true})`).
  7. Persist the ledger.
  8. Remove the FTL links.
  9. `chromeHandle.destruct()`, then drop the wasm instances and `delete Zotero.ZoteroFullText`.

  The sandbox is reused on disable→enable [01 §2.1], so every module-level variable is re-initialised in `startup()`. A **generation counter** (Beaver pattern [01 §2.1]) guards late continuations: any `await` resumption in an old generation returns before touching the DB or UI.
- **Zotero shutdown (`APP_SHUTDOWN`)** → call `pluginScope.cancel("shutdown")` synchronously, persist the ledger, and return **without awaiting**. Zotero tears down windows itself [01 §2.5].

### 2.11 Back-pressure and UI responsiveness
- The pull model bounds memory: an item has at most 2 attempts in flight, and the pool admits at most 4 items. Bodies are capped: HTML is truncated to 2 MiB before reaching Rust, and PDFs are capped at 200 MB (`content-length` precheck; otherwise abort on `progress` events past the cap).
- When licence caps are reached, Rust **denies** instead of letting work queue indefinitely. Items get `Deferred{daily_cap|hourly_cap|batch_cap}` immediately ("try again later") rather than holding slots.
- UI updates are events coalesced at 4 Hz by `ui/progress-controller.js`. A burst of state changes never re-renders more than 4×/s.

---

## 3. Per-item pipeline

### 3.1 Flow

```
Queued ─► Eligibility ─► Identify ─► OaLookup ═╦═► OaAttempt ──(PDF)──────────────────────────► Validate ─► Saving ─► ATTACHED
              │              │                 ║        │ exhausted
              ▼              ▼                 ║        ▼
          SKIPPED        SKIPPED               ║   InstGate ─(deny)─► DEFERRED / BLOCKED     (OA-only run → NOT_FOUND)
       (has PDF, synced  (no DOI/PMID/         ║        ▼
        not downloaded,   PMCID/arXiv)         ║   Entitlement ─(no getFullTxt)─► NOT_ENTITLED (+hand-off)
        read-only lib,                         ║        ▼
        not regular)                           ║   InstDirect ─(PDF)─► Validate
                                               ║        ▼ no access / challenge
                                               ║   InstBrowse (redirector in HiddenBrowser, plain UA, PDF capture)
                                               ║     ├─(PDF captured / landing→PDF)─► Validate
                                               ║     ├─(idp_login)─► AwaitingLogin ─(session)─► InstBrowse (once)
                                               ║     │                    └─(closed/timeout/handoff mode)─► NEEDS_BROWSER
                                               ║     ├─(OpenAthens 404 & EZproxy-configured host)─► InstEzproxy ─► (same as InstBrowse)
                                               ║     └─(idp_error e.g. AADSTS53000 / blocked)─► NEEDS_BROWSER (+Windows-SSO hint)
          any state ── cancel ─► CANCELLED     any inst state ── halt effect ─► BLOCKED     unexpected throw ─► FAILED
```

### 3.2 State machine (table-driven in `lib/item-task.js`; every guard is a Rust enum)

| State | Entry action | Event → next |
|---|---|---|
| `Queued` | row added | slot acquired → `Eligibility` |
| `Eligibility` | `isRegularItem`, not a feed item, `Zotero.Libraries.get(libraryID).filesEditable`, `numFileAttachmentsWithContentType('application/pdf')==0`, `getBestAttachmentState()` [02 §8] | ineligible → **Skipped{reason}**; ok → `Identify` |
| `Identify` | `ids.normalise` over DOI, Extra (`getExtraField`), PMID, PMCID, `archiveID`, URL | no usable id → **Skipped{no_identifier}**; else `OaLookup` |
| `OaLookup` | §2.5 | `early_start` or all settled → `OaAttempt` |
| `OaAttempt` | §2.6 with the fetch sub-machine §3.3 | verified PDF → `Validate`; queue exhausted → `InstGate` (if the run allows inst) else **NotFound** |
| `InstGate` | first inst ticket for this item; S checks `batch_cap`, ledger, halt | deny → **Deferred{cap}** / **Blocked{halt}**; grant → `Entitlement` |
| `Entitlement` | if `inst.checkAlma`: GET Alma CTO [04 §4.5] → `inst.alma_parse` | `known && services==[]` → **NotEntitled**; else `inst.plan` → first step |
| `InstDirect` | GET `https://doi.org/<doi>`, default jar, plain UA (campus IP / VPN / existing session) | PDF → `Validate`; landing → extract → PDF fetch; `no_access_hint` / HTML-instead-of-PDF → record `knowledge.hosts[h].direct="no_access"`, next step; challenge → browser tier |
| `InstBrowse` | HiddenBrowser (plain UA) loads the redirector URL `go.openathens.net/redirector/kcl.ac.uk?url=<enc https://doi.org/DOI>`; poll `url.classify` every 500 ms; PDF capture armed | captured PDF → `Validate`; settled on publisher → `fx_page` on `PageData.documentHTML` → candidate PDF fetched via XHR with the same UA, else navigated in the **same** browser with capture; `idp_login`/`ezproxy_login`/`saml_autopost`-stuck → `AwaitingLogin`; `openathens_unknown` → EZproxy step if host configured, else **NeedsBrowser** |
| `AwaitingLogin` | release slot; `LoginCoordinator.awaitSession(realm, entryURL)` | session → re-acquire slot → `InstBrowse` (**one** retry); declined/timeout → **NeedsBrowser** |
| `InstEzproxy` | only if `proxy` reports the host as configured (seed list of ~19 hosts [04 §2.4] plus the cached no-follow probe [SUMMARY c]); entry `login?qurl=`; same browser logic | as `InstBrowse` |
| `Validate` | `fx_pdf_inspect(head, tail)`; size ≥ 1 KiB | ok → `Saving`; not PDF / truncated → back to the previous attempt loop, candidate marked failed |
| `Saving` | §3.7 | ok → **Attached**; error → **Failed** |

### 3.3 Fetch sub-machine (one candidate URL; tiers per [SUMMARY C4])
1. **Lease:** `pump.withLease(scope, {host, kind: page|pdf, route, item}, …)`. The grant supplies the UA.
2. **T0 plain HTTP:** `Zotero.HTTP.request('GET', url, {responseType:'arraybuffer', successCodes:false, timeout, errorDelayMax:0, noRetryOnThrottle:true, headers:{Accept, ['User-Agent']: plainUA if granted plain}, cancellerReceiver})`. JS builds `ResponseMeta{status, final_url: xhr.responseURL, content_type, headers{server, cf-mitigated, retry-after, content-length}}`.
   - If `fx_pdf_inspect` says PDF, go to `Validate`.
   - Otherwise call `fx_page(meta, bytes)` and switch on `class.kind`:
     - `landing` → candidates (+ translators §3.4)
     - `redirect{next}` → follow (≤ 3 hops)
     - `reader_shell{rewrite}` → retry with the rewritten URL (`/epdf/` → `/pdfdirect/…?download=true` etc. [06 §3.1])
     - `challenge{interactive:false}` → **T1**
     - `challenge{interactive:true}` or `blocked`:
       - OA: skip the candidate; S marks the host.
       - inst: `complete(challenge|blocked)`; S may emit `halt`.
     - `login{…}` → only meaningful on inst → `AwaitingLogin`
     - `licence_stop` → `complete(suspended)` → S halts inst
     - `not_found`/4xx → next candidate.
3. **T1 browser capture:** a `HiddenBrowser({customUserAgent: plainUA})` from `chrome://zotero/content/HiddenBrowser.mjs`, with our PDF dispatcher armed for this browser element. Wait until it settles (≤ 45 s). If a PDF was captured, take its Blob → `Uint8Array` → `Validate`. Otherwise read `PageData.documentHTML` and call `fx_page` again (one level only). We never solve CAPTCHAs [06 §5.2].
4. **T2 visible window (user acts):** used only when T1 ends `interactive` *and* the item is institutional or the user enabled "show challenge windows". This is the login window (§4.5) with the same capture. Otherwise the outcome is **NeedsBrowser** with hand-off URLs.
5. **T3 hand-off:** "Open via King's in browser" (redirector URL), "Open LibKey" (`libkey.io/libraries/286/<doi>` [04 §5]) and "Attach downloaded PDF…" (§4.3).

**PDF capture dispatcher.** There is exactly **one** `Zotero.MIMETypeHandler.addHandlers('application/pdf', handler, true)` registration for the plugin's lifetime. Handlers are global and iterated (`mimeTypeHandler.js:105-141, 283-297`). The handler's `onStartRequest(name, type, channel)` and `onContent(blob, name, type, channel)` resolve `channel.notificationCallbacks…nsILoadContext.topFrameElement` against a `Map<browserElement, CaptureSession>`. It returns `true` only for our browsers, so Zotero's own handling is untouched. This is our own implementation of the pattern: we do not copy `_makePDFMIMETypeHandler` (AGPL [02 §14]). It is removed in `shutdown()`.

### 3.4 Landing-page extraction: Rust extractor plus Zotero translators
- **Rust `fx_page`** (html5gum 0.8.4, `naively_switch_states(true)` [07 §2.3]) produces:
  - page ids (`citation_doi`, `dc.identifier[scheme=doi]`, `prism.doi`, PII);
  - `doi_match`;
  - the class (§3.3);
  - access hints [06 §2.2];
  - ranked candidates (publisher rule > `citation_pdf_url` > `link[rel=alternate][type=application/pdf]` > other head metas > JSON state > scored anchors > frames > interstitial);
  - rejections (supplements, citation exports, other-DOI links, reader shells, `mailto:`/`javascript:`, refreshes inside `<noscript>`).

  Rules are matched on the hyphen-normalised `host_key` [06 §4.1].
- **Translators** use `Zotero.Utilities.Internal.blobToHTMLDocument(new Blob([bytes],{type}), finalURL)` (`utilities_internal.js:966`). JS removes every `<noscript>` element, then calls `getFileFromDocument(doc)` (`:1475`), which runs only the first matching translator. The hit goes through `cand.from_translator`, which applies the same reject rules, and enters the queue scored just below `citation_pdf_url`.
- **OA route:** translators run **alongside** Rust (parallel, `translatorSlots`=1, 8 s cap).
- **Inst route:** translators run **only as a fallback**, when Rust found nothing (`"translators":"fallback_only"`). Translators can issue their own requests, which our scheduler cannot see. While translators run, the host lease is **held**, so per-host concurrency stays 1.

### 3.5 Institutional route specifics
- **Preconditions** (checked in the run's preflight):
  - `inst.enabled`;
  - the run was not started as "OA only";
  - the user confirmed when > 10 items were selected;
  - a group-library warning was confirmed if any item is in a group library (licence 4.1.4/4.1.6 [04 §8.1]).
- **`inst.plan`** orders the steps `direct → redirector → ezproxy` and elides any step that is useless:
  - `direct` is skipped once the host showed `no_access` in this run, so we don't hammer it per item;
  - `redirector` is skipped for passthrough hosts (LWW) [04 §4.3];
  - `ezproxy` is only used for configured hosts.

  The plan always returns hand-off URLs.
- **Redirector target** is always `https://doi.org/<doi>`, never `thelancet.com` [04 §4.3-4.4].
- **Saving** uses `attach.clean_url`: never a proxied, redirector, SAML or signed URL [04 §7].

### 3.6 Login handling (`LoginCoordinator`, `lib/login.js`)
1. **One coordinator per realm** (`kcl`). The first item that needs login opens the window. Later items join `awaitSession(realm)` and add to the window's "N items waiting" counter. Items stay parked with their slots released.
2. **The window** is our `login.xhtml`, from the sso-spike template [08 §6.1]:
   - no `messagemanagergroup`, so there is no ExternalLinkHandler hand-off to the system browser [SUMMARY C5];
   - an origin and TLS bar from `browsingContext.secureBrowserUI`;
   - popups shown visibly through `window.browserDOMWindow`;
   - plain Firefox UA via `browsingContext.customUserAgent`;
   - the load starts at the **entry URL** (the redirector or EZproxy `login?qurl=` for the first item), never at the IdP URL [SUMMARY c].
3. **Completion detection** polls `browser.currentURI` every 500 ms → `url.classify`. Success is any of:
   - the URL left the IdP / OpenAthens / EZproxy-login hosts and has been on a publisher or proxied host for 2 s;
   - a PDF was captured (it is handed to the first item directly);
   - the user pressed "I've signed in".

   An `idp_error` class (Entra Conditional Access codes, low confidence [08 §5]) → `LoginFailed{ca}`. Cookies are a hint only [SUMMARY C6].
4. **After success** the window closes. Parked items resume `InstBrowse` in hidden browsers. With a live OpenAthens/Entra session the redirector completes silently (T4 in [08 §4.1]). If a resumed item lands on the IdP again, it is **not** looped: it becomes `NeedsBrowser{session_not_reused}`, and the report suggests "sign in once per batch".
5. **Fallbacks (degradation ladder):**
   1. custom window;
   2. hidden-browser session reuse;
   3. `inst.loginMode="handoff"`, which never opens a window and always gives hand-off buttons;
   4. "Attach downloaded PDF…".

   The optional, off-by-default **Windows SSO** pref sets `network.http.windows-sso.enabled=true`, and restores it when unticked or on uninstall, only if we changed it. It is offered when `LoginFailed{ca}` occurs [08 §2.5].
6. **Privacy and safety:**
   - we never read form fields, cookie values or page text in the login window, and only redacted URLs are logged;
   - we never autofill;
   - Turnstile and CAPTCHAs are answered by the user, in the visible window.

### 3.7 Validation and save
- **Validate:**
  - `fx_pdf_inspect` must report `%PDF-` within 1 KiB and `%%EOF` in the last 1 KiB (detects truncation).
  - If the offset is > 0, we strip the leading junk before saving, so the saved file satisfies Zotero's offset-0 sniff (`mime.js:33-34` [SUMMARY C7]).
  - Encrypted PDFs are allowed (logged).
  - v1 has no content-to-DOI matching (§8).
- **Save (JS `SavePort`):**
  1. Re-check eligibility: the item still exists, is not trashed, and still has no PDF. This guards against concurrent edits and other runs. If it fails → **Skipped{pdf_added_meanwhile}**.
  2. `title = Zotero.getString('attachment.fullText' | 'attachment.acceptedVersion' | 'attachment.submittedVersion')`, chosen by the candidate version [02 §2.7].
  3. `filename = Zotero.Attachments.getFileBaseNameFromItem(parent, {attachmentTitle: title}) + '.pdf'` (`attachments.js:2380`).
  4. `dir = (await Zotero.Attachments.createTemporaryStorageDirectory()).path`, then track `dir` → `await IOUtils.write(PathUtils.join(dir, filename), bytes)`.
  5. `await Zotero.Attachments.createURLAttachmentFromTemporaryStorageDirectory({directory: dir, libraryID: parent.libraryID, parentItemID: parent.id, filename, title, url: cleanURL, contentType: 'application/pdf'})`. This moves the dir and queues full-text indexing (`attachments.js:682-736`). On error, remove `dir`.
- **Feature detection.** Every Zotero internal we call is feature-detected at startup: `HiddenBrowser`, `BrowserRequest`, `MIMETypeHandler`, `getFileFromDocument`, `blobToHTMLDocument`, `createURLAttachmentFromTemporaryStorageDirectory`, `getOpenAccessPDFURLs`. If one is missing, its tier is disabled and logged rather than crashing [SUMMARY g].

### 3.8 Terminal outcomes
`Attached{route: oa|inst, src, version, via: http|browser|login|manual}`, `Skipped{reason}`, `NotFound{tried[]}`, `NotEntitled{handoff[]}`, `NeedsBrowser{reason, handoff[]}`, `Deferred{hourly_cap|daily_cap|batch_cap}`, `Blocked{host|vendor|licence_halt}`, `Cancelled`, `Failed{error_id}`. Each outcome maps to a Fluent message and a list of row actions.

---

## 4. UI

### 4.1 Menus (`Zotero.MenuManager.registerMenu`, l10n IDs only [01 §3.1])
| Target | menuID | l10nID → label | Behaviour |
|---|---|---|---|
| `main/library/item` | `get` | `zotero-fulltext-menu-get` → "Get Full Text" | Visible if ≥ 1 regular parent item is selected (attachments and notes map to their parent); enabled if all libraries are `filesEditable`. Runs OA, then institutional. |
| `main/library/item` | `more` (submenu) | `zotero-fulltext-menu-more` → "Full Text Options" | — |
| ↳ | `get-oa` | `zotero-fulltext-menu-get-oa` → "Get Open-Access Full Text Only" | Run with the inst route off |
| ↳ | `open-inst` | `zotero-fulltext-menu-open-inst` → "Open via King's in Browser" | Single item; `Zotero.launchURL(redirector)` |
| ↳ | `attach-file` | `zotero-fulltext-menu-attach-file` → "Attach Downloaded PDF…" | File picker → `fx_pdf_inspect` → `importFromFile` |
| `main/menubar/tools` | `signin` | `zotero-fulltext-menu-signin` → "Sign In to King's for Full Text…" | Opens the login window at `inst.warmupURL` |
| `main/menubar/tools` | `report` | `zotero-fulltext-menu-report` → "Full Text: Last Report" | Re-opens the last report |

There is no collection-level "get all" in v1. That is deliberate, because bulk modes conflict with licence 4.1.7 [04 §8.1]. The FTL file is `locale/en-US/zotero-fulltext.ftl`. `insertFTLIfNeeded("zotero-fulltext.ftl")` runs for every window in `startup()` and `onMainWindowLoad` (WeakSet guard [03 §9]).

### 4.2 Progress window (`content/progress.xhtml`, custom)
We use a custom window instead of `ProgressQueue`, because rows need actions (hand-off, attach, retry sign-in), which `ProgressQueue` cannot host [01 §6.2].
- **Header:** "Getting full text: 7 of 12 done · 2 waiting for sign-in · 1 rate-limited". The institutional usage line reads "Institutional PDFs today: 12 / 80".
- **Banner slot:** licence halt (`zotero-fulltext-banner-halt`), login needed (`…-banner-login` with a "Sign in" button), or degraded mode (`…-banner-degraded`, shown when the extractor is disabled after repeated panics or a Zotero internal is missing).
- **Table:** item icon (`getItemTypeIconName()`), title, status text (`zotero-fulltext-status-{queued,checking,lookup,trying,browser,waiting-rate,waiting-login,saving}` with `$host`/`$source` args), route badge (OA/KCL), and an action button.
- **Footer:** `Cancel` (`zotero-fulltext-progress-cancel`), which becomes `Close` when the run finishes, and `Copy report`.
- **Behaviour:**
  - Closing the window does **not** cancel the run. A small `Zotero.ProgressWindow` headline ("Full text: running…") lets the user reopen it.
  - Auto-close happens 4 s after a fully successful run if `ui.autoClose`.
  - Single-item runs use the same window in a compact layout.

### 4.3 End-of-run report
- **Summary line** (`zotero-fulltext-report-summary` with `$attached $notFound $needsBrowser $deferred $skipped $failed`), then one line per item: outcome, source/route, and the redacted final host.
- **Row actions:**
  - NeedsBrowser / NotEntitled: "Open via King's", "Open LibKey", "Attach downloaded PDF…".
  - Deferred: "Retry later" (no timer; the user re-runs it).
  - Failed: "Copy diagnostics".
- **"Copy report"** writes a plain-text, redacted report with no cookies or query values. It is also logged to `Zotero.debug` under the `[ZFT]` prefix.

### 4.4 Preferences pane (`Zotero.PreferencePanes.register`, `content/preferences.xhtml`; keys under `extensions.zotero-fulltext.`)
| Key | Default | UI |
|---|---|---|
| `oa.openalex` / `oa.europepmc` / `oa.biorxiv` / `oa.pmcS3` / `oa.arxiv` | true | Checkboxes, "Open-access sources" |
| `oa.zoteroOA` | true (used only as the OpenAlex fallback) | Checkbox, noting that it uses Zotero's private OA service |
| `oa.unpaywallEmail` | "" (empty = Unpaywall off; no default ever ships) | Text field with format validation |
| `oa.openalexKey` / `oa.semanticScholarKey` / `oa.coreKey` | "" | Password-style fields (advanced); S2 and CORE stay hidden until v1.1 |
| `oa.versions` | "published,accepted,submitted" | Three checkboxes and an order |
| `inst.enabled` | true | "Use King's College London access (user-initiated only)" |
| `inst.institution` | "kcl" | Preset menu (only KCL ships); advanced: `inst.configJSON` override |
| `inst.tryDirect` | true | "Try direct access first (campus / VPN / existing session)" |
| `inst.checkAlma` | true | "Check King's library holdings before signing in" |
| `inst.loginMode` | "window" | Radio: "Sign in inside Zotero" / "Always open in my browser" |
| `inst.loginTimeoutSec` | 300 | Number |
| `inst.ezproxyHosts` | seed list [04 §2.4] | Textarea (advanced) |
| `inst.windowsSSO` | false | Checkbox with a warning text; toggles the Gecko pref and remembers that we set it |
| `inst.confirmAbove` / `inst.batchMax` | 10 / 20 | Numbers; Rust clamps `batchMax` ≤ 20 |
| `inst.hourlyCap` / `inst.dailyCap` | 30 / 80 | Numbers; Rust clamps to these ceilings; shows "used today: n" |
| `inst.warmupURL` | redirector → `https://www.nature.com/` | Advanced |
| `inst.ledger` | "[]" | Internal (not shown) |
| `net.maxConcurrentItems` / `net.hedge` | 4 / true | Advanced |
| `ui.autoClose` / `ui.showChallengeWindows` | true / false | Checkboxes |
| `debug.verbose` | false | Checkbox |

Pane text discloses that the redirector sees target DOIs, and that no personal email is ever sent [SUMMARY g].

### 4.5 Login window behaviour
- **Title:** `zotero-fulltext-login-title` ("Sign in to King's College London"), with a sub-line "for: <item title> (+N waiting)".
- **Origin and TLS bar**, a visible popup pane, and three buttons: "I've signed in" (`…-login-done`), "Open in my browser instead" (`…-login-open-system`, which hands off with the entry URL and resolves the waiters as `NeedsBrowser`), and "Cancel" (`…-login-cancel`).
- **Status line:** "Waiting for sign-in… 4:12 left".
- Turnstile pages stay visible with a hint "If you see 'Just a moment…', wait or tick the box" [08 §4.3].
- It never steals focus repeatedly: it activates once on open.

---

## 5. Code layout

### 5.1 Rust workspace

```
Cargo.toml          [workspace] resolver="3"; [workspace.package] version="0.1.0", edition="2024"
                    [workspace.lints.clippy] unwrap_used/expect_used/panic/unreachable/todo/unimplemented/string_slice = deny,
                    indexing_slicing = warn   [07 §1.2, §4]
                    [profile.release] opt-level="z", lto=true, codegen-units=1, panic="abort", strip=true, incremental=false
Cargo.lock          idna_adapter pinned =1.0.0  [07 §1.4]
.cargo/config.toml  alias xtask = "run -q -p xtask --"
crates/fulltext-core/   (rlib; no I/O, no clock, no RNG source; natively tested)
  src/lib.rs            pub mod …; pub fn dispatch(op:&str, args:&serde_json::value::RawValue) -> Result<Value, CoreError>
  src/error.rs          pub struct CoreError { code: ErrCode, message: String }  // bad_json|bad_args|bad_url|unknown_op|too_large|internal
  src/model.rs          Ids, Doi, Pmcid, Source, Version, HostKind, Route, Candidate, RequestSpec, ResponseMeta
  src/ids.rs            pub fn normalise(f: &ItemFields) -> Ids
  src/oa/{mod,openalex,europepmc,biorxiv,pmc_s3,unpaywall,zotero_oa,crossref}.rs
                        pub fn requests(ids:&Ids, cfg:&OaConfig) -> Vec<RequestSpec>
                        pub fn parse(src:Source, meta:&ResponseMeta, body:&str, ids:&Ids) -> Result<OaParse, CoreError>
  src/rank.rs           pub fn rank(c:&[Candidate], attempted:&BTreeSet<String>, p:&RankPrefs, ids:&Ids) -> Ranked
                        pub fn dedupe_key(url:&Url) -> String     // includes the query string [05 §5]
  src/page/{tokenize,facts,classify,extract,rules,refresh}.rs
                        pub fn analyse(ctx:&PageCtx, html:&[u8]) -> PageAnalysis
                        pub fn classify_url(u:&Url, inst:&Institution) -> UrlClass
                        pub enum PageClass { Pdf, Landing, Redirect{next}, ReaderShell{rewrite}, Challenge{vendor, interactive},
                                             Blocked{vendor}, Login{kind}, IdpError{code}, LicenceStop, EzproxyNotConfigured,
                                             EzproxyError, OpenAthensUnknown, OpenAthensBadOrg, NotFound, Html }
                        pub fn from_translator(hit:&TranslatorHit, page:&PageAnalysis, ids:&Ids) -> Option<Candidate>
  src/proxy.rs          pub struct Ezproxy{login_host, suffix}; login_url; proxify_host -> Option<Url>;
                        unproxify -> Unproxied{Exact|Ambiguous|NotProxied}; redirector_url; unwrap_redirector; host_key; HostMap
  src/inst/{config,plan,alma}.rs
                        pub struct Institution{ openathens_domain, ezproxy: Option<Ezproxy>, alma: Option<Alma>, libkey_id: Option<u32>,
                                                idp_hosts, passthrough_hosts, ezproxy_hosts }   // KCL preset as const data [04 §6.1]
                        pub fn plan(i:&PlanInput) -> InstPlan;  pub fn parse_alma_cto(xml:&str) -> Result<Entitlement, CoreError>
  src/pdf.rs            pub fn inspect(head:&[u8], tail:&[u8]) -> PdfInfo
  src/attach.rs         pub fn clean_url(final_url:&str, landing:Option<&str>, ids:&Ids, inst:&Institution) -> String
  src/redact.rs         pub fn redact_url(u:&str) -> String
  src/sched/{mod,config,hosts,bucket,ledger,rng}.rs
                        pub struct Scheduler { cfg, hosts: BTreeMap<String,HostState>, queue: Vec<Ticket>, inflight: BTreeMap<u32,Inflight>,
                                               ledger: Ledger, halts: Vec<Halt>, inst_items: BTreeSet<u32>, rng: XorShift64, next_id: u32 }
                        impl Scheduler { pub fn new(cfg:SchedConfig, ledger:Ledger, seed:u64, now:u64) -> Self;
                          pub fn submit(&mut self, r:TicketReq, now:u64) -> u32; pub fn poll(&mut self, now:u64) -> PollOut;
                          pub fn complete(&mut self, id:u32, o:Outcome, now:u64) -> Vec<Effect>; pub fn cancel(&mut self, id:u32, now:u64);
                          pub fn cancel_item(&mut self, item:u32, now:u64); pub fn snapshot(&self) -> Snapshot }
                        pub const CEILINGS: Ceilings = Ceilings{ inst_global:2, inst_per_host:1, pdf_gap_ms:5000, page_gap_ms:2000,
                                                                 hourly:30, daily:80, mb_per_hour:300, batch_items:20, item_fetches:4 };
  tests/{fixtures_html.rs, fixtures_oa.rs, proxy_vectors.rs, classify.rs, ids.rs, rank_props.rs, sched_sim.rs, envelope.rs}
  fuzz/                 cargo-fuzz targets (x86_64 only): page_analyse, oa_parse, unproxify, envelope
crates/fulltext-wasm/   (cdylib only, so LTO stays on [07 §1.3])
  src/lib.rs            #[unsafe(no_mangle)] exports of §1.3; thread_local! { static SCHEDS: RefCell<Vec<Option<Scheduler>>> }
                        (try_borrow_mut → err envelope, never a panic); panic hook → static LAST_PANIC
xtask/                  dist | wasm | check-wasm | test | harness | dev-proxy   (zip 9 + zlib-rs [07 §6])
```

Size budget: about 308 KB raw / 110 KB gzip with html5gum and idna 1.0 [07 §1.4]. The scheduler adds an estimated ≤ 15 KB. `check-wasm` fails above 400 KB raw.

### 5.2 JS files (`addon/`, loaded as classic IIFE subscripts with `{target: ctx, ignoreCache: true}` from `chrome://zotero-fulltext/content/`)

| File | Responsibility |
|---|---|
| `manifest.json` | `manifest_version` 2; `applications.zotero` = `{id:"zotero-fulltext@aborakati", update_url:"https://raw.githubusercontent.com/ABorakati/zotero-fulltext/main/updates.json", strict_min_version:"10.0", strict_max_version:"10.*"}` |
| `bootstrap.js` | Lifecycle only: registerChrome, load subscripts in order, generation counter, idempotent `startup`/`shutdown`, window init for existing windows [03 §9] |
| `prefs.js` | Defaults for §4.4 |
| `content/lib/core-glue.js` | `CoreHost` (compile once, instances D and S, poison/reset/replay, caps, op logging at verbose level) |
| `content/lib/async.js` | `CancelScope`, `CancelledError`, `Semaphore`, `TaskSet`, `firstSuccess`, `idle()` |
| `content/lib/pump.js` | `SchedulerPump`, `Lease` (`withLease` guarantees `complete` in `finally`) |
| `content/lib/ports/http.js` | `HttpPort.get(scope, url, {lease, kind})` → `{meta, bytes}`; maps Zotero exceptions to outcomes |
| `content/lib/ports/browser.js` | `BrowserPort` (HiddenBrowser sessions, PageData queries, PDF dispatcher, tracked destroy) |
| `content/lib/ports/translators.js` | `blobToHTMLDocument` + noscript strip + `getFileFromDocument` with timeout |
| `content/lib/ports/save.js` | Eligibility, field extraction, `SavePort.attach`, tracked temp dirs |
| `content/lib/ports/zotero-oa.js` | `getOpenAccessPDFURLs(doi)` wrapper, feature-detected |
| `content/lib/oa.js` | OA phase: lookups, `CandidateQueue`, early start, attempt loop, hedge |
| `content/lib/fetch.js` | Fetch sub-machine T0–T2 (§3.3) |
| `content/lib/inst.js` | Institutional phase: Alma, plan execution, browser steps |
| `content/lib/login.js` | `LoginCoordinator`, login window driver |
| `content/lib/item-task.js` | Per-item FSM table and outcomes |
| `content/lib/run.js` | `RunManager`: preflight confirms, pool, joins, halts, report assembly, events |
| `content/lib/ui/menus.js`, `ui/progress-controller.js`, `ui/report.js` | Menus, coalesced UI updates, report text |
| `content/progress.xhtml` + `progress.js`, `login.xhtml` + `login.js`, `preferences.xhtml` + `preferences.js`, `icons/` | Windows and pane |
| `content/fulltext_core.wasm` | Built by xtask (git-ignored in source) |
| `locale/en-US/zotero-fulltext.ftl` | All strings |

Public test hook: `Zotero.ZoteroFullText = { run(items, {mode}), cancel(), lastReport(), version }`.

---

## 6. Testing strategy

### 6.1 Native `cargo test` (no network, deterministic)
- **`fixtures_html.rs`:** iterates `fixtures/html/expected.json`. It asserts `class.kind`, the top candidate ∈ {`expected_pdf_url`} ∪ `alternates`, and no `must_not_select` URL anywhere in the candidates. Status and headers for the Cloudflare and Akamai fixtures come from the README [06 §6].
- **`fixtures_oa.rs`:** iterates `fixtures/oa/manifest.json`, which supplies the status per body. Every body must parse without error. Golden expectations per case are kept in `tests/golden/oa/*.json`:
  - gold → PMC S3 / Europe PMC candidates;
  - green → arXiv + AAM-has-no-PDF;
  - the arXiv merge is down-weighted [05 §2.3];
  - medRxiv → early-path URL;
  - paywalled → none;
  - HTML or text 404s → `not_found`, not an error.
- **`proxy_vectors.rs`:** every vector in [04 §6.4] (qurl decode, `url=` verbatim, hyphen labels, the Thieme table, `--` → Ambiguous, lowercase) plus a redirector unwrap.
- **`classify.rs`:** URL and page classes for every row of [SUMMARY c] `classify()`.
- **`ids.rs`:** DOI cleaning (`%2F`, `doi:` prefixes, Extra lines, case), PMCID forms, the arXiv DOI, `10.1101/`.
- **`rank_props.rs`** (proptest): rank is deterministic; no duplicates by key; attempted keys are never returned; published > accepted > submitted under default prefs.
- **`sched_sim.rs`** (simulated clock, fixed seed). The licence proofs:
  - 25 inst items on one host → ≤ 20 items admitted, ≤ 1 concurrent per host, ≤ 2 globally, consecutive PDF grants ≥ 5000 ms apart;
  - the 31st PDF in an hour is denied, as is the 81st in 24 h, including a ledger loaded from "yesterday";
  - 429 → halt effect, and no inst grants afterwards;
  - 5 consecutive failures → host skip;
  - fairness: a slow host never blocks grants for other hosts;
  - `cancel` releases in-flight accounting;
  - a property test over random submit/complete/cancel interleavings never violates an invariant.
- **`envelope.rs`:** every op with malformed args returns `err` and never panics. The fuzz targets run nightly locally (`cargo +nightly fuzz run page_analyse -- -max_total_time=600`).

### 6.2 Node tests (`tests/js/*.test.cjs`, `node --test`, no npm dependencies)
- **ABI contract** against the **release** wasm, using the shipped glue loaded through `vm` into a context *without* `AbortController`/`console`, the same way `spikes/wasm-spike/js/sim-bootstrap.cjs` does:
  - memory pages are stable over 10k calls;
  - alloc/free pairing holds (instrumented debug build);
  - UTF-8 edge cases and 2 MiB HTML truncation work;
  - a `debug-panic` feature build traps → poison → reset → `cfg.set` replay → the next call succeeds;
  - D and S are isolated (a trap in D leaves S's `snapshot` intact).
- **Orchestrator** with fake ports (`FakeHttp` serving `fixtures/oa` + `fixtures/html` by URL, `FakeBrowser`, `FakeLogin`, `FakeSave`, `FakeClock` driving `setTimeout`). It asserts:
  - pool bound;
  - parking releases slots;
  - the hedge cancels the loser (the fake XHR sees `abort`);
  - early start;
  - login join (3 items → 1 window);
  - a halt effect cancels inst work run-wide;
  - every state reacts to cancel;
  - shutdown mid-run leaves **0 pending timers, 0 open fake browsers, 0 temp dirs, 0 MIME registrations**;
  - a `process.on('unhandledRejection')` hook fails the test.
- **Boundary lint:** an `ast-grep` rule (§1.1) run by `cargo xtask test`.

### 6.3 End-to-end in the throwaway Zotero harness (no login, never the user's profile)
`tools/zotero-harness/run.ps1` with the packed XPI built by `cargo xtask dist --e2e`. That build adds `content/test/e2e.js`, which bootstrap loads **only** if present and the harness pref `extensions.zotero-harness.resultFile` is set; release builds exclude the file. Fakes run on `httpd.sys.mjs` at `127.0.0.1:<random>`, as in [03 §10]. Test builds accept endpoint and institution overrides (`cfg.set{endpoints, institution}`), so OA APIs, the "publisher", the "redirector", the "EZproxy" and the "IdP" all point at local fakes.

| # | Scenario | Asserts |
|---|---|---|
| E1 | Startup | menu registered; FTL inserted into the already-open window; prefs pane registered; wasm `meta.build` == manifest |
| E2 | OA gold via fake PMC S3 | attachment `LINK_MODE_IMPORTED_URL`, title "Full Text", clean `url`, file starts with `%PDF` |
| E3 | Landing with `citation_pdf_url` + translator | Rust and translator agree; supplements rejected |
| E4 | JS challenge page (sets a cookie, then reloads) | T1 HiddenBrowser capture works; dispatcher unregistered afterwards |
| E5 | Fake "IdP" login | the login window opens once for 3 items; the harness clicks the fake page's own button (no real credentials anywhere); parked items resume; PDFs attached |
| E6 | Inst licence behaviour (6 items) | fake-server timestamps show ≥ 5 s spacing and 1/host; a 429 → halt banner and `Blocked` outcomes |
| E7 | Cancel mid-run, then disable the plugin mid-run, then re-enable | no requests reach fakes 1 s after cancel; no windows, browsers, temp dirs or MIME handlers left; re-enable works (sandbox reuse) |
| E8 | Ledger persistence | two runs with `-KeepProfile` (proposed harness flag) respect the daily cap across restarts |
| E9 | Opt-in live OA smoke (unauthenticated) | 3 OA DOIs against the real OpenAlex, Europe PMC and PMC S3 |
| E10 | Probes N1, N2, N11 (unauthenticated) | challenge frequency of `Zotero.HTTP.request` vs HiddenBrowser on publisher hosts; compile and parse times of the 308 KB core in Gecko |

### 6.4 What needs the user
1. **B1:** `pwsh -NoProfile -File tools\sso-spike\run-manual.ps1` (20–30 min) [08 §4]. The outcome→design table in [08 §5] sets `inst.loginMode`, the UA policy and whether T1-XHR or T2-browser is the institutional download path.
2. **Pilot.** The user runs the real plugin in a *throwaway* harness profile (`-Headless:$false`) on about 5 entitled DOIs (Wiley, Lancet via DOI, OUP/BJS, LWW/Annals, BMJ via EZproxy), signing in themselves. This checks session reuse across items (T4), the redaction of report output, and the `NeedsBrowser` hand-offs.
3. Campus or VPN direct-access check (`InstDirect`). The Windows SSO toggle only if a CA error appears.
4. Installing into the real profile is done by the user.

---

## 7. Build, packaging, versioning, dev install

- **`cargo xtask wasm`:**
  1. `cargo rustc -p fulltext-wasm --release --target wasm32-unknown-unknown --crate-type cdylib`, with `CARGO_ENCODED_RUSTFLAGS=--remap-path-prefix=<USERPROFILE>=~` (plus the cargo registry and rustup roots) [07 §6].
  2. Find the output through `cargo metadata` `target_directory`, which respects the user's global `CARGO_TARGET_DIR`; the existing `kache` wrapper is left alone [07 §1.1].
  3. Copy to `addon/content/fulltext_core.wasm`.
- **`cargo xtask check-wasm`** asserts: `WebAssembly.Module.imports == []` (parsed in Rust with `wasmparser`), the expected export set, no `\Users\` / `/home/` byte strings, raw size ≤ 400 KB, and `fx_abi_version` == 2.
- **`cargo xtask dist [--e2e]`:**
  1. Run `wasm` + `check-wasm`.
  2. Stamp `version` from `[workspace.package].version` into `manifest.json`, and the build stamp (version + short git hash) into the wasm `meta` via `env!`.
  3. Stage `addon/`; exclude `content/test/` unless `--e2e`.
  4. Zip sorted entries with fixed timestamps and mode 0644 (reproducible XPI [07 §6]) to `dist/zotero-fulltext-<ver>.xpi`.
  5. Print the SHA-256 and write `dist/updates.json`: `{"addons":{"zotero-fulltext@aborakati":{"updates":[{"version","update_link":"https://github.com/ABorakati/zotero-fulltext/releases/download/v<ver>/zotero-fulltext-<ver>.xpi","update_hash":"sha256:<hex>","applications":{"zotero":{"strict_min_version":"10.0","strict_max_version":"10.*"}}}]}}}` [01 §1.3].
- **Versioning.** Semver, with one source of truth. ABI and op versions are separate (`fx_abi_version` = 2, op `v` = 1). The glue checks both, and `meta.build` must equal the manifest version or the plugin refuses to start and logs "stale wasm". Releases are GitHub Releases. `updates.json` is committed to `main`, which serves the `update_url` raw path. Licence is MIT, with no Zotero code copied verbatim [02 §14].
- **`cargo xtask test`:** `cargo test --workspace` + clippy (wasm target too) + `node --test tests/js` + the ast-grep boundary rule.
- **`cargo xtask harness [--e2e]`:** builds and runs `pwsh -NoProfile -File tools/zotero-harness/run.ps1 -PluginDir dist/…xpi -PluginId zotero-fulltext@aborakati`.
- **Dev install via proxy file:**
  - `cargo xtask dev-proxy --profile <dir>` writes `<dir>/extensions/zotero-fulltext@aborakati`, whose content is the absolute path of `addon/` (no BOM), after running `xtask wasm`.
  - It **refuses** any profile under `%APPDATA%\Zotero\Zotero\Profiles` and any directory listed in the user's `profiles.ini`. It defaults to `.dev/profile` in the repo (git-ignored), launched with `zotero.exe -profile .dev/profile -no-remote -datadir .dev/data -ZoteroDebugText -purgecaches`. A harness-based `tools/dev/launch-dev.ps1` is proposed; it would reuse the harness safety prefs.
  - Edit loop: JS edit → disable/enable in Add-ons (subscripts use `ignoreCache` [01 §2.1]). Rust edit → `xtask wasm` → disable/enable (the wasm is fetched with `?b=<stamp>`). `bootstrap.js` edit → restart.

---

## 8. Risks, mitigations, and what v1 leaves out

| Risk | Mitigation |
|---|---|
| B1 fails (Entra CA or MFA inside Gecko) | The degradation ladder (§3.6), `inst.loginMode="handoff"`, the Windows SSO opt-in, and campus/VPN direct. OA is unaffected. |
| Cloudflare and Akamai walls on most publishers [06 §5.1] | Tiered fetcher. Plain UA, sticky per host. Browser capture in the same UA context. The user acts in the visible window. No CAPTCHA automation. |
| A licence breach suspends KCL access | Compiled ceilings, a persisted ledger, halt-on-signal, user-initiated runs only, no collection mode, a group warning, and simulation tests. |
| Translators issue requests our scheduler cannot see | Inst route: fallback-only, and the lease is held during the run. OA route: the 1-slot translator semaphore. |
| Hidden-browser hops (IdP, redirector) are not individually scheduled | One browser session counts as one fetch. The per-item budget caps sessions at 1 plus a login retry. |
| A panic in the extractor | D is stateless: reset + replay. Three panics in a run disable the extractor (translators only), with a banner. Fuzzing. |
| A trap in the scheduler | Fail closed: halt inst, rebuild S from the persisted ledger (§2.8). |
| Zotero internals change (`HiddenBrowser`, `MIMETypeHandler`, `createURLAttachment…`, `BrowserRequest`) | Feature-detect at startup and disable a tier instead of crashing. Harness E1 on every Zotero update. |
| Main-thread jank on huge pages | 2 MiB cap, `idle()` before analysis, an async-shaped D facade ready for ChromeWorker offload (N11 measurement). |
| A wrong article attached (OpenAlex merges, other-DOI links) | `record_doi` down-weighting, the reject-other-DOI rule, `doi_match` on landing pages. PDF content matching comes in v1.1. |
| Signed or short-lived URLs (ScienceDirect, LWW) | Fetch immediately in the same lease. `attach.clean_url` never stores them. |
| Endpoint drift (Europe PMC `/api/getPdf`, medRxiv early path, PMC PoW) | Each source can be toggled; parsers assert on structure; fixtures are refreshed per [fixtures/oa/README]. |
| JS/Rust policy drift | The boundary rule plus ast-grep CI; the FSM switches only on Rust enums. |
| Large PDFs held in memory (XHR arraybuffer) | 200 MB cap. v1.1: `Zotero.HTTP.download` streaming for `kind:pdf`. |
| Privacy (redirector sees DOIs; proxied or signed URLs; local paths in the wasm) | Disclosure in prefs, clean URLs, `--remap-path-prefix`, redacted logs and reports, no personal email anywhere. |

**Deliberately not in v1:**
- EPUB;
- collection or "whole library" modes, and auto-fetch on item add (licence);
- title-only matching for items without identifiers (wrong-paper risk);
- PDF-content DOI/title verification;
- Semantic Scholar and CORE (parsers exist; UI hidden);
- the paid OpenAlex content API;
- Crossref TDM `link[]` as a download source (used only as a candidate hint on the inst route);
- LibKey API (needs a KCL token [04 §5]);
- a ChromeWorker parser;
- streaming PDF downloads;
- multi-institution presets beyond KCL (the data model supports them);
- automatic retries of institutional failures;
- a Downloads-folder watcher for hand-offs;
- non-Windows validation (nothing Windows-specific apart from the Windows SSO toggle);
- moving the item FSM into Rust (the migration path is kept open by §1.2).
