//! Landing-page extraction: the Rust candidates JS already attached (`PageInspection.candidates`, from the pure
//! instance) merged with Zotero's translators (`Effect::RunTranslators` on the same page bytes).
//!
//! # Contract (`extract_landing`)
//! * Never re-extract a page already taken: `pool.mark_extracted(url)` is checked first. (It is not `was_tried`:
//!   `attempt::fetch_candidate` marks every URL it fetches as tried, so by the time a page arrives here its URL is
//!   always "tried".)
//! * Translators run when a page body is available and the page is a landing/HTML/reader page: the engine waits at
//!   most `TRANSLATOR_WAIT_MS`, then goes on with the Rust candidates; a late hit is ignored (dropping the op
//!   cancels it). On institutional pages translators are used ONLY when Rust found no candidate (their side
//!   requests escape the limiter); `LandingResult::ran_translators` tells the institutional route to count that
//!   against the item's fetch budget.
//! * Ordering: a Rust publisher-rule candidate (`signal` starts with `rule:`) first, else the translator URL
//!   (mime PDF), then the generic Rust candidates in the order core produced them. Agreement between the two
//!   (the same URL from both) boosts that candidate. core's `rank` recomputes scores, so the order is carried to
//!   the pool as a small per-URL bonus (`CandidatePool::add_boosted`).
//! * Candidates whose `record_doi` differs from the requested DOI (or from the published DOI of a preprint) are
//!   dropped, and so is everything from a page whose own DOI differs (`doi_match == Some(false)`). Rejections are
//!   traced (`Effect::Trace`, only when tracing is on).
//! * At most `MAX_LANDING_CANDIDATES` candidates come out of one page.
use crate::cx::ItemCx;
use crate::ops::Body;
use crate::pipeline::pool::CandidatePool;
use crate::pipeline::pure;
use crate::proto::TranslatorHit;
use crate::sched::policy;
use fulltext_core::model::*;
use fulltext_core::rank::new_candidate;
use serde_json::json;
use std::collections::BTreeSet;

/// Most candidates one landing page may add to the pool.
const MAX_LANDING_CANDIDATES: usize = 6;
/// Ordering bonuses (see the module docs); small next to core's score steps so a better tier or version still wins.
const RULE_BONUS: i32 = 15;
const TRANSLATOR_BONUS: i32 = 8;
const AGREEMENT_BONUS: i32 = 10;

/// What extraction produced.
#[derive(Clone, Debug)]
pub(crate) struct LandingResult {
    pub candidates: Vec<Candidate>,
    #[allow(dead_code)] // kept so the struct mirrors its source; nothing reads it yet
    pub class: PageClass,
    pub access: AccessHint,
    #[allow(dead_code)] // kept so the struct mirrors its source; nothing reads it yet
    pub doi_match: Option<bool>,
    /// Zotero translators were asked (the institutional route counts that against its fetch budget).
    pub ran_translators: bool,
}

/// A short stable name of a page class, for traces and logs.
pub(crate) fn class_name(class: &PageClass) -> &'static str {
    match class {
        PageClass::Pdf { .. } => "pdf",
        PageClass::Landing => "landing",
        PageClass::Html => "html",
        PageClass::Redirect { .. } => "redirect",
        PageClass::ReaderShell { .. } => "reader_shell",
        PageClass::Challenge { .. } => "challenge",
        PageClass::Blocked { .. } => "blocked",
        PageClass::RateLimited { .. } => "rate_limited",
        PageClass::NotFound => "not_found",
        PageClass::AccessDenied => "access_denied",
        PageClass::EzproxyNeedLogin => "ezproxy_need_login",
        PageClass::EzproxyHostNotConfigured => "ezproxy_host_not_configured",
        PageClass::EzproxyError { .. } => "ezproxy_error",
        PageClass::IdpLogin { .. } => "idp_login",
        PageClass::SamlAutoPost => "saml_auto_post",
        PageClass::OpenathensWait => "openathens_wait",
        PageClass::OpenathensUnknownResource => "openathens_unknown_resource",
        PageClass::OpenathensBadOrg => "openathens_bad_org",
        PageClass::PublisherSsoHop => "publisher_sso_hop",
        PageClass::LicenceStop => "licence_stop",
    }
}

