# core-wasm-xtask: ABI hardening, trap-recovery test build, xtask checks, harness command

Package id `core-wasm-xtask`. Implementer: Sonnet. Depends on: nothing (works against the skeleton and against whatever state the other
packages are in; the engine crate is owned by another area and may still be placeholders).
Read first: `docs/design/core.md` section 4-5; `crates/fulltext-wasm/src/{lib,abi,json,panic_hook}.rs`; `xtask/src/main.rs`;
`tests/js/abi.test.mjs`; `research/07-rust-wasm-toolchain.md` sections 3-6; `spikes/wasm-spike/{crates/core/src/abi.rs,js/test-panic.cjs,js/test-node.cjs}`
(proven prototype of the same ABI and of the panic test); `tools/zotero-harness/README.md` and `run.ps1`; `docs/design/contracts/proto.rs` (`Envelope`).

## Files you own
`crates/fulltext-wasm/**`, `xtask/**`, `tests/js/abi.test.mjs`, `tests/js/abi-recovery.test.mjs`. Read-only: everything else, in particular
`crates/fulltext-engine/**` (another area) and the rest of `tests/js/**` (JS area). Environment:
`CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\core-wasm-xtask`. The user's global cargo config sets `rustc-wrapper = "kache"`; do not fight it.
Never touch the user's running Zotero, profile or `~/Zotero`; never log in anywhere.

## Rules for all code
No `unwrap`/`expect`/`panic!`/indexing/string slicing in non-test Rust (xtask included; use `?` with `Box<dyn Error>`). `unsafe` only in
`crates/fulltext-wasm/src/abi.rs` with a `SAFETY:` comment per block. JS: ESM, `node:test`, no npm dependencies, Node 24. Remove every `SKELETON` marker and the `not implemented` harness stub.

## 1. fulltext-wasm: keep the pinned ABI, harden it
The exports and their contracts are in `crates/fulltext-wasm/src/lib.rs` (do not rename or change the pinned seven; `fx_login_state` is the one additive export).
Tasks:
1. **Panic test build.** Add a cargo feature `debug-panic` (off by default, never enabled by `xtask build` for the plugin) that adds `#[unsafe(no_mangle)] pub extern "C" fn fx_debug_panic(code: u32) -> u32` to `abi.rs`: code 1 panics with a message, code 2 triggers an out-of-bounds index (use `#[allow(clippy::indexing_slicing)]` locally), code 3 allocates `u32::MAX` bytes (must return 0 from `fx_alloc`, not trap), any other value returns the code. `cargo xtask build --debug-panic` builds that variant to `<target>/wasm32-unknown-unknown/release/fulltext_wasm_debug.wasm` (copy to `target/xtask/fulltext-debug.wasm` under the workspace target dir; NEVER into `plugin/`).
2. **Allocation discipline.** Add unit tests (native, in `json.rs`/new `tests` module) proving every `json::*_bytes` returns valid JSON for hostile input: empty slice, 5 MB of `{`, invalid UTF-8, deeply nested arrays (serde_json's recursion limit must produce `Envelope::Err`, not a stack overflow: check `StepIn` with 10 000 nested `[`), a `u64`-overflowing integer. Add a test that `err()` and `ok()` never return an empty buffer.
3. **`fx_last_panic` lifetime.** Verify (Node test below) that the buffer is readable after a trap, that `fx_free` on it does not corrupt the heap of a NEW instance (it is a different instance), and that calling `fx_free(ptr,len)` on it in the SAME poisoned instance is a no-op that does not trap.
4. **Concurrency model.** Two instances of one compiled `WebAssembly.Module` (pure + engine) must not share state: test that `fx_step` on instance A and `fx_reset` on instance B are independent.
Keep `abi.rs` thin; logic stays in `json.rs`.

## 2. xtask
Commands (existing `build` works; keep the CLI names): `build [--debug-panic]`, `test`, `xpi`, `harness [--gui]`, plus new `check`.
* **`build` verification step** (after the copy, fail the command on any violation, print one summary line): parse the `.wasm` BINARY in Rust with a hand-written LEB128 section reader (no dependencies; sections 2 = imports, 7 = exports): (a) the import section is empty or absent; (b) the export names are exactly `memory`, `__data_end`, `__heap_base`, `fx_abi_version`, `fx_alloc`, `fx_free`, `fx_inspect`, `fx_step`, `fx_reset`, `fx_last_panic`, `fx_login_state` (plus `fx_debug_panic` only for the debug build); (c) the bytes contain none of the builder's path strings (`USERPROFILE`/`HOME` value, `\.cargo\`, `.rustup`, `C:\Users`); (d) size: print raw and gzip-estimated bytes (use `zip`'s deflate on the file in memory) and warn above 450 KB raw, fail above 700 KB raw. Add unit tests for the section reader using a tiny hand-assembled wasm module (bytes in the test) and the staged real module when present.
* **`xpi`**: already deterministic; add exclusion of `*.map`, `*.md` and `.gitkeep` from the archive, refuse to run if `plugin/manifest.json` is missing or its `version` is not `\d+\.\d+\.\d+`, and print the SHA-256 of the archive (a small private `sha256` module of about 60 lines with the standard `abc` test vector; no new dependency). Add a test (`xtask` unit test using a temp dir) that zipping the same directory twice gives identical bytes.
* **`test`**: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `build`, `node --test tests/js`, stopping at the first failure with a clear message.
* **`check`**: everything in `test` plus `cargo clippy -p fulltext-core -p fulltext-wasm --target wasm32-unknown-unknown -- -D warnings` and `cargo fmt --check` if `rustfmt` is installed (skip with a note if not).
* **`harness`**: read `tools/zotero-harness/run.ps1` and its README and implement `cargo xtask harness [--gui]`: run `build`, then `pwsh -NoProfile -File tools/zotero-harness/run.ps1` with the exact parameters the script documents to load `plugin/` (unpacked) into a THROWAWAY profile and data directory under the workspace target dir; refuse to start if a profile/data path resolves inside `%APPDATA%\Zotero`, `~/Zotero`, or any directory not created by the command; headless unless `--gui`; relay the exit code and print the script's result file path. If `run.ps1` needs a Zotero binary path, read it from the same default the script uses; if that binary is missing print how to set it and exit non-zero (do not install anything). Never pass real credentials; never open login pages.
Document each command's behaviour in the module docs of `xtask/src/main.rs` (keep it under 60 lines of docs).

