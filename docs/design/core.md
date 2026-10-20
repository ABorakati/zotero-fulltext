# core: workspace, fulltext-core, fulltext-wasm, xtask

Status: skeleton complete (compiles, `cargo test -p fulltext-core` green, wasm builds, Node ABI smoke test green).
Contracts: `docs/design/contracts/model.rs` and `proto.rs` are frozen. Background: `research/SUMMARY.md`
(cited below as S.a .. S.h), `docs/design/judgements.json`. Work packages: `docs/wp/core-*.md`.

## 1. What this area owns

| path | role |
|---|---|
| `Cargo.toml`, `.cargo/config.toml`, `clippy.toml`, `.gitignore` | workspace, lints, `cargo xtask` alias |
| `crates/fulltext-core` | pure, total, I/O-free logic (rlib, native-tested, fuzzable) |
| `crates/fulltext-wasm` | the only crate with `#[no_mangle]`; hand ABI, zero imports (cdylib only) |
| `xtask` | `build`, `test`, `xpi`, `harness` |
| `tests/js/abi.test.mjs` | Node smoke test of the real `.wasm` |

The engine (`crates/fulltext-engine`) and the JS plugin belong to other areas. The engine depends on
`fulltext-core` as a plain rlib: it calls `urls::*`, `ids::*`, `rank::rank`, `oa::*` natively. Only
`fx_inspect` / `fx_login_state` are reached through wasm, because JS owns the bytes.

```
          JS host (I/O, Zotero)                          wasm32 module (no imports)
  response bytes + RespMeta ──fx_inspect──▶ fulltext-wasm ─▶ fulltext_core::inspect::inspect ─▶ Inspection
  window URL / html        ──fx_login_state▶              ─▶ fulltext_core::inspect::login_state ─▶ LoginState
  StepIn {commands,completions} ──fx_step─▶              ─▶ fulltext_engine::Engine::step ─▶ StepOut {effects}
```

## 2. Decisions taken here (additions to the frozen architecture)

1. **`oa::*::parse` takes `&RespMeta`, not a bare status.** `parse(resp, body, requested) -> ApiInspection`.
   Reason: 429/503 `Retry-After` and the final URL (S3 key, PMC id) live in the response facts. `oa::gate` does the
   status-first mapping (404/410 NotFound, 429 Throttled, other non-2xx Unparseable) before any body parsing,
   because error bodies are HTML, XML or text (S.d "Errors are often not JSON").
2. **Crossref is reported as `Source::Doi`.** The frozen `Source` enum has no Crossref variant and adding one could
   break exhaustive matches in the engine. `Source::Doi` therefore means "DOI-registry metadata". If the engine ever
   wants a distinct trace label, add `Source::Crossref` additively (owner: core-ids-oa) and re-run `cargo check`.
3. **`login_state` rides on an ADDITIVE export `fx_login_state`.** The pinned ABI has only `fx_inspect`, and `Inspection`
   has no login variant. The additive export takes UTF-8 `url` and optional `html` and returns `Envelope<LoginState>`.
   The same classification is also reachable as `InspectSpec::Probe` (which returns `PageClass`).
4. **Response-shape rule** (`inspect.rs` module docs): the returned `Inspection` describes what the bytes ARE, not what
   was expected. `Page` spec + PDF bytes gives `Inspection::Pdf`; `Pdf` spec + HTML bytes gives `Inspection::Page`
   (so a challenge or login page is classified, not merely "not a PDF"). The engine must accept both for both specs.
5. **Institution knowledge is built in.** `InspectMeta` carries no `Institution`, so `page::classify` and
   `urls::url_class` hard-code the KCL values (login host `login.kcl.idm.oclc.org`, suffix `kcl.idm.oclc.org`,
   `go.openathens.net`, `login.microsoftonline.com`, `login.live.com`, `login.openathens.net`, `kclidp.kcl.ac.uk`)
   plus generic patterns (any host ending `.idm.oclc.org` with path `/login`, `/Shibboleth.sso/`, SAML form fields).
   Pure helpers that need config (`proxify`, `unproxify`, `handoff`) take it as arguments (`ProxyConf`, `HandoffConf`).
6. **`<noscript>` content is invisible to the extractor by construction.** html5gum with `naively_switch_states(true)`
   treats `<noscript>` as raw text, like a browser with scripting on. LWW, ScienceDirect and Europe PMC put a
   false-positive `<meta refresh>` there (S.e). Zotero's own DOMParser path does the opposite (S.a), so keep it this way.
