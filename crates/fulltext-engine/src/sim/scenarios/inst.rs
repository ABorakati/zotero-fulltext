//! The institutional route end to end through the run manager: F1 login mid-batch (one window for twenty
//! items), F11 429 stops the route, F24 sign-in window ignored, F25 sign-in that does not stick, F26 licence
//! budget. F1 and F11 and the open-access happy path are also the golden traces.
use super::*;
use crate::sim::SimHost;
use crate::sim::fixtures::{http_ok, login_ok};
use std::cell::Cell;
use std::rc::Rc;

fn landed_without_session() -> Reply {
    Reply::ok(
        500,
        login_ok(
            LoginResult::Landed,
            PUBLISHER_PAGE,
            &Inspection::None,
            None,
            None,
        ),
    )
}

fn requests_to_the_institution(host: &SimHost) -> usize {
    count(host, |e| {
        matches!(
            e,
            Effect::Http {
                route: Route::Inst,
                ..
            } | Effect::BrowserLoad {
                route: Route::Inst,
                ..
            }
        )
    })
}

// -------------------------------------------------------------------------------------------- golden: OA

#[test]
fn golden_open_access_happy_path() {
    let d = dois("gold", 3);
    let mut host = run_scenario(World::open_access(&d));
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    for item in 1..=3 {
        assert!(is_saved(&outcome(&host, 1, item), Route::Oa));
    }
    assert_eq!(
        count(&host, |e| matches!(
            e,
            Effect::LoginWindow { .. } | Effect::BrowserLoad { .. }
        )),
        0,
        "no institutional traffic"
    );
    assert_eq!(ledger_counts(&host), (0, 0));
    assert_golden("oa_happy_path", &host);
}

// ----------------------------------------------------------------------------------------------------- F1

#[test]
fn f01_login_mid_batch_twenty_items_share_one_window_and_all_save() {
    let d = dois("f01", 20);
    let mut host = run_scenario(World::inst_only(&d));
    let out = start(&mut host, 1, Mode::Full, true, items(&d));
    assert!(
        out.effects
            .iter()
            .all(|e| !matches!(e, Effect::Http { .. })),
        "nothing starts before the batch prompt is answered"
    );
    settle(&mut host);
    assert_eq!(
        host.effects()
            .filter_map(|e| if let Effect::Confirm { req, .. } = e {
                Some((req.kind, req.n, req.cap))
            } else {
                None
            })
            .collect::<Vec<_>>(),
        vec![(ConfirmKind::LargeBatch, 20, 20)]
    );
    assert_eq!(
        count(&host, |e| matches!(e, Effect::LoginWindow { .. })),
        1,
        "ONE window for twenty items"
    );
    for item in 1..=20 {
        assert!(
            is_saved(&outcome(&host, 1, item), Route::Inst),
            "item {item}: {:?}",
            outcome(&host, 1, item)
        );
    }
    let r = report(&host, 1);
    assert_eq!((r.counts.saved, r.halted.clone()), (20, None));
    assert_eq!(
        ledger_counts(&host),
        (20, 0),
        "every institutional PDF is counted"
    );
    assert_eq!(r.ledger.hour, 20);
    let waiting = (1..=20)
        .filter(|i| {
            rows(&host, 1, *i)
                .iter()
                .any(|row| row == crate::l10n::id::WAITING_LOGIN)
        })
        .count();
    assert!(waiting >= 1, "the item that asked for the sign-in says so");
    let effects: Vec<&Effect> = host.effects().collect();
    let window_at = effects
        .iter()
        .position(|e| matches!(e, Effect::LoginWindow { .. }))
        .expect("a window");
    let loads_before = effects[..window_at]
        .iter()
        .filter(|e| matches!(e, Effect::BrowserLoad { .. }))
        .count();
    assert_eq!(
        loads_before, 1,
        "only the prober loads the redirector before anybody has signed in"
    );
    assert_eq!(
        count(&host, |e| matches!(e, Effect::BrowserLoad { .. })),
        21,
        "the prober twice, everybody else once"
    );
    assert_golden("login_batch_20", &host);
}

