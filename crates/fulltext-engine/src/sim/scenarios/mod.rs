//! Failure-catalogue scenarios (docs/design/candidates/C-failure-first.md section 1, F1-F32) and scheduler
//! scenarios, each a `#[test]` driving `SimHost` through the real engine (run manager, scheduler, login gate,
//! pipeline) with a scripted `Responder` and `invariants::standard()` checking every step.
//!
//! * `run.rs`: F17 F18 F20 F21 F22 F23 F27: shutdown / duplicates / cancel / offline / group libraries / save
//!   failures, driven through `StartRun`.
//! * `inst.rs`: the institutional route end to end through the run manager: F1 (one sign-in window for twenty
//!   items), F11 (429 stops the route), F24 (sign-in window ignored), F25 (sign-in that does not stick), F26
//!   (licence budget), plus the golden traces.
//! * `sched.rs`: scheduler-only scenarios (one request per family, global 2, PDF spacing, ledger caps across an
//!   engine restart, reserve-then-commit, cancelled-after-sent tickets, slots released while parked).
//! * `property.rs`: seeded random completion orders and latencies over 20-item batches.
//!
//! The other rows of the catalogue (F2-F10, F12-F16, F19, F28-F32) are exercised by the unit tests of the
//! module that owns them (`pipeline/attempt.rs`, `pipeline/inst.rs`, `login.rs`, `sched/**`).
//!
//! # Golden traces
//! `tests/golden/*.json` hold the effect log of three canonical scenarios, one `{"at", "effect"}` entry per line.
//! A mismatch fails the test; regenerate with `ZFT_UPDATE_GOLDEN=1 cargo test -p fulltext-engine golden` and
//! review the diff. A missing file also fails: goldens are never created silently.
mod inst;
mod property;
mod run;
mod sched;

use crate::proto::*;
use crate::sim::fixtures::{
    api_found, body, browser_ok, candidate, http_ok, login_ok, pdf_inspection, save_ok,
    test_config, translators_ok,
};
use crate::sim::invariants::standard;
use crate::sim::{PendingOp, Reply, Responder, SimEnd, SimHost};
use fulltext_core::model::*;
use serde_json::json;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

// ------------------------------------------------------------------------------------------------ scenery

pub(super) const ALMA_HOST: &str = "librarysearch.kcl.ac.uk";
pub(super) const IDP: &str =
    "https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/saml2";
pub(super) const PUBLISHER_PAGE: &str = "https://www.bmj.com/content/372/bmj.n71";

/// Publisher DOI prefixes spread over ten institutional families (see `sched::hosts::FAMILIES`).
const PREFIXES: [&str; 10] = [
    "10.1136", "10.1016", "10.1007", "10.1093", "10.1097", "10.1056", "10.1001", "10.1080",
    "10.1177", "10.1017",
];

/// `n` distinct DOIs (`tag` keeps the sets of different scenarios apart), one family per DOI, round robin.
pub(super) fn dois(tag: &str, n: u32) -> Vec<String> {
    (1..=n)
        .map(|i| {
            format!(
                "{}/{tag}.{i}",
                PREFIXES
                    .get(usize::try_from(i).unwrap_or(0) % PREFIXES.len())
                    .copied()
                    .unwrap_or("10.1136")
            )
        })
        .collect()
}

/// User-library items `1..=n`, one per DOI.
pub(super) fn items(dois: &[String]) -> Vec<ItemSnapshot> {
    dois.iter()
        .enumerate()
        .map(|(i, doi)| crate::sim::fixtures::item(u32::try_from(i + 1).unwrap_or(u32::MAX), doi))
        .collect()
}

/// Where the open-access copy of `doi` lives: one host per DOI, so OA host gates never interfere.
pub(super) fn oa_pdf_url(doi: &str) -> String {
    let slug: String = doi
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("https://oa-{slug}.example.org/{slug}.pdf")
}

/// Where the publisher serves the PDF that campus access links to.
pub(super) fn pub_pdf_url(doi: &str) -> String {
    let slug: String = doi
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("https://pub-{slug}.example.org/{slug}.full.pdf")
}

pub(super) fn idp_class() -> PageClass {
    PageClass::IdpLogin {
        idp: Idp::Entra,
        ca_error: None,
    }
}

pub(super) fn page_inspection_of(class: PageClass) -> Inspection {
    Inspection::Page(PageInspection {
        class,
        page_ids: PageIds::default(),
        doi_match: Some(true),
        access: AccessHint::Unknown,
        candidates: Vec::new(),
        rejected: Vec::new(),
        rule: None,
    })
}

