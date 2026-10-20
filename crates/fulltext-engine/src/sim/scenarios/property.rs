//! Property tests: seeded random worlds for 20-item batches through the whole engine.
//!
//! The world answers every request with a random latency (0-30 s) and a random kind of reply (ok, timeout,
//! network error, challenge, 429, "not entitled", a window that is closed or ignored ...). The *kind* of an
//! answer is a pure function of the request (kind seed + DOI / URL), the *latency* comes from a separate seeded
//! generator, so the same kind seed can be replayed under different latencies.
//!
//! Properties checked (besides the nine invariants after every step): no panic, the run finishes with every item
//! reported, nothing is left pending, the engine never stalls or loses a wake-up (`NoStall`), the same seeds give
//! the same effect log byte for byte, a random `CancelRun` / `CancelItem` at a random moment breaks nothing, and
//! with *stable* replies (nothing whose effect depends on which item finishes first) the outcome of every item
//! is the same whatever the latencies are.
use super::*;
use crate::sched::Rng;
use std::cell::Cell;
use std::collections::BTreeSet;
use std::rc::Rc;

const BATCH: u32 = 20;

#[derive(Clone, Copy, Debug)]
struct Knobs {
    kind_seed: u64,
    latency_seed: u64,
    /// Only replies whose effect does not depend on the order of completion (see the module docs).
    stable: bool,
}