// ---------------------------------------------------------------------------------------------------- F11

fn rate_limited(url: &str) -> Reply {
    let insp = Inspection::Page(PageInspection {
        class: PageClass::RateLimited {
            retry_after_s: Some(120),
        },
        page_ids: PageIds::default(),
        doi_match: None,
        access: AccessHint::Unknown,
        candidates: Vec::new(),
        rejected: Vec::new(),
        rule: None,
    });
    Reply::ok(80, http_ok(429, url, &insp, None))
}

#[test]
fn f11_a_429_from_the_publisher_stops_the_institutional_route_for_everyone() {
    let d = dois("f11", 6);
    let seen_429 = Rc::new(Cell::new(false));
    let flag = Rc::clone(&seen_429);
    let world = World::inst_only(&d).hook(move |op, _| match &op.effect {
        // the first publisher request of the batch is answered with 429
        Effect::Http {
            req,
            route: Route::Inst,
            ..
        } if req.url.starts_with("https://doi.org/") && !flag.replace(true) => {
            Some(rate_limited(&req.url))
        }
        _ => None,
    });
    let mut host = run_scenario(world);
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    let r = report(&host, 1).clone();
    assert_eq!(r.halted.as_deref(), Some("rate_limited"));
    assert_eq!(r.counts.saved, 0, "{:?}", outcomes(&host, 1));
    for item in 1..=6 {
        assert!(
            matches!(outcome(&host, 1, item), Outcome::Halted { .. }),
            "item {item}: {:?}",
            outcome(&host, 1, item)
        );
    }
    let halts = host
        .persisted
        .get(&PersistKey::Halts)
        .cloned()
        .unwrap_or_default();
    assert!(
        halts["inst_until"].as_u64().unwrap_or(0) >= host.wall.saturating_sub(10 * 60_000),
        "the halt is persisted: {halts}"
    );
    assert_eq!(halts["reason"], "rate_limited");
    assert_eq!(
        count(&host, |e| matches!(e, Effect::LoginWindow { .. })),
        0,
        "no sign-in is asked for while halted"
    );
    assert_eq!(ledger_counts(&host), (0, 0), "nothing was downloaded");
    assert_golden("rate_limit_halt", &host);

    // The halt outlives the run: the next batch does not even try ...
    let before = requests_to_the_institution(&host);
    start(
        &mut host,
        2,
        Mode::Full,
        true,
        items(&d[..2])
            .into_iter()
            .map(|mut it| {
                it.item += 100;
                it
            })
            .collect(),
    );
    settle(&mut host);
    assert_eq!(
        requests_to_the_institution(&host),
        before,
        "no request to the institution while halted"
    );
    assert!(matches!(outcome(&host, 2, 101), Outcome::Halted { .. }));
    assert_eq!(report(&host, 2).halted.as_deref(), Some("rate_limited"));

    // ... until the cool-down is over (at least 30 minutes).
    host.advance(31 * 60_000);
    start(
        &mut host,
        3,
        Mode::Full,
        true,
        items(&d[..2])
            .into_iter()
            .map(|mut it| {
                it.item += 200;
                it
            })
            .collect(),
    );
    settle(&mut host);
    assert!(
        is_saved(&outcome(&host, 3, 201), Route::Inst),
        "{:?}",
        outcomes(&host, 3)
    );
    assert_eq!(report(&host, 3).halted, None);
}

// ---------------------------------------------------------------------------------------------------- F24