/// An HTTP answer that is a page of `class` (no body).
pub(super) fn page(after: u64, url: &str, class: PageClass) -> Reply {
    Reply::ok(after, http_ok(200, url, &page_inspection_of(class), None))
}

pub(super) fn alma_reply(held: bool) -> Reply {
    let status = if held {
        ApiStatus::Found
    } else {
        ApiStatus::Empty
    };
    let insp = Inspection::Api(ApiInspection {
        source: Source::Alma,
        status,
        learned: LearnedIds::default(),
        candidates: Vec::new(),
        oa_status: None,
        note: None,
        retry_after_s: None,
    });
    Reply::ok(
        60,
        http_ok(
            200,
            "https://librarysearch.kcl.ac.uk/view/uresolver/44KCL_INST/openurl",
            &insp,
            None,
        ),
    )
}

// ------------------------------------------------------------------------------------------------- World

/// What the sites say about one DOI.
#[derive(Clone, Debug)]
pub(super) struct Doc {
    pub doi: String,
    /// OpenAlex knows a downloadable open-access PDF.
    pub oa: bool,
    /// Alma says the library holds it.
    pub held: bool,
    /// doi.org itself lands on a page with a PDF link (campus access): no sign-in is ever needed.
    pub direct: bool,
}

/// A hook sees every new op before the default behaviour does; `Some` overrides the answer.
pub(super) type Hook = Box<dyn FnMut(&PendingOp, u64) -> Option<Reply>>;

/// A model of the sites behind the engine: OpenAlex, open-access hosts, Alma, doi.org ("no access" off campus),
/// the OpenAthens redirector (bounces to the Entra sign-in until a window landed, then hands out the PDF), the
/// sign-in window, the save. Everything else (other lookups) fails fast with a network error.
pub(super) struct World {
    docs: Vec<Doc>,
    pub session: Rc<Cell<bool>>,
    handles: u32,
    pub api_ms: u64,
    pub pdf_ms: u64,
    pub load_ms: u64,
    pub window_ms: u64,
    pub save_ms: u64,
    /// How a sign-in window ends.
    pub window: LoginResult,
    /// The user's answer to the confirmation prompts.
    pub confirm: ConfirmChoice,
    hooks: Vec<Hook>,
}

impl World {
    pub(super) fn new(docs: Vec<Doc>) -> Self {
        Self {
            docs,
            session: Rc::new(Cell::new(false)),
            handles: 1_000,
            api_ms: 100,
            pdf_ms: 400,
            load_ms: 800,
            window_ms: 3_000,
            save_ms: 100,
            window: LoginResult::Landed,
            confirm: ConfirmChoice::Yes,
            hooks: Vec::new(),
        }
    }

    /// Every DOI is only available through the institution.
    pub(super) fn inst_only(dois: &[String]) -> Self {
        Self::new(
            dois.iter()
                .map(|d| Doc {
                    doi: d.clone(),
                    oa: false,
                    held: true,
                    direct: false,
                })
                .collect(),
        )
    }

    /// Every DOI is held and doi.org lands on the article straight away (campus network).
    pub(super) fn on_campus(dois: &[String]) -> Self {
        Self::new(
            dois.iter()
                .map(|d| Doc {
                    doi: d.clone(),
                    oa: false,
                    held: true,
                    direct: true,
                })
                .collect(),
        )
    }

    /// Every DOI has an open-access copy.
    pub(super) fn open_access(dois: &[String]) -> Self {
        Self::new(
            dois.iter()
                .map(|d| Doc {
                    doi: d.clone(),
                    oa: true,
                    held: true,
                    direct: false,
                })
                .collect(),
        )
    }

    pub(super) fn hook(
        mut self,
        hook: impl FnMut(&PendingOp, u64) -> Option<Reply> + 'static,
    ) -> Self {
        self.hooks.push(Box::new(hook));
        self
    }

    /// The document an URL is about: the longest known DOI it mentions, plain or percent-encoded.
    fn doc_of(&self, url: &str) -> Option<&Doc> {
        self.docs
            .iter()
            .filter(|d| url.contains(&d.doi) || url.contains(&d.doi.replace('/', "%2F")))
            .max_by_key(|d| d.doi.len())
    }

    fn next_handle(&mut self) -> u32 {
        self.handles += 1;
        self.handles
    }

