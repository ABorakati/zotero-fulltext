# core-integration: cross-module acceptance, totality, fixtures through the real wasm

Package id `core-integration`. Implementer: Sonnet. Depends on: `core-ids-oa`, `core-page`, `core-pdf-urls-inspect`, `core-wasm-xtask`
(all four merged). Do not start before they are; this package only adds tests and fixes small cross-module seams.
Read first: `docs/design/core.md` (all), the four sibling specs in `docs/wp/core-*.md`, `docs/design/contracts/model.rs`,
`fixtures/html/expected.json`, `fixtures/oa/manifest.json`.

## Files you own
`crates/fulltext-core/tests/e2e.rs`, `crates/fulltext-core/tests/totality.rs`, `tests/js/core-wasm-fixtures.test.mjs`.
You may make SMALL fixes (under 30 lines each) in other core files when a cross-module seam is wrong, but only after writing the failing
test, and you must list each such fix in your final message (file, line, reason). Larger problems: report, do not rewrite.
Environment: `CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\core-integration`. Use `mod common;` from `crates/fulltext-core/tests/common/mod.rs`.

## 1. tests/e2e.rs (native, exactly what the wasm does)
Always go through the wire form: build an `InspectMeta`, `serde_json::to_vec` it, parse it back, call `fulltext_core::inspect::inspect`, serialize the `Inspection`, parse it back and compare equal. A helper `run(spec, resp, body) -> Inspection` does that.
1. **Every API fixture** (the `fixtures/oa` files with a parser, see `core-ids-oa` section 2 table) through `InspectSpec::Api { source, requested }` with `requested` built by `ids::normalise` from the case DOI: result is `Inspection::Api` with the expected status class (found / empty / not_found / throttled / unparseable per manifest status and case name).
2. **Source pools.** For each case (`gold`, `green`, `arxiv`, `medrxiv`, `paywalled`) merge the candidates of every available source fixture with `rank::merge`, `rank::rank` them, and assert the ordering invariants: tiers non-decreasing, PMC S3 before Europe PMC `getPdf` before arXiv before publisher before landing; `gold` first candidate is the PMC S3 PDF; `arxiv` has `arxiv.org/pdf/1706.03762` before the `10.65215/...` mirror; `paywalled` has no `Pdf` candidate with tier below `LANDING` except aggregator false positives (CORE) which rank last; no duplicate `canonical_key`.
3. **Every HTML fixture** through `Page` spec: same oracle as `page_fixtures.rs` (top candidate in expected/alternates, none in `must_not_select`, class per `kind`). Through `Pdf` spec: HTML bodies come back as `Inspection::Page`, never as a bare `Pdf` verdict. Through `Probe`: the class equals the class inside the `Page` result. Synthetic PDF bytes (`%PDF-1.7 ... %%EOF`, also with a 3-byte BOM prefix and with `head_len` staging) through `Page`, `Pdf` and `Probe` specs: `Inspection::Pdf` / `Probe { class: Pdf { junk_prefix } }` with the right prefix.
4. **login_state end to end** from raw strings: Entra sign-in URL -> `Pending`; `go.openathens.net/redirector/kcl.ac.uk?...` -> `Pending`; the EZproxy `Shibboleth Authentication Request` page from `tests/data/page` (core-page) at the login host -> `Pending`; a Cloudflare managed-challenge page -> `ChallengeShown`; an Entra error page containing `AADSTS53000` -> `Refused { code: Some("AADSTS53000") }`; a publisher landing fixture URL + HTML -> `Landed`; a synthetic PDF landing -> `Landed`.
5. **Response-shape rule** table from `inspect.rs` docs: one assertion per cell.

## 2. tests/totality.rs (no panic, ever)
Seeded xorshift (no `rand` crate). For every fixture body in `fixtures/html` and `fixtures/oa` and for every spec variant (every `Source` for `Api`, `Page`, `Pdf`, `Probe`, `None`, with `Route::Oa` and `Route::Inst`): run `inspect` inside `std::panic::catch_unwind` on the original, on truncations (0, 1, 7, 100, len/2, len-1 bytes), on 10 random single-byte flips, on 10 random byte-range deletions, with NUL / `0xFF` / `<` fills, with a UTF-16LE re-encoding (BOM + text), and with `resp` mutated (status 0, 99, 999; empty `final_url`; garbage `final_url`; `body_len` smaller and larger than the body; `head_len` larger than the body; headers containing 100 KB values). Also hostile synthetic bodies: 2 MiB of `<`, 2 MiB of `<a href=`, 200 000 nested `<div>`, a 1 MiB attribute value, 2 MiB of NULs, a 5 MiB body (over the 2 MiB text cap), lone surrogates as invalid UTF-8, a million `&` entities. Every call must (a) not panic, (b) return a value that serializes to JSON and round-trips, (c) finish within 2 s in debug builds (use `std::time::Instant`, fail with the case name). Keep total runtime under 60 s in debug: sample mutations rather than enumerating when the fixture count makes it exceed that.
Also test JSON-level robustness of `InspectMeta` parsing: unknown fields ignored, missing optional fields defaulted, wrong types rejected as an error value (not a panic).

## 3. tests/js/core-wasm-fixtures.test.mjs (the real `.wasm`)
Self-contained ESM `node:test` file (copy the tiny `instantiate()` helper from `tests/js/abi.test.mjs`; do not import across test files). Requires `plugin/content/fulltext.wasm` (`cargo xtask build`).
* Load `fixtures/html/expected.json` and apply the same oracle as the native test through `fx_inspect` with `{"expect":"page",...}` and the capture-time status/headers (re-implement the small status table from `tests/common/mod.rs` in JS).
* Load `fixtures/oa/manifest.json`; for 10 representative files (one per source, including a 404 HTML body and a 200-"not found" body) assert the envelope is `ok`, `expect == "api"`, and `status` matches the manifest case semantics.
* Binary path: build a 5 MB synthetic PDF in JS, stage `head(1024) ++ tail(1024)` with `head_len = 1024`, `body_len = 5_000_000`, and assert `expect == "pdf"`, `is_pdf`, `has_eof`, `len == 5000000`; a BOM-prefixed PDF reports `junk_prefix == 3`.
* `fx_login_state` for 6 representative URL/HTML pairs.
* Performance guard: all 31 HTML fixtures inspected in under 3 s total on this machine (generous; print the number).
* Memory guard: 500 fixture inspections do not grow `memory.buffer.byteLength` after warm-up.

## 4. Whole-area checks (run, fix or report)
* `cargo xtask build` size line: raw size under 450 KB with the real engine (warn) / 700 KB (fail). Report the number.
* `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` (if the engine area is still broken, run them with `-p fulltext-core -p fulltext-wasm -p xtask` and say so).
* No leftovers: `git grep --no-index -n -E "SKELETON|not implemented:|TODO|FIXME|unimplemented|todo!" -- crates/fulltext-core crates/fulltext-wasm xtask` prints nothing.
* Public API still matches `docs/design/core.md` section 3.1 (names and signatures); list any divergence in your final message so the doc can be corrected.

## 5. Acceptance
```
set CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\core-integration
cargo test -p fulltext-core --test e2e --test totality
cargo xtask build
node --test tests/js/core-wasm-fixtures.test.mjs tests/js/abi.test.mjs
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p fulltext-core -p fulltext-wasm --target wasm32-unknown-unknown -- -D warnings
cargo test --workspace
git grep --no-index -n -E "SKELETON|not implemented:|todo!|unimplemented!" -- crates/fulltext-core crates/fulltext-wasm xtask
```
The last command must print nothing.
