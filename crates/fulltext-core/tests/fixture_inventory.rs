//! Guards the fixture loaders in `common`: every indexed fixture exists and loads. Written by the skeleton author.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;

#[test]
fn every_oa_fixture_in_the_manifest_loads() {
    let manifest = common::oa_manifest();
    assert!(
        manifest.len() > 70,
        "manifest unexpectedly small: {}",
        manifest.len()
    );
    for e in &manifest {
        let file = e["file"].as_str().unwrap();
        let (resp, body) = common::oa_fixture(file);
        assert!(resp.status >= 200, "{file}");
        assert!(!body.is_empty(), "{file} is empty");
    }
}

#[test]
fn every_html_fixture_in_expected_json_loads() {
    let expected = common::html_expected();
    assert!(
        expected.len() >= 30,
        "expected.json unexpectedly small: {}",
        expected.len()
    );
    for e in &expected {
        let file = e["file"].as_str().unwrap();
        let (resp, body) = common::html_fixture(file);
        assert!(
            resp.final_url.starts_with("https://"),
            "{file}: {}",
            resp.final_url
        );
        assert!(!body.is_empty(), "{file} is empty");
    }
}

#[test]
fn bot_wall_fixtures_carry_their_capture_status() {
    assert_eq!(
        common::html_fixture("cloudflare__managed-challenge.html")
            .0
            .status,
        403
    );
    assert_eq!(
        common::html_fixture("pmc__pdf-proof-of-work-interstitial.html")
            .0
            .status,
        200
    );
    assert_eq!(
        common::oa_fixture("medrxiv-web__pdf-403-block.html")
            .0
            .status,
        403
    );
}