## 3. Node tests (`tests/js/abi.test.mjs`, `tests/js/abi-recovery.test.mjs`)
`abi.test.mjs` exists and passes; extend it: a 2 MiB body through `fx_inspect`, 1 MiB of non-ASCII text in `fx_login_state`'s `html`, `fx_alloc` of 64 MiB then free, results are always valid UTF-8 JSON, two instances independent (item 4 above), `fx_reset` then `fx_step` works, `WebAssembly.Module.exports` equals the allowlist.
`abi-recovery.test.mjs` (builds nothing itself: skips with an explanatory message when `target/xtask/fulltext-debug.wasm` is absent, and `cargo xtask test` builds it first): for panic codes 1 and 2 assert (a) the call throws `WebAssembly.RuntimeError`; (b) `fx_last_panic()` on that instance returns a non-empty JSON string with `code == "panic"` and a message containing the panic text and NO absolute builder path; (c) a second call on the poisoned instance also throws (documenting that JS must discard it); (d) a fresh instance from the SAME compiled module works and `fx_last_panic()` there is empty; (e) code 3 returns 0 from `fx_alloc` without trapping; (f) 100 trap-and-reinstantiate cycles do not leak (module reused).

## 4. Acceptance
```
set CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\core-wasm-xtask
cargo test -p fulltext-wasm -p xtask
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p fulltext-wasm --target wasm32-unknown-unknown -- -D warnings
cargo xtask build
cargo xtask build --debug-panic
node --test tests/js/abi.test.mjs tests/js/abi-recovery.test.mjs
cargo xtask xpi
git grep --no-index -n -E "SKELETON|not implemented" -- crates/fulltext-wasm xtask
```
`cargo xtask xpi` needs `plugin/manifest.json` from the JS area; if it does not exist yet create a throwaway one OUTSIDE the repo (set `ZFT_PLUGIN_DIR` for the test) rather than committing a fake manifest: add that override to xtask (`ZFT_PLUGIN_DIR`, default `plugin`) and use it in the xtask unit test. The last command must print nothing.
