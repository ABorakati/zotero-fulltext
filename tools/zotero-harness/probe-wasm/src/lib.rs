//! Probe core for the Zotero runtime harness.
//!
//! Hand-written ABI (no wasm-bindgen):
//!   zf_alloc(len) -> ptr            allocate `len` bytes (align 1) in linear memory
//!   zf_dealloc(ptr, len)            free a block returned by zf_alloc / zf_process_json
//!   zf_process_json(ptr, len) -> u64
//!       input:  UTF-8 JSON text at [ptr, ptr+len)
//!       output: packed u64 = out_ptr | (out_len << 32); UTF-8 JSON text owned by the
//!               module, caller frees it with zf_dealloc(out_ptr, out_len)
//!   zf_abi_version() -> i32         constant, lets the JS side check the ABI
//!   zf_add(a, b) -> i32             trivial sanity export

use serde_json::{json, Value};
use std::alloc::{alloc, dealloc, Layout};

#[no_mangle]
pub extern "C" fn zf_abi_version() -> i32 {
    1
}

#[no_mangle]
pub extern "C" fn zf_add(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}

#[no_mangle]
pub extern "C" fn zf_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return core::ptr::NonNull::<u8>::dangling().as_ptr();
    }
    match Layout::from_size_align(len, 1) {
        // SAFETY: layout has non-zero size.
        Ok(layout) => unsafe { alloc(layout) },
        Err(_) => core::ptr::null_mut(),
    }
}

/// # Safety
/// `ptr`/`len` must come from `zf_alloc` or `zf_process_json` and be freed once.
#[no_mangle]
pub unsafe extern "C" fn zf_dealloc(ptr: *mut u8, len: usize) {
    if len == 0 || ptr.is_null() {
        return;
    }
    dealloc(ptr, Layout::from_size_align_unchecked(len, 1));
}

/// # Safety
/// `[ptr, ptr+len)` must be a readable region of this module's linear memory.
#[no_mangle]
pub unsafe extern "C" fn zf_process_json(ptr: *const u8, len: usize) -> u64 {
    let input = if len == 0 {
        &[][..]
    } else {
        core::slice::from_raw_parts(ptr, len)
    };
    let out = process(input).into_bytes().into_boxed_slice();
    let out_len = out.len();
    let out_ptr = Box::into_raw(out) as *mut u8;
    (out_ptr as usize as u64) | ((out_len as u64) << 32)
}

fn process(input: &[u8]) -> String {
    let value: Value = match serde_json::from_slice(input) {
        Ok(v) => v,
        Err(e) => {
            return json!({ "ok": false, "error": format!("invalid JSON: {e}") }).to_string();
        }
    };

    let doi_raw = value.get("doi").and_then(Value::as_str).unwrap_or("");
    let text = value.get("text").and_then(Value::as_str).unwrap_or("");
    let numbers: Vec<f64> = value
        .get("numbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();

    json!({
        "ok": true,
        "engine": "rust-wasm32-unknown-unknown",
        "abi": zf_abi_version(),
        "doi_normalized": normalize_doi(doi_raw),
        "text_chars": text.chars().count(),
        "text_upper": text.to_uppercase(),
        "sum": numbers.iter().sum::<f64>(),
        "echo_keys": value.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
    })
    .to_string()
}

/// Strip common DOI prefixes and lowercase (DOIs are case-insensitive).
fn normalize_doi(raw: &str) -> Option<String> {
    let s = raw.trim();
    let lower = s.to_lowercase();
    let prefixes = ["https://doi.org/", "http://doi.org/", "https://dx.doi.org/", "http://dx.doi.org/", "doi:"];
    let stripped = prefixes
        .iter()
        .find_map(|p| lower.strip_prefix(p))
        .unwrap_or(&lower)
        .trim();
    if stripped.starts_with("10.") && stripped.contains('/') {
        Some(stripped.to_string())
    } else {
        None
    }
}