7. **Tiers are numbers in `rank::tier`** (1 PMC S3, 2 Europe PMC getPdf, 3 arXiv, 4 rxiv early, 5 publisher direct,
   6 repository direct, 7 landing, 8 browser only, 9 paid API) and mirror S.d "Phase 2". The engine sorts by
   (`tier`, `-score`); `Candidate.tier == 0` means "unranked".
8. **Totality contract.** No function in `fulltext-core` or `fulltext-wasm` may panic: workspace clippy denies
   `unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `string_slice`, `unreachable`, `todo`, `unimplemented`
   (`clippy.toml` re-allows them inside tests; integration-test files add a file-level `#![allow]`). The release profile
   is `panic = "abort"`, so a panic poisons the wasm instance. Bad input becomes a value:
   `ApiStatus::Unparseable`, `PageClass::Html`, `Inspection::Failed { reason }`, `Envelope::Err`.
9. **`#![forbid(unsafe_code)]` in `fulltext-core`.** The only `unsafe` in the area is `crates/fulltext-wasm/src/abi.rs`.
10. **MSRV 1.88** (let chains, `is_none_or`); built and tested with Rust 1.96.

## 3. fulltext-core

### 3.1 Module map

| module | public surface (all total) | package |
|---|---|---|
| `model` | frozen contract, verbatim copy of `contracts/model.rs` | (frozen) |
| `ids` | `normalise(&ItemFields) -> Ids`, `clean_doi/pmid/pmcid/arxiv`, `arxiv_from_doi`, `is_rxiv_doi`, `extra_field`, `apply_learned`, `same_doi` | core-ids-oa |
| `oa` | `parse(Source, &RespMeta, &[u8], &Ids) -> ApiInspection`; per source `openalex`, `europepmc`, `biorxiv`, `pmc_s3`, `arxiv`, `unpaywall`, `semantic_scholar`, `core_api`, `crossref`, `alma`, `zotero_oa`; helpers `gate`, `empty`, `unparsed`, `version_from_str`, `host_kind`, `s3_to_https` | core-ids-oa |
| `rank` | `new_candidate`, `merge`, `score`, `rank`, `RankPrefs`, `tier::*` | core-ids-oa |
| `page::html` | `parse(&str) -> Doc` (title, metas, links, anchors, frames, forms, inputs, scripts, refreshes) | core-page |
| `page::classify` | `classify(&Facts) -> PageClass` | core-page |
| `page::extract` | `extract(&PageCtx) -> Extraction`, `page_ids` | core-page |
| `page::rules` | `RULES`, `rule_for(host_key)` | core-page |
| `page` | `inspect_page(&RespMeta,&Ids,Route,&[u8]) -> PageInspection`, `classify_response(&RespMeta,&[u8]) -> PageClass` | core-page |
| `pdf` | `sniff(head, tail, len) -> PdfVerdict`, `verdict(&RespMeta,&[u8])` | core-pdf-urls-inspect |
| `urls` | `parse_http`, `host_of`, `host_key`, `resolve`, `canonical_key`, `percent_encode_component`, `percent_decode`, `doi_url`, `redirector_url`, `ezproxy_login_url`, `libkey_url` (all implemented); `proxify`, `unproxify`, `url_class`, `strip_tracking`, `handoff` (to do) | core-pdf-urls-inspect |
| `text` | `decode`, `head`, `tail`, `contains_ci`, `MAX_TEXT_BYTES` (implemented) | core-pdf-urls-inspect |
| `inspect` | `inspect(&InspectMeta,&[u8]) -> Inspection`, `login_state(&str,Option<&str>) -> LoginState` | core-pdf-urls-inspect |

Dependency direction (no cycles): `model, text` <- `ids, urls` <- `oa, rank, page, pdf` <- `inspect`.
Skeleton-implemented helpers that several packages rely on and that must keep their signatures: `ids::clean_doi`,
`urls::{parse_http, host_of, host_key, resolve, canonical_key, percent_*}`, `oa::{gate, empty, unparsed}`,
`rank::new_candidate`, `text::*`, `page::html::*`.

### 3.2 Pipelines, in the order the engine uses them

