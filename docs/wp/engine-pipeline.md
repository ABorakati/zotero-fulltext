# WP engine-pipeline: item pipeline (OA path), landing, save, rows

Owner of: `crates/fulltext-engine/src/pipeline/{mod,item,precheck,pool,requests,oa,attempt,landing,save,pure}.rs` and
`src/l10n.rs`. Touch nothing else (`pipeline/inst.rs` and `handoff.rs` belong to engine-inst; they call your
`attempt::fetch_candidate`, `landing::extract_landing`, `save::save_pdf_blob`, `pool` and `pure` APIs, which are
frozen as in the skeleton: you may add, not change).
Read first: `docs/design/engine.md` sections 4, 8, 9; headers of every file you own; `docs/design/core.md` sections 2,
8 (response-shape rule, tiers, `published_doi:` note); `research/05-oa-sources.md` (endpoints, rates, error shapes),
`research/06-publisher-pdf-patterns.md` (tiers, challenges), `research/SUMMARY.md` (d)(e); fixtures in `fixtures/oa`.
Depends on: engine-executor-ops (real executor/ops/SimHost) and engine-sched-login (real gates) for the async tests;
the pure parts (pool, requests, precheck, pure, validate, rows) can be done and tested first.
Env: `CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\engine-pipeline`.

## What to build
1. **requests.rs `api_request`**: per source, URL + `Accept: application/json` + keys, and `InspectSpec::Api {
   source, requested: ids.clone() }`; `max_bytes` 1 MiB, `timeout_ms = policy::timeout_ms(OaApi, false)`:
   OpenAlex `https://api.openalex.org/works/doi:{doi}` (+ `Authorization: Bearer <key>` when set); Europe PMC REST
   search by `DOI:"{doi}"` (else `EXT_ID:{pmid} AND SRC:MED`, else `PMCID:{pmcid}`), `resultType=core&format=json`;
   bioRxiv `https://api.biorxiv.org/details/{biorxiv|medrxiv}/{doi}/na/json` (try biorxiv then medrxiv in one
   lookup); PMC S3 `https://pmc-oa-opendata.s3.amazonaws.com/metadata/{PMCID}.1.json`; Unpaywall only with a
   non-empty email (`https://api.unpaywall.org/v2/{doi}?email=..`); Semantic Scholar / CORE only with keys. Exact
   shapes are in research/05 section 2; honour the `OaConfig` toggles. Percent-encode with `pure::pct_encode`.
   Zotero OA is not HTTP (`cx.zotero_oa(doi)` -> candidates with `rank::new_candidate(.., Source::ZoteroOa)`).
   Crossref (`Source::Doi`, `https://api.crossref.org/works/{doi}`) only when `cfg.inst.enabled` and mode is Full:
   its `link[]` PDFs are publisher URLs that usually need a subscription, so `attempt_oa` SKIPS candidates with
   `source == Source::Doi` (engine-inst tries them in the institutional phase; keep them in the pool).
   Tests: golden URLs per source from the fixtures manifest.
2. **pure.rs**: finish `storable_url` (strip proxy/redirector/login wrappers via `unproxify`, `strip_tracking`, drop
   signed-asset hosts and token params -> landing URL -> DOI URL; vectors in research/04 section 6.4 and F31) with
   tests. Keep it the only file importing `fulltext_core::urls/ids`.
3. **pool.rs** is a thin wrapper over `fulltext_core::rank`; add tests (dedupe incl. query, learned ids never
   overwrite, strong detection via `rank::tier`, `ranked()` skips tried URLs, preprint `published_doi:` note exposed
   via a `take_published_doi()` accessor you add).
4. **oa.rs `discover_oa`**: per its header. Each lookup future: `acquire_api(src)` -> `cx.timeout(OaApi ms,
   cx.http(Route::Oa, false, req, Retain::None, spec))` -> match `HttpResp.inspect`: `Inspection::Api(a)` with
   `Found`/`NotFound`/`Empty` fine, `Throttled` or status 429/5xx -> one retry after `min(retry_after, 10 s)`
   (virtual `cx.sleep`), `Unparseable`/`Failed`/other variants -> source failed. `FuturesUnordered` + a settle timer
   (`select_biased!`); drop the set at the end. Trace each source (`cx.trace_step`).
5. **attempt.rs**: `fetch_candidate` (tier ladder in the header; both OA and inst policies, the inst gate calls use
   `reserve_licence` + `acquire_inst` + `barrier: true`) and `attempt_oa` (sequential over `pool.ranked(&cfg.oa)`,
   OA hedging per header when `cfg.run.hedge`, `pool.mark_tried` everything, landing candidates appended).
   Retain: `Retain::Pdf` for PDF candidates, `Retain::Page` for landing/unknown. Size: `max_bytes = pdf_max_mb`
   for PDFs, 4 MiB for pages. Slow hosts: `cx.timeout_for(class, host)`. UA: `cx.ua_for(host)`; after a
   `Challenge` on a host call `cx.pin_plain_ua(host)` and use `Ua::Plain` for every later request/browser load there.
   Map every `PageClass` the core can return (see `fulltext_core::model::PageClass`) to a `FetchEnd`; unknown or
   `Inspection::Failed` never loops. `OpErrKind::Timeout/Network/TooLarge/Tls/Offline` map to the matching
   `FetchEnd`; `Offline` triggers the NetworkDown pause (row `NETWORK_DOWN`, probe every 15 s, give up after 2 min).