/// FNV-1a over the seed, a salt and the key, folded to `0..100`.
fn pick(seed: u64, salt: &str, key: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for b in salt.bytes().chain(std::iter::once(b'/')).chain(key.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    (h >> 20) % 100
}

struct Fuzz {
    dois: Vec<String>,
    knobs: Knobs,
    lat: Rng,
    handles: u32,
    windows: u64,
    session: Rc<Cell<bool>>,
}

impl Fuzz {
    fn new(dois: &[String], knobs: Knobs) -> Self {
        Self {
            dois: dois.to_vec(),
            knobs,
            lat: Rng::new(knobs.latency_seed),
            handles: 0,
            windows: 0,
            session: Rc::new(Cell::new(false)),
        }
    }

    /// Latency in ms: up to `stable_cap` in stable worlds (below every engine timeout), up to 30 s otherwise.
    fn ms(&mut self, stable_cap: u64) -> u64 {
        let cap = if self.knobs.stable {
            stable_cap
        } else {
            30_000
        };
        self.lat.next_u64() % (cap + 1)
    }

    fn handle(&mut self) -> u32 {
        self.handles += 1;
        self.handles
    }

    fn doi_in(&self, url: &str) -> Option<String> {
        self.dois
            .iter()
            .filter(|d| {
                url.contains(d.as_str())
                    || url.contains(&d.replace('/', "%2F"))
                    || url.contains(&oa_slug(d))
            })
            .max_by_key(|d| d.len())
            .cloned()
    }

    fn kind(&self, salt: &str, doi: &str) -> u64 {
        pick(self.knobs.kind_seed, salt, doi)
    }

    fn api(&mut self, doi: &str, url: &str) -> Reply {
        let k = self.kind("oa", doi);
        let (found, status) = if self.knobs.stable {
            match k {
                0..=49 => (true, 200),
                50..=84 => (false, 200),
                _ => (true, 200),
            }
        } else {
            match k {
                0..=39 => (true, 200),
                40..=54 => (false, 200),
                55..=64 => return Reply::err(self.ms(5_000), OpErrKind::Network),
                65..=72 => return Reply::err(self.ms(5_000), OpErrKind::Timeout),
                73..=81 => {
                    let insp = Inspection::Api(ApiInspection {
                        source: Source::Openalex,
                        status: ApiStatus::Throttled,
                        learned: LearnedIds::default(),
                        candidates: Vec::new(),
                        oa_status: None,
                        note: None,
                        retry_after_s: Some(3),
                    });
                    return Reply::ok(self.ms(5_000), http_ok(429, url, &insp, None));
                }
                _ => (true, 200),
            }
        };
        let insp = if found {
            let mut c = candidate(&oa_pdf_url(doi), CandKind::Pdf, Source::Openalex, 0);
            c.record_doi = Some(doi.to_owned());
            api_found(Source::Openalex, vec![c])
        } else {
            Inspection::Api(ApiInspection {
                source: Source::Openalex,
                status: ApiStatus::NotFound,
                learned: LearnedIds::default(),
                candidates: Vec::new(),
                oa_status: None,
                note: None,
                retry_after_s: None,
            })
        };
        Reply::ok(self.ms(5_000), http_ok(status, url, &insp, None))
    }

    fn oa_pdf(&mut self, doi: &str, url: &str) -> Reply {
        let k = self.kind("pdf", doi);
        let k = if self.knobs.stable && k >= 88 { 0 } else { k };
        let ms = self.ms(15_000);
        match k {
            0..=64 => {
                let handle = self.handle();
                Reply::ok(
                    ms,
                    http_ok(
                        200,
                        url,
                        &pdf_inspection(80_000),
                        Some(body(handle, 80_000)),
                    ),
                )
            }
            65..=77 => Reply::ok(
                ms,
                http_ok(
                    403,
                    url,
                    &page_inspection_of(PageClass::Challenge {
                        vendor: Vendor::Cloudflare,
                        interactive: false,
                    }),
                    None,
                ),
            ),
            78..=87 => Reply::ok(
                ms,
                http_ok(200, url, &page_inspection_of(PageClass::Landing), None),
            ),
            88..=93 => Reply::err(ms, OpErrKind::Network),
            _ => Reply::err(ms, OpErrKind::Timeout),
        }
    }

    fn http(&mut self, url: &str) -> Reply {
        let doi = self.doi_in(url);
        if url.contains("api.openalex.org") {
            return match doi {
                Some(d) => self.api(&d, url),
                None => Reply::err(10, OpErrKind::Network),
            };
        }
        if url.contains(ALMA_HOST) {
            let Some(d) = doi else {
                return Reply::err(10, OpErrKind::Network);
            };
            if !self.knobs.stable && self.kind("alma-err", &d) < 6 {
                return Reply::err(self.ms(5_000), OpErrKind::Network);
            }
            let held = self.kind("alma", &d) < 80;
            let ms = self.ms(5_000);
            return match alma_reply(held) {
                Reply::Ok { value, .. } => Reply::ok(ms, value),
                other => other,
            };
        }
        if url.starts_with("https://doi.org/") {
            let ms = self.ms(5_000);
            return page(ms, PUBLISHER_PAGE, PageClass::AccessDenied);
        }
        if url.contains("://oa-") {
            return match doi {
                Some(d) => self.oa_pdf(&d, url),
                None => Reply::err(10, OpErrKind::Network),
            };
        }
        Reply::err(10, OpErrKind::Network)
    }

    fn browser(&mut self, url: &str) -> Reply {
        let ms = self.ms(20_000);
        if url.contains("://oa-") {
            let doi = self.doi_in(url).unwrap_or_default();
            return if self.kind("cb", &doi) < 60 {
                let handle = self.handle();
                Reply::ok(
                    ms,
                    browser_ok(
                        url,
                        &pdf_inspection(80_000),
                        Some(body(handle, 80_000)),
                        None,
                    ),
                )
            } else {
                Reply::ok(
                    ms,
                    browser_ok(
                        url,
                        &page_inspection_of(PageClass::Challenge {
                            vendor: Vendor::Cloudflare,
                            interactive: false,
                        }),
                        None,
                        None,
                    ),
                )
            };
        }
        if self.session.get() {
            let handle = self.handle();
            Reply::ok(
                ms,
                browser_ok(
                    PUBLISHER_PAGE,
                    &pdf_inspection(80_000),
                    Some(body(handle, 80_000)),
                    None,
                ),
            )
        } else {
            Reply::ok(
                ms,
                browser_ok(IDP, &page_inspection_of(idp_class()), None, None),
            )
        }
    }

    fn window(&mut self) -> Reply {
        self.windows += 1;
        let k = if self.knobs.stable {
            0
        } else {
            pick(self.knobs.kind_seed, "window", &self.windows.to_string())
        };
        let result = match k {
            0..=69 => LoginResult::Landed,
            70..=84 => LoginResult::ClosedByUser,
            _ => LoginResult::Timeout,
        };
        if result == LoginResult::Landed {
            self.session.set(true);
        }
        Reply::ok(
            self.ms(30_000),
            login_ok(result, PUBLISHER_PAGE, &Inspection::None, None, None),
        )
    }
}

/// The slug part of an open-access URL (to find the DOI in it).
fn oa_slug(doi: &str) -> String {
    doi.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

impl Responder for Fuzz {
    fn respond(&mut self, op: &PendingOp, _now: u64, _wall: u64) -> Reply {
        match &op.effect {
            Effect::Http { req, .. } => self.http(&req.url),
            Effect::BrowserLoad { req, .. } => self.browser(&req.url),
            Effect::LoginWindow { .. } => self.window(),
            Effect::SavePdf { req, .. } => {
                let ms = self.ms(3_000);
                match if self.knobs.stable {
                    99
                } else {
                    pick(self.knobs.kind_seed, "save", &req.item.to_string())
                } {
                    0..=5 => Reply::err(ms, OpErrKind::Io),
                    6..=8 => Reply::err(ms, OpErrKind::AlreadyHasPdf),
                    9..=10 => Reply::err(ms, OpErrKind::ParentGone),
                    _ => Reply::ok(ms, save_ok(&format!("1/K{:04}", req.item))),
                }
            }
            Effect::Confirm { .. } => Reply::ok(self.ms(5_000), json!({ "choice": "yes" })),
            Effect::ZoteroOa { .. } => Reply::ok(self.ms(5_000), json!([])),
            Effect::RunTranslators { .. } => Reply::ok(self.ms(5_000), translators_ok(None)),
            _ => Reply::err(10, OpErrKind::Internal),
        }
    }
}

fn batch_world(knobs: Knobs) -> (Vec<String>, Fuzz) {
    let d = dois("prop", BATCH);
    let fuzz = Fuzz::new(&d, knobs);
    (d, fuzz)
}

/// One full run; the host after it ended (all end-of-scenario invariants hold).
fn run_batch(knobs: Knobs) -> SimHost {
    let (d, fuzz) = batch_world(knobs);
    let mut cfg = test_config();
    cfg.run.item_concurrency =
        u32::try_from(1 + pick(knobs.kind_seed, "concurrency", "") % 6).unwrap_or(4);
    let mut host = run_scenario_with(cfg, Persisted::default(), fuzz);
    start(&mut host, 1, Mode::Full, true, items(&d));
    settle(&mut host);
    host
}

fn assert_whole(host: &SimHost, knobs: Knobs) {
    let r = report(host, 1);
    assert_eq!(
        r.items.len(),
        BATCH as usize,
        "{knobs:?}: every item is reported"
    );
    assert!(
        host.pending.is_empty(),
        "{knobs:?}: nothing is left pending"
    );
    assert_eq!(host.reports.len(), 1, "{knobs:?}: one RunFinished");
}

#[test]
fn random_batches_never_break_an_invariant_stall_or_lose_a_wake() {
    let mut saved = 0;
    let mut kinds: BTreeSet<String> = BTreeSet::new();
    for seed in 1..=30u64 {
        let knobs = Knobs {
            kind_seed: seed,
            latency_seed: seed.wrapping_mul(7919) + 1,
            stable: false,
        };
        let host = run_batch(knobs);
        assert_whole(&host, knobs);
        saved += report(&host, 1).counts.saved;
        for o in report(&host, 1).items.iter().map(|i| &i.outcome) {
            kinds.insert(
                serde_json::to_value(o)
                    .ok()
                    .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
                    .unwrap_or_default(),
            );
        }
    }
    assert!(
        saved > 100,
        "the worlds are not all hopeless: {saved} items saved"
    );
    assert!(
        kinds.len() >= 6,
        "the random worlds reach a variety of outcomes: {kinds:?}"
    );
}

#[test]
fn the_same_seeds_give_the_same_effect_log() {
    for seed in [3u64, 11, 29] {
        let knobs = Knobs {
            kind_seed: seed,
            latency_seed: seed + 100,
            stable: false,
        };
        let (a, b) = (run_batch(knobs), run_batch(knobs));
        assert_eq!(trace_text(&a), trace_text(&b), "{knobs:?}");
        assert_eq!(a.reports, b.reports);
        let other = run_batch(Knobs {
            latency_seed: seed + 101,
            ..knobs
        });
        assert_ne!(
            trace_text(&a),
            trace_text(&other),
            "different latencies, different timeline"
        );
    }
}

#[test]
fn outcomes_do_not_depend_on_latencies_when_the_replies_are_deterministic() {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for kind_seed in 1..=10u64 {
        let reference = {
            let knobs = Knobs {
                kind_seed,
                latency_seed: 1,
                stable: true,
            };
            let host = run_batch(knobs);
            assert_whole(&host, knobs);
            outcomes(&host, 1)
        };
        for latency_seed in [2u64, 3, 4] {
            let knobs = Knobs {
                kind_seed,
                latency_seed,
                stable: true,
            };
            let host = run_batch(knobs);
            assert_eq!(
                outcomes(&host, 1),
                reference,
                "kind seed {kind_seed}: latencies {latency_seed} changed an outcome"
            );
        }
        for o in reference.values() {
            seen.insert(match o {
                Outcome::Saved { route, .. } => format!("saved:{route:?}"),
                other => serde_json::to_value(other)
                    .ok()
                    .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
                    .unwrap_or_default(),
            });
        }
    }
    assert!(
        seen.contains("saved:Oa") && seen.contains("saved:Inst"),
        "both routes are exercised: {seen:?}"
    );
    assert!(
        seen.contains("not_entitled"),
        "and some items are not held: {seen:?}"
    );
}

#[test]
fn a_cancel_at_a_random_moment_breaks_nothing() {
    for seed in 1..=40u64 {
        let knobs = Knobs {
            kind_seed: seed,
            latency_seed: seed * 31 + 5,
            stable: false,
        };
        let (d, fuzz) = batch_world(knobs);
        let mut host = run_scenario(fuzz);
        start(&mut host, 1, Mode::Full, true, items(&d));
        let at = pick(seed, "cancel-at", "") * 700;
        run_for(&mut host, at);
        let command = if pick(seed, "cancel-what", "") < 50 {
            Command::CancelRun { run: 1 }
        } else {
            Command::CancelItem {
                run: 1,
                item: u32::try_from(1 + pick(seed, "cancel-item", "") % u64::from(BATCH))
                    .unwrap_or(1),
            }
        };
        let run_cancelled = matches!(command, Command::CancelRun { .. });
        host.step(vec![command]).expect("cancel");
        if !run_cancelled {
            // the rest of the batch carries on and the run still ends with one report
            settle(&mut host);
            assert_whole(&host, knobs);
        } else {
            settle(&mut host);
            assert_eq!(host.reports.len(), 1, "seed {seed}");
            assert!(host.pending.is_empty());
        }
    }
}
