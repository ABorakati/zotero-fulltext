//! Same logic exposed through wasm-bindgen 0.2.126, to compare the generated glue.
use fulltext_core::logic::{LandingIn, landing_pdf_candidates};
use wasm_bindgen::prelude::*;

/// Returns JSON (string) so no serde-wasm-bindgen is needed.
#[wasm_bindgen]
pub fn landing(html: String, url: String) -> Result<String, JsError> {
    let out = landing_pdf_candidates(LandingIn { html, url, raw: false }).map_err(|e| JsError::new(&e.message))?;
    serde_json::to_string(&out.candidates).map_err(|e| JsError::new(&e.to_string()))
}

#[wasm_bindgen]
pub fn sniff_pdf(bytes: &[u8]) -> bool {
    fulltext_core::logic::looks_like_pdf(bytes)
}
