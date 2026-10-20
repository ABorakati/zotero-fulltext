# 07 — Rust → WebAssembly toolchain spike (measured on this machine)

Date: 2026-10-20. Machine: Windows 11, 16 logical CPUs. Spike (throwaway):
`C:\Users\abora\zotero-fulltext\spikes\wasm-spike\`.

Confidence key: **high** = verified in source or measured here; **medium** = official docs / a shipping
plugin's code; **low** = inferred.

---

## 0. Bottom line

| Decision | Recommendation | Evidence |
|---|---|---|
| Core language | **Rust** (Zig is viable but bigger for the same JSON job and has no HTML/WHATWG-URL story) | §8 |
| Target | `wasm32-unknown-unknown`, `cdylib`, **no imports** (verified on every variant) | §1, §3 |
| Crate layout | **Split**: `fulltext-core` (rlib, all logic, native tests) + thin `fulltext-wasm` (cdylib, only `extern "C"` exports). Cargo drops `-C lto` for a crate that is both `cdylib` and `rlib` (+16% size) | §1.3 |
| Profile | `opt-level="z"`, `lto=true`, `codegen-units=1`, `panic="abort"`, `strip=true` ("s" is 7% bigger, 3 is 28% bigger) | §1.2 |
| URL crate | Keep `url` but **pin `idna_adapter` to 1.0.0** in `Cargo.lock` (−152 KB raw / −49 KB gzip, −44%; non-ASCII hostnames are rejected, fine for publisher/EZproxy hosts) | §1.4 |
| HTML parser | **html5gum 0.8.4** (WHATWG tokenizer, 0 mismatches on the messy fixture, +126 KB raw / +38 KB gzip). Fallback if size matters: the 185-line hand-rolled scanner (+8 KB, fastest, 0 mismatches, but not spec-backed). Reject tl (wrong on unquoted attributes and script contents), lol_html (+388 KB, no entity decoding), scraper (+524 KB, slowest) | §2 |
| ABI | **Hand-written ABI** (alloc/free + `(ptr,len)` UTF-8 JSON in, packed `u64` out), plus a **two-buffer raw variant for HTML** (2–2.3× faster than putting the HTML inside JSON). Do not use wasm-bindgen glue in the plugin sandbox | §3 |
| Panics | Must never happen. Return `{"ok":…}` / `{"err":{code,message}}`. A **panic hook writes the message to a static buffer**; JS reads it after the trap (`fx_last_panic`), marks the instance poisoned and re-instantiates from the cached `WebAssembly.Module` | §4 |
| Native tests | Same crate runs `cargo test` on `x86_64-pc-windows-msvc` (11 tests pass); clean build+test 17 s | §5 |
| Build | `cargo xtask dist` (Rust only, zip crate with zlib-rs): build wasm → stage → reproducible XPI (same SHA-256 on rebuild). Also **remap path prefixes**, otherwise the shipped .wasm contains `C:\Users\<name>\...` | §6 |
| wasm-opt | **Not worth a required step**: −12…−15% raw but only −0.4…−4% gzip (one variant +4%); the XPI is deflated anyway | §7 |

---

## 1. Crate, profile and sizes

### 1.1 Toolchain actually used (high)

- rustc/cargo 1.96.0 (LLVM 22.1.2), targets `wasm32-unknown-unknown`, `wasm32-wasip1`, `x86_64-pc-windows-msvc`.
- `rustc --print cfg --target wasm32-unknown-unknown` → `panic="abort"` (the target default; unwinding needs nightly `-Zbuild-std` + wasm EH) and target features `bulk-memory multivalue mutable-globals nontrapping-fptoint reference-types sign-ext`. Gecko 140 ESR supports all of these.
- Zotero 10.0.6 runs Gecko **140.17.0 ESR** (`C:\Program Files\Zotero\platform.ini`: `Milestone=140.17.0`).
- Locked deps: serde 1.0.229, serde_json 1.0.154, url 2.5.8, idna 1.1.0, idna_adapter 1.2.1 (default), tl 0.7.8, lol_html 3.0.1, html5gum 0.8.4, scraper 0.27.0 (html5ever 0.39.0), zip 9.0.5 (flate2 1.1.10 + zlib-rs 0.6.8), wasm-bindgen 0.2.126.
- The user's global `~/.cargo/config.toml` sets `rustc-wrapper = "kache"` and `CARGO_TARGET_DIR=C:\dev\cache\cargo-target`. All measurements set `RUSTC_WRAPPER=""` and a spike-local target dir (see `measure.sh`).

### 1.2 Workspace `Cargo.toml` (as built)

```toml
[workspace]
resolver = "3"
members = ["crates/core", "crates/ladder", "crates/bindgen-demo", "xtask"]

