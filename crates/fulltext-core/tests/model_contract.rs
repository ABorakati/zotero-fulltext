//! Wire-shape tests for the frozen contract types that live in `fulltext_core::model`
//! (the `proto` half of the contract is tested in fulltext-engine). Owned by core-ids-oa (skeleton author: core).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use fulltext_core::model::*;
use serde_json::json;

#[test]
fn inspect_meta_parses_with_defaults() {
    let meta = json!({"spec":{"expect":"page","requested":{"doi":"10.1097/sla.0000000000005603"},"route":"oa"},
      "resp":{"status":200,"final_url":"https://journals.lww.com/annalsofsurgery/fulltext/x.aspx",
        "headers":{"content-type":"text/html; charset=utf-8","server":"cloudflare"},"body_len":231044}});
    let m: InspectMeta = serde_json::from_value(meta).unwrap();
    assert_eq!(m.resp.body_len, 231044);
    assert!(!m.resp.truncated);
    assert_eq!(m.resp.head_len, None);
    match m.spec {
        InspectSpec::Page { requested, route } => {
            assert_eq!(
                requested.doi.as_deref(),
                Some("10.1097/sla.0000000000005603")
            );
            assert!(!requested.rxiv);
            assert_eq!(route, Route::Oa);
        }
        other => panic!("wrong spec {other:?}"),
    }
}

#[test]
fn api_inspection_parses_the_documented_example() {
    let j = json!({"expect":"api","source":"openalex","status":"found","learned":{"pmcid":"PMC2707599","pmid":"19621072"},
      "candidates":[{"url":"https://journals.plos.org/plosmedicine/article?id=10.1371/journal.pmed.1000097",
        "kind":"landing","source":"openalex","version":"published","license":"cc-by","host_kind":"publisher",
        "record_doi":"10.1371/journal.pmed.1000097"}]});
    match serde_json::from_value::<Inspection>(j).unwrap() {
        Inspection::Api(a) => {
            assert_eq!(a.status, ApiStatus::Found);
            assert_eq!(a.learned.pmcid.as_deref(), Some("PMC2707599"));
            assert_eq!(a.candidates[0].kind, CandKind::Landing);
            assert_eq!(a.candidates[0].tier, 0);
        }
        other => panic!("wrong inspection {other:?}"),
    }
}

#[test]
fn inspection_and_class_shapes() {
    assert_eq!(
        serde_json::to_value(Inspection::None).unwrap(),
        json!({"expect": "none"})
    );
    assert_eq!(
        serde_json::to_value(PageClass::Landing).unwrap(),
        json!({"kind": "landing"})
    );
    assert_eq!(
        serde_json::to_value(Inspection::Probe {
            class: PageClass::OpenathensWait
        })
        .unwrap(),
        json!({"expect": "probe", "class": {"kind": "openathens_wait"}})
    );
    assert_eq!(
        serde_json::to_value(PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive: false
        })
        .unwrap(),
        json!({"kind": "challenge", "vendor": "cloudflare", "interactive": false})
    );
    let pdf = Inspection::Pdf(PdfVerdict {
        is_pdf: true,
        junk_prefix: 0,
        has_eof: true,
        encrypted: false,
        looks_html: false,
        len: 7,
    });
    assert_eq!(serde_json::to_value(pdf).unwrap()["expect"], "pdf");
}

#[test]
fn login_state_and_outcome_shapes() {
    assert_eq!(
        serde_json::to_value(LoginState::Refused {
            code: Some("AADSTS53000".into())
        })
        .unwrap(),
        json!({"state": "refused", "code": "AADSTS53000"})
    );
    assert_eq!(
        serde_json::to_value(LoginState::Pending).unwrap(),
        json!({"state": "pending"})
    );
    let o = Outcome::NeedsUser {
        why: NeedsWhy::LoginDeclined,
        handoff: Handoff {
            libkey_url: Some("https://libkey.io/libraries/286/10.1002/bjs.11646".into()),
            ..Default::default()
        },
    };
    let v = serde_json::to_value(&o).unwrap();
    assert_eq!(v["kind"], "needs_user");
    assert!(
        v["handoff"].get("redirector_url").is_none(),
        "None handoff fields are skipped"
    );
}

#[test]
fn persisted_defaults_from_empty_object() {
    let p: Persisted = serde_json::from_value(json!({})).unwrap();
    assert_eq!(p, Persisted::default());
}