1. **Phase 0.** `ids::normalise(&snapshot.fields)` gives `Ids` (DOI from field, Extra, URL or archive id; PMID/PMCID;
   arXiv incl. `10.48550` DOIs; `rxiv` for `10.1101/`). The engine builds request URLs from `Ids`.
2. **Phase 1 (parallel APIs).** JS performs each GET, stages the body, calls `fx_inspect` with
   `InspectSpec::Api { source, requested }`. `oa::parse` returns `ApiInspection { status, learned, candidates, oa_status,
   note, retry_after_s }`. `ids::apply_learned` folds `learned` ids into `Ids` (never overwriting).
3. **Phase 2 (rank).** `rank::merge` pools candidates from all sources and the page extractor; `rank::rank` dedupes
   (canonical URL INCLUDING query), maps aliases, scores, assigns `tier`, sorts.
4. **Attempt.** `InspectSpec::Page` for landing pages (`page::inspect_page`: classify, then extract for
   `Landing`/`Html`/`ReaderShell`), `InspectSpec::Pdf` for direct files (`pdf::verdict`), `InspectSpec::Probe` for
   no-follow probes (`page::classify_response`). `PageClass::Redirect { next }` tells the engine to follow.
5. **Institutional.** `urls::redirector_url` / `ezproxy_login_url` / `proxify` build entry URLs; `urls::unproxify`
   recovers the real URL to store; `urls::handoff` builds the "open in browser" links; the login window polls
   `login_state`.

### 3.3 Publisher rules and generic extraction

Order (S.e): publisher rule > `citation_pdf_url` (name-suffix match) > `link[rel=alternate][type=application/pdf]` >
other head metas > JSON state > scored anchors > frames > interstitial redirects. Rules match on
`urls::host_key(host)` of the UNPROXIED host, and also on its proxified form by prefix. Rejections are reported, not
silently dropped (`PageInspection.rejected`), so the failure catalogue can assert them. Acceptance oracle:
`fixtures/html/expected.json` (31 pages; 16 are archived, so tests assert structure).

## 4. fulltext-wasm ABI v1

Exports (all `u32`, UTF-8 JSON, results `(ptr << 32) | len` as BigInt in JS, JS frees every result with `fx_free`):

| export | contract |
|---|---|
| `fx_abi_version() -> u32` | 1 |
| `fx_alloc(len) -> ptr` | `Layout{len,1}`; `len == 0` returns a dangling non-null; `0` only on OOM |
| `fx_free(ptr, len)` | no-op for `ptr == 0`, `len == 0`, or the static panic buffer |
| `fx_inspect(meta_ptr, meta_len, body_ptr, body_len) -> u64` | `InspectMeta` JSON + raw body; `Envelope<Inspection>`; callee frees both inputs |
| `fx_step(in_ptr, in_len) -> u64` | `StepIn` JSON; `Envelope<StepOut>`; callee frees the input; engine is a `thread_local RefCell<Option<Engine>>` |
| `fx_reset()` | drops the engine; the next `fx_step` calls `Engine::new()` |
| `fx_last_panic() -> u64` | `{"code":"panic","message":...}` of the last panic or len 0; allocation-free, valid after a trap |
| `fx_login_state(url_ptr,url_len,html_ptr,html_len) -> u64` | ADDITIVE; `Envelope<LoginState>`; `html_len == 0` = no document |

Rules: JS writes only into buffers from `fx_alloc` and re-creates its `Uint8Array` views after every call (memory
may grow); every export returns an envelope and never traps on bad input; panics install a hook lazily (first export
call) that stores the message for `fx_last_panic` before the abort trap; JS marks the instance poisoned on
`WebAssembly.RuntimeError`, re-instantiates the cached module, and fails the current run if it was the engine
instance (no journal replay in v1). `xtask` and `tests/js/abi.test.mjs` assert: zero imports, exports are exactly the
documented set plus `memory`, `__data_end`, `__heap_base`; steady-state calls do not grow memory.

Code layout: `json.rs` (bytes in, bytes out, native unit tests), `panic_hook.rs`, `abi.rs` (wasm32 only, the only
`unsafe`). Size at skeleton stage: 305 KB raw with the engine skeleton; budget for the finished build is about
420 KB raw / 150 KB gzip (S.f: core 308 KB with html5gum and `idna_adapter` 1.0.0, plus engine and futures-util).

## 5. xtask