/// Does this class carry links worth extracting?
fn extractable(class: &PageClass) -> bool {
    matches!(
        class,
        PageClass::Landing | PageClass::Html | PageClass::ReaderShell { .. }
    )
}

/// A candidate from the same record as the request (or no record DOI at all)?
fn record_ok(c: &Candidate, requested: &Ids, published: Option<&str>) -> bool {
    match (c.record_doi.as_deref(), requested.doi.as_deref()) {
        (Some(record), Some(wanted)) => {
            pure::same_doi(record, wanted) || published.is_some_and(|p| pure::same_doi(record, p))
        }
        _ => true,
    }
}

fn is_pdf_mime(mime: &str) -> bool {
    mime.trim()
        .to_ascii_lowercase()
        .starts_with("application/pdf")
}

fn translator_candidate(hit: &TranslatorHit, page_url: &str) -> Option<Candidate> {
    pure::host_of(&hit.url)?;
    let mut c = new_candidate(hit.url.clone(), CandKind::Pdf, Source::Translator);
    c.host_kind = pure::host_kind(&hit.url);
    c.landing_url = Some(page_url.to_owned());
    c.signal = Some("translator".to_owned());
    Some(c)
}

/// Ask the translators for the page's attachment; `None` on no hit, an error, a non-PDF hit or after the wait.
async fn translator_hit(cx: &ItemCx, page: &Body, url: &str) -> Option<TranslatorHit> {
    match cx
        .timeout(policy::TRANSLATOR_WAIT_MS, cx.run_translators(page, url))
        .await
    {
        Ok(Ok(done)) => {
            let hit = done.found.filter(|h| is_pdf_mime(&h.mime));
            cx.trace_step(
                "translator",
                "landing",
                if hit.is_some() { "hit" } else { "none" },
                done.ms,
            );
            hit
        }
        Ok(Err(err)) => {
            cx.trace_step("translator", "landing", "error", 0);
            cx.trace_event(
                json!({ "ev": "translators_failed", "kind": format!("{:?}", err.kind) }),
            );
            None
        }
        Err(_) => {
            cx.trace_step(
                "translator",
                "landing",
                "timeout",
                u32::try_from(policy::TRANSLATOR_WAIT_MS).unwrap_or(u32::MAX),
            );
            None
        }
    }
}

/// Everything extraction needs from the pool, so that it can run without borrowing it.
struct Extraction {
    result: LandingResult,
    /// The bonus of `result.candidates[i]` (same order).
    bonuses: Vec<i32>,
}