[workspace.lints.clippy]          # wasm build is panic=abort: forbid panic sources in lib code
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
indexing_slicing = "warn"         # (add: string_slice = "deny", unreachable/todo/unimplemented = "deny")
unreachable = "deny"
todo = "deny"
unimplemented = "deny"

[profile.release]                 # shipped profile
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true
debug = false
incremental = false

[profile.release-s]               # comparison only
inherits = "release"
opt-level = "s"

[profile.release-o3]              # comparison only
inherits = "release"
opt-level = 3
```

`crates/core/Cargo.toml` (one feature per HTML backend so each can be measured alone):

```toml
[lib]
crate-type = ["cdylib", "rlib"]   # see 1.3: build the wasm with `cargo rustc --crate-type cdylib`
[features]
default = ["hand"]
hand = []
tl = ["dep:tl"]
lolhtml = ["dep:lol_html"]
html5gum = ["dep:html5gum"]
scraper = ["dep:scraper"]
debug-panic = []                  # exports fx_debug_panic (spike only)
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
url = "2.5"
tl = { version = "0.7.8", optional = true }
lol_html = { version = "3.0.1", optional = true }
html5gum = { version = "0.8.4", optional = true }
scraper = { version = "0.27.0", optional = true, default-features = false }
```

Build command used for every wasm number below:

```bash
cargo rustc -p fulltext-core --lib --crate-type cdylib --release \
  --target wasm32-unknown-unknown --no-default-features --features <backend>
