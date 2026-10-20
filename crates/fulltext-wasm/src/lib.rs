//! fulltext-wasm: the ONLY crate with `#[no_mangle]` exports. Owned by core-wasm-xtask
//! (spec: `docs/wp/core-wasm-xtask.md`).
//!
//! The module imports NOTHING (no wasm-bindgen, no WASI, no env): JS does all I/O, so the same compiled module is
//! instantiated twice by the host, once as the `pure` instance (`fx_inspect`, `fx_login_state`) and once as the
//! `engine` instance (`fx_step`). A trap poisons that instance; JS re-instantiates from the cached module.
//!
//! # ABI version 1 (all integers `u32`, all payloads UTF-8 JSON unless stated; `u64` results reach JS as BigInt)
//!
//! | export | signature | contract |
//! |---|---|---|
//! | `fx_abi_version` | `() -> u32` | returns [`ABI_VERSION`] (1). The host refuses any other value. |
//! | `fx_alloc` | `(len) -> ptr` | allocate `len` bytes the host may fill; `0` means out of memory (only for `len > 0`). |
//! | `fx_free` | `(ptr, len)` | release a buffer from `fx_alloc` or a returned buffer; `ptr == 0` or `len == 0` is a no-op. |
//! | `fx_inspect` | `(meta_ptr, meta_len, body_ptr, body_len) -> u64` | `meta` = JSON `InspectMeta`, `body` = raw bytes; result `Envelope<Inspection>`. The callee frees BOTH inputs. |
//! | `fx_step` | `(in_ptr, in_len) -> u64` | input JSON `StepIn`; result `Envelope<StepOut>`. The callee frees the input. |
//! | `fx_reset` | `()` | drop the engine (next `fx_step` starts from `Engine::new()`). |
//! | `fx_last_panic` | `() -> u64` | JSON `{"code":"panic","message":...}` of the last panic, or `0` (len 0) if none. Allocation-free and valid after a trap. The host frees it with `fx_free` like any result; `fx_free` ignores this buffer. |
//! | `fx_login_state` | `(url_ptr, url_len, html_ptr, html_len) -> u64` | ADDITIVE export. `url` UTF-8 text, `html` UTF-8 text (`html_len == 0` means no document); result `Envelope<LoginState>`. The callee frees both inputs. |
//!
//! Results are `(ptr << 32) | len`; the host copies the bytes out and calls `fx_free(ptr, len)`.
//! Every JSON result is `Envelope<T>`: `{"ok": ...}` or `{"err": {"code", "message"}}`. Errors are values, never traps.
//!
//! # Failure model (what the host may rely on)
//!
//! * Bad input is a value: every export answers with an `err` envelope (`bad_json`, `reentered`, the engine's own
//!   codes, `serialize`) and the instance stays healthy. Hostile input (empty, 5 MB of `{`, invalid UTF-8, 10 000
//!   nested `[`, out-of-range integers) is covered by the unit tests in [`json`].
//! * A panic (or any other trap) POISONS the instance: `panic = "abort"` turns it into `unreachable`, which JS sees as
//!   `WebAssembly.RuntimeError`. Before the trap the hook stores `{"code":"panic","message":...}`; `fx_last_panic`
//!   still answers on the dead instance, and `fx_free` of that buffer is a harmless no-op. The host copies the message,
//!   drops the instance and instantiates the SAME compiled module again (fresh memory, empty panic buffer). It must
//!   never reuse a poisoned instance (the allocator and the engine may be half-updated), and it must treat ANY exception
//!   thrown by an export as poison, not only `RuntimeError` (a wasm stack overflow arrives as `RangeError`).
//! * Allocation failure is a value too: `fx_alloc` returns `0` when the request exceeds `isize::MAX` or memory cannot
//!   grow; it never traps.
//! * The test-only export `fx_debug_panic(code)` exists solely behind the cargo feature `debug-panic`
//!   (`cargo xtask build --debug-panic`); `cargo xtask build` fails if the shipped module exports it.
//!
//! The testable logic lives in [`json`] (bytes in, bytes out, no pointers) so `cargo test` covers it natively; the
//! pointer layer in `abi` exists only for `wasm32`.
pub mod json;
pub mod panic_hook;

#[cfg(target_arch = "wasm32")]
mod abi;

/// Version of the calling convention above. Bump on any breaking change; the host checks it first.
pub const ABI_VERSION: u32 = 1;