async fn extract(
    cx: &ItemCx,
    page: Option<&Body>,
    url: &str,
    inspection: &PageInspection,
    requested: &Ids,
    published: Option<&str>,
    route: Route,
) -> Extraction {
    let class = inspection.class.clone();
    let wrong_article = inspection.doi_match == Some(false);

    let mut rust: Vec<Candidate> = Vec::new();
    if wrong_article {
        cx.trace_event(json!({ "ev": "landing_rejected", "url": url, "why": "other_doi", "dropped": inspection.candidates.len() }));
    } else {
        for c in &inspection.candidates {
            if record_ok(c, requested, published) {
                rust.push(c.clone());
            } else {
                cx.trace_event(
                    json!({ "ev": "landing_rejected", "url": c.url, "why": "other_doi" }),
                );
            }
        }
    }
    for r in &inspection.rejected {
        cx.trace_event(json!({ "ev": "landing_rejected", "url": r.url, "why": r.why }));
    }

    let wants_translators =
        extractable(&class) && !wrong_article && (route == Route::Oa || rust.is_empty());
    let (ran_translators, hit) = match page {
        Some(body) if wants_translators => (true, translator_hit(cx, body, url).await),
        _ => (false, None),
    };
    let translated = hit.and_then(|h| translator_candidate(&h, url));
    let translated_key = translated.as_ref().map(|c| pure::canonical_key(&c.url));
    let rust_keys: BTreeSet<String> = rust.iter().map(|c| pure::canonical_key(&c.url)).collect();

    let is_rule = |c: &Candidate| c.signal.as_deref().is_some_and(|s| s.starts_with("rule:"));
    let (rules, generic): (Vec<Candidate>, Vec<Candidate>) = rust.into_iter().partition(is_rule);
    let agrees =
        |c: &Candidate| translated_key.as_deref() == Some(pure::canonical_key(&c.url).as_str());

    let mut ordered: Vec<(Candidate, i32)> = Vec::new();
    for c in rules {
        let bonus = RULE_BONUS + if agrees(&c) { AGREEMENT_BONUS } else { 0 };
        ordered.push((c, bonus));
    }
    // The translator's URL is a candidate of its own only when Rust did not find it too.
    if let Some(t) = translated.filter(|t| !rust_keys.contains(&pure::canonical_key(&t.url))) {
        ordered.push((t, TRANSLATOR_BONUS));
    }
    for c in generic {
        let bonus = if agrees(&c) { AGREEMENT_BONUS } else { 0 };
        ordered.push((c, bonus));
    }
    ordered.truncate(MAX_LANDING_CANDIDATES);

    // Earlier means better: a small positional bonus keeps the order inside a tier.
    let n = ordered.len();
    let mut candidates = Vec::with_capacity(n);
    let mut bonuses = Vec::with_capacity(n);
    for (i, (mut c, bonus)) in ordered.into_iter().enumerate() {
        c.landing_url = c.landing_url.or_else(|| Some(url.to_owned()));
        let position = i32::try_from(n.saturating_sub(i + 1)).unwrap_or(0).min(4);
        bonuses.push(bonus.saturating_add(position));
        candidates.push(c);
    }
    cx.trace_step("extractor", "landing", class_name(&class), 0);
    Extraction {
        result: LandingResult {
            candidates,
            class,
            access: inspection.access,
            doi_match: inspection.doi_match,
            ran_translators,
        },
        bonuses,
    }
}