```

### 1.3 Finding: `crate-type = ["cdylib","rlib"]` silently disables LTO (high)

`cargo build -v` on the dual-type crate passes `--crate-type cdylib --crate-type rlib -C opt-level=z -C strip=symbols`
**without `-C lto`**; the cdylib-only build (`cargo rustc --crate-type cdylib`) passes `-C opt-level=z -C lto -C strip=symbols`.
Result: core-hand **395,628 B (dual) vs 341,635 B (cdylib only)**, gzip 140,312 vs 123,549.
→ Production layout: `fulltext-core` = rlib (logic + native tests), `fulltext-wasm` = cdylib depending on it (the
`bindgen-demo` crate in the spike is exactly this shape and got the same small size).

### 1.4 Size ladder (opt-level "z", LTO, stripped; bytes; gzip -9)

| Variant | What is in it | raw | gzip |
|---|---|---:|---:|
| `ladder-abi-only` | `fx_alloc`/`fx_free`/echo, std + dlmalloc | 13,162 | 5,571 |
| `ladder-json` | + serde_json derive round-trip | 61,728 | 27,012 |
| `ladder-json-url` | + `url::Url::parse/join` (idna_adapter 1.2 = ICU4X) | 275,302 | 100,726 |
| `ladder-json-url` idna_adapter **1.0.0** | IDNA stub | 123,435 | 51,560 |
| `ladder-json-url` idna_adapter 1.1.0 | unicode-rs tables | 376,159 | 159,059 |
| `core-none` | real logic (DOI, EZproxy rewrite, Unpaywall ranking, candidate scoring, PDF sniff), no HTML parser | 333,937 | 120,775 |
| `core-hand` | + hand-rolled HTML scanner | 341,635 | 123,549 |
| `core-hand` + panic hook (§4) | | 343,101 | 124,360 |
| `core-hand` idna 1.0.0 | | **189,905** | **74,854** |
| `core-html5gum` | html5gum backend | 460,100 | 158,860 |
| `core-html5gum` idna 1.0.0 | | **308,190** | **109,656** |
| `core-hand` opt-level "s" | | 367,102 | 131,294 |
| `core-hand` opt-level 3 | | 437,571 | 155,526 |

Pinning: `cargo update -p idna_adapter --precise 1.0.0` (documented in the idna_adapter README: 1.2.x = ICU4X,
1.1.x = unicode-rs, 1.0.x = stub that "rejects non-ASCII domain name inputs"). (high: measured; README in
`~/.cargo/registry/src/index.crates.io-*/idna_adapter-1.2.1/README.md`.) Alternative: keep URL resolution in JS
(`URL` is a sandbox global) and drop `url` entirely (−~210 KB); not recommended because native tests would then
not cover URL joining.

Where the bytes go in core-hand (unstripped build + `js/wasm-top.mjs`, name-section attribution): code 231 KB
(url 25 KB, core 23 KB, fulltext_core 19 KB, serde_json ~20 KB, idna 12 KB, float formatting `flt2dec` ~13 KB pulled in
by `core::fmt` of f64, dlmalloc 5 KB), data 160 KB (mostly ICU4X normalizer/properties tables; 26 KB with idna 1.0).

### 1.5 Build times (high)

| Step | Time |
|---|---:|
| Clean `cargo rustc … --release --target wasm32-unknown-unknown` (core-hand, all deps) | 20 s |
| Incremental after touching `logic.rs` | 6 s |
| Clean native `cargo test -p fulltext-core` (x86_64-pc-windows-msvc) | 17 s |
| `cargo xtask dist` (incremental wasm + zip) / no-op | 9.3 s / 0.95 s |
| Full `measure.sh` (19 variants + wasm-opt) | 4 min 14 s |

---

## 2. HTML parsing options (all compile for wasm32-unknown-unknown with NO imports)

Each backend fills the same struct (`crates/core/src/html/mod.rs`): `metas{name|property, content}`,
`links{rel,type,href}`, `anchors{href, text}`, `iframes[src]` (+frame/embed), `base_href`.

### 2.1 Messy fixture (`fixtures/messy.html`, test `crates/core/tests/messy.rs`)

Covers: uppercase tags/attrs, reversed attribute order, unquoted values (`name=citation_doi content=10.1234/abc.def`,
`HREF=relative/full.pdf`), `&amp;` in attribute URLs, a commented-out `citation_pdf_url`, fake `<a>` inside
`<script>` and `<textarea>`, nested `<b>` in link text, `&nbsp;`, `&eacute;`/`&#233;`/`&#x2014;`, `<base href>` after links,
`javascript:` hrefs, `<a>` without href.

| Backend | Compiles (wasm32-u-u) | Size Δ vs core-none (raw / gzip) | Messy fixture | Notes |
|---|---|---:|---|---|
| hand-rolled (185 lines) | yes, 0 deps | **+7.7 KB / +2.8 KB** | **0 mismatches** | 30-entry named-entity table + numeric refs; treats `<noscript>` content as markup (like DOMParser with scripting off) |
| tl 0.7.8 | yes | +22.6 KB / +9.4 KB | **4 mismatches**: drops unquoted attribute values (citation_doi, `HREF=relative/full.pdf`), tokenizes inside `<script>` and `<textarea>` (2 fake anchors) | no entity decoding; on real pages misses `<link>`s (BMC 2 of 9, Frontiers 13 of 42) |
| html5gum 0.8.4 | yes (dep: jetscii) | +126 KB / +38 KB | **0 mismatches** | needs `DefaultEmitter::naively_switch_states(true)` so script/style/textarea are raw text; full WHATWG named-entity table (bulk of the size); README: passes html5lib tokenizer tests, no unsafe |
| lol_html 3.0.1 | yes (cssparser, selectors, encoding_rs, hashbrown…) | +388 KB / +207 KB | 0 mismatches **only with an external entity decoder** | `get_attribute` and text chunks are NOT entity-decoded (lol_html `src/rewritable_units/tokens/text_chunk.rs:32-36`, `attributes.rs:105`); end-tag handlers must be `'static` (Rc<RefCell>) |
| scraper 0.27 (html5ever 0.39) | yes (`default-features=false`; no getrandom problem) | +524 KB / +180 KB | 0 mismatches | full tree builder; slowest |

### 2.2 Real OA landing pages (fetched once with curl, unauthenticated GET; `fixtures/*.html`)