#[test]
fn f24_a_sign_in_window_nobody_answers_times_out_and_open_access_results_are_unaffected() {
    let d = dois("f24", 5);
    let docs = vec![
        Doc {
            doi: d[0].clone(),
            oa: false,
            held: true,
            direct: false,
        },
        Doc {
            doi: d[1].clone(),
            oa: false,
            held: true,
            direct: false,
        },
        Doc {
            doi: d[2].clone(),
            oa: false,
            held: true,
            direct: false,
        },
        Doc {
            doi: d[3].clone(),
            oa: true,
            held: true,
            direct: false,
        },
        Doc {
            doi: d[4].clone(),
            oa: true,
            held: true,
            direct: false,
        },
    ];
    let world = World::new(docs)
        .hook(|op, _| matches!(op.effect, Effect::LoginWindow { .. }).then_some(Reply::Never));
    let mut cfg = test_config();
    cfg.inst.login_timeout_sec = 60;
    let mut host = run_scenario_with(cfg, Persisted::default(), world);
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    for item in 1..=3 {
        match outcome(&host, 1, item) {
            Outcome::NeedsUser {
                why: NeedsWhy::LoginTimeout,
                handoff,
            } => assert!(
                handoff.redirector_url.is_some(),
                "the report offers the manual way in"
            ),
            other => panic!("item {item}: {other:?}"),
        }
    }
    assert!(
        is_saved(&outcome(&host, 1, 4), Route::Oa) && is_saved(&outcome(&host, 1, 5), Route::Oa)
    );
    assert_eq!(
        count(&host, |e| matches!(e, Effect::LoginWindow { .. })),
        1,
        "the window is not reopened for the same run"
    );
    assert_eq!(report(&host, 1).counts.needs_user, 3);
}

// ---------------------------------------------------------------------------------------------------- F25

#[test]
fn f25_a_sign_in_that_does_not_stick_stops_the_prompts() {
    let d = dois("f25", 4);
    // the window reports success but the redirector keeps bouncing to the sign-in page
    let world = World::inst_only(&d)
        .hook(|op, _| matches!(op.effect, Effect::LoginWindow { .. }).then(landed_without_session));
    let mut host = run_scenario(world);
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    for item in 1..=4 {
        assert!(
            matches!(
                outcome(&host, 1, item),
                Outcome::NeedsUser {
                    why: NeedsWhy::LoginIneffective,
                    ..
                }
            ),
            "item {item}: {:?}",
            outcome(&host, 1, item)
        );
    }
    assert_eq!(
        count(&host, |e| matches!(e, Effect::LoginWindow { .. })),
        1,
        "no second prompt"
    );
    assert_eq!(ledger_counts(&host), (0, 0));
}

// ---------------------------------------------------------------------------------------------------- F26

#[test]
fn f26_the_licence_budget_runs_out_mid_batch_and_resets_later() {
    let d = dois("f26", 8);
    let mut cfg = test_config();
    cfg.inst.hourly_cap = 5;
    let mut host = run_scenario_with(cfg, Persisted::default(), World::on_campus(&d));
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    let saved = (1..=8)
        .filter(|i| is_saved(&outcome(&host, 1, *i), Route::Inst))
        .count();
    let deferred = (1..=8)
        .filter(|i| {
            outcome(&host, 1, *i)
                == Outcome::Deferred {
                    cap: CapKind::Hourly,
                }
        })
        .count();
    assert_eq!((saved, deferred), (5, 3), "{:?}", outcomes(&host, 1));
    assert_eq!(ledger_counts(&host), (5, 0), "never more than the cap");
    assert_eq!(report(&host, 1).ledger.hour_cap, 5);
    assert_eq!(report(&host, 1).counts.deferred, 3);

    // An hour later the oldest entries have aged out: the deferred items go through.
    host.advance(3_600_000 + 60_000);
    let again: Vec<ItemSnapshot> = items(&d)
        .into_iter()
        .filter(|it| {
            outcome(&host, 1, it.item)
                == Outcome::Deferred {
                    cap: CapKind::Hourly,
                }
        })
        .collect();
    assert_eq!(again.len(), 3);
    start(&mut host, 2, Mode::Full, true, again);
    settle(&mut host);
    assert_eq!(report(&host, 2).counts.saved, 3);
    assert!(is_saved(
        &outcome(&host, 2, report(&host, 2).items[0].item),
        Route::Inst
    ));
}

