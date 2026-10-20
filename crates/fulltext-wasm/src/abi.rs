//! The `extern "C"` layer (wasm32 only). Owned by core-wasm-xtask. Keep it thin: every export validates nothing
//! beyond pointer arithmetic and hands the bytes to [`crate::json`], which is where the logic and the tests live.
//!
//! Memory protocol: the host allocates input buffers with `fx_alloc`, writes into them, and passes (ptr, len) to an
//! export, which FREES them before returning. Result buffers are leaked `Box<[u8]>` that the host copies out of and
//! releases with `fx_free(ptr, len)`. All buffers use `Layout { size: len, align: 1 }`, so alloc and free always agree.
#![allow(unsafe_code)]

use crate::{json, panic_hook};
use std::alloc::{Layout, alloc, dealloc};

/// A non-null, well-aligned address for zero-length buffers (never dereferenced or freed).
const DANGLING: u32 = 1;

fn layout(len: u32) -> Option<Layout> {
    Layout::from_size_align(len as usize, 1).ok()
}

/// Release a buffer; ignores null, zero-length and the static panic buffer.
fn release(ptr: u32, len: u32) {
    if ptr == 0 || len == 0 || ptr as usize == panic_hook::buffer().0 {
        return;
    }
    if let Some(l) = layout(len) {
        // SAFETY: the host contract says (ptr, len) came from `fx_alloc(len)` or from a returned result, both of which
        // use exactly this layout, and that the buffer is released at most once.
        unsafe { dealloc(ptr as usize as *mut u8, l) }
    }
}

/// Borrow a host-owned input for the duration of `f`, then free it (the callee-frees rule).
fn with_input<R>(ptr: u32, len: u32, f: impl FnOnce(&[u8]) -> R) -> R {
    let result = if ptr == 0 || len == 0 {
        f(&[])
    } else {
        // SAFETY: the host passes a buffer from `fx_alloc(len)` holding `len` initialised bytes.
        f(unsafe { std::slice::from_raw_parts(ptr as usize as *const u8, len as usize) })
    };
    release(ptr, len);
    result
}

/// Leak `bytes` to the host as `(ptr << 32) | len`.
fn hand_over(bytes: Vec<u8>) -> u64 {
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len() as u64;
    let ptr = Box::into_raw(boxed).cast::<u8>() as usize as u64;
    (ptr << 32) | len
}

#[unsafe(no_mangle)]
pub extern "C" fn fx_abi_version() -> u32 {
    crate::ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn fx_alloc(len: u32) -> u32 {
    panic_hook::install();
    if len == 0 {
        return DANGLING;
    }
    match layout(len) {
        // SAFETY: `len > 0`, so the layout has non-zero size.
        Some(l) => unsafe { alloc(l) as usize as u32 },
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn fx_free(ptr: u32, len: u32) {
    release(ptr, len);
}

#[unsafe(no_mangle)]
pub extern "C" fn fx_inspect(meta_ptr: u32, meta_len: u32, body_ptr: u32, body_len: u32) -> u64 {
    panic_hook::install();
    with_input(meta_ptr, meta_len, |meta| {
        with_input(body_ptr, body_len, |body| {
            hand_over(json::inspect_bytes(meta, body))
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn fx_login_state(url_ptr: u32, url_len: u32, html_ptr: u32, html_len: u32) -> u64 {
    panic_hook::install();
    with_input(url_ptr, url_len, |url| {
        with_input(html_ptr, html_len, |html| {
            hand_over(json::login_state_bytes(url, html))
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn fx_step(in_ptr: u32, in_len: u32) -> u64 {
    panic_hook::install();
    with_input(in_ptr, in_len, |input| hand_over(json::step_bytes(input)))
}

#[unsafe(no_mangle)]
pub extern "C" fn fx_reset() {
    json::reset();
}

#[unsafe(no_mangle)]
pub extern "C" fn fx_last_panic() -> u64 {
    let (ptr, len) = panic_hook::buffer();
    ((ptr as u64) << 32) | len as u64
}

/// Trap-recovery probe, compiled only with the `debug-panic` feature (never in the plugin build; `cargo xtask build`
/// verifies the shipped module does not export it). The Node tests use it to exercise the failure paths of the ABI:
///
/// * `1`: panics with a message (the hook stores it for `fx_last_panic`, then `panic = "abort"` traps).
/// * `2`: out-of-bounds index, the other common panic source (message `index out of bounds: ...`).
/// * `3`: asks `fx_alloc` for `u32::MAX` bytes; that exceeds `isize::MAX` on wasm32, so the answer must be `0`, not
///   a trap, and the instance must stay usable. Returns the pointer `fx_alloc` gave (always `0`).
/// * anything else: returns `code` unchanged (proves the export is callable without side effects).
#[cfg(feature = "debug-panic")]
#[unsafe(no_mangle)]
#[allow(
    clippy::panic,
    clippy::indexing_slicing,
    reason = "the whole point of this export is to panic"
)]
pub extern "C" fn fx_debug_panic(code: u32) -> u32 {
    panic_hook::install();
    match code {
        1 => panic!("fx_debug_panic: deliberate panic"),
        2 => {
            let table = [1_u32, 2, 3];
            // black_box hides the index from the optimiser so the bounds check is real, not a compile error.
            table[std::hint::black_box(table.len() + 5)]
        }
        3 => {
            let ptr = fx_alloc(u32::MAX);
            fx_free(ptr, u32::MAX);
            ptr
        }
        other => other,
    }
}
