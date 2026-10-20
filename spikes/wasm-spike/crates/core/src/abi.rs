//! Hand-written wasm ABI: no wasm-bindgen, no imports.
//!
//! Contract (all integers are wasm32 i32/i64):
//!   fx_alloc(len) -> ptr                 JS allocates an input buffer inside linear memory
//!   fx_free(ptr, len)                    JS frees any buffer it got from fx_alloc or from a call result
//!   <fn>(ptr, len) -> u64                input = UTF-8 JSON at [ptr, ptr+len); Rust only borrows it
//!                                        output = (out_ptr << 32) | out_len, UTF-8 JSON owned by JS
//!                                        output JSON is always {"ok": ...} or {"err": {"code","message"}}
//!
//! Every export is infallible at the ABI level: errors are values, never traps.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// ABI version, bumped on any breaking change to the calling convention.
pub const ABI_VERSION: u32 = 1;

#[unsafe(no_mangle)]
pub extern "C" fn fx_abi_version() -> u32 {
    ABI_VERSION
}

/// Allocate `len` bytes (capacity is exact for `Vec::with_capacity`).
#[unsafe(no_mangle)]
pub extern "C" fn fx_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len);
    let p = v.as_mut_ptr();
    core::mem::forget(v);
    p
}

/// Free a buffer previously returned by `fx_alloc` or packed into a call result.
///
/// # Safety
/// `ptr`/`len` must come from `fx_alloc(len)` or from a result of this module.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fx_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: caller contract above; capacity == len for both producers.
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
}

#[derive(Serialize, Debug)]
pub struct ErrBody {
    pub code: &'static str,
    pub message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Envelope<T: Serialize> {
    Ok(T),
    Err(ErrBody),
}

pub type CoreResult<T> = Result<T, ErrBody>;

pub fn err(code: &'static str, message: impl Into<String>) -> ErrBody {
    ErrBody { code, message: message.into() }
}

/// Hand a byte buffer to JS: leak it and pack (ptr, len) into a u64.
/// Only meaningful on wasm32 (32-bit pointers).
#[cfg(target_arch = "wasm32")]
pub fn pack(bytes: Vec<u8>) -> u64 {
    let boxed = bytes.into_boxed_slice(); // capacity == len
    let len = boxed.len() as u64;
    let ptr = Box::into_raw(boxed) as *mut u8 as usize as u64;
    (ptr << 32) | len
}

/// Serialize a result envelope; never panics (falls back to a static error).
pub fn respond_bytes<T: Serialize>(r: CoreResult<T>) -> Vec<u8> {
    let env = match r {
        Ok(v) => Envelope::Ok(v),
        Err(e) => Envelope::Err(e),
    };
    serde_json::to_vec(&env).unwrap_or_else(|_| {
        br#"{"err":{"code":"serialize","message":"failed to serialize result"}}"#.to_vec()
    })
}

#[cfg(target_arch = "wasm32")]
pub fn respond<T: Serialize>(r: CoreResult<T>) -> u64 {
    pack(respond_bytes(r))
}

/// Decode a borrowed JSON input buffer.
///
/// # Safety
/// `ptr` must point at `len` initialised bytes inside linear memory.
pub unsafe fn decode<T: DeserializeOwned>(ptr: *const u8, len: usize) -> CoreResult<T> {
    if ptr.is_null() && len != 0 {
        return Err(err("bad_input", "null input pointer"));
    }
    // SAFETY: caller contract.
    let input = if len == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(ptr, len) } };
    serde_json::from_slice(input).map_err(|e| err("bad_json", e.to_string()))
}

/// Declare a JSON-in/JSON-out export: `export_json!(fx_name, InputType, fn(InputType) -> CoreResult<Out>)`.
#[macro_export]
macro_rules! export_json {
    ($export:ident, $in:ty, $f:path) => {
        #[cfg(target_arch = "wasm32")]
        /// # Safety
        /// `ptr`/`len` must describe a buffer obtained from `fx_alloc`.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $export(ptr: *const u8, len: usize) -> u64 {
            $crate::abi::respond(unsafe { $crate::abi::decode::<$in>(ptr, len) }.and_then($f))
        }
    };
}

/// Raw-bytes export (no JSON): returns 1 if the buffer looks like a PDF.
///
/// # Safety
/// `ptr`/`len` must describe a buffer obtained from `fx_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fx_sniff_pdf(ptr: *const u8, len: usize) -> u32 {
    if ptr.is_null() {
        return 0;
    }
    // SAFETY: caller contract.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    u32::from(crate::logic::looks_like_pdf(bytes))
}

/// Last panic message, written by the panic hook just before the abort/trap.
/// (std Mutex is a no-op lock on single-threaded wasm32-unknown-unknown.)
static LAST_PANIC: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Call once after instantiation: the hook still runs under panic = "abort",
/// so the message survives the trap and JS can read it with `fx_last_panic`.
#[unsafe(no_mangle)]
pub extern "C" fn fx_init() {
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut s) = LAST_PANIC.lock() {
            *s = info.to_string();
        }
    }));
}

/// (ptr << 32 | len) of the last panic message; NOT owned by JS (do not free).
/// Allocation-free, so it is safe to call on an instance that just trapped.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fx_last_panic() -> u64 {
    match LAST_PANIC.lock() {
        Ok(s) => ((s.as_ptr() as usize as u64) << 32) | s.len() as u64,
        Err(_) => 0,
    }
}

/// Spike only: deliberately panic so the JS side can observe the trap.
#[cfg(feature = "debug-panic")]
#[unsafe(no_mangle)]
#[allow(clippy::panic)]
pub extern "C" fn fx_debug_panic(code: u32) -> u32 {
    if code == 1 {
        panic!("deliberate panic from fx_debug_panic");
    }
    if code == 2 {
        let v: Vec<u32> = Vec::new();
        #[allow(clippy::indexing_slicing)]
        return v[code as usize]; // bounds-check panic
    }
    code
}