#[test]
fn f26_the_batch_cap_defers_the_items_beyond_the_first_twenty() {
    let d = dois("f26b", 24);
    let mut host = run_scenario(World::on_campus(&d));
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    let deferred = outcomes(&host, 1)
        .values()
        .filter(|o| {
            **o == Outcome::Deferred {
                cap: CapKind::Batch,
            }
        })
        .count();
    assert_eq!(deferred, 4, "{:?}", outcomes(&host, 1));
    assert_eq!(report(&host, 1).counts.saved, 20);
}

// ------------------------------------------------------------------------------ SAML and EZproxy-only institutions

const OX_IDP: &str = "https://registry.shibboleth.ox.ac.uk/idp";
const SHIB_IDP: &str =
    "https://idp.shibboleth.ox.ac.uk/idp/profile/SAML2/Redirect/SSO?execution=e1s1";

fn shib_class() -> PageClass {
    PageClass::IdpLogin {
        idp: Idp::Other,
        ca_error: None,
    }
}

/// DOIs of publishers whose article URL (and so WAYFless link) follows from the DOI alone.
fn saml_dois() -> Vec<String> {
    [
        "10.1038/saml.1",
        "10.1007/saml.2",
        "10.1002/saml.3",
        "10.1080/saml.4",
        "10.1177/saml.5",
        "10.1056/saml.6",
    ]
    .iter()
    .map(|d| (*d).to_owned())
    .collect()
}

/// While nobody has signed in, every hidden load of the publisher's sign-in ends on the institution's own
/// Shibboleth IdP page.
fn shibboleth_bounce(world: World) -> World {
    let session = Rc::clone(&world.session);
    world.hook(move |op, _| match &op.effect {
        Effect::BrowserLoad { .. } if !session.get() => Some(Reply::ok(
            800,
            crate::sim::fixtures::browser_ok(
                SHIB_IDP,
                &page_inspection_of(shib_class()),
                None,
                None,
            ),
        )),
        _ => None,
    })
}

fn effect_urls(host: &SimHost) -> Vec<String> {
    host.effects()
        .filter_map(|e| crate::sim::fixtures::effect_url(e).map(str::to_owned))
        .collect()
}

fn source_of(o: &Outcome) -> Option<&str> {
    match o {
        Outcome::Saved { source, .. } => Some(source.as_str()),
        _ => None,
    }
}