PLOS Med (10.1371/journal.pmed.1000097, 170 KB), arXiv abs 1706.03762 (43 KB), BMC Med 10.1186/1741-7015-8-18
(→ link.springer.com, 346 KB), PMC2707599 (178 KB), Frontiers 10.3389/fpsyg.2013.00863 (767 KB). (eLife returned a
3 KB bot-challenge page — itself a useful "not a landing page" fixture for the login/challenge detector.)

All backends found the same `citation_pdf_url` on all 5 pages:
`…plosmedicine/article/file?id=…&type=printable`, `https://arxiv.org/pdf/1706.03762`,
`https://link.springer.com/content/pdf/10.1186/1741-7015-8-18.pdf`,
`https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/pdf/pmed.1000097.pdf`,
`https://www.frontiersin.org/journals/psychology/articles/10.3389/fpsyg.2013.00863/pdf`. Candidate lists differ only in
low-score `<noscript>` GTM iframes (hand sees them; spec parsers treat `<noscript>` as raw text).

Native timings (`cargo run --release --example bench --features tl,lolhtml,html5gum,scraper`, ms per parse):

| Page | hand | tl | html5gum | lol_html | scraper |
|---|---:|---:|---:|---:|---:|
| plos-med 170 KB | 1.01 | 2.61 | 3.61 | 1.17 | 6.10 |
| arxiv 43 KB | 0.45 | 1.34 | 1.32 | 0.49 | 2.55 |
| bmc-med 346 KB | 1.99 | 3.83 | 5.63 | 1.67 | 9.31 |
| pmc 178 KB | 1.32 | 2.92 | 4.25 | 1.32 | 7.32 |
| frontiers 767 KB | 4.35 | 6.09 | 10.80 | 2.21 | 14.17 |

In wasm under Node 24 (whole call incl. marshalling, raw two-buffer ABI; JSON ABI in brackets):
hand 0.5–4.7 ms [0.8–11.0], lol_html 1.1–6.9 [1.4–13.0], html5gum 1.5–17.1 [1.7–23.6], scraper (JSON) 5.8–38.5.
A 6 MB page: ~140 ms (hand), ~320 ms (html5gum) → cap input size or run the core in a ChromeWorker for huge pages.

### 2.3 Recommendation

**html5gum** inside the core: spec tokenizer, correct on every messy case, small enough (with idna 1.0 the whole
core is 308 KB raw / 110 KB gzip), one tiny dependency, no unsafe. Code is 31 lines (`html/gum.rs`):

```rust
let mut emitter = html5gum::DefaultEmitter::default();
emitter.naively_switch_states(true); // script/style/textarea/title as raw text
for Ok(token) in html5gum::Tokenizer::new_with_emitter(html, emitter) {
    match token {
        Token::StartTag(t) => /* t.name: HtmlString (lower-case), t.attributes.get(b"href".as_slice()).map(|v| &v.value) */,
        Token::EndTag(t) => …, Token::String(s) => /* decoded text, s.value */, _ => {}
    }
}
```

Keep the hand-rolled scanner as the documented fallback if the XPI must be minimal (whole core 190 KB / 75 KB gzip).
Alternative architecture worth noting: the bootstrap sandbox has `DOMParser` (`plugins.js:148`), so JS could parse
and pass only the extracted `{metas, links, anchors, iframes}` JSON to Rust; it trades native testability of the
extraction step for zero parser bytes.

---

## 3. ABI choice

### 3.1 (a) Hand-written ABI — prototyped fully (high)

Rust side (`crates/core/src/abi.rs`):

