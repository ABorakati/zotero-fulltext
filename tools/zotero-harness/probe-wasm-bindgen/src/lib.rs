//! wasm-bindgen variant of the probe core: same JSON-in/JSON-out function as probe-wasm,
//! but strings cross the boundary via generated glue instead of a hand-written ABI.

use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn add(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}

#[wasm_bindgen]
pub fn process_json(input: &str) -> String {
    let value: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "error": format!("invalid JSON: {e}") }).to_string(),
    };
    let doi = value.get("doi").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
    let doi = doi
        .strip_prefix("https://doi.org/")
        .or_else(|| doi.strip_prefix("doi:"))
        .unwrap_or(&doi)
        .to_string();
    json!({
        "ok": true,
        "engine": "rust-wasm-bindgen",
        "doi_normalized": doi,
        "text_upper": value.get("text").and_then(Value::as_str).unwrap_or("").to_uppercase(),
    })
    .to_string()
}

/// Returns a JS Error to test Result<_, JsValue> plumbing.
#[wasm_bindgen]
pub fn fail_if_empty(input: &str) -> Result<usize, JsValue> {
    if input.is_empty() {
        Err(JsValue::from_str("input was empty"))
    } else {
        Ok(input.len())
    }
}