    fn http(&mut self, req: &HttpReq) -> Reply {
        let url = req.url.as_str();
        let doc = self.doc_of(url).cloned();
        if url.contains("api.openalex.org") {
            let Some(doc) = doc else {
                return Reply::err(10, OpErrKind::Network);
            };
            let insp = if doc.oa {
                let mut c = candidate(&oa_pdf_url(&doc.doi), CandKind::Pdf, Source::Openalex, 0);
                c.record_doi = Some(doc.doi.clone());
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
            return Reply::ok(self.api_ms, http_ok(200, url, &insp, None));
        }
        if url.contains(ALMA_HOST) {
            return alma_reply(doc.is_some_and(|d| d.held));
        }
        if url.starts_with("https://doi.org/") {
            let Some(doc) = doc.filter(|d| d.direct) else {
                return page(80, PUBLISHER_PAGE, PageClass::AccessDenied);
            };
            let mut link = candidate(&pub_pdf_url(&doc.doi), CandKind::Pdf, Source::Extractor, 0);
            link.record_doi = Some(doc.doi.clone());
            let insp = Inspection::Page(PageInspection {
                class: PageClass::Landing,
                page_ids: PageIds::default(),
                doi_match: Some(true),
                access: AccessHint::Granted,
                candidates: vec![link],
                rejected: Vec::new(),
                rule: None,
            });
            let handle = self.next_handle();
            return Reply::ok(
                80,
                http_ok(200, PUBLISHER_PAGE, &insp, Some(body(handle, 5_000))),
            );
        }
        if url.contains("://oa-") || url.contains("://pub-") {
            let handle = self.next_handle();
            return Reply::ok(
                self.pdf_ms,
                http_ok(
                    200,
                    url,
                    &pdf_inspection(80_000),
                    Some(body(handle, 80_000)),
                ),
            );
        }
        Reply::err(10, OpErrKind::Network)
    }

    fn browser(&mut self) -> Reply {
        if self.session.get() {
            let handle = self.next_handle();
            Reply::ok(
                self.load_ms,
                browser_ok(
                    PUBLISHER_PAGE,
                    &pdf_inspection(80_000),
                    Some(body(handle, 80_000)),
                    None,
                ),
            )
        } else {
            Reply::ok(
                self.load_ms,
                browser_ok(IDP, &page_inspection_of(idp_class()), None, None),
            )
        }
    }
}

impl Responder for World {
    fn respond(&mut self, op: &PendingOp, now: u64, _wall: u64) -> Reply {
        for hook in &mut self.hooks {
            if let Some(reply) = hook(op, now) {
                return reply;
            }
        }
        match &op.effect {
            Effect::Http { req, .. } => self.http(req),
            Effect::BrowserLoad { .. } => self.browser(),
            Effect::LoginWindow { req, .. } => {
                if req.kind == LoginKind::Login
                    && matches!(self.window, LoginResult::Landed | LoginResult::Pdf)
                {
                    self.session.set(true);
                }
                Reply::ok(
                    self.window_ms,
                    login_ok(self.window, PUBLISHER_PAGE, &Inspection::None, None, None),
                )
            }
            Effect::SavePdf { req, .. } => {
                Reply::ok(self.save_ms, save_ok(&format!("1/K{:04}", req.item)))
            }
            Effect::Confirm { .. } => Reply::ok(300, json!({ "choice": self.confirm })),
            Effect::ZoteroOa { .. } => Reply::ok(60, json!([])),
            Effect::RunTranslators { .. } => Reply::ok(100, translators_ok(None)),
            _ => Reply::Never,
        }
    }
}

// ----------------------------------------------------------------------------------------------- driving

/// `SimHost` with the production engine, `config` applied, every invariant on.
pub(super) fn run_scenario_with(
    config: EngineConfig,
    persisted: Persisted,
    responder: impl Responder + 'static,
) -> SimHost {
    let mut host = SimHost::new(Box::new(responder)).with_invariants(standard());
    host.configure(config, persisted, 7).expect("configure");
    host
}

/// The usual setup: KCL-like config, empty persisted state.
pub(super) fn run_scenario(responder: impl Responder + 'static) -> SimHost {
    run_scenario_with(test_config(), Persisted::default(), responder)
}

/// `StartRun` for `items` (no resume hints).
pub(super) fn start(
    host: &mut SimHost,
    run: RunId,
    mode: Mode,
    interactive: bool,
    items: Vec<ItemSnapshot>,
) -> StepOut {
    host.step(vec![Command::StartRun {
        run,
        mode,
        interactive,
        items,
        resume: Vec::new(),
    }])
    .expect("start run")
}

/// Run until nothing can happen any more (two virtual hours at most) and check the end-of-scenario invariants.
pub(super) fn settle(host: &mut SimHost) {
    assert_eq!(
        host.run_until_idle(2 * 3_600_000)
            .expect("the simulation ran"),
        SimEnd::Idle,
        "the scenario ends"
    );
    host.finish().expect("end-of-scenario invariants");
}

/// Run for `ms` of virtual time (the scenario is not over).
pub(super) fn run_for(host: &mut SimHost, ms: u64) {
    host.run_until_idle(ms).expect("the simulation ran");
}

pub(super) fn report(host: &SimHost, run: RunId) -> &RunReport {
    host.reports
        .iter()
        .find(|r| r.run == run)
        .unwrap_or_else(|| {
            panic!(
                "run {run} has no report; reports: {:?}",
                host.reports.iter().map(|r| r.run).collect::<Vec<_>>()
            )
        })
}

pub(super) fn outcome(host: &SimHost, run: RunId, item: ItemId) -> Outcome {
    report(host, run)
        .items
        .iter()
        .find(|i| i.item == item)
        .map(|i| i.outcome.clone())
        .unwrap_or_else(|| panic!("item {item} is not in the report of run {run}"))
}

pub(super) fn outcomes(host: &SimHost, run: RunId) -> BTreeMap<ItemId, Outcome> {
    report(host, run)
        .items
        .iter()
        .map(|i| (i.item, i.outcome.clone()))
        .collect()
}

/// Number of logged effects that match.
pub(super) fn count(host: &SimHost, f: impl Fn(&Effect) -> bool) -> usize {
    host.effects().filter(|e| f(e)).count()
}

/// Ids of the progress rows an item showed, in order.
pub(super) fn rows(host: &SimHost, run: RunId, item: ItemId) -> Vec<String> {
    host.effects()
        .filter_map(|e| match e {
            Effect::Progress {
                run: r,
                item: i,
                row,
            } if *r == run && *i == item => Some(row.l10n.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn is_saved(o: &Outcome, route: Route) -> bool {
    matches!(o, Outcome::Saved { route: r, .. } if *r == route)
}

/// `(committed, reserved)` entry counts of the last persisted ledger.
pub(super) fn ledger_counts(host: &SimHost) -> (usize, usize) {
    let state: LedgerState = host
        .persisted
        .get(&PersistKey::Ledger)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    (state.committed.len(), state.reserved.len())
}

/// Every effect of the log as JSON, one line per entry, in order.
pub(super) fn trace_lines(host: &SimHost) -> Vec<String> {
    host.log
        .iter()
        .map(|l| {
            serde_json::to_string(&json!({ "at": l.at, "effect": l.effect })).unwrap_or_default()
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------- goldens

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.json"))
}

/// Compare the effect log of `host` with `tests/golden/<name>.json` (see the module docs).
pub(super) fn assert_golden(name: &str, host: &SimHost) {
    let lines = trace_lines(host);
    let text = format!("[\n{}\n]\n", lines.join(",\n"));
    let path = golden_path(name);
    if std::env::var("ZFT_UPDATE_GOLDEN").is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().expect("golden dir has a parent"))
            .expect("create tests/golden");
        std::fs::write(&path, &text).expect("write the golden trace");
        return;
    }
    let stored = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| {
            panic!(
                "golden trace {} is missing ({e}); create it deliberately with ZFT_UPDATE_GOLDEN=1",
                path.display()
            )
        })
        .replace("\r\n", "\n");
    if stored != text {
        let first = stored
            .lines()
            .zip(text.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| stored.lines().count().min(text.lines().count()));
        panic!(
            "effect log differs from {} at entry {first} (stored {} entries, now {}):\n  stored: {}\n  now:    {}\nRegenerate with ZFT_UPDATE_GOLDEN=1 if the change is intended.",
            path.display(),
            stored.lines().count().saturating_sub(2),
            lines.len(),
            stored.lines().nth(first).unwrap_or("<end>"),
            text.lines().nth(first).unwrap_or("<end>"),
        );
    }
}

/// The effect log as one comparable string (determinism checks).
pub(super) fn trace_text(host: &SimHost) -> String {
    trace_lines(host).join("\n")
}