6. **landing.rs `extract_landing`**: per its header (translators via `cx.run_translators`, 8 s, rule candidates
   first, other-DOI candidates rejected using `doi_match`).
7. **save.rs `save_pdf_blob`**: per its header; outcomes mapped as listed; the `Body` is dropped after the op.
8. **item.rs `run_item`**: per its header outcome map; phases wrapped in timeouts; slot handling with
   `pipeline::Slot`; rows at every phase; the `NotFound` reason from `OaEnd::Exhausted` flags (`saw_access_denied`
   -> `AccessDenied`, `saw_timeout` -> `Timeout`, nothing tried -> `NoOaCopy`, OA-only run -> `OaOnly`). The
   institutional call is `crate::pipeline::inst::run_institutional(cx, &mut slot, &ids, &mut pool)` (engine-inst).
9. **l10n.rs `outcome_row`**: implement this table (state, id, args, actions). Actions use
   `RowAction::OpenBrowser { url, l10n }` with `l10n::action::*`, `AttachFile`, `SignIn`, `Retry`.

| Outcome | RowState | id | args | actions |
|---|---|---|---|---|
| Saved{Oa} / Saved{Inst} | Done | SAVED_OA / SAVED_INST | source | - |
| Skipped HasPdf, PdfAddedMeanwhile | Skipped | SKIPPED_HAS_PDF | | |
| Skipped PdfNotDownloaded / ReadOnly / NotRegular, Feed / InProgressElsewhere | Skipped | SKIPPED_NOT_DOWNLOADED / SKIPPED_READ_ONLY / SKIPPED_NOT_REGULAR / SKIPPED_IN_PROGRESS | | |
| NoIdentifier | Attention | NO_IDENTIFIER | | AttachFile |
| NotFound OaOnly | Attention | NOT_FOUND_OA_ONLY | | Retry, AttachFile |
| NotFound NoPdfLink | Attention | NO_PDF_LINK | | AttachFile |
| NotFound NoOaCopy, NotHeld, Timeout, AccessDenied | Attention | NOT_FOUND (Timeout -> TIMED_OUT) | | AttachFile |
| NotEntitled | Attention | NOT_ENTITLED | | OpenBrowser(libkey / doi), AttachFile |
| NeedsUser why | Attention | LoginDeclined LOGIN_DECLINED (+SignIn), LoginTimeout LOGIN_TIMEOUT, LoginRefused LOGIN_REFUSED, LoginIneffective LOGIN_INEFFECTIVE, Misconfigured MISCONFIGURED, Challenge CHALLENGE, NoPdfLink NO_PDF_LINK, EngineRestarted ENGINE_RESTARTED, HandoffRequested NEEDS_USER, GroupLibrary GROUP_LIBRARY, NetworkDown NETWORK_DOWN | | OpenBrowser(redirector / ezproxy / libkey / doi from the Handoff), AttachFile, Retry |
| Blocked | Attention | BLOCKED | host | OpenBrowser(from handoff), AttachFile |
| Deferred cap | Attention | CAP_REACHED | cap = `hourly` / `daily` / `mb_per_hour` / `batch` | Retry |
| Halted | Attention | HALTED | | OpenBrowser(from handoff), Retry |
| Cancelled | Cancelled | CANCELLED | | |
| Failed code | Failed | FAILED | code | Retry |

Tests (native): `outcome_row` for every variant (exhaustive `match` so a new variant fails to compile); async tests
with `SimHost` + `ScriptResponder` from fixtures: OA happy path (OpenAlex found + PMC S3 PDF -> `SavePdf` ->
Saved), discovery settle early cancels stragglers, throttled retry once, hedge starts at 3 s on a different host
and not on the same host, challenge -> browser tier with plain UA pinned, PDF URL serving HTML landing -> extract
-> next candidate, truncated/non-PDF bytes rejected, translators timeout ignored, save `AlreadyHasPdf` ->
`Skipped(PdfAddedMeanwhile)`, cancel mid-fetch emits `Cancel` + `Release`, no-DOI items (PMID/arXiv only,
`NoIdentifier`). Use fixtures from `fixtures/oa` (parse results are canned `Inspection` values; parsing itself is
core's job).

## Acceptance
```
cargo test -p fulltext-engine pipeline:: l10n::
cargo clippy -p fulltext-engine --all-targets -- -D warnings
cargo check -p fulltext-engine --target wasm32-unknown-unknown
```