```rust
#[unsafe(no_mangle)] pub extern "C" fn fx_abi_version() -> u32            // = 1
#[unsafe(no_mangle)] pub extern "C" fn fx_alloc(len: usize) -> *mut u8    // Vec::with_capacity(len), forgotten
#[unsafe(no_mangle)] pub unsafe extern "C" fn fx_free(ptr: *mut u8, len: usize) // Vec::from_raw_parts(ptr, 0, len)
#[unsafe(no_mangle)] pub extern "C" fn fx_init()                          // installs panic hook
#[unsafe(no_mangle)] pub extern "C" fn fx_last_panic() -> u64             // static buffer, not owned by JS
// JSON in/out: input (ptr,len) borrowed; output Box<[u8]> leaked, returned as (ptr << 32) | len
export_json!(fx_normalize_doi, DoiIn, normalize_doi);
export_json!(fx_proxy_url, ProxyIn, proxy_url);           // {"mode":"hostname"|"prefix", ...}
export_json!(fx_rank_unpaywall, UnpaywallIn, rank_unpaywall);
export_json!(fx_landing_pdf_candidates, LandingIn, landing_pdf_candidates);
// raw variants (no JSON):
pub unsafe extern "C" fn fx_landing_raw(url_ptr, url_len, html_ptr, html_len) -> u64
pub unsafe extern "C" fn fx_sniff_pdf(ptr, len) -> u32
```

Every JSON result is `{"ok": <value>}` or `{"err": {"code": "bad_json"|"bad_url"|"bad_utf8"|…, "message": "…"}}`.
`pack()`/exports are `#[cfg(target_arch = "wasm32")]` (pointers are 64-bit natively); `respond_bytes()` is the
natively testable part.

JS glue (`js/fulltext-core.js`, ~60 lines of code): a **plain script in an IIFE** (no `export`, no top-level
`let/const/class` — Zotero's own `citeprocRsBridge.js:52-61` documents "redeclaration" errors when re-loading a
subscript with top-level `let`). It uses only `WebAssembly`, `BigInt`, `TextEncoder`/`TextDecoder` — all present in the
plugin sandbox (`.ref\omni-app\chrome\content\zotero\xpcom\plugins.js:137-156`). Key points:

```js
const packed = this.x[name](ptr, bytes.length);          // i64 -> BigInt
const outPtr = Number(packed >> 32n), outLen = Number(packed & 0xffffffffn);
new Uint8Array(this.x.memory.buffer, ptr, n)             // ALWAYS re-create views after any call (memory.grow detaches)
enc.encodeInto(html, new Uint8Array(mem, hPtr, html.length * 3)) // raw path: no intermediate copy
```

**Imports: none.** `WebAssembly.Module.imports()` returns `[]` for all 19 Rust variants and both Zig builds
(`node js/wasm-info.mjs out/*.wasm`). Exports: `memory`, the `fx_*` functions, `__data_end`, `__heap_base`.

Node 24 results (`node js/test-node.cjs out/core-hand.wasm`): all logic round-trips, error envelopes, UTF-8, `<base>`
handling OK; **no leak** (71 → 71 wasm pages after 300 calls with a 767 KB page); a 6 MB input grows memory to
433 pages (27 MB) and wasm memory **never shrinks** → after a huge page, drop and re-instantiate (cheap: instantiate
0.2–0.9 ms from a cached `WebAssembly.Module`).

JSON vs raw two-buffer call, per page: plos-med 3.13 → 1.38 ms, bmc-med 4.84 → 2.20, frontiers 10.96 → 4.74.
`JSON.stringify`+`TextEncoder.encode` of a 767 KB HTML string alone costs 8.4 ms (vs 2.6 ms for `encode`).
→ Small inputs as JSON; large payloads (HTML, PDF head bytes) as raw UTF-8/bytes.

Simulated plugin sandbox (`node js/sim-bootstrap.cjs`): a bare `vm` global with only the Zotero-granted globals runs
`plugin-skel/bootstrap.js` → `loadSubScript(glue, this)` → `fetch(chrome://…/fulltext_core.wasm)` → `FulltextCore.load`
→ calls → `shutdown()`. Works. (`this` in bootstrap methods is the sandbox: `plugins.js:258`
`func.call(scope, params, reason)`.)

### 3.2 (b) wasm-bindgen 0.2.126 (CLI and crate pinned `=0.2.126`) — inspected (high, Node-verified)

`wasm-bindgen --target web|no-modules|nodejs` on the same logic (cdylib-only crate): `_bg.wasm` 329,508 B, glue
8.5 KB / 9.4 KB / 5.4 KB. The wasm then **imports** `./bindgen_demo_bg.js.__wbg_Error_…` (for `JsError`); more imports
appear as soon as JsValue/closures/structs are used.