`cargo xtask build` : `cargo build -p fulltext-wasm --release --target wasm32-unknown-unknown` with
`CARGO_ENCODED_RUSTFLAGS` remapping home, cargo, rustup and workspace paths (no `C:\Users\...` in the binary), output
located via `cargo metadata` (the user's global config sets `CARGO_TARGET_DIR` and the `kache` wrapper), copied to
`plugin/content/fulltext.wasm`. `test` runs `cargo test --workspace`, `build`, `node --test tests/js`.
`xpi` builds then writes a deterministic zip (sorted entries, 1980-01-01 timestamps, deflate 9) to
`dist/zotero-fulltext-<version>.xpi`, version from `plugin/manifest.json`. `harness` is a stub until core-wasm-xtask
wires `tools/zotero-harness/run.ps1` (throwaway profile only).

## 6. Testing strategy

| layer | where | what |
|---|---|---|
| unit | `#[cfg(test)]` in each module | small vectors |
| contract | `tests/model_contract.rs` | wire shapes of the model types |
| fixtures | `tests/oa_fixtures.rs`, `tests/page_fixtures.rs`, `tests/classify.rs` | every `fixtures/oa` and `fixtures/html` file through the real parser, structural assertions (`tests/common` loads body + status + URL) |
| vectors | `tests/urls.rs`, `tests/ids.rs`, `tests/rank.rs`, `tests/pdf.rs` | proxify / unproxify (research/04 6.3-6.4), DOI forms, ranking cases, PDF sniffing |
| e2e | `tests/e2e.rs`, `tests/totality.rs` | `inspect()` on every fixture with every spec; random/mutated bytes never panic |
| wasm | `crates/fulltext-wasm` unit tests, `tests/js/abi.test.mjs`, `tests/js/abi-recovery.test.mjs`, `tests/js/core-wasm-fixtures.test.mjs` | envelopes, ABI, trap recovery, fixtures through the real module |

Commands: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo clippy -p fulltext-core -p fulltext-wasm --target wasm32-unknown-unknown -- -D warnings`, `cargo xtask build`,
`node --test tests/js`, `cargo xtask xpi`. Parallel agents set `CARGO_TARGET_DIR=...\target\<package-id>`.

## 7. Work packages

| id | files | depends on | deliverable |
|---|---|---|---|
| core-ids-oa | `ids.rs`, `oa/**`, `rank.rs`, `tests/{ids,oa_fixtures,rank}.rs` | none | identifiers, 11 parsers, ranking |
| core-page | `page/**`, `tests/{page_fixtures,classify}.rs` | none | tokenizer, classifier, extractor, rules |
| core-pdf-urls-inspect | `pdf.rs`, `urls.rs`, `text.rs`, `inspect.rs`, `tests/{pdf,urls,inspect}.rs` | none (logic); `inspect` rules reference page/oa behaviour | PDF sniff, proxy URLs, dispatch rules, `login_state` |
| core-wasm-xtask | `crates/fulltext-wasm/**`, `xtask/**`, `tests/js/abi.test.mjs`, `tests/js/abi-recovery.test.mjs` | none | ABI hardening, `debug-panic` trap-recovery build, xtask wasm verification, `check`, harness command |
| core-integration | `crates/fulltext-core/tests/{e2e,totality}.rs`, `tests/js/core-wasm-fixtures.test.mjs` | the four above | cross-module acceptance, totality fuzzing, fixtures through the real wasm, size budget, final clippy on both targets |

## 8. Hand-offs to other areas

* **Engine:** accept both `Inspection::Pdf` and `Inspection::Page` for `Page` and `Pdf` specs (decision 4); read the
  published DOI of a preprint from `ApiInspection.note` (`published_doi:<doi>`); use `rank::tier::*` as the attempt order;
  call `urls::*` and `ids::*` directly (rlib). `EngineError::to_err_body()` is what `fx_step` serializes.
* **JS glue:** instantiate the same module twice; copy result bytes out before the next call; call `fx_free(ptr,len)` for
  every result, including `fx_last_panic`; map BigInt results with `Number(p >> 32n)`, `Number(p & 0xffffffffn)`; use
  `fx_login_state` for the login window poll; evaluate `BrowserReq.hold_while` with a tiny host matcher or via
  `fx_login_state(url)` (`Pending` means still in SSO); no other exports exist (debug exports only in test builds).
* **Packaging:** `cargo xtask xpi` expects `plugin/manifest.json` with a `version`.