/// Extract candidates from a landing page and add them to the pool (with `landing_url` set).
pub(crate) async fn extract_landing(
    cx: &ItemCx,
    page: Option<&Body>,
    url: &str,
    inspection: &PageInspection,
    pool: &mut CandidatePool,
    route: Route,
) -> LandingResult {
    if !pool.mark_extracted(url) {
        cx.trace_step("extractor", "landing", "already_extracted", 0);
        return LandingResult {
            candidates: Vec::new(),
            class: inspection.class.clone(),
            access: inspection.access,
            doi_match: inspection.doi_match,
            ran_translators: false,
        };
    }
    let requested = pool.ids().clone();
    let published = pool.published_doi().map(str::to_owned);
    let Extraction { result, bonuses } = extract(
        cx,
        page,
        url,
        inspection,
        &requested,
        published.as_deref(),
        route,
    )
    .await;
    for (c, bonus) in result.candidates.iter().zip(bonuses) {
        pool.add_boosted(c.clone(), bonus);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::testkit::{Rig, script};
    use crate::proto::{Effect, OpErrKind};
    use crate::sim::Reply;
    use crate::sim::fixtures::{self, body, is_type, page_inspection, translators_ok};

    const PAGE: &str = "https://pub.example.org/article/10.1234/x";

    fn snap() -> ItemSnapshot {
        fixtures::item(1, "10.1234/x")
    }

    /// A candidate of the Rust extractor with the same version and host kind as a translator hit would have, so that
    /// the pool order between the two depends on the ordering bonuses alone.
    fn rust_pdf(url: &str, signal: Option<&str>) -> Candidate {
        let mut c = new_candidate(url, CandKind::Pdf, Source::Extractor);
        c.host_kind = pure::host_kind(url);
        c.signal = signal.map(str::to_owned);
        c.record_doi = Some("10.1234/x".into());
        c
    }

    fn inspection(candidates: Vec<Candidate>) -> PageInspection {
        match page_inspection(PageClass::Landing, candidates) {
            Inspection::Page(p) => p,
            other => panic!("not a page inspection: {other:?}"),
        }
    }

    /// Run `extract_landing` on a fresh pool for `10.1234/x` with a page body of handle 5.
    fn run(
        rig: &mut Rig,
        insp: PageInspection,
        route: Route,
        with_body: bool,
    ) -> Option<(LandingResult, Vec<String>)> {
        rig.exec(snap(), move |cx| async move {
            let mut pool = CandidatePool::new(Ids {
                doi: Some("10.1234/x".into()),
                ..Ids::default()
            });
            let page = with_body.then(|| Body::new(std::rc::Rc::clone(&cx.shared), body(5, 1000)));
            let res = extract_landing(&cx, page.as_ref(), PAGE, &insp, &mut pool, route).await;
            let order: Vec<String> = pool.ranked(&cx.cfg.oa).into_iter().map(|c| c.url).collect();
            (res, order)
        })
    }

    fn translator_sent(rig: &Rig) -> usize {
        rig.count(|e| matches!(e, Effect::RunTranslators { .. }))
    }

    #[test]
    fn rust_candidates_pass_through_when_the_translators_find_nothing() {
        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(100, translators_ok(None)),
        ));
        let insp = inspection(vec![rust_pdf("https://pub.example.org/a.pdf", None)]);
        let (res, order) = run(&mut rig, insp, Route::Oa, true).expect("done");
        assert_eq!(res.candidates.len(), 1);
        assert_eq!(res.candidates[0].landing_url.as_deref(), Some(PAGE));
        assert!(res.ran_translators);
        assert_eq!(
            order,
            vec!["https://pub.example.org/a.pdf".to_owned()],
            "added to the pool"
        );
        assert_eq!(translator_sent(&rig), 1);
        assert_eq!(
            rig.count(|e| matches!(e, Effect::Release { handle: 5 })),
            1,
            "the page body is released by its owner"
        );
    }

    #[test]
    fn a_translator_hit_is_added_and_agreement_is_not_duplicated() {
        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(
                100,
                translators_ok(Some(("https://pub.example.org/t.pdf", "application/pdf"))),
            ),
        ));
        let insp = inspection(vec![rust_pdf("https://pub.example.org/a.pdf", None)]);
        let (res, order) = run(&mut rig, insp, Route::Oa, true).expect("done");
        let urls: Vec<&str> = res.candidates.iter().map(|c| c.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://pub.example.org/t.pdf",
                "https://pub.example.org/a.pdf"
            ],
            "translator before generic Rust"
        );
        assert_eq!(res.candidates[0].source, Source::Translator);
        assert_eq!(
            order.first().map(String::as_str),
            Some("https://pub.example.org/t.pdf")
        );

        // The same URL from both sides is one candidate and wins over an unrelated one.
        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(
                100,
                translators_ok(Some((
                    "https://pub.example.org/b.pdf",
                    "application/pdf; charset=binary",
                ))),
            ),
        ));
        let insp = inspection(vec![
            rust_pdf("https://pub.example.org/a.pdf", None),
            rust_pdf("https://pub.example.org/b.pdf", None),
        ]);
        let (res, order) = run(&mut rig, insp, Route::Oa, true).expect("done");
        assert_eq!(res.candidates.len(), 2, "no duplicate");
        assert_eq!(
            order.first().map(String::as_str),
            Some("https://pub.example.org/b.pdf"),
            "agreement boosts b over a"
        );
    }

    #[test]
    fn a_rule_candidate_goes_first() {
        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(
                100,
                translators_ok(Some(("https://pub.example.org/t.pdf", "application/pdf"))),
            ),
        ));
        let insp = inspection(vec![
            rust_pdf(
                "https://pub.example.org/generic.pdf",
                Some("citation_pdf_url"),
            ),
            rust_pdf("https://pub.example.org/rule.pdf", Some("rule:wiley")),
        ]);
        let (res, order) = run(&mut rig, insp, Route::Oa, true).expect("done");
        assert_eq!(res.candidates[0].url, "https://pub.example.org/rule.pdf");
        assert_eq!(res.candidates[1].url, "https://pub.example.org/t.pdf");
        assert_eq!(
            order.first().map(String::as_str),
            Some("https://pub.example.org/rule.pdf")
        );
    }

    #[test]
    fn translators_that_hang_are_dropped_after_eight_seconds() {
        let mut rig = Rig::new(script().on(is_type("run_translators"), Reply::Never));
        let insp = inspection(vec![rust_pdf("https://pub.example.org/a.pdf", None)]);
        let start = rig.host.now;
        let (res, _) = run(&mut rig, insp, Route::Oa, true).expect("done");
        assert_eq!(res.candidates.len(), 1, "the Rust candidates are used");
        assert_eq!(rig.host.now, start + policy::TRANSLATOR_WAIT_MS);
        assert_eq!(
            rig.count(|e| matches!(e, Effect::Cancel { .. })),
            1,
            "the late op is cancelled"
        );
        assert!(rig.host.pending.is_empty());
    }

    #[test]
    fn translator_errors_and_non_pdf_hits_are_ignored() {
        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::err(10, OpErrKind::Internal),
        ));
        let insp = inspection(vec![rust_pdf("https://pub.example.org/a.pdf", None)]);
        let (res, _) = run(&mut rig, insp, Route::Oa, true).expect("done");
        assert_eq!(res.candidates.len(), 1);

        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(
                10,
                translators_ok(Some((
                    "https://pub.example.org/book.epub",
                    "application/epub+zip",
                ))),
            ),
        ));
        let insp = inspection(vec![]);
        let (res, _) = run(&mut rig, insp, Route::Oa, true).expect("done");
        assert!(res.candidates.is_empty(), "an EPUB is not a PDF");
    }

    #[test]
    fn candidates_of_another_article_are_rejected() {
        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(10, translators_ok(None)),
        ));
        let mut other = rust_pdf("https://pub.example.org/other.pdf", None);
        other.record_doi = Some("10.9999/different".into());
        let insp = inspection(vec![other, rust_pdf("https://pub.example.org/a.pdf", None)]);
        let (res, order) = run(&mut rig, insp, Route::Oa, true).expect("done");
        assert_eq!(res.candidates.len(), 1);
        assert_eq!(order, vec!["https://pub.example.org/a.pdf".to_owned()]);

        // The page itself is another article: nothing is taken and the translators are not even asked.
        let mut rig = Rig::new(script());
        let mut insp = inspection(vec![rust_pdf("https://pub.example.org/a.pdf", None)]);
        insp.doi_match = Some(false);
        let (res, order) = run(&mut rig, insp, Route::Oa, true).expect("done");
        assert!(res.candidates.is_empty() && order.is_empty());
        assert_eq!(res.doi_match, Some(false));
        assert!(!res.ran_translators);
        assert_eq!(translator_sent(&rig), 0);
    }

    #[test]
    fn a_published_record_of_a_preprint_is_accepted() {
        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(10, translators_ok(None)),
        ));
        let outcome = rig.exec(snap(), |cx| async move {
            let mut pool = CandidatePool::new(Ids {
                doi: Some("10.1101/preprint".into()),
                ..Ids::default()
            });
            let mut api = fixtures::api_found(Source::Biorxiv, vec![]);
            if let Inspection::Api(a) = &mut api {
                a.note = Some("published_doi:10.1371/journal.pone.1".into());
            }
            if let Inspection::Api(a) = &api {
                pool.merge(a, "biorxiv");
            }
            let mut c = rust_pdf("https://pub.example.org/a.pdf", None);
            c.record_doi = Some("10.1371/journal.pone.1".into());
            let insp = inspection(vec![c]);
            let page = Body::new(std::rc::Rc::clone(&cx.shared), body(5, 1000));
            extract_landing(&cx, Some(&page), PAGE, &insp, &mut pool, Route::Oa)
                .await
                .candidates
                .len()
        });
        assert_eq!(outcome, Some(1));
    }

    #[test]
    fn institutional_pages_use_translators_only_when_rust_found_nothing() {
        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(10, translators_ok(None)),
        ));
        let insp = inspection(vec![rust_pdf("https://pub.example.org/a.pdf", None)]);
        let (res, _) = run(&mut rig, insp, Route::Inst, true).expect("done");
        assert_eq!((res.candidates.len(), res.ran_translators), (1, false));
        assert_eq!(translator_sent(&rig), 0);

        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(
                10,
                translators_ok(Some(("https://pub.example.org/t.pdf", "application/pdf"))),
            ),
        ));
        let (res, _) = run(&mut rig, inspection(vec![]), Route::Inst, true).expect("done");
        assert_eq!((res.candidates.len(), res.ran_translators), (1, true));
        assert_eq!(translator_sent(&rig), 1);
    }

    #[test]
    fn no_body_and_unextractable_classes_skip_the_translators() {
        let mut rig = Rig::new(script());
        let (res, _) = run(
            &mut rig,
            inspection(vec![rust_pdf("https://pub.example.org/a.pdf", None)]),
            Route::Oa,
            false,
        )
        .expect("done");
        assert_eq!((res.candidates.len(), res.ran_translators), (1, false));

        let mut rig = Rig::new(script());
        let mut insp = inspection(vec![]);
        insp.class = PageClass::AccessDenied;
        let (res, _) = run(&mut rig, insp, Route::Oa, true).expect("done");
        assert_eq!(
            (res.class, res.ran_translators),
            (PageClass::AccessDenied, false)
        );
        assert_eq!(translator_sent(&rig), 0);
    }

    #[test]
    fn a_page_is_extracted_once() {
        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(10, translators_ok(None)),
        ));
        let again = rig.exec(snap(), |cx| async move {
            let mut pool = CandidatePool::new(Ids {
                doi: Some("10.1234/x".into()),
                ..Ids::default()
            });
            let insp = inspection(vec![rust_pdf("https://pub.example.org/a.pdf", None)]);
            let page = Body::new(std::rc::Rc::clone(&cx.shared), body(5, 1000));
            // the fetch tier has already marked the page tried; that must not block extraction
            pool.mark_tried(PAGE);
            let first = extract_landing(&cx, Some(&page), PAGE, &insp, &mut pool, Route::Oa).await;
            let second = extract_landing(&cx, Some(&page), PAGE, &insp, &mut pool, Route::Oa).await;
            (first.candidates.len(), second.candidates.len())
        });
        assert_eq!(again, Some((1, 0)));
        assert_eq!(translator_sent(&rig), 1);
    }

    #[test]
    fn at_most_six_candidates_come_out_of_one_page() {
        let mut rig = Rig::new(script().on(
            is_type("run_translators"),
            Reply::ok(10, translators_ok(None)),
        ));
        let many: Vec<Candidate> = (0..10)
            .map(|i| rust_pdf(&format!("https://pub.example.org/{i}.pdf"), None))
            .collect();
        let (res, _) = run(&mut rig, inspection(many), Route::Oa, true).expect("done");
        assert_eq!(res.candidates.len(), MAX_LANDING_CANDIDATES);
    }

    #[test]
    fn class_names_are_distinct() {
        let classes = [
            PageClass::Pdf { junk_prefix: 0 },
            PageClass::Landing,
            PageClass::Html,
            PageClass::Redirect {
                next: String::new(),
            },
            PageClass::ReaderShell { rewrite: None },
            PageClass::Challenge {
                vendor: Vendor::Other,
                interactive: false,
            },
            PageClass::Blocked {
                vendor: Vendor::Other,
            },
            PageClass::RateLimited {
                retry_after_s: None,
            },
            PageClass::NotFound,
            PageClass::AccessDenied,
            PageClass::EzproxyNeedLogin,
            PageClass::EzproxyHostNotConfigured,
            PageClass::EzproxyError { suspend: false },
            PageClass::IdpLogin {
                idp: Idp::Entra,
                ca_error: None,
            },
            PageClass::SamlAutoPost,
            PageClass::OpenathensWait,
            PageClass::OpenathensUnknownResource,
            PageClass::OpenathensBadOrg,
            PageClass::PublisherSsoHop,
            PageClass::LicenceStop,
        ];
        let names: std::collections::BTreeSet<&str> = classes.iter().map(class_name).collect();
        assert_eq!(names.len(), classes.len());
    }
}
