//! Shared helpers for the integration tests: load recorded fixtures with the response facts they were captured with.
//! Written by the skeleton author (core); the signatures are frozen so the work packages can rely on them.
//! Use from a test file with `mod common;`.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use fulltext_core::model::RespMeta;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Repository root (two levels above this crate).
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn read(path: PathBuf) -> Vec<u8> {
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read fixture {}: {e}", path.display()))
}

fn json(path: PathBuf) -> Value {
    serde_json::from_slice(&read(path)).expect("fixture index is valid JSON")
}

/// One entry of `fixtures/oa/manifest.json` (`file`, `source`, `case`, `url`, `status`, `content_type`, ...).
pub fn oa_manifest() -> Vec<Value> {
    json(root().join("fixtures/oa/manifest.json"))["fixtures"]
        .as_array()
        .expect("manifest.fixtures is an array")
        .clone()
}

/// A recorded OA-API response: the body bytes plus a [`RespMeta`] built from the manifest (status, URL,
/// `content-type`). `file` is the fixture file name, for example `"openalex__gold.json"`.
pub fn oa_fixture(file: &str) -> (RespMeta, Vec<u8>) {
    let entry = oa_manifest()
        .into_iter()
        .find(|e| e["file"] == file)
        .unwrap_or_else(|| panic!("{file} is not in fixtures/oa/manifest.json"));
    let body = read(root().join("fixtures/oa").join(file));
    let mut headers = BTreeMap::new();
    if let Some(ct) = entry["content_type"].as_str() {
        headers.insert("content-type".to_owned(), ct.to_owned());
    }
    let resp = RespMeta {
        status: entry["status"].as_u64().unwrap_or(200) as u16,
        final_url: entry["url"].as_str().unwrap_or_default().to_owned(),
        headers,
        body_len: body.len() as u64,
        ..Default::default()
    };
    (resp, body)
}

/// The `fixtures` array of `fixtures/html/expected.json` (`file`, `kind`, `base_url`, `expected_pdf_url`,
/// `alternates`, `must_not_select`, `via`, ...).
pub fn html_expected() -> Vec<Value> {
    json(root().join("fixtures/html/expected.json"))["fixtures"]
        .as_array()
        .expect("expected.fixtures is an array")
        .clone()
}

/// Status and headers the HTML fixtures were captured with (`fixtures/html/README.md`); everything else was 200.
fn html_response_facts(file: &str) -> (u16, Vec<(&'static str, &'static str)>) {
    match file {
        "cloudflare__managed-challenge.html" => (
            403,
            vec![("server", "cloudflare"), ("cf-mitigated", "challenge")],
        ),
        "cloudflare__blocked-attention-required.html" => (403, vec![("server", "cloudflare")]),
        "akamai__access-denied.html" => (
            403,
            vec![
                ("server", "AkamaiGHost"),
                ("akamai-grn", "0.1a2b3c4d.1792496400.1"),
            ],
        ),
        "europepmc-web__pdf-render-cloudflare-challenge.html" => (
            403,
            vec![("server", "cloudflare"), ("cf-mitigated", "challenge")],
        ),
        _ => (200, vec![]),
    }
}

/// A recorded HTML page with `final_url = base_url` from `expected.json` and the capture-time status / headers.
/// `file` is for example `"lww__annals-of-surgery-article.html"`. Files outside `expected.json` (the OA-fixture
/// challenge pages `europepmc-web__...`, `pmc-web__...`, `medrxiv-web__...` live in `fixtures/oa`) are looked up
/// there with their manifest URL.
pub fn html_fixture(file: &str) -> (RespMeta, Vec<u8>) {
    if let Some(e) = html_expected().into_iter().find(|e| e["file"] == file) {
        let body = read(root().join("fixtures/html").join(file));
        let (status, hdrs) = html_response_facts(file);
        let mut headers: BTreeMap<String, String> = hdrs
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        headers.insert(
            "content-type".to_owned(),
            "text/html; charset=utf-8".to_owned(),
        );
        let resp = RespMeta {
            status,
            final_url: e["base_url"].as_str().unwrap_or_default().to_owned(),
            headers,
            body_len: body.len() as u64,
            ..Default::default()
        };
        return (resp, body);
    }
    oa_fixture(file)
}