fn login_windows(host: &SimHost) -> Vec<LoginReq> {
    host.effects()
        .filter_map(|e| match e {
            Effect::LoginWindow { req, .. } => Some(req.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn saml_only_institution_signs_six_publishers_in_through_one_window() {
    let d = saml_dois();
    let world = shibboleth_bounce(World::inst_only(&d));
    let mut cfg = test_config();
    cfg.institution.openathens_domain = String::new();
    cfg.institution.ezproxy = None;
    cfg.institution.ezproxy_hosts.clear();
    cfg.institution.alma = None;
    cfg.institution.passthrough_hosts.clear();
    cfg.institution.saml_entity_id = Some(OX_IDP.into());
    cfg.inst.check_alma = false;
    let mut host = run_scenario_with(cfg, Persisted::default(), world);
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    for item in 1..=6 {
        let o = outcome(&host, 1, item);
        assert!(is_saved(&o, Route::Inst), "item {item}: {o:?}");
        assert_eq!(source_of(&o), Some("saml"), "item {item}");
    }
    let windows = login_windows(&host);
    assert_eq!(windows.len(), 1, "ONE window for six items");
    assert_eq!(windows[0].realm, Realm::Saml);
    assert!(
        windows[0]
            .entry_url
            .contains("idp=https%3A%2F%2Fregistry.shibboleth.ox.ac.uk%2Fidp")
            || windows[0]
                .entry_url
                .contains("entityId=https%3A%2F%2Fregistry.shibboleth.ox.ac.uk%2Fidp"),
        "{}",
        windows[0].entry_url
    );
    assert_eq!(ledger_counts(&host), (6, 0));
    assert_eq!(report(&host, 1).counts.saved, 6);
    for url in effect_urls(&host) {
        assert!(
            !url.contains("openathens") && !url.contains("qurl="),
            "a route that is not configured was used: {url}"
        );
    }
}

#[test]
fn ezproxy_for_every_host_works_without_an_openathens_domain() {
    let d = dois("ezall", 4);
    let world = World::inst_only(&d);
    let mut cfg = test_config();
    cfg.institution.openathens_domain = String::new();
    cfg.institution.ezproxy_hosts.clear();
    cfg.institution.ezproxy_all_hosts = true;
    cfg.institution.alma = None;
    cfg.institution.passthrough_hosts.clear();
    cfg.inst.check_alma = false;
    let mut host = run_scenario_with(cfg, Persisted::default(), world);
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    for item in 1..=4 {
        let o = outcome(&host, 1, item);
        assert!(is_saved(&o, Route::Inst), "item {item}: {o:?}");
        assert_eq!(source_of(&o), Some("ezproxy"), "item {item}");
    }
    let windows = login_windows(&host);
    assert_eq!(windows.len(), 1, "one EZproxy sign-in for the batch");
    assert_eq!(windows[0].realm, Realm::Ezproxy);
    assert!(
        windows[0]
            .entry_url
            .starts_with("https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2F"),
        "{}",
        windows[0].entry_url
    );
    assert_eq!(ledger_counts(&host), (4, 0));
    for url in effect_urls(&host) {
        assert!(!url.contains("openathens"), "no redirector URL: {url}");
    }
}

#[test]
fn openathens_falls_back_to_the_publisher_login_when_it_does_not_know_the_publisher() {
    let d = saml_dois();
    let world = World::inst_only(&d);
    let session = Rc::clone(&world.session);
    let world = world.hook(move |op, _| match &op.effect {
        // The redirector has never heard of the publisher ...
        Effect::BrowserLoad { req, .. } if req.url.contains("go.openathens.net") => {
            Some(Reply::ok(
                300,
                crate::sim::fixtures::browser_ok(
                    "https://go.openathens.net/unknown",
                    &page_inspection_of(PageClass::OpenathensUnknownResource),
                    None,
                    None,
                ),
            ))
        }
        // ... and the publisher's own login bounces to the IdP until a window was answered.
        Effect::BrowserLoad { .. } if !session.get() => Some(Reply::ok(
            800,
            crate::sim::fixtures::browser_ok(
                SHIB_IDP,
                &page_inspection_of(shib_class()),
                None,
                None,
            ),
        )),
        _ => None,
    });
    let mut cfg = test_config();
    cfg.institution.saml_entity_id = Some(OX_IDP.into());
    cfg.inst.use_ezproxy = false;
    cfg.inst.check_alma = false;
    let mut host = run_scenario_with(cfg, Persisted::default(), world);
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    for item in 1..=6 {
        let o = outcome(&host, 1, item);
        assert_eq!(source_of(&o), Some("saml"), "item {item}: {o:?}");
    }
    let windows = login_windows(&host);
    assert_eq!(windows.len(), 1, "one window, in the SAML realm");
    assert_eq!(windows[0].realm, Realm::Saml);
    assert_eq!(
        count(
            &host,
            |e| matches!(e, Effect::BrowserLoad { req, .. } if req.url.contains("go.openathens.net"))
        ),
        6,
        "OpenAthens was asked once per item before the fallback"
    );
    assert_eq!(ledger_counts(&host), (6, 0));
}