| Construct | web | no-modules | Gecko/Zotero impact |
|---|---|---|---|
| `export …` / `import.meta.url` | yes (1) | no | must be loaded as an ES module (`import()` or `ChromeUtils.importESModule`), not `loadSubScript` |
| `fetch(new URL('…_bg.wasm', import.meta.url))` | default path | default via `document.currentScript` (guarded by `typeof document`) | avoid by always passing bytes/Module to `initSync` |
| `instantiateStreaming` / `Response` checks | yes | yes | only on the fetch path |
| `new TextDecoder('utf-8', {ignoreBOM:true, fatal:true})` + `encodeInto` | yes | yes | fine in Gecko; available in sandbox |
| `FinalizationRegistry` | not emitted here | not emitted | emitted for exported structs; supported by Gecko 140 |
| top-level `let wasm_bindgen = (function…)` | — | yes | `let` does not become a property of a loadSubScript target; reload → redeclaration error (citeprocRsBridge.js:52-53) |
| `Object.getPrototypeOf(opts) === Object.prototype` and `instanceof WebAssembly.Module` in `initSync` | yes | yes | **cross-realm failure**: options object created in another global → `TypeError: WebAssembly.Module(): Argument 0 must be a buffer source` (reproduced with Node `vm`); in Gecko, objects crossing compartments (plugin sandbox ↔ window ↔ module global) behave the same way |

Precedents: Zotero itself ships citeproc-rs (Rust/wasm-bindgen) but needed a **custom build** and a hand-made loading
context (`chrome/content/zotero/xpcom/citeprocRsBridge.js:41-70`, fetches `resource://zotero/citeproc_rs_wasm_bg.wasm`
via `Zotero.HTTP.request(…, {responseType:"arraybuffer"})`). Beaver 0.26.0 (user's profile) ships wasm-bindgen
`sentencex_wasm.js` and loads it with `fetch("chrome://beaver/…_bg.wasm")` + dynamic `import("chrome://beaver/…/sentencex_wasm.js")`
+ `mod.initSync({ module: binary })` (`content/scripts/beaver.js:30289-30297, 30382`), and in a worker via XHR
(`mupdf-worker.js:1214, 3837`). Gecko: `javascript.options.wasm_trustedprincipals = true`
(`.ref\omni-gre\greprefs.js:392`), so wasm runs with the system principal; Zotero also ships pdf.js/ONNX wasm.

Performance is equal: bindgen `landing(html,url)` 4.1–4.4 ms vs hand raw 4.7 ms on the 767 KB page.

**Verdict:** hand ABI. Zero imports, no ESM requirement, no realm-sensitive checks, no generated code to audit,
same speed. wasm-bindgen would only pay off if the core needed to call JS (it should not: I/O stays in JS).

---

## 4. Panics with `panic = "abort"` (high, measured)

- A panic calls the panic hook, then aborts → wasm `unreachable` → JS `WebAssembly.RuntimeError: unreachable`
  (`node js/test-panic.cjs`; both `panic!()` and an out-of-bounds index). The default message is lost.
- `catch_unwind` cannot help: nothing unwinds under abort (and wasm32-unknown-unknown has no unwinding on stable).
- **Panic hook trick (prototyped):** `fx_init()` sets `std::panic::set_hook` writing `info.to_string()` into
  `static LAST_PANIC: Mutex<String>`; `fx_last_panic()` returns `(ptr<<32)|len` of that static without allocating.
  After the trap JS reads it: `RuntimeError: unreachable (rust panic: panicked at crates\core\src\abi.rs:168:17:
  index out of bounds: the len is 0 but the index is 2)`. Cost +1.5 KB.
- After a trap the instance still answers raw calls, but its heap/allocator state is undefined → the glue sets
  `poisoned = true`, refuses further calls, and `reset()` re-instantiates from the cached `WebAssembly.Module`
  (verified: next call works).
- API rule: every export returns the JSON envelope; errors are values (`bad_json`, `bad_url`, `bad_utf8`, …).
- Lints (workspace, lib code; tests `#![allow]` them): `unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`,
  `unimplemented` = deny; `indexing_slicing` = warn; add `string_slice = "deny"`. Clippy on the wasm target with all
  backends: no violations (only style lints). Use `.get(..)`, `checked_*`, `?` with `err(code, msg)`.
- Panics lints cannot see: allocation failure (abort → trap), panics inside dependencies (e.g. RefCell borrow in
  lol_html handlers, slice indexing in parsers), integer division by zero, `format!` of a buggy `Display`, deep
  recursion (Rust links with the stack first in linear memory, so overflow should trap rather than corrupt; low).
  Mitigate with input caps (HTML size), fuzzing the extractor natively (`cargo fuzz` on x86_64), and the reset path.

---

## 5. Native testing (high)

Same crate, no cfg tricks except `pack()`/exports being wasm32-only:

```bash
cargo test -p fulltext-core                                          # hand only, 17 s clean
cargo test -p fulltext-core --features tl,lolhtml,html5gum,scraper   # all backends
cargo run --release -p fulltext-core --example bench --features tl,lolhtml,html5gum,scraper
```

Result on x86_64-pc-windows-msvc: `tests/logic.rs` 6 passed (DOI normalisation incl. `%2F`, EZproxy hostname mode
`www.nature.com → www-nature-com.kcl.idm.oclc.org`, hyphen doubling, Unpaywall ranking, envelope shape, PDF sniff,
messy landing page incl. `<base href>`), `tests/messy.rs` 5 passed (hand/html5gum/scraper/lol_html 0 mismatches; tl
reports 4). Note: Cargo ignores `panic = "abort"` for test/bench builds, so native tests unwind normally.

---

## 6. Build orchestration without Node (high, prototyped)

`xtask/` (binary crate) + `.cargo/config.toml` alias `xtask = "run -q -p xtask --"`:

```text
cargo xtask dist   # 1) cargo rustc -p fulltext-core --lib --crate-type cdylib --release --target wasm32-unknown-unknown
                   #    with CARGO_ENCODED_RUSTFLAGS=--remap-path-prefix=<USERPROFILE>=~
                   # 2) copy .wasm + js/fulltext-core.js into plugin-skel/content/
                   # 3) zip plugin-skel/** -> out/zotero-fulltext-spike.xpi
cargo xtask wasm   # steps 1-2 only
```

```toml
# xtask/Cargo.toml
zip = { version = "9", default-features = false, features = ["deflate-flate2-zlib-rs"] }  # pure Rust
```

Zip options: `SimpleFileOptions::default().compression_method(Deflated).compression_level(Some(9))
.last_modified_time(zip::DateTime::default()).unix_permissions(0o644)`, files sorted, `/` separators →
**byte-identical XPI on rebuild** (SHA-256 `92e420b9…` twice). XPI = 127,806 B (wasm 341,635 → 125,059 deflated);
128,670 B after adding the panic hook and path remapping.

**Privacy finding:** `strip = true` does not remove panic `Location` file paths; every Rust wasm built here (and the
wasm-bindgen one) contained `C:\Users\abora\.rustup\…` and `C:\Users\abora\.cargo\registry\…` (23 occurrences in
core-hand, 60 in core-scraper; 0 in Zig output). Cargo `trim-paths` still requires `-Z` on 1.96
(`CARGO_PROFILE_RELEASE_TRIM_PATHS=all` → "feature `trim-paths` is required"). Fix used: `--remap-path-prefix` (the
xtask derives it from `USERPROFILE`/`HOME`; `measure.sh` uses `[target.wasm32-unknown-unknown] rustflags` in
`.cargo/config.toml`) → 0 occurrences.

---

## 7. wasm-opt (binaryen) (high, measured)

Found already on disk: `C:\Users\abora\AppData\Local\.wasm-pack\wasm-opt-cdcb9e877b68d02e\bin\wasm-opt.exe`
(**version_117**, Feb 2024, downloaded earlier by wasm-pack). Needs explicit features for Rust 1.96 output:
`--enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals
--enable-reference-types --enable-multivalue -Oz`.

| Input | raw before → after | gzip before → after | time |
|---|---|---|---:|
| core-hand | 341,635 → 289,605 (−15.2%) | 123,549 → 123,007 (−0.4%) | 1.2 s |
| core-hand idna 1.0 | 189,905 → 165,914 (−12.6%) | 74,854 → 72,089 (−3.7%) | 1.0 s |
| core-html5gum | 460,100 → 393,861 (−14.4%) | 158,860 → 158,735 (−0.1%) | 1.2 s |
| core-html5gum idna 1.0 | 308,190 → 270,073 (−12.4%) | 109,656 → 108,145 (−1.4%) | 1.1 s |
| core-scraper | 857,811 → 749,472 (−12.6%) | 301,191 → 313,321 (**+4.0%**) | 2.1 s |

-Os ≈ -Oz. Verdict: optional at most; the XPI is a deflated zip, so download/install size barely changes, and it adds
a C++ tool to the build. Bigger wins come from §1.3 (cdylib-only, −14%) and §1.4 (idna 1.0, −44%).

V8 compile cost (Node, first compile, stand-in for Gecko's baseline+Ion tiers; low for Gecko): baseline 3.7–9.8 ms,
optimizing 100–215 ms for 185–838 KB modules; compile once per session and cache the `WebAssembly.Module`.

---

## 8. Zig 0.16 as the core language (short assessment)

Prototype `spikes/wasm-spike/zig/` (`zig build`, `wasm32-freestanding`, `ReleaseSmall`, `entry = .disabled`,
`rdynamic = true`, `std.heap.wasm_allocator`): same hand ABI, compiled first try (22 s incl. std), **no imports**,
same JS glue works.

| | Zig 0.16 | Rust 1.96 |
|---|---|---|
| JSON parse+serialize same struct | 75,337 B (gzip 37,998) via `std.json.parseFromSlice` + `std.json.Stringify.valueAlloc` | 61,728 B (gzip 27,012) |
| + URL resolve | `std.Uri` +6 KB, RFC 3986 only (no WHATWG special-scheme rules, no IDNA) | `url` (WHATWG) +213 KB, or +62 KB with idna 1.0 |
| HTML | nothing in std; community tokenizers are pre-1.0 (low) | html5gum / html5ever / lol_html / tl, all wasm-ready |
| Paths in binary | none | needs `--remap-path-prefix` |
| Stability | std APIs change every 0.x release (0.15 I/O rewrite, 0.16 `std.Io`) (medium) | stable |

Default stays Rust; Zig offers no size advantage for this JSON-heavy workload and lacks a spec HTML tokenizer.

---

## 9. Files in the spike

- `Cargo.toml`, `.cargo/config.toml`, `measure.sh` (rebuilds all variants into `out/`, incl. idna swaps + wasm-opt)
- `crates/core/src/{abi.rs,logic.rs,html/{mod,hand,gum,tl_impl,lol,scrap}.rs}`, `tests/{logic,messy}.rs`, `examples/bench.rs`
- `crates/ladder` (size ladder), `crates/bindgen-demo` (wasm-bindgen comparison; outputs in `out/bindgen-*`)
- `js/fulltext-core.js` (glue), `js/test-node.cjs`, `js/test-panic.cjs`, `js/test-bindgen.mjs`, `js/sim-bootstrap.cjs`,
  `js/wasm-info.mjs` (imports/exports/sections), `js/wasm-top.mjs` (per-crate code attribution)
- `xtask/src/main.rs`, `plugin-skel/{manifest.json,bootstrap.js,content/}`, `out/zotero-fulltext-spike.xpi`
- `zig/{build.zig,ladder.zig}`; fixtures in `fixtures/`

## 10. Risks / open items

- Not run inside real Zotero (hard rule: do not touch the user's instance). Gecko behaviour is inferred from source
  (`plugins.js`, `greprefs.js`), Zotero's citeproc-rs loader and Beaver; first real test should load
  `out/zotero-fulltext-spike.xpi` in a **separate** Zotero profile **and** separate data directory.
- `fetch("chrome://…")` of the wasm from the bootstrap sandbox is Beaver's pattern (medium); Zotero core uses
  `Zotero.HTTP.request(…resource://…, {responseType:"arraybuffer"})` (high) as an alternative.
- i64 → BigInt at the JS boundary is standard (Firefox 78+, medium). If ever a problem: return a pointer to an 8-byte
  result slot instead.
- Hand-rolled ABI needs its own fuzz/contract tests (alloc/free pairing, UTF-8 validity, memory growth).
- Large-page memory high-water mark persists until re-instantiation.
