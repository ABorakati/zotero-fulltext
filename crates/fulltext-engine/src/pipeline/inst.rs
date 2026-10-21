//! The institutional route (King's College London: OpenAthens redirector, EZproxy for configured hosts, Alma
//! entitlement check, direct/campus access). Called by `item::run_item` after `claim_inst_slot()` is `Granted`.
//!
//! # Steps (research/SUMMARY (c); state diagram in docs/design/engine.md section 7)
//! 0. **Gates before any request**: route halted -> `Halted`; no DOI and no http(s) item URL -> `NotFound
//!    { NotHeld }`. Every route aims at one target: `https://doi.org/<doi>`, else the item's un-proxied URL.
//! 1. **Entitlement** (`prefs.check_alma`, needs a DOI and `institution.alma`): cached `cx.alma_known(doi)`, else
//!    one GET of the Alma CTO URL (`Source::Alma`, gated by `acquire_api(Alma)`); no `getFullTxt` service ->
//!    `Outcome::NotEntitled { handoff }`. An Alma error continues (unknown = try). Group libraries ask for
//!    consent here, before the first licence reservation.
//! 2. **Direct** (`prefs.try_direct`): `https://doi.org/<doi>` through `attempt::fetch_candidate` with
//!    `FetchPolicy::inst(InstKey::for_doi(doi))` (campus IP or an existing publisher session). A page that says
//!    "no access" is remembered (`direct_denied`) and never retried.
//! 3. **OpenAthens** (`prefs.use_openathens`): `cx.session_admit(Openathens)`; then hidden `browser_load` of
//!    `pure::redirector_url(inst, target)` with `Ua::Plain`, `capture_pdf`, `hold_while [OpenathensRedirector]`,
//!    `barrier: true`, under `acquire_inst(key, Page)` + `acquire_browser()` and a licence reservation (rolled
//!    back unless the load captured a PDF). The settled page decides: publisher landing -> `session_report(
//!    LoggedIn)` + extract (landing.rs) + `fetch_candidate` the PDFs; `IdpLogin` / `SamlAutoPost` / a stuck SSO
//!    hop -> `session_report(NeedsLogin)`, release the item slot (`pipeline::parked`) and `cx.session_login(..)`;
//!    verdict `Retry { epoch }` -> repeat the load ONCE (a second bounce is `Ineffective`); `Declined` /
//!    `TimedOut` / `Refused` / `Ineffective` / `Misconfigured` -> `NeedsUser` with the matching `NeedsWhy`;
//!    `Suspended` -> `Halted`. `OpenathensUnknownResource` -> next route; `OpenathensBadOrg` -> `Misconfigured`;
//!    `OpenathensWait` (the browser gave up on the "Please wait..." page) is HEAD-polled every
//!    `OPENATHENS_POLL_MS` up to `OPENATHENS_POLL_MAX_MS` (201, or 302 + Location, means ready), then reloaded.
//!    Publisher pages the redirector does not know (`Institution.passthrough_hosts`) skip this step.
//!    An institution without an OpenAthens domain (`Institution.openathens_domain` blank) has no such step: no
//!    redirector URL is ever built from an empty domain.
//! 4. **SAML / Shibboleth** (`prefs.use_saml`, `Institution.saml_entity_id`): the publisher's own "institutional
//!    login" for institutions that are not on OpenAthens (or whose OpenAthens does not know the publisher). The
//!    entry is the publisher WAYFless deep link `pure::wayfless_url(..)` for the first page this item reached, else
//!    the item URL, else the article its DOI stands for (`fulltext_core::wayfless`); an unsupported publisher skips
//!    the step (`Miss::NotKnown`). It runs only while nothing more specific than "not known" was learnt (a working
//!    OpenAthens session that was refused, found no PDF link or met a bot check would get the same answer from the
//!    same identity provider). The machinery is the OpenAthens one with its own realm, `Realm::Saml`: silent hidden
//!    load (plain UA, `hold_while [PublisherSsoHop]`), the single-flight login gate, and a visible window only when
//!    the load ends on an interactive IdP page (`IdpLogin`, which core classifies for any Shibboleth / SAML IdP).
//! 5. **EZproxy** (`prefs.use_ezproxy`): only for a known publisher host (the page a route reached, else the OA
//!    phase's publisher hint, else the item URL; none known: the step is skipped, except with
//!    `ezproxy_all_hosts` where `doi.org` stands in for a DOI) that is cached configured by `cx.ezproxy_known` (7
//!    days; what the proxy said last beats the list), in `Institution.ezproxy_hosts`, or confirmed by a no-follow
//!    probe of the proxified host; entry `pure::ezproxy_login_url` through the same hidden-browser machinery with
//!    `Realm::Ezproxy`. With `Institution.ezproxy_all_hosts` every host is tried (the probe answering "not
//!    configured" or the proxy's own needhost / not-configured page is what excludes one: it is learnt `false` and
//!    skipped for 7 days, a page reached through the proxy is learnt `true`).
//!    `EzproxyHostNotConfigured` -> learn `false`, skip; `EzproxyError { suspend }` / `LicenceStop` ->
//!    `halt_inst` and `Outcome::Halted`.
//! 6. Every failure carries `handoff::handoff(..)` links (including `wayfless_url`).
//!
//! # Configuration check (step 0)
//! An institution with no usable route at all (no direct access, no OpenAthens domain, no `saml_entity_id`, no
//! EZproxy, or those routes switched off in the preferences) ends as `NeedsUser { Misconfigured }` before any
//! request; the row text is `zotero-fulltext-status-misconfigured`.
//!
//! # Limits enforced here (the gates enforce the rest)
//! * At most `INST_FETCH_BUDGET_PER_ITEM` fetches (every `fetch_candidate` call and every hidden-browser load
//!   counts 1; a landing page without Rust candidates counts 1 more for the translators); then `NeedsUser
//!   { NoPdfLink }`. Alma, the EZproxy probe and the OpenAthens HEAD polls are not publisher fetches.
//! * Stop signals: 429, suspend page, licence-stop text, a second challenge on a host, the third hard block ->
//!   `cx.halt_inst(..)` and `Outcome::Halted`. No retries on the institutional route except the single
//!   post-login retry.
//! * Never hedge. Never fetch two PDFs of one family concurrently (the gate does that).
//! * Saves go through `save::save_pdf_blob` with `route = Inst`; the stored URL is un-proxied. Every PDF that
//!   reaches a save carries a licence ticket: a page fetch that turned out to be the PDF (`doi.org` answering with
//!   the file) reserved nothing beforehand, so the ticket is taken when the bytes arrive (a spent cap then defers
//!   the item instead of saving a download the ledger would not know).
//! * Why the item ends the way it does: [`Miss`] keeps the most informative failure seen (a refusal beats a bot
//!   check beats "no PDF link on the page" beats "the redirector does not know the publisher"); a walk over a
//!   page's PDF links that ends without a more specific reason is "no usable PDF link".
//!
//! # Design notes
//! * The route owns a fresh `CandidatePool` for its page visits: a landing page the OA phase saw without
//!   credentials must be extractable again once the session works. The OA pool is only read, for the Crossref
//!   (`Source::Doi`) PDF links that `attempt_oa` leaves for this phase and for a publisher-host hint.
//! * The three calls into other work packages (`fetch_candidate`, `extract_landing`, `save_pdf_blob`) and the
//!   group-library prompt go through the private [`Collab`] trait so that tests can script them (the tests below
//!   script the group-library answer and the save); the production implementation, [`Real`], is a plain forward.
//! * Hard-block signals have no scheduler counter, so [`Inst::note_block_signal`] borrows the 3-failures-in-a-row
//!   breaker of an OA API gate (`BLOCK_SIGNAL_GATE`, a source nothing else acquires).
use crate::cx::ItemCx;
use crate::l10n::{self, row};
use crate::login::{Admission, LoginRequest, ProbeResult, SessionVerdict};
use crate::ops::{Body, BrowserResp, LoginResp, TimedOut};
use crate::pipeline::attempt::{self, FetchEnd, FetchPolicy, LandingPage, PdfBlob, fail};
use crate::pipeline::handoff::{handoff, handoff_with_landing};
use crate::pipeline::landing::{self, LandingResult, class_name};
use crate::pipeline::pool::CandidatePool;
use crate::pipeline::{Slot, parked, pure, requests, save};
use crate::proto::*;
use crate::sched::hosts::HaltReason;
use crate::sched::policy::{self, FetchClass};
use crate::sched::{FetchKind, InstKey, LedgerTicket, Refused};
use fulltext_core::model::*;
use futures_util::FutureExt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::ops::ControlFlow::{self, Break, Continue};
use std::pin::Pin;

/// Fetch budget of one item on this route.
const BUDGET: u32 = policy::INST_FETCH_BUDGET_PER_ITEM;
/// Upper bound for the Alma CTO answer (about 20 KB in practice).
const ALMA_MAX_BYTES: u64 = 1024 * 1024;
/// The URL must be stable this long for a hidden load to count as settled.
const SETTLE_MS: u32 = 3_000;
/// Host of the OpenAthens redirector (timeout class lookups).
const REDIRECTOR_HOST: &str = "go.openathens.net";
/// A Wiley-class host: the slow timeout profile applies to loads that end on that family.
const SLOW_HOST: &str = "onlinelibrary.wiley.com";
/// Pseudo OA-API gate whose breaker counts hard-block signals (see the module docs).
const BLOCK_SIGNAL_GATE: Source = Source::Extractor;

/// How a step ends: `Break` carries the item's final outcome (success included), `Continue` hands over to the
/// next route / step.
type Flow<T = ()> = ControlFlow<Outcome, T>;

/// Turn a gate refusal into the flow it ends in (`?` does not convert `Result` into `ControlFlow`).
fn brk<T>(r: Result<T, Outcome>) -> Flow<T> {
    match r {
        Ok(v) => Continue(v),
        Err(outcome) => Break(outcome),
    }
}

/// Why no route produced a PDF: the best explanation seen, ordered from least to most informative. It decides
/// the terminal outcome when every route is exhausted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Miss {
    #[default]
    Other,
    Network,
    Timeout,
    /// The redirector / EZproxy do not know the publisher.
    NotKnown,
    /// A publisher page was reached but carried no usable PDF link.
    NoPdfLink,
    /// A bot check the user can clear in a browser: the most useful thing to tell about a copy that exists.
    Challenge,
    /// The publisher said "no access" to a working session.
    Denied,
}

/// What one candidate fetch means for the route, after the stop signals were handled.
enum Got {
    Pdf(PdfBlob),
    Landing(LandingPage),
    Denied,
    /// The publisher / proxy asked for a sign-in: the session is missing.
    Bounce,
    Miss,
}

/// How `try_pdfs` ended.
enum Tried {
    Exhausted,
    Bounce,
}

/// PDF candidates a landing page offered.
struct Harvest {
    pdfs: Vec<Candidate>,
    denied: bool,
}

/// What a hidden-browser load produced.
enum Loaded {
    Pdf(PdfBlob),
    Page(Landed),
    Failed,
}

/// A page the hidden browser settled on.
struct Landed {
    class: PageClass,
    inspection: PageInspection,
    page: Option<Body>,
    final_url: String,
}

/// What a settled page means for the session (`seen`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    Reached,
    NeedsLogin,
    Wait,
    UnknownResource,
    BadOrg,
    NotConfigured,
    Suspended,
    RateLimited(Option<u32>),
    LicenceStop,
    Blocked,
    Challenge(bool),
    Denied,
    Missing,
}

/// Classify a settled page. Total: every `PageClass` has an answer.
fn seen(class: &PageClass) -> Seen {
    match class {
        PageClass::IdpLogin { .. }
        | PageClass::SamlAutoPost
        | PageClass::PublisherSsoHop
        | PageClass::EzproxyNeedLogin => Seen::NeedsLogin,
        PageClass::OpenathensWait => Seen::Wait,
        PageClass::OpenathensUnknownResource => Seen::UnknownResource,
        PageClass::OpenathensBadOrg => Seen::BadOrg,
        PageClass::EzproxyHostNotConfigured => Seen::NotConfigured,
        PageClass::EzproxyError { suspend: true } => Seen::Suspended,
        PageClass::EzproxyError { suspend: false } | PageClass::NotFound => Seen::Missing,
        PageClass::RateLimited { retry_after_s } => Seen::RateLimited(*retry_after_s),
        PageClass::LicenceStop => Seen::LicenceStop,
        PageClass::Blocked { .. } => Seen::Blocked,
        PageClass::Challenge { interactive, .. } => Seen::Challenge(*interactive),
        PageClass::AccessDenied => Seen::Denied,
        PageClass::Pdf { .. }
        | PageClass::Landing
        | PageClass::Html
        | PageClass::Redirect { .. }
        | PageClass::ReaderShell { .. } => Seen::Reached,
    }
}

/// A page the classifier calls reachable can still be a sign-in or redirector page by its URL (a settle that
/// gave up early). The URL class overrules `Reached`.
fn refine(kind: UrlClassKind) -> Seen {
    match kind {
        UrlClassKind::Other => Seen::Reached,
        UrlClassKind::OpenathensRedirector => Seen::Missing,
        UrlClassKind::IdpLogin | UrlClassKind::EzproxyLogin | UrlClassKind::PublisherSsoHop => {
            Seen::NeedsLogin
        }
    }
}

/// Alma CTO answer: `Found` = held, `Empty` = not held, anything else = unknown.
fn alma_answer(i: &Inspection) -> Option<bool> {
    match i {
        Inspection::Api(a) if a.source == Source::Alma => match a.status {
            ApiStatus::Found => Some(true),
            ApiStatus::Empty => Some(false),
            ApiStatus::NotFound | ApiStatus::Throttled | ApiStatus::Unparseable => None,
        },
        _ => None,
    }
}

/// The CTO request: `{base}/view/uresolver/{inst}/openurl?...&rft_id=info:doi/{doi}&svc_dat=CTO`.
fn alma_cto_url(alma: &Alma, doi: &str) -> String {
    let base = alma.base.trim_end_matches('/');
    let doi_path = doi
        .split('/')
        .map(pure::pct_encode)
        .collect::<Vec<_>>()
        .join("/");
    format!(
        "{base}/view/uresolver/{}/openurl?ctx_ver=Z39.88-2004&url_ver=Z39.88-2004&rft_id=info:doi/{doi_path}&svc_dat=CTO",
        pure::pct_encode(&alma.inst)
    )
}

/// What a no-follow probe of a proxified host says about the EZproxy configuration. `None` = cannot tell.
fn probe_configured(class: &PageClass) -> Option<bool> {
    match class {
        PageClass::EzproxyNeedLogin
        | PageClass::Landing
        | PageClass::Html
        | PageClass::Pdf { .. } => Some(true),
        PageClass::EzproxyHostNotConfigured => Some(false),
        _ => None,
    }
}

/// One answer of the OpenAthens "Please wait..." HEAD poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PollEnd {
    Ready,
    Unknown,
    BadOrg,
    GaveUp,
}

/// 201, or a redirect with a `Location`, means the redirector has its answer; 404 / 403 are final; anything
/// else (200: still waiting) is `None`.
fn poll_status(status: u16, location: Option<&String>) -> Option<PollEnd> {
    match status {
        201 => Some(PollEnd::Ready),
        301 | 302 | 303 | 307 | 308 if location.is_some_and(|l| !l.trim().is_empty()) => {
            Some(PollEnd::Ready)
        }
        404 => Some(PollEnd::Unknown),
        403 => Some(PollEnd::BadOrg),
        _ => None,
    }
}

/// `www.x.org` and `x.org` are the same host for a hand-written host list.
fn same_host(a: &str, b: &str) -> bool {
    let norm = |h: &str| {
        h.trim()
            .to_ascii_lowercase()
            .trim_start_matches("www.")
            .to_owned()
    };
    norm(a) == norm(b)
}

/// Whether the configuration leaves the institutional route anything to do: direct access (campus IP or an
/// existing publisher session), or a sign-in route that is switched on AND has its settings (OpenAthens domain,
/// SAML `entityID`, EZproxy login host and suffix).
fn has_usable_route(cfg: &EngineConfig) -> bool {
    let (prefs, inst) = (&cfg.inst, &cfg.institution);
    prefs.try_direct
        || (prefs.use_openathens && pure::has_openathens(inst))
        || (prefs.use_saml && pure::saml_entity(inst).is_some())
        || (prefs.use_ezproxy && pure::has_ezproxy(inst))
}

/// What every route aims at: the DOI URL, else the item's own URL with proxy / redirector wrappers removed.
fn target_of(inst: &Institution, ids: &Ids) -> Option<String> {
    if let Some(doi) = ids.doi.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        return Some(pure::doi_url(doi));
    }
    let url = pure::unproxify(inst, ids.url.as_deref()?, &BTreeMap::new());
    pure::host_of(&url).map(|_| url)
}

// ------------------------------------------------------------------------------------------------ collaborators

type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// The pipeline calls this route makes into other work packages. Production forwards to them ([`Real`]); the
/// tests script them so that this module's decisions are tested on their own.
trait Collab {
    fn fetch<'a>(
        &'a self,
        cx: &'a ItemCx,
        cand: &'a Candidate,
        pol: &'a FetchPolicy,
        pool: &'a mut CandidatePool,
    ) -> Fut<'a, FetchEnd>;
    fn extract<'a>(
        &'a self,
        cx: &'a ItemCx,
        page: Option<&'a Body>,
        url: &'a str,
        inspection: &'a PageInspection,
        pool: &'a mut CandidatePool,
    ) -> Fut<'a, LandingResult>;
    fn save<'a>(&'a self, cx: &'a ItemCx, blob: PdfBlob, ids: &'a Ids) -> Fut<'a, Outcome>;
    /// The run's one answer to "may institutional access be used for this group library?".
    fn group_consent<'a>(&'a self, cx: &'a ItemCx) -> Fut<'a, bool>;
}

struct Real;

impl Collab for Real {
    fn fetch<'a>(
        &'a self,
        cx: &'a ItemCx,
        cand: &'a Candidate,
        pol: &'a FetchPolicy,
        pool: &'a mut CandidatePool,
    ) -> Fut<'a, FetchEnd> {
        Box::pin(attempt::fetch_candidate(cx, cand, pol, pool))
    }

    fn extract<'a>(
        &'a self,
        cx: &'a ItemCx,
        page: Option<&'a Body>,
        url: &'a str,
        inspection: &'a PageInspection,
        pool: &'a mut CandidatePool,
    ) -> Fut<'a, LandingResult> {
        Box::pin(landing::extract_landing(
            cx,
            page,
            url,
            inspection,
            pool,
            Route::Inst,
        ))
    }

    fn save<'a>(&'a self, cx: &'a ItemCx, blob: PdfBlob, ids: &'a Ids) -> Fut<'a, Outcome> {
        Box::pin(save::save_pdf_blob(cx, blob, ids))
    }

    fn group_consent<'a>(&'a self, cx: &'a ItemCx) -> Fut<'a, bool> {
        Box::pin(cx.group_consent())
    }
}

// -------------------------------------------------------------------------------------------------- probe guard

/// The right to load the redirector / EZproxy as the realm's prober. Every load must end in a report; a guard
/// dropped without one (the item was cancelled, a step bailed out) reports `Failed`, which hands the probe to
/// the next caller instead of making the others wait for the stale-probe timer.
struct Probe<'a> {
    cx: &'a ItemCx,
    realm: Realm,
    epoch: u32,
    done: bool,
}

impl<'a> Probe<'a> {
    fn new(cx: &'a ItemCx, realm: Realm, epoch: u32) -> Self {
        Self {
            cx,
            realm,
            epoch,
            done: false,
        }
    }

    fn report(mut self, result: ProbeResult) {
        self.done = true;
        self.cx.session_report(self.realm, self.epoch, result);
    }
}

impl Drop for Probe<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.cx
                .session_report(self.realm, self.epoch, ProbeResult::Failed);
        }
    }
}

// ------------------------------------------------------------------------------------------------------ the route

/// Run the institutional route to a terminal outcome.
pub(crate) async fn run_institutional(
    cx: &ItemCx,
    slot: &mut Slot,
    ids: &Ids,
    pool: &mut CandidatePool,
) -> Outcome {
    run_with(cx, slot, ids, pool, &Real).await
}

async fn run_with<'a>(
    cx: &'a ItemCx,
    slot: &'a mut Slot,
    ids: &'a Ids,
    pool: &CandidatePool,
    pipe: &'a dyn Collab,
) -> Outcome {
    match Inst::begin(cx, slot, ids, pool, pipe) {
        Ok(inst) => inst.run().await,
        Err(done) => *done,
    }
}

/// The state of one item's institutional attempt.
struct Inst<'a> {
    cx: &'a ItemCx,
    ids: &'a Ids,
    pipe: &'a dyn Collab,
    slot: &'a mut Slot,
    /// Pages visited and URLs tried in THIS phase (fresh: see the module docs).
    visits: CandidatePool,
    /// Crossref `Source::Doi` PDF links from the OA phase, tried after a publisher page was reached.
    crossref: Vec<Candidate>,
    /// A publisher host the OA phase knew about (for the EZproxy and pass-through decisions).
    hint_host: Option<String>,
    key: InstKey,
    target: String,
    handoff: Handoff,
    spent: u32,
    direct_denied: bool,
    /// First publisher page reached (un-proxied URL).
    landing: Option<String>,
    /// The host the EZproxy route assumed configured.
    ezproxy_host: Option<String>,
    /// Hosts that showed a challenge after the browser tier.
    challenged: BTreeSet<String>,
    miss: Miss,
}

impl<'a> Inst<'a> {
    /// Step 0: the gates that need no request.
    fn begin(
        cx: &'a ItemCx,
        slot: &'a mut Slot,
        ids: &'a Ids,
        pool: &CandidatePool,
        pipe: &'a dyn Collab,
    ) -> Result<Self, Box<Outcome>> {
        let inst = &cx.cfg.institution;
        let hand = handoff(inst, ids);
        if cx.inst_halted().is_some() {
            return Err(Box::new(Outcome::Halted { handoff: hand }));
        }
        if !has_usable_route(&cx.cfg) {
            return Err(Box::new(Outcome::NeedsUser {
                why: NeedsWhy::Misconfigured,
                handoff: hand,
            }));
        }
        let Some(target) = target_of(inst, ids) else {
            return Err(Box::new(Outcome::NotFound {
                reason: NotFoundReason::NotHeld,
            }));
        };
        let key = match ids.doi.as_deref() {
            Some(doi) => InstKey::for_doi(doi),
            None => InstKey::for_host(&pure::host_of(&target).unwrap_or_default()),
        };
        let ranked = pool.ranked(&cx.cfg.oa);
        let hint_host = ranked
            .iter()
            .find(|c| c.host_kind == HostKind::Publisher)
            .and_then(|c| pure::host_of(&c.url));
        let crossref = ranked
            .into_iter()
            .filter(|c| c.source == Source::Doi && c.kind == CandKind::Pdf)
            .collect();
        Ok(Self {
            cx,
            ids,
            pipe,
            slot,
            visits: CandidatePool::new(ids.clone()),
            crossref,
            hint_host,
            key,
            target,
            handoff: hand,
            spent: 0,
            direct_denied: false,
            landing: None,
            ezproxy_host: None,
            challenged: BTreeSet::new(),
            miss: Miss::Other,
        })
    }

    async fn run(mut self) -> Outcome {
        match self.routes().await {
            Break(outcome) => outcome,
            Continue(()) => self.exhausted(),
        }
    }

    async fn routes(&mut self) -> Flow {
        self.working();
        self.entitlement().await?;
        self.group_gate().await?;
        self.direct().await?;
        self.openathens().await?;
        self.saml().await?;
        self.ezproxy().await
    }

    /// Every route is spent: the best explanation seen picks the outcome.
    fn exhausted(&self) -> Outcome {
        match self.miss {
            Miss::Denied | Miss::NotKnown => Outcome::NotEntitled {
                handoff: self.handoff.clone(),
            },
            Miss::NoPdfLink => self.needs(NeedsWhy::NoPdfLink),
            Miss::Challenge => self.needs(NeedsWhy::Challenge),
            Miss::Network => self.needs(NeedsWhy::NetworkDown),
            Miss::Timeout | Miss::Other => self.needs(NeedsWhy::HandoffRequested),
        }
    }

    // ------------------------------------------------------------------------------------------ small helpers

    fn needs(&self, why: NeedsWhy) -> Outcome {
        Outcome::NeedsUser {
            why,
            handoff: self.handoff.clone(),
        }
    }

    fn note(&mut self, miss: Miss) {
        self.miss = self.miss.max(miss);
    }

    /// Stop the whole route (every item) and end this item as `Halted`.
    fn halt(&self, reason: HaltReason, retry_after_s: Option<u32>) -> Outcome {
        self.cx.halt_inst(reason, retry_after_s);
        Outcome::Halted {
            handoff: self.handoff.clone(),
        }
    }

    fn check_halt(&self) -> Flow {
        if self.cx.inst_halted().is_some() {
            return Break(Outcome::Halted {
                handoff: self.handoff.clone(),
            });
        }
        Continue(())
    }

    /// A gate refused: the matching terminal outcome.
    fn refusal(&self, r: Refused) -> Outcome {
        match r {
            Refused::InstHalted { .. } => Outcome::Halted {
                handoff: self.handoff.clone(),
            },
            Refused::Cap(hit) => Outcome::Deferred { cap: hit.kind },
            Refused::HostSkipped { host } => Outcome::Blocked {
                host,
                handoff: self.handoff.clone(),
            },
            Refused::ShuttingDown => Outcome::Cancelled {
                by: CancelledBy::Shutdown,
            },
        }
    }

    /// One fetch of the item's budget.
    fn spend(&mut self) -> Flow {
        if self.spent >= BUDGET {
            return Break(self.needs(NeedsWhy::NoPdfLink));
        }
        self.spent += 1;
        Continue(())
    }

    /// The row shown while this item works on the route (login.rs shows its own waiting rows).
    fn working(&self) {
        self.cx
            .progress(row(RowState::Working, l10n::id::INSTITUTIONAL).on_route(Route::Inst));
    }

    fn trace(&self, src: &str, tier: &str, class: &str, started: u64) {
        let ms = u32::try_from(self.cx.now().saturating_sub(started)).unwrap_or(u32::MAX);
        self.cx.trace_step(src, tier, class, ms);
    }

    fn entry_candidate(&self, url: &str, kind: CandKind, source: Source) -> Candidate {
        Candidate {
            url: url.to_owned(),
            kind,
            source,
            version: Version::Published,
            license: None,
            host_kind: HostKind::Publisher,
            record_doi: self.ids.doi.clone(),
            landing_url: None,
            signal: None,
            score: 0,
            tier: 0,
            needs_browser: false,
        }
    }

    /// Remember the first publisher page reached; an item without a DOI hands off to it.
    fn remember_landing(&mut self, url: &str) {
        if self.landing.is_some() {
            return;
        }
        let clean = pure::unproxify(&self.cx.cfg.institution, url, &BTreeMap::new());
        if self.ids.doi.is_none() {
            self.handoff = handoff_with_landing(&self.cx.cfg.institution, self.ids, Some(&clean));
        } else if self.handoff.wayfless_url.is_none() {
            // The DOI alone gave no WAYFless link (OUP, JAMA, Cambridge, ...); the page that was reached does.
            self.handoff.wayfless_url = pure::wayfless_url(
                &self.cx.cfg.institution,
                self.ids.doi.as_deref(),
                &[clean.as_str()],
            );
        }
        self.landing = Some(clean);
    }

    /// The publisher host, as far as anything has shown it.
    fn publisher_host(&self) -> Option<String> {
        self.landing
            .as_deref()
            .and_then(pure::host_of)
            .or_else(|| self.hint_host.clone())
            .or_else(|| {
                let url = self.ids.url.as_deref()?;
                pure::host_of(&pure::unproxify(
                    &self.cx.cfg.institution,
                    url,
                    &BTreeMap::new(),
                ))
            })
    }

    /// The redirector is a no-op for this publisher (`Institution.passthrough_hosts`).
    fn passthrough(&self) -> bool {
        self.publisher_host().is_some_and(|h| {
            self.cx
                .cfg
                .institution
                .passthrough_hosts
                .iter()
                .any(|p| same_host(p, &h))
        })
    }

    // ---------------------------------------------------------------------------------------------- 1. entitlement

    async fn entitlement(&mut self) -> Flow {
        self.check_halt()?;
        let cx = self.cx;
        let (Some(doi), Some(alma)) = (self.ids.doi.as_deref(), cx.cfg.institution.alma.as_ref())
        else {
            return Continue(());
        };
        if !cx.cfg.inst.check_alma {
            return Continue(());
        }
        let held = match cx.alma_known(doi) {
            Some(held) => Some(held),
            None => self.ask_alma(doi, alma).await,
        };
        if held == Some(false) {
            return Break(Outcome::NotEntitled {
                handoff: self.handoff.clone(),
            });
        }
        Continue(())
    }

    /// One CTO request. `None` = unknown (error, timeout, throttled, unparseable): the route goes on.
    async fn ask_alma(&self, doi: &str, alma: &Alma) -> Option<bool> {
        let cx = self.cx;
        let url = alma_cto_url(alma, doi);
        let host = pure::host_of(&alma.base).unwrap_or_default();
        let ms = cx.timeout_for(FetchClass::OaApi, &host);
        let mut req = requests::get(&url, Ua::Default, ms, ALMA_MAX_BYTES);
        req.headers
            .insert("Accept".into(), "text/xml, application/xml;q=0.9".into());
        let mut permit = cx.acquire_api(Source::Alma).await.ok()?;
        let started = cx.now();
        let spec = InspectSpec::Api {
            source: Source::Alma,
            requested: self.ids.clone(),
        };
        let answer = cx
            .timeout(
                u64::from(ms),
                cx.http(Route::Inst, false, req, Retain::None, spec),
            )
            .await;
        let (held, throttled) = match &answer {
            Ok(Ok(resp)) => {
                let throttled = match &resp.inspect {
                    Inspection::Api(a) if a.status == ApiStatus::Throttled => Some(a.retry_after_s),
                    _ => None,
                };
                (alma_answer(&resp.inspect), throttled)
            }
            _ => (None, None),
        };
        permit.report(held.is_some());
        drop(permit);
        if let Some(retry_after_s) = throttled {
            cx.disable_api_for(
                Source::Alma,
                retry_after_s.map_or(policy::API_COOLDOWN_MS, |s| u64::from(s) * 1000),
            );
        }
        self.trace(
            "alma",
            "api",
            held.map_or("unknown", |h| if h { "held" } else { "not_held" }),
            started,
        );
        if let Some(held) = held {
            cx.learn_alma(doi, held);
        }
        held
    }

    /// Group libraries: the institutional licence may not cover them, so the user decides once per run.
    async fn group_gate(&mut self) -> Flow {
        let cx = self.cx;
        if cx.snap.library.kind != LibraryKind::Group {
            return Continue(());
        }
        let allowed = match cx.cfg.inst.group_libraries {
            GroupPolicy::Allow => true,
            GroupPolicy::Never => false,
            GroupPolicy::Ask => cx.interactive && self.pipe.group_consent(cx).await,
        };
        if allowed {
            Continue(())
        } else {
            Break(self.needs(NeedsWhy::GroupLibrary))
        }
    }

    // ------------------------------------------------------------------------------------------------ 2. direct

    async fn direct(&mut self) -> Flow {
        self.check_halt()?;
        if !self.cx.cfg.inst.try_direct || self.direct_denied {
            return Continue(());
        }
        let entry = self.entry_candidate(&self.target.clone(), CandKind::Landing, Source::Doi);
        let end = self.fetch(&entry).await?;
        match self.settle(end, &entry).await? {
            Got::Pdf(blob) => self.finish_blob(blob, Source::Doi).await,
            Got::Landing(page) => {
                let h = self.harvest(&page).await?;
                drop(page);
                if h.denied {
                    // Off campus this is the normal answer; it must not outrank what the sign-in routes find.
                    self.direct_denied = true;
                    return Continue(());
                }
                let mut pdfs = h.pdfs;
                pdfs.extend(self.crossref.iter().cloned());
                self.try_pdfs(pdfs, Source::Doi).await?;
                Continue(())
            }
            Got::Denied => {
                self.direct_denied = true;
                Continue(())
            }
            Got::Bounce | Got::Miss => Continue(()),
        }
    }

    // ------------------------------------------------------------------------------------------ fetch plumbing

    /// One `fetch_candidate` call on the institutional policy (never hedged, challenge windows are ours). The
    /// fetch records its own trace step.
    async fn fetch(&mut self, cand: &Candidate) -> Flow<FetchEnd> {
        self.check_halt()?;
        self.spend()?;
        let policy = FetchPolicy {
            allow_challenge_window: false,
            ..FetchPolicy::inst(self.key.clone())
        };
        let (cx, pipe) = (self.cx, self.pipe);
        Continue(pipe.fetch(cx, cand, &policy, &mut self.visits).await)
    }

    /// Handle the stop signals of a fetch and say what is left for the route.
    async fn settle(&mut self, end: FetchEnd, cand: &Candidate) -> Flow<Got> {
        Continue(match end {
            FetchEnd::Pdf(blob) => Got::Pdf(blob),
            FetchEnd::Landing(page) => Got::Landing(page),
            FetchEnd::AccessDenied => Got::Denied,
            FetchEnd::NeedsLogin { .. } => Got::Bounce,
            FetchEnd::NotPdf | FetchEnd::NotFound | FetchEnd::TooLarge => {
                self.note(Miss::Other);
                Got::Miss
            }
            FetchEnd::Failed(code) => self.failed(&code)?,
            FetchEnd::Timeout => {
                self.note(Miss::Timeout);
                Got::Miss
            }
            FetchEnd::Network => {
                self.note(Miss::Network);
                Got::Miss
            }
            FetchEnd::Challenge { host, interactive } => {
                self.challenge(host, interactive, cand).await?
            }
            FetchEnd::Blocked { host } => return Break(self.blocked(host).await),
            FetchEnd::RateLimited { retry_after_s } => {
                return Break(self.halt(HaltReason::RateLimited, retry_after_s));
            }
            FetchEnd::LicenceStop => return Break(self.halt(HaltReason::LicenceStop, None)),
            FetchEnd::Refused(r) => return Break(self.refusal(r)),
        })
    }

    /// Page classes only this route understands come out of `fetch_candidate` as `Failed(<code>)` (`attempt::fail`).
    fn failed(&mut self, code: &str) -> Flow<Got> {
        match code {
            fail::EZPROXY_SUSPENDED => Break(self.halt(HaltReason::EzproxySuspended, None)),
            fail::OPENATHENS_BAD_ORG => Break(self.needs(NeedsWhy::Misconfigured)),
            fail::EZPROXY_NOT_CONFIGURED => {
                if let Some(host) = self.ezproxy_host.as_deref() {
                    self.cx.learn_ezproxy(host, false);
                }
                self.note(Miss::NotKnown);
                Continue(Got::Miss)
            }
            fail::OPENATHENS_UNKNOWN_RESOURCE => {
                self.note(Miss::NotKnown);
                Continue(Got::Miss)
            }
            fail::OFFLINE => {
                self.note(Miss::Network);
                Continue(Got::Miss)
            }
            // The redirector never got past "Please wait...": the browser route polls it, a plain fetch just gives up.
            fail::OPENATHENS_WAIT => {
                self.note(Miss::Timeout);
                Continue(Got::Miss)
            }
            _ => {
                self.note(Miss::Other);
                Continue(Got::Miss)
            }
        }
    }

    /// Extract the PDF candidates of a reached page (and note whether the page says "no access").
    async fn harvest(&mut self, page: &LandingPage) -> Flow<Harvest> {
        let (cx, pipe) = (self.cx, self.pipe);
        self.remember_landing(&page.final_url);
        let res = pipe
            .extract(
                cx,
                page.page.as_ref(),
                &page.final_url,
                &page.inspection,
                &mut self.visits,
            )
            .await;
        if res.ran_translators {
            // The translators' side requests escape the limiter, so they cost a fetch of the item's budget.
            self.spent = (self.spent + 1).min(BUDGET);
        }
        let denied =
            res.access == AccessHint::NoAccess || page.inspection.access == AccessHint::NoAccess;
        let pdfs = res
            .candidates
            .into_iter()
            .filter(|c| c.kind == CandKind::Pdf)
            .collect();
        Continue(Harvest { pdfs, denied })
    }

    /// Harvest a reached page and try what it offers (plus the Crossref links).
    async fn land(&mut self, page: LandingPage, via: Source) -> Flow<Tried> {
        let h = self.harvest(&page).await?;
        drop(page);
        if h.denied {
            self.note(Miss::Denied);
            return Continue(Tried::Exhausted);
        }
        let mut pdfs = h.pdfs;
        pdfs.extend(self.crossref.iter().cloned());
        self.try_pdfs(pdfs, via).await
    }

    /// Fetch PDF candidates in order until one saves or the route has to stop. Pages that turn up where a PDF was
    /// expected are harvested for more links; the budget and the visit set bound the walk. When the links all
    /// failed without a more specific reason, the page was one "without a usable PDF link".
    async fn try_pdfs(&mut self, pdfs: Vec<Candidate>, via: Source) -> Flow<Tried> {
        let before = std::mem::take(&mut self.miss);
        let walked = self.walk_pdfs(pdfs, via).await;
        let plain = matches!(walked, Continue(Tried::Exhausted)) && self.miss == Miss::Other;
        self.miss = before.max(if plain { Miss::NoPdfLink } else { self.miss });
        walked
    }

    async fn walk_pdfs(&mut self, pdfs: Vec<Candidate>, via: Source) -> Flow<Tried> {
        let mut queue: VecDeque<Candidate> = pdfs.into();
        while let Some(cand) = queue.pop_front() {
            if self.visits.was_tried(&cand.url) {
                continue;
            }
            let end = self.fetch(&cand).await?;
            match self.settle(end, &cand).await? {
                Got::Pdf(blob) => self.finish_blob(blob, via).await?,
                Got::Landing(page) => {
                    let h = self.harvest(&page).await?;
                    drop(page);
                    if h.denied {
                        self.note(Miss::Denied);
                        return Continue(Tried::Exhausted);
                    }
                    for c in h.pdfs.into_iter().rev() {
                        queue.push_front(c);
                    }
                }
                Got::Denied => {
                    self.note(Miss::Denied);
                    return Continue(Tried::Exhausted);
                }
                Got::Bounce => return Continue(Tried::Bounce),
                Got::Miss => {}
            }
        }
        Continue(Tried::Exhausted)
    }

    /// Validate and save a PDF. `Break(outcome)` when it was handled (saved or a save-side verdict); `Continue`
    /// when the bytes were not a usable PDF and the route goes on.
    async fn finish_blob(&mut self, mut blob: PdfBlob, via: Source) -> Flow {
        if save::validate(&blob.verdict).is_err() {
            // Not a download of the licensed content: do not count it.
            if let Some(ticket) = blob.ticket.take() {
                ticket.rollback();
            }
            self.note(Miss::Other);
            return Continue(());
        }
        let cx = self.cx;
        if blob.ticket.is_none() {
            // A page fetch that turned out to be the PDF reserved nothing beforehand. The bytes are here, so they
            // are counted now (or refused when a cap is spent): the ledger never under-counts.
            let mut ticket = brk(cx
                .reserve_licence(blob.verdict.len)
                .map_err(|r| self.refusal(r)))?;
            ticket.mark_sent();
            blob.ticket = Some(ticket);
        }
        blob.source = via;
        blob.route = Route::Inst;
        if blob.version == Version::Unknown {
            blob.version = Version::Published;
        }
        let (pipe, ids) = (self.pipe, self.ids);
        Break(pipe.save(cx, blob, ids).await)
    }

    // --------------------------------------------------------------------------------------------- stop signals

    /// A hard block (WAF "you have been blocked"): the family is blocked for 30 minutes, the third signal in a row
    /// stops the whole route.
    async fn blocked(&mut self, host: String) -> Outcome {
        let cx = self.cx;
        let halt = self.note_block_signal().await;
        cx.block_host(&self.key.0, policy::HALT_MIN_MS);
        if halt {
            return self.halt(HaltReason::BlockSignals, None);
        }
        Outcome::Blocked {
            host,
            handoff: self.handoff.clone(),
        }
    }

    /// Count one hard-block signal; `true` once `BLOCK_SIGNALS_TO_HALT` of them arrived without a break.
    async fn note_block_signal(&self) -> bool {
        let cx = self.cx;
        match cx.acquire_api(BLOCK_SIGNAL_GATE).await {
            Ok(mut permit) => permit.report(false),
            Err(_) => return true,
        }
        // The breaker trips on the third failure in a row; a tripped gate refuses.
        cx.acquire_api(BLOCK_SIGNAL_GATE).await.is_err()
    }

    /// A challenge that survived the HTTP and browser tiers. The second one on a host ends the route; the first
    /// may open a visible window (the user acts, we never solve anything) when the run is interactive.
    async fn challenge(&mut self, host: String, interactive: bool, cand: &Candidate) -> Flow<Got> {
        let cx = self.cx;
        if !self.challenged.insert(host.clone()) {
            return Break(self.halt(HaltReason::ChallengeLoop, None));
        }
        self.note(Miss::Challenge);
        if !(interactive && cx.interactive && cx.cfg.inst.challenge_windows) {
            return Continue(Got::Miss);
        }
        let mut ticket = brk(cx
            .reserve_licence(policy::RESERVE_BYTES_DEFAULT)
            .map_err(|r| self.refusal(r)))?;
        ticket.mark_sent();
        cx.progress(row(RowState::Waiting, l10n::id::CHALLENGE).on_route(Route::Inst));
        let req = LoginReq {
            kind: LoginKind::Challenge,
            realm: Realm::Openathens,
            entry_url: cand.url.clone(),
            item_title: cx.snap.title.clone(),
            waiting: 0,
            timeout_ms: cx.timeout_for(FetchClass::Challenge, &host),
            capture_pdf: true,
        };
        let (answer, back) = parked(cx, &mut *self.slot, cx.challenge_window(req)).await;
        if back.is_err() {
            return Break(Outcome::Cancelled {
                by: CancelledBy::Shutdown,
            });
        }
        self.working();
        Continue(match answer {
            Ok(resp) => window_result(resp, ticket, cand),
            Err(_) => {
                ticket.rollback();
                Got::Miss
            }
        })
    }

    // --------------------------------------------------------------------------- 3. / 4. the sign-in routes

    async fn openathens(&mut self) -> Flow {
        self.check_halt()?;
        let cx = self.cx;
        if !cx.cfg.inst.use_openathens {
            return Continue(());
        }
        // No OpenAthens domain, no redirector: an empty domain would build a URL that 403s.
        let Some(entry) = pure::redirector_url(&cx.cfg.institution, &self.target) else {
            return Continue(());
        };
        if self.passthrough() {
            self.note(Miss::NotKnown);
            return Continue(());
        }
        self.session_route(Realm::Openathens, entry).await
    }

    /// The publisher's own SAML / Shibboleth sign-in through a WAYFless link (module docs, step 4).
    async fn saml(&mut self) -> Flow {
        self.check_halt()?;
        let cx = self.cx;
        if !cx.cfg.inst.use_saml || pure::saml_entity(&cx.cfg.institution).is_none() {
            return Continue(());
        }
        // A publisher page was reached and refused / had no PDF link / showed a bot check: the same institution
        // signing in again through the publisher gets the same answer.
        if self.miss > Miss::NotKnown {
            return Continue(());
        }
        let Some(entry) = self.wayfless_entry() else {
            self.note(Miss::NotKnown);
            return Continue(());
        };
        self.session_route(Realm::Saml, entry).await
    }

    /// The WAYFless sign-in URL for this item: the first publisher page reached, else the item's own URL, else the
    /// article the DOI stands for. `None` when the publisher has no known WAYFless form.
    fn wayfless_entry(&self) -> Option<String> {
        let urls: Vec<&str> = [self.landing.as_deref(), self.ids.url.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        pure::wayfless_url(&self.cx.cfg.institution, self.ids.doi.as_deref(), &urls)
    }

    async fn ezproxy(&mut self) -> Flow {
        self.check_halt()?;
        let cx = self.cx;
        let inst = &cx.cfg.institution;
        if !cx.cfg.inst.use_ezproxy || !pure::has_ezproxy(inst) {
            return Continue(());
        }
        let host = match self.publisher_host() {
            Some(host) => host,
            // An EZproxy that proxies everything can start from the DOI resolver.
            None if inst.ezproxy_all_hosts && self.ids.doi.is_some() => "doi.org".to_owned(),
            None => return Continue(()),
        };
        if !self.ezproxy_configured(&host).await {
            self.note(Miss::NotKnown);
            return Continue(());
        }
        let target = self.landing.clone().unwrap_or_else(|| self.target.clone());
        let Some(entry) = pure::ezproxy_login_url(inst, &target) else {
            return Continue(());
        };
        self.ezproxy_host = Some(host);
        self.session_route(Realm::Ezproxy, entry).await
    }

    /// Is `host` configured in the institution's EZproxy? Cached answers (7 days) and listed hosts need no
    /// request; an unknown host is probed once (no-follow HEAD of the proxified host) and the answer cached. What
    /// the proxy itself said last beats the configured list: a listed host the proxy rejected stays rejected.
    async fn ezproxy_configured(&self, host: &str) -> bool {
        let cx = self.cx;
        let inst = &cx.cfg.institution;
        if let Some(known) = cx.ezproxy_known(host) {
            return known;
        }
        if inst
            .ezproxy_hosts
            .iter()
            .any(|h| h.trim().eq_ignore_ascii_case(host))
        {
            return true;
        }
        let Some(url) = pure::proxify(inst, &format!("https://{host}/")) else {
            return false;
        };
        // An EZproxy that proxies every host is tried unless it said no (or could not be asked): the probe, the
        // proxy's own not-configured page and the cache decide which hosts are skipped.
        let try_when_unsure = inst.ezproxy_all_hosts;
        let Ok(permit) = cx.acquire_api(Source::Ezproxy).await else {
            return try_when_unsure;
        };
        let ms = cx.timeout_for(FetchClass::Probe, host);
        let started = cx.now();
        let req = requests::probe(&url, Ua::Default, ms);
        let spec = InspectSpec::Probe { route: Route::Inst };
        let answer = cx
            .timeout(
                u64::from(ms),
                cx.http(Route::Inst, false, req, Retain::None, spec),
            )
            .await;
        drop(permit);
        let configured = match answer {
            Ok(Ok(resp)) => match resp.inspect {
                Inspection::Probe { class } => probe_configured(&class),
                _ => None,
            },
            _ => None,
        };
        self.trace(
            "ezproxy",
            "probe",
            configured.map_or(
                "unknown",
                |c| if c { "configured" } else { "not_configured" },
            ),
            started,
        );
        if let Some(configured) = configured {
            cx.learn_ezproxy(host, configured);
        }
        configured.unwrap_or(try_when_unsure)
    }

    /// Wait for the realm's session gate, load the entry URL in a hidden browser and act on what settles. The
    /// first caller probes alone; callers that find a sign-in window open wait without holding their item slot.
    async fn session_route(&mut self, realm: Realm, entry: String) -> Flow {
        let cx = self.cx;
        let via = match realm {
            Realm::Openathens => Source::Redirector,
            Realm::Ezproxy => Source::Ezproxy,
            Realm::Saml => Source::Saml,
        };
        let (mut attempt, mut waited) = (0u8, false);
        loop {
            let epoch = self.admit(realm).await?;
            let probe = Probe::new(cx, realm, epoch);
            let landed = match self.hidden_load(realm, &entry).await? {
                Loaded::Pdf(blob) => {
                    probe.report(ProbeResult::LoggedIn);
                    return self.finish_blob(blob, via).await;
                }
                Loaded::Failed => {
                    probe.report(ProbeResult::Failed);
                    return Continue(());
                }
                Loaded::Page(landed) => landed,
            };
            let mut verdict = seen(&landed.class);
            if verdict == Seen::Reached {
                verdict = refine(pure::url_class(&cx.cfg.institution, &landed.final_url));
            }
            match verdict {
                Seen::Reached => {
                    probe.report(ProbeResult::LoggedIn);
                    self.learn_ezproxy_reached(realm);
                    let page = LandingPage {
                        url: entry.clone(),
                        final_url: landed.final_url,
                        page: landed.page,
                        inspection: landed.inspection,
                    };
                    match self.land(page, via).await? {
                        Tried::Exhausted => return Continue(()),
                        // The load got through but the publisher wants a sign-in for the PDF: same as a bounce.
                        Tried::Bounce => cx.session_report(realm, epoch, ProbeResult::NeedsLogin),
                    }
                }
                Seen::NeedsLogin => probe.report(ProbeResult::NeedsLogin),
                Seen::Wait => {
                    probe.report(ProbeResult::Failed);
                    if waited {
                        self.note(Miss::Timeout);
                        return Continue(());
                    }
                    waited = true;
                    match self.poll_redirector(&entry).await {
                        PollEnd::Ready => continue,
                        PollEnd::Unknown => {
                            self.note(Miss::NotKnown);
                            return Continue(());
                        }
                        PollEnd::BadOrg => {
                            cx.session_report(realm, epoch, ProbeResult::Misconfigured);
                            return Break(self.needs(NeedsWhy::Misconfigured));
                        }
                        PollEnd::GaveUp => {
                            self.note(Miss::Timeout);
                            return Continue(());
                        }
                    }
                }
                Seen::UnknownResource => {
                    probe.report(ProbeResult::Failed);
                    self.note(Miss::NotKnown);
                    return Continue(());
                }
                Seen::BadOrg => {
                    probe.report(ProbeResult::Misconfigured);
                    return Break(self.needs(NeedsWhy::Misconfigured));
                }
                Seen::NotConfigured => {
                    probe.report(ProbeResult::Failed);
                    if let Some(host) = self.ezproxy_host.as_deref() {
                        cx.learn_ezproxy(host, false);
                    }
                    self.note(Miss::NotKnown);
                    return Continue(());
                }
                Seen::Suspended => {
                    probe.report(ProbeResult::Suspended);
                    return Break(self.halt(HaltReason::EzproxySuspended, None));
                }
                Seen::RateLimited(retry_after_s) => {
                    probe.report(ProbeResult::Failed);
                    return Break(self.halt(HaltReason::RateLimited, retry_after_s));
                }
                Seen::LicenceStop => {
                    probe.report(ProbeResult::Failed);
                    return Break(self.halt(HaltReason::LicenceStop, None));
                }
                Seen::Blocked => {
                    probe.report(ProbeResult::Failed);
                    let host = pure::host_of(&landed.final_url).unwrap_or_default();
                    return Break(self.blocked(host).await);
                }
                Seen::Challenge(interactive) => {
                    probe.report(ProbeResult::Failed);
                    let host = pure::host_of(&landed.final_url).unwrap_or_default();
                    let cand = self.entry_candidate(&entry, CandKind::Landing, via);
                    return match self.challenge(host, interactive, &cand).await? {
                        Got::Pdf(blob) => self.finish_blob(blob, via).await,
                        Got::Landing(page) => {
                            self.land(page, via).await?;
                            Continue(())
                        }
                        Got::Denied | Got::Bounce | Got::Miss => Continue(()),
                    };
                }
                Seen::Denied => {
                    // The publisher answered a working session with "no access".
                    probe.report(ProbeResult::LoggedIn);
                    self.note(Miss::Denied);
                    return Continue(());
                }
                Seen::Missing => {
                    probe.report(ProbeResult::Failed);
                    // The proxy's own "this proxy cannot serve the address" page (needhost): the host is not
                    // configured there, whatever the probe or the list said.
                    if realm == Realm::Ezproxy
                        && matches!(landed.class, PageClass::EzproxyError { suspend: false })
                        && let Some(host) = self.ezproxy_host.as_deref()
                    {
                        cx.learn_ezproxy(host, false);
                        self.note(Miss::NotKnown);
                    } else {
                        self.note(Miss::Other);
                    }
                    return Continue(());
                }
            }
            self.sign_in(realm, epoch, &entry, attempt).await?;
            attempt += 1;
        }
    }

    /// A publisher page came back through the EZproxy of an institution that proxies every host: that host is
    /// configured there, so the next item skips the probe. (Only with `ezproxy_all_hosts`; listed hosts need no
    /// cache entry.)
    fn learn_ezproxy_reached(&self, realm: Realm) {
        if realm == Realm::Ezproxy
            && self.cx.cfg.institution.ezproxy_all_hosts
            && let Some(host) = self.ezproxy_host.as_deref()
        {
            self.cx.learn_ezproxy(host, true);
        }
    }

    /// Wait for the realm's session gate. `Break` carries the hand-off outcome when this run may not (or can not)
    /// load the entry URL any more.
    async fn admit(&mut self, realm: Realm) -> Flow<u32> {
        let cx = self.cx;
        // Usually the gate answers at once; only a real wait gives the item slot back.
        let admission = match cx.session_admit(realm).now_or_never() {
            Some(admission) => admission,
            None => {
                let (admission, back) = parked(cx, &mut *self.slot, cx.session_admit(realm)).await;
                if back.is_err() {
                    return Break(Outcome::Cancelled {
                        by: CancelledBy::Shutdown,
                    });
                }
                self.working();
                admission
            }
        };
        match admission {
            Admission::Proceed { epoch } | Admission::Blocked(SessionVerdict::Retry { epoch }) => {
                Continue(epoch)
            }
            Admission::Blocked(verdict) => Break(self.verdict_outcome(&verdict)),
        }
    }

    /// The load landed on a sign-in page. A first bounce asks the user (one window for everybody) and retries
    /// once under the new epoch; a second bounce means the sign-in did not take effect.
    async fn sign_in(&mut self, realm: Realm, epoch: u32, entry: &str, attempt: u8) -> Flow {
        let cx = self.cx;
        if attempt >= 1 {
            return Break(self.needs(NeedsWhy::LoginIneffective));
        }
        if cx.cfg.inst.login_mode == LoginMode::Handoff || !cx.interactive {
            // No window will open: give the probe back, or the items behind this one wait for the stale timer.
            cx.session_report(realm, epoch, ProbeResult::Failed);
            return Break(self.needs(NeedsWhy::HandoffRequested));
        }
        let req = LoginRequest {
            entry_url: entry.to_owned(),
            item_title: cx.snap.title.clone(),
            seen_epoch: epoch,
            capture_pdf: true,
        };
        let (verdict, back) = parked(cx, &mut *self.slot, cx.session_login(realm, req)).await;
        if back.is_err() {
            return Break(Outcome::Cancelled {
                by: CancelledBy::Shutdown,
            });
        }
        self.working();
        match verdict {
            SessionVerdict::Retry { .. } => Continue(()),
            other => Break(self.verdict_outcome(&other)),
        }
    }

    /// The outcome of a sign-in that did not lead to a retry.
    fn verdict_outcome(&self, verdict: &SessionVerdict) -> Outcome {
        match verdict {
            SessionVerdict::Declined => self.needs(NeedsWhy::LoginDeclined),
            SessionVerdict::TimedOut => self.needs(NeedsWhy::LoginTimeout),
            SessionVerdict::Refused { code } => {
                self.cx.trace_step(
                    "login",
                    "window",
                    &format!("refused:{}", code.as_deref().unwrap_or("-")),
                    0,
                );
                self.needs(NeedsWhy::LoginRefused)
            }
            SessionVerdict::Ineffective => self.needs(NeedsWhy::LoginIneffective),
            SessionVerdict::Misconfigured => self.needs(NeedsWhy::Misconfigured),
            SessionVerdict::Suspended => Outcome::Halted {
                handoff: self.handoff.clone(),
            },
            SessionVerdict::Cancelled => Outcome::Cancelled {
                by: CancelledBy::Shutdown,
            },
            SessionVerdict::Retry { .. } => self.needs(NeedsWhy::HandoffRequested),
        }
    }

    /// HEAD-poll the redirector's "Please wait..." page until it has its answer (research/04 section 4.3).
    async fn poll_redirector(&self, entry: &str) -> PollEnd {
        let cx = self.cx;
        let deadline = cx.now().saturating_add(policy::OPENATHENS_POLL_MAX_MS);
        let ms = cx.timeout_for(FetchClass::Probe, REDIRECTOR_HOST);
        while cx.now() < deadline {
            cx.sleep(policy::OPENATHENS_POLL_MS).await;
            let Ok(mut permit) = cx.acquire_api(Source::Redirector).await else {
                return PollEnd::GaveUp;
            };
            let req = requests::probe(entry, Ua::Default, ms);
            let spec = InspectSpec::Probe { route: Route::Inst };
            let answer = cx
                .timeout(
                    u64::from(ms),
                    cx.http(Route::Inst, false, req, Retain::None, spec),
                )
                .await;
            let Ok(Ok(resp)) = answer else {
                permit.report(false);
                continue;
            };
            permit.report(true);
            if let Some(end) = poll_status(resp.status, resp.headers.get("location")) {
                return end;
            }
        }
        PollEnd::GaveUp
    }

    /// One hidden-browser load of `entry` on the institutional gates: family slot, browser slot, licence
    /// reservation (persisted before the `barrier` load leaves). The reservation is rolled back unless the load
    /// captured a PDF.
    async fn hidden_load(&mut self, realm: Realm, entry: &str) -> Flow<Loaded> {
        self.check_halt()?;
        self.spend()?;
        let cx = self.cx;
        let inst_permit = brk(cx
            .acquire_inst(&self.key, FetchKind::Page)
            .await
            .map_err(|r| self.refusal(r)))?;
        let browser = brk(cx.acquire_browser().await.map_err(|r| self.refusal(r)))?;
        let mut ticket = brk(cx
            .reserve_licence(policy::RESERVE_BYTES_DEFAULT)
            .map_err(|r| self.refusal(r)))?;

        let slow = self.key.0 == "wiley";
        let host = if slow { SLOW_HOST } else { REDIRECTOR_HOST };
        let ms = cx.timeout_for(FetchClass::BrowserLoad, host);
        // Do not settle on a page that is only a hop on the way: the redirector's "Please wait", the EZproxy
        // login page (it auto-posts to the IdP), a publisher SSO start page (SAML: the WAYFless link itself).
        let hold = match realm {
            Realm::Openathens => UrlClassKind::OpenathensRedirector,
            Realm::Ezproxy => UrlClassKind::EzproxyLogin,
            Realm::Saml => UrlClassKind::PublisherSsoHop,
        };
        let req = BrowserReq {
            url: entry.to_owned(),
            ua: Ua::Plain,
            capture_pdf: true,
            want_page: true,
            settle_ms: SETTLE_MS,
            timeout_ms: ms,
            hold_while: vec![hold],
        };
        let spec = InspectSpec::Page {
            requested: self.ids.clone(),
            route: Route::Inst,
        };
        let src = match realm {
            Realm::Openathens => "redirector",
            Realm::Ezproxy => "ezproxy",
            Realm::Saml => "saml",
        };
        let started = cx.now();
        // From here on a cancelled item still counts the reservation: the request may have reached the publisher.
        ticket.mark_sent();
        let answer = cx
            .timeout(u64::from(ms), cx.browser_load(Route::Inst, true, req, spec))
            .await;
        drop(browser);
        drop(inst_permit);

        let resp = match answer {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                match e.kind {
                    OpErrKind::Network | OpErrKind::Tls | OpErrKind::Offline => {
                        // Nothing was transferred.
                        ticket.rollback();
                        self.note(Miss::Network);
                    }
                    OpErrKind::Timeout => self.note(Miss::Timeout),
                    _ => self.note(Miss::Other),
                }
                self.trace(src, "browser", "error", started);
                return Continue(Loaded::Failed);
            }
            Err(TimedOut) => {
                // A timed-out load may have fetched the PDF: the dropped ticket keeps the reservation.
                self.note(Miss::Timeout);
                self.trace(src, "browser", "timeout", started);
                return Continue(Loaded::Failed);
            }
        };
        let BrowserResp {
            final_url,
            inspect,
            pdf,
            page,
            ..
        } = resp;
        if let Some(h) = pure::host_of(&final_url) {
            // The jar holds the clearance of the plain-UA browser: follow-up requests to the host must match it.
            cx.pin_plain_ua(&h);
        }
        Continue(match (inspect, pdf) {
            (Inspection::Pdf(verdict), Some(body)) if save::validate(&verdict).is_ok() => {
                self.trace(src, "browser", "pdf", started);
                Loaded::Pdf(PdfBlob {
                    body,
                    verdict,
                    url: entry.to_owned(),
                    final_url,
                    landing_url: None,
                    source: Source::Redirector,
                    version: Version::Published,
                    route: Route::Inst,
                    ticket: Some(ticket),
                })
            }
            (Inspection::Page(inspection), _) => {
                ticket.rollback();
                self.trace(src, "browser", class_name(&inspection.class), started);
                Loaded::Page(Landed {
                    class: inspection.class.clone(),
                    inspection,
                    page,
                    final_url,
                })
            }
            (other, _) => {
                ticket.rollback();
                self.note(Miss::Other);
                self.trace(
                    src,
                    "browser",
                    if matches!(other, Inspection::Pdf(_)) {
                        "bad_pdf"
                    } else {
                        "unusable"
                    },
                    started,
                );
                Loaded::Failed
            }
        })
    }
}

/// What a challenge window captured, as a fetch result: a PDF the user's click released, a page to extract, or
/// nothing (the clearance cookie is set; later requests to the host benefit).
fn window_result(resp: LoginResp, ticket: LedgerTicket, cand: &Candidate) -> Got {
    let LoginResp {
        final_url,
        inspect,
        pdf,
        page,
        ..
    } = resp;
    match (inspect, pdf) {
        (Inspection::Pdf(verdict), Some(body)) if save::validate(&verdict).is_ok() => {
            Got::Pdf(PdfBlob {
                body,
                verdict,
                url: cand.url.clone(),
                final_url,
                landing_url: cand.landing_url.clone(),
                source: cand.source,
                version: cand.version,
                route: Route::Inst,
                ticket: Some(ticket),
            })
        }
        (Inspection::Page(inspection), _) => {
            ticket.rollback();
            Got::Landing(LandingPage {
                url: cand.url.clone(),
                final_url,
                page,
                inspection,
            })
        }
        _ => {
            ticket.rollback();
            Got::Miss
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::Owner;
    use crate::login::{HaltWhy, SessionState};
    use crate::pipeline::testkit::{Rig, script};
    use crate::sched::Prio;
    use crate::sim::fixtures::{
        self, ScriptResponder, body, browser_ok, candidate, http_ok, is_type, login_ok,
        pdf_inspection, save_ok, translators_ok, url_has,
    };
    use crate::sim::{PendingOp, Reply, Responder, SimEnd};
    use serde_json::{Value, json};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    // ------------------------------------------------------------------------------------------------ scenery

    const DOI: &str = "10.1136/bmj.n71";
    const DOI_URL: &str = "https://doi.org/10.1136/bmj.n71";
    const ARTICLE: &str = "https://www.bmj.com/content/372/bmj.n71";
    const PDF: &str = "https://www.bmj.com/content/bmj/372/bmj.n71.full.pdf";
    const IDP: &str =
        "https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/saml2";
    const ALMA_HOST: &str = "librarysearch.kcl.ac.uk";
    const SAVED_KEY: &str = "1/INST0001";

    fn cfg() -> EngineConfig {
        fixtures::test_config()
    }

    fn snap() -> ItemSnapshot {
        fixtures::item(1, DOI)
    }

    fn ids_of(doi: &str) -> Ids {
        Ids {
            doi: Some(doi.to_owned()),
            ..Ids::default()
        }
    }

    fn hand_for(doi: &str) -> Handoff {
        handoff(&cfg().institution, &ids_of(doi))
    }

    fn hand() -> Handoff {
        hand_for(DOI)
    }

    fn redirector() -> String {
        pure::redirector_url(&cfg().institution, DOI_URL).expect("KCL has an OpenAthens domain")
    }

    fn page_insp(
        class: PageClass,
        access: AccessHint,
        candidates: Vec<Candidate>,
    ) -> PageInspection {
        PageInspection {
            class,
            page_ids: PageIds::default(),
            doi_match: Some(true),
            access,
            candidates,
            rejected: Vec::new(),
            rule: None,
        }
    }

    fn alma(held: bool) -> Reply {
        let status = if held {
            ApiStatus::Found
        } else {
            ApiStatus::Empty
        };
        alma_status(status)
    }

    fn alma_status(status: ApiStatus) -> Reply {
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

    /// An HTTP answer that is a page of `class` without a body.
    fn page(after: u64, url: &str, class: PageClass) -> Reply {
        Reply::ok(
            after,
            http_ok(
                200,
                url,
                &Inspection::Page(page_insp(class, AccessHint::Unknown, Vec::new())),
                None,
            ),
        )
    }

    /// An HTTP answer that is a landing page with a body.
    fn landing(
        after: u64,
        url: &str,
        access: AccessHint,
        cands: Vec<Candidate>,
        handle: u32,
    ) -> Reply {
        let insp = Inspection::Page(page_insp(PageClass::Landing, access, cands));
        Reply::ok(after, http_ok(200, url, &insp, Some(body(handle, 5_000))))
    }

    fn pdf(after: u64, url: &str, handle: u32) -> Reply {
        Reply::ok(
            after,
            http_ok(
                200,
                url,
                &pdf_inspection(80_000),
                Some(body(handle, 80_000)),
            ),
        )
    }

    fn browser_pdf(after: u64, url: &str, handle: u32) -> Reply {
        Reply::ok(
            after,
            browser_ok(
                url,
                &pdf_inspection(80_000),
                Some(body(handle, 80_000)),
                None,
            ),
        )
    }

    fn browser_page(after: u64, url: &str, class: PageClass) -> Reply {
        Reply::ok(
            after,
            browser_ok(
                url,
                &Inspection::Page(page_insp(class, AccessHint::Unknown, Vec::new())),
                None,
                None,
            ),
        )
    }

    fn browser_landing(
        after: u64,
        url: &str,
        access: AccessHint,
        cands: Vec<Candidate>,
        handle: u32,
    ) -> Reply {
        let insp = Inspection::Page(page_insp(PageClass::Landing, access, cands));
        Reply::ok(
            after,
            browser_ok(url, &insp, None, Some(body(handle, 5_000))),
        )
    }

    /// The answer to a no-follow HEAD: `status`, optional `Location`, and the class the pure instance gave it.
    fn probe_reply(after: u64, status: u16, class: PageClass, location: Option<&str>) -> Reply {
        let mut ok = http_ok(
            status,
            "https://probe.example/",
            &Inspection::Probe { class },
            None,
        );
        if let Some(l) = location {
            ok["headers"] = json!({ "location": l });
        }
        Reply::ok(after, ok)
    }

    fn window_ok(result: LoginResult) -> Value {
        login_ok(
            result,
            "https://www.bmj.com/content/372/bmj.n71",
            &Inspection::None,
            None,
            None,
        )
    }

    fn idp() -> PageClass {
        PageClass::IdpLogin {
            idp: Idp::Entra,
            ca_error: None,
        }
    }

    fn pdf_cand(url: &str) -> Candidate {
        let mut c = candidate(url, CandKind::Pdf, Source::Extractor, 0);
        c.record_doi = Some(DOI.to_owned());
        c
    }

    /// KCL holds the article and saving always works.
    fn kcl() -> ScriptResponder {
        script()
            .on(url_has(ALMA_HOST), alma(true))
            .on(is_type("save_pdf"), Reply::ok(100, save_ok(SAVED_KEY)))
    }

    /// ... and doi.org answers "no access" (off campus).
    fn off_campus() -> ScriptResponder {
        kcl().on(
            url_has("https://doi.org/"),
            page(80, ARTICLE, PageClass::AccessDenied),
        )
    }

    /// ... and doi.org lands on the paywalled article page.
    fn paywalled() -> ScriptResponder {
        kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, ARTICLE, AccessHint::NoAccess, Vec::new(), 3),
            )
            .on(
                is_type("run_translators"),
                Reply::ok(100, translators_ok(None)),
            )
    }

    /// Every hidden load bounces to the Entra sign-in page; a window answers with `window`.
    fn bouncing(window: Reply) -> ScriptResponder {
        off_campus()
            .on(is_type("browser_load"), browser_page(500, IDP, idp()))
            .on(is_type("login_window"), window)
    }

    // ------------------------------------------------------------------------------------------------ driving

    /// The route as `item::institutional` runs it: an item slot, a fresh pool, the given collaborators.
    async fn drive(cx: &ItemCx, extra: Vec<Candidate>, pipe: &dyn Collab) -> Outcome {
        let Ok(mut slot) = Slot::acquire(cx, Prio::New).await else {
            return Outcome::Cancelled {
                by: CancelledBy::Shutdown,
            };
        };
        let ids = pure::normalise_ids(&cx.snap);
        let mut pool = CandidatePool::new(ids.clone());
        for c in extra {
            pool.add(c);
        }
        run_with(cx, &mut slot, &ids, &pool, pipe).await
    }

    fn run_with_extra(rig: &mut Rig, snap: ItemSnapshot, extra: Vec<Candidate>) -> Outcome {
        rig.exec(
            snap,
            move |cx| async move { drive(&cx, extra, &Real).await },
        )
        .expect("the route ended")
    }

    fn run_one(rig: &mut Rig, snap: ItemSnapshot) -> Outcome {
        run_with_extra(rig, snap, Vec::new())
    }

    /// Items that run side by side in one rig; their outcomes land in `out`.
    struct Farm {
        rig: Rig,
        cfg: Rc<EngineConfig>,
        out: Rc<RefCell<BTreeMap<ItemId, Outcome>>>,
    }

    impl Farm {
        fn new(cfg: EngineConfig, responder: impl Responder + 'static) -> Self {
            Self {
                rig: Rig::with_config(cfg.clone(), responder),
                cfg: Rc::new(cfg),
                out: Rc::default(),
            }
        }

        fn spawn(&mut self, snap: ItemSnapshot, interactive: bool) {
            self.spawn_with(snap, interactive, Rc::new(Real));
        }

        fn spawn_with(&mut self, snap: ItemSnapshot, interactive: bool, pipe: Rc<dyn Collab>) {
            let id = snap.item;
            let shared = Rc::clone(self.rig.host.engine.shared());
            let cx = ItemCx::new(
                shared,
                1,
                Mode::Full,
                interactive,
                Rc::clone(&self.cfg),
                Rc::new(snap),
            );
            let out = Rc::clone(&self.out);
            self.rig
                .host
                .engine
                .spawn_test(Owner::Item { run: 1, item: id }, move |_| {
                    Box::pin(async move {
                        let outcome = drive(&cx, Vec::new(), &*pipe).await;
                        out.borrow_mut().insert(id, outcome);
                    })
                })
                .expect("spawn the item");
        }

        /// Cancel the item's task the way the run manager does.
        fn cancel(&mut self, item: ItemId) {
            let gone = self.rig.host.engine.exec_mut().cancel_where(
                |owner| matches!(owner, Owner::Item { run: 1, item: i } if *i == item),
            );
            assert_eq!(gone.len(), 1, "one task was running");
        }

        fn outcome(&self, item: ItemId) -> Option<Outcome> {
            self.out.borrow().get(&item).cloned()
        }
    }

    // ------------------------------------------------------------------------------------------- reading the log

    fn is_alma(e: &Effect) -> bool {
        url_has(ALMA_HOST)(e)
    }

    /// Requests that go to the publisher side: HTTP and hidden loads, without the Alma lookup, in order.
    fn requests(rig: &Rig) -> Vec<(u64, Effect)> {
        rig.timeline()
            .into_iter()
            .filter(|(_, e)| {
                matches!(e, Effect::Http { .. } | Effect::BrowserLoad { .. }) && !is_alma(e)
            })
            .collect()
    }

    fn request_urls(rig: &Rig) -> Vec<String> {
        requests(rig)
            .into_iter()
            .filter_map(|(_, e)| fixtures::effect_url(&e).map(str::to_owned))
            .collect()
    }

    fn loads(rig: &Rig) -> Vec<BrowserReq> {
        rig.effects()
            .into_iter()
            .filter_map(|e| match e {
                Effect::BrowserLoad { req, .. } => Some(req),
                _ => None,
            })
            .collect()
    }

    fn windows(rig: &Rig) -> Vec<LoginReq> {
        rig.effects()
            .into_iter()
            .filter_map(|e| match e {
                Effect::LoginWindow { req, .. } => Some(req),
                _ => None,
            })
            .collect()
    }

    fn heads(rig: &Rig) -> Vec<(u64, HttpReq)> {
        rig.timeline()
            .into_iter()
            .filter_map(|(at, e)| match e {
                Effect::Http { req, .. } if req.method == Method::Head => Some((at, req)),
                _ => None,
            })
            .collect()
    }

    fn ledger(rig: &Rig) -> (usize, usize) {
        let state: LedgerState = rig
            .host
            .persisted
            .get(&PersistKey::Ledger)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        (state.committed.len(), state.reserved.len())
    }

    fn session(rig: &Rig, realm: Realm) -> SessionState {
        rig.host
            .engine
            .shared()
            .with(|s| s.login.realms.get(&realm).map(|r| r.state.clone()))
            .flatten()
            .expect("the realm exists")
    }

    fn halted(rig: &mut Rig) -> bool {
        rig.exec(snap(), |cx| async move { cx.inst_halted().is_some() })
            .expect("done")
    }

    fn saved_source(outcome: &Outcome) -> Option<&str> {
        match outcome {
            Outcome::Saved {
                route: Route::Inst,
                source,
                ..
            } => Some(source.as_str()),
            _ => None,
        }
    }

    /// Every ledger `Persist` precedes the barrier request it protects; every barrier request has one.
    fn assert_persist_before_barrier(rig: &Rig) {
        let mut persisted = false;
        let mut barriers = 0;
        for e in rig.effects() {
            match e {
                Effect::Persist {
                    key: PersistKey::Ledger,
                    flush: true,
                    ..
                } => persisted = true,
                Effect::Http { barrier: true, .. } | Effect::BrowserLoad { barrier: true, .. } => {
                    assert!(
                        persisted,
                        "a barrier request went out without a ledger Persist before it"
                    );
                    persisted = false;
                    barriers += 1;
                }
                _ => {}
            }
        }
        assert!(barriers > 0, "the scenario made no barrier request");
    }

    /// The hand-off a failed item carries: all four links of the item's DOI.
    fn assert_handed_off(outcome: &Outcome, doi: &str) {
        let h = match outcome {
            Outcome::NotEntitled { handoff }
            | Outcome::NeedsUser { handoff, .. }
            | Outcome::Blocked { handoff, .. }
            | Outcome::Halted { handoff } => handoff,
            other => panic!("not a failure that carries links: {other:?}"),
        };
        assert_eq!(*h, hand_for(doi));
        assert!(
            h.redirector_url.is_some()
                && h.ezproxy_url.is_some()
                && h.libkey_url.is_some()
                && h.doi_url.is_some(),
            "{h:?}"
        );
    }

    // -------------------------------------------------------------------------------------- pure helpers

    #[test]
    fn inst_every_page_class_has_a_meaning_for_the_session() {
        let cases: Vec<(PageClass, Seen)> = vec![
            (idp(), Seen::NeedsLogin),
            (PageClass::SamlAutoPost, Seen::NeedsLogin),
            (PageClass::PublisherSsoHop, Seen::NeedsLogin),
            (PageClass::EzproxyNeedLogin, Seen::NeedsLogin),
            (PageClass::OpenathensWait, Seen::Wait),
            (PageClass::OpenathensUnknownResource, Seen::UnknownResource),
            (PageClass::OpenathensBadOrg, Seen::BadOrg),
            (PageClass::EzproxyHostNotConfigured, Seen::NotConfigured),
            (PageClass::EzproxyError { suspend: true }, Seen::Suspended),
            (PageClass::EzproxyError { suspend: false }, Seen::Missing),
            (PageClass::NotFound, Seen::Missing),
            (
                PageClass::RateLimited {
                    retry_after_s: Some(9),
                },
                Seen::RateLimited(Some(9)),
            ),
            (PageClass::LicenceStop, Seen::LicenceStop),
            (
                PageClass::Blocked {
                    vendor: Vendor::Cloudflare,
                },
                Seen::Blocked,
            ),
            (
                PageClass::Challenge {
                    vendor: Vendor::Cloudflare,
                    interactive: true,
                },
                Seen::Challenge(true),
            ),
            (PageClass::AccessDenied, Seen::Denied),
            (PageClass::Landing, Seen::Reached),
            (PageClass::Html, Seen::Reached),
            (PageClass::Pdf { junk_prefix: 0 }, Seen::Reached),
            (
                PageClass::Redirect {
                    next: "https://x/".into(),
                },
                Seen::Reached,
            ),
            (PageClass::ReaderShell { rewrite: None }, Seen::Reached),
        ];
        for (class, want) in cases {
            assert_eq!(seen(&class), want, "{class:?}");
        }
    }

    #[test]
    fn inst_the_url_decides_when_the_class_says_reached() {
        assert_eq!(refine(UrlClassKind::Other), Seen::Reached);
        assert_eq!(refine(UrlClassKind::IdpLogin), Seen::NeedsLogin);
        assert_eq!(refine(UrlClassKind::EzproxyLogin), Seen::NeedsLogin);
        assert_eq!(refine(UrlClassKind::PublisherSsoHop), Seen::NeedsLogin);
        assert_eq!(refine(UrlClassKind::OpenathensRedirector), Seen::Missing);
    }

    #[test]
    fn inst_the_wait_poll_reads_201_and_redirects_as_ready() {
        let loc = "https://www.bmj.com/x".to_owned();
        assert_eq!(poll_status(201, None), Some(PollEnd::Ready));
        assert_eq!(poll_status(302, Some(&loc)), Some(PollEnd::Ready));
        assert_eq!(poll_status(307, Some(&loc)), Some(PollEnd::Ready));
        assert_eq!(
            poll_status(302, None),
            None,
            "a redirect without a target is no answer"
        );
        assert_eq!(poll_status(302, Some(&String::from("  "))), None);
        assert_eq!(poll_status(200, None), None, "still waiting");
        assert_eq!(poll_status(404, None), Some(PollEnd::Unknown));
        assert_eq!(poll_status(403, None), Some(PollEnd::BadOrg));
        assert_eq!(poll_status(503, None), None);
    }

    #[test]
    fn inst_the_alma_request_is_the_documented_cto_url() {
        let alma = Alma {
            base: "https://librarysearch.kcl.ac.uk/".into(),
            inst: "44KCL_INST".into(),
        };
        assert_eq!(
            alma_cto_url(&alma, "10.1136/bmj.n71"),
            "https://librarysearch.kcl.ac.uk/view/uresolver/44KCL_INST/openurl?ctx_ver=Z39.88-2004&url_ver=Z39.88-2004&rft_id=info:doi/10.1136/bmj.n71&svc_dat=CTO"
        );
        let odd = alma_cto_url(
            &alma,
            "10.1002/(sici)1097-0258(19980430)17:8<873::aid-sim780>3.0.co;2-i",
        );
        assert!(
            odd.contains("/10.1002/") && !odd.contains('<') && !odd.contains('>'),
            "{odd}"
        );
    }

    #[test]
    fn inst_alma_answers_and_probe_classes_are_read_in_one_place() {
        let api = |status| {
            Inspection::Api(ApiInspection {
                source: Source::Alma,
                status,
                learned: LearnedIds::default(),
                candidates: Vec::new(),
                oa_status: None,
                note: None,
                retry_after_s: None,
            })
        };
        assert_eq!(alma_answer(&api(ApiStatus::Found)), Some(true));
        assert_eq!(alma_answer(&api(ApiStatus::Empty)), Some(false));
        for unknown in [
            ApiStatus::NotFound,
            ApiStatus::Throttled,
            ApiStatus::Unparseable,
        ] {
            assert_eq!(alma_answer(&api(unknown)), None, "{unknown:?}");
        }
        assert_eq!(alma_answer(&Inspection::None), None);

        assert_eq!(probe_configured(&PageClass::EzproxyNeedLogin), Some(true));
        assert_eq!(probe_configured(&PageClass::Landing), Some(true));
        assert_eq!(
            probe_configured(&PageClass::EzproxyHostNotConfigured),
            Some(false)
        );
        assert_eq!(probe_configured(&PageClass::NotFound), None);
    }

    #[test]
    fn inst_the_target_is_the_doi_url_else_the_unproxied_item_url() {
        let inst = cfg().institution;
        assert_eq!(target_of(&inst, &ids_of(DOI)).as_deref(), Some(DOI_URL));
        let url_only = Ids {
            url: Some("https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71".into()),
            ..Ids::default()
        };
        assert_eq!(target_of(&inst, &url_only).as_deref(), Some(ARTICLE));
        let wrapped = Ids {
            url: Some(format!(
                "https://go.openathens.net/redirector/kcl.ac.uk?url={}",
                pure::pct_encode(ARTICLE)
            )),
            ..Ids::default()
        };
        assert_eq!(target_of(&inst, &wrapped).as_deref(), Some(ARTICLE));
        assert_eq!(target_of(&inst, &Ids::default()), None);
        assert_eq!(
            target_of(
                &inst,
                &Ids {
                    url: Some("ftp://files.example.org/x".into()),
                    ..Ids::default()
                }
            ),
            None
        );
        assert!(same_host("WWW.bmj.com", "bmj.com") && !same_host("bmj.com", "bmjopen.com"));
    }

    // ---------------------------------------------------------------------------------------------- 0. gates

    #[test]
    fn inst_a_halted_route_ends_before_any_request() {
        let mut rig = Rig::new(off_campus());
        let outcome = rig
            .exec(snap(), |cx| async move {
                cx.halt_inst(HaltReason::RateLimited, None);
                drive(&cx, Vec::new(), &Real).await
            })
            .expect("done");
        assert_eq!(outcome, Outcome::Halted { handoff: hand() });
        assert_handed_off(&outcome, DOI);
        assert!(
            rig.http().is_empty() && loads(&rig).is_empty(),
            "no request of any kind"
        );
    }

    #[test]
    fn inst_an_item_without_doi_or_usable_url_is_not_held() {
        let mut rig = Rig::new(off_campus());
        assert_eq!(
            run_one(&mut rig, fixtures::item(1, "")),
            Outcome::NotFound {
                reason: NotFoundReason::NotHeld
            }
        );
        let mut ftp = fixtures::item(2, "");
        ftp.fields.url = "ftp://files.example.org/x.pdf".into();
        assert_eq!(
            run_one(&mut rig, ftp),
            Outcome::NotFound {
                reason: NotFoundReason::NotHeld
            }
        );
        assert!(rig.http().is_empty(), "nothing was asked");
    }

    #[test]
    fn inst_an_item_with_only_a_url_aims_every_route_at_the_unproxied_url() {
        let mut item = fixtures::item(1, "");
        item.fields.url = "https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71".into();
        let s = script()
            .on(
                url_has("https://www.bmj.com/"),
                page(80, ARTICLE, PageClass::AccessDenied),
            )
            .on(is_type("browser_load"), browser_pdf(900, ARTICLE, 7))
            .on(is_type("save_pdf"), Reply::ok(100, save_ok(SAVED_KEY)));
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, item);
        assert_eq!(saved_source(&outcome), Some("redirector"), "{outcome:?}");
        let urls = request_urls(&rig);
        assert_eq!(
            urls.first().map(String::as_str),
            Some(ARTICLE),
            "direct: the item's own page, not the proxied host"
        );
        let expected = pure::redirector_url(&cfg().institution, ARTICLE);
        assert_eq!(urls.get(1), expected.as_ref());
        assert!(
            rig.http().iter().all(|(u, _, _)| !u.contains(ALMA_HOST)),
            "no DOI, no entitlement lookup"
        );
        if let Outcome::Saved { url, .. } = &outcome {
            assert_eq!(
                url, ARTICLE,
                "the stored URL is un-proxied and has no redirector in it"
            );
        }
    }

    #[test]
    fn inst_run_institutional_is_the_plain_forward() {
        let mut rig = Rig::new(script().on(url_has(ALMA_HOST), alma(false)));
        let outcome = rig
            .exec(snap(), |cx| async move {
                let Ok(mut slot) = Slot::acquire(&cx, Prio::New).await else {
                    return Outcome::Cancelled {
                        by: CancelledBy::Shutdown,
                    };
                };
                let ids = pure::normalise_ids(&cx.snap);
                let mut pool = CandidatePool::new(ids.clone());
                run_institutional(&cx, &mut slot, &ids, &mut pool).await
            })
            .expect("done");
        assert_eq!(outcome, Outcome::NotEntitled { handoff: hand() });
    }

    // ------------------------------------------------------------------------------------------ 1. entitlement

    #[test]
    fn inst_alma_not_held_stops_before_any_publisher_request_and_is_cached() {
        let mut rig = Rig::new(script().on(url_has(ALMA_HOST), alma(false)));
        let outcome = run_one(&mut rig, snap());
        assert_eq!(outcome, Outcome::NotEntitled { handoff: hand() });
        assert_handed_off(&outcome, DOI);

        let sent: Vec<Effect> = rig
            .effects()
            .into_iter()
            .filter(|e| matches!(e, Effect::Http { .. }))
            .collect();
        assert_eq!(sent.len(), 1, "one lookup and nothing else");
        match &sent[0] {
            Effect::Http {
                req,
                retain,
                inspect,
                route,
                barrier,
                ..
            } => {
                assert_eq!(
                    req.url,
                    "https://librarysearch.kcl.ac.uk/view/uresolver/44KCL_INST/openurl?ctx_ver=Z39.88-2004&url_ver=Z39.88-2004&rft_id=info:doi/10.1136/bmj.n71&svc_dat=CTO"
                );
                assert_eq!(
                    (req.method, req.ua, *retain, *route, *barrier),
                    (Method::Get, Ua::Default, Retain::None, Route::Inst, false)
                );
                assert!(matches!(
                    inspect,
                    InspectSpec::Api {
                        source: Source::Alma,
                        ..
                    }
                ));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            rig.exec(
                fixtures::item(9, DOI),
                |cx| async move { cx.alma_known(DOI) }
            ),
            Some(Some(false))
        );
        assert!(
            rig.host.persisted.contains_key(&PersistKey::Alma),
            "the answer is persisted"
        );

        // The next item with the same DOI does not ask again.
        let again = run_one(&mut rig, fixtures::item(2, DOI));
        assert_eq!(again, Outcome::NotEntitled { handoff: hand() });
        assert_eq!(rig.http().len(), 1);
    }

    #[test]
    fn inst_alma_held_goes_on_and_an_unknown_answer_goes_on_too() {
        for (name, reply, cached) in [
            ("held", alma(true), Some(true)),
            ("network error", Reply::err(10, OpErrKind::Network), None),
            ("unparseable", alma_status(ApiStatus::Unparseable), None),
            ("throttled", alma_status(ApiStatus::Throttled), None),
            ("timeout", Reply::Never, None),
        ] {
            let s = script()
                .on(url_has(ALMA_HOST), reply)
                .on(
                    url_has("https://doi.org/"),
                    page(80, ARTICLE, PageClass::AccessDenied),
                )
                .on(is_type("browser_load"), browser_pdf(900, PDF, 7))
                .on(is_type("save_pdf"), Reply::ok(100, save_ok(SAVED_KEY)));
            let mut rig = Rig::new(s);
            let outcome = run_one(&mut rig, snap());
            assert_eq!(
                saved_source(&outcome),
                Some("redirector"),
                "{name}: {outcome:?}"
            );
            let known = rig
                .exec(
                    fixtures::item(9, DOI),
                    |cx| async move { cx.alma_known(DOI) },
                )
                .expect("done");
            assert_eq!(known, cached, "{name}: only a real answer is remembered");
        }
    }

    #[test]
    fn inst_alma_is_skipped_when_switched_off_or_not_configured() {
        let mut off = cfg();
        off.inst.check_alma = false;
        let mut none = cfg();
        none.institution.alma = None;
        for (name, config) in [("pref off", off), ("no alma in the institution", none)] {
            let mut rig = Rig::with_config(
                config,
                off_campus().on(is_type("browser_load"), browser_pdf(900, PDF, 7)),
            );
            let outcome = run_one(&mut rig, snap());
            assert_eq!(
                saved_source(&outcome),
                Some("redirector"),
                "{name}: {outcome:?}"
            );
            assert!(
                rig.http().iter().all(|(u, _, _)| !u.contains(ALMA_HOST)),
                "{name}"
            );
        }
    }

    // ------------------------------------------------------------------------------------------------ 2. direct

    #[test]
    fn inst_direct_pdf_on_campus_is_saved_and_counted() {
        // doi.org lands straight on a PDF: no reservation was made by the page fetch, the route counts it anyway.
        let s = kcl().on(url_has("https://doi.org/"), pdf(300, PDF, 5));
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, snap());
        assert_eq!(saved_source(&outcome), Some("doi"), "{outcome:?}");
        assert!(
            loads(&rig).is_empty(),
            "no redirector when the campus network already works"
        );
        assert_eq!(rig.count(|e| matches!(e, Effect::Release { handle: 5 })), 1);
        assert_eq!(
            ledger(&rig),
            (1, 0),
            "the download is on the licence ledger"
        );
    }

    #[test]
    fn inst_direct_landing_page_yields_the_pdf_that_is_saved() {
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, ARTICLE, AccessHint::Unknown, vec![pdf_cand(PDF)], 3),
            )
            .on(url_has("full.pdf"), pdf(400, PDF, 4));
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, snap());
        match &outcome {
            Outcome::Saved {
                route,
                source,
                version,
                url,
                attachment_key,
            } => {
                assert_eq!(
                    (
                        *route,
                        source.as_str(),
                        *version,
                        url.as_str(),
                        attachment_key.as_str()
                    ),
                    (Route::Inst, "doi", Version::Published, PDF, SAVED_KEY)
                );
            }
            other => panic!("{other:?}"),
        }
        // The page fetch is not a barrier fetch; the PDF is, and its ledger entry was persisted first.
        let barriers: Vec<(String, Retain, bool)> = rig
            .effects()
            .into_iter()
            .filter_map(|e| match e {
                Effect::Http {
                    req,
                    retain,
                    barrier,
                    route: Route::Inst,
                    ..
                } if !req.url.contains(ALMA_HOST) => Some((req.url, retain, barrier)),
                _ => None,
            })
            .collect();
        assert_eq!(
            barriers,
            vec![
                (DOI_URL.to_owned(), Retain::Page, false),
                (PDF.to_owned(), Retain::Pdf, true)
            ],
            "page first, then the PDF under a ledger barrier"
        );
        assert_persist_before_barrier(&rig);
        assert_eq!(ledger(&rig), (1, 0), "reserved, then committed by the save");
        assert_eq!(
            rig.count(|e| matches!(e, Effect::Release { handle: 3 | 4 })),
            2,
            "page and PDF bodies are released"
        );
        assert!(
            rig.rows().iter().any(|r| r == l10n::id::INSTITUTIONAL),
            "{:?}",
            rig.rows()
        );
        assert!(rig.host.pending.is_empty());
    }

    #[test]
    fn inst_crossref_links_from_the_oa_phase_are_tried_here() {
        // The publisher page offers nothing (the translators find nothing either); Crossref named a PDF.
        let mut crossref = candidate(PDF, CandKind::Pdf, Source::Doi, 0);
        crossref.record_doi = Some(DOI.to_owned());
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, ARTICLE, AccessHint::Unknown, Vec::new(), 3),
            )
            .on(
                is_type("run_translators"),
                Reply::ok(100, translators_ok(None)),
            )
            .on(url_has("full.pdf"), pdf(400, PDF, 4));
        let mut rig = Rig::new(s);
        let outcome = run_with_extra(&mut rig, snap(), vec![crossref]);
        assert_eq!(saved_source(&outcome), Some("doi"), "{outcome:?}");
        assert_eq!(rig.count(|e| matches!(e, Effect::RunTranslators { .. })), 1);
    }

    #[test]
    fn inst_the_fetch_budget_is_four_then_no_pdf_link() {
        // A landing page (1) with five PDF links that all turn out to be nothing (2, 3, 4): the fifth is not tried.
        let cands: Vec<Candidate> = (1..=5)
            .map(|n| {
                pdf_cand(&format!(
                    "https://www.bmj.com/content/bmj/372/bmj.n71.full{n}.pdf"
                ))
            })
            .collect();
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, ARTICLE, AccessHint::Unknown, cands, 3),
            )
            .on(url_has("full"), page(50, ARTICLE, PageClass::NotFound));
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, snap());
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::NoPdfLink,
                handoff: hand()
            }
        );
        assert_eq!(requests(&rig).len(), 4, "{:?}", request_urls(&rig));
        assert!(
            loads(&rig).is_empty(),
            "the redirector is out of budget too"
        );
        assert_eq!(
            ledger(&rig),
            (0, 0),
            "nothing was downloaded, nothing is counted"
        );
    }

    #[test]
    fn inst_translators_cost_one_fetch_of_the_budget() {
        // landing (1) + translators (2) + two dead links (3, 4) + a third link that no longer fits.
        let cands = Vec::new();
        let hit = translators_ok(Some(("https://www.bmj.com/dead1.pdf", "application/pdf")));
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, ARTICLE, AccessHint::Unknown, cands, 3),
            )
            .on(is_type("run_translators"), Reply::ok(100, hit))
            .on(url_has("dead"), page(50, ARTICLE, PageClass::NotFound));
        let mut rig = Rig::new(s);
        let mut crossref = candidate(
            "https://www.bmj.com/dead2.pdf",
            CandKind::Pdf,
            Source::Doi,
            0,
        );
        crossref.record_doi = Some(DOI.to_owned());
        let mut crossref2 = candidate(
            "https://www.bmj.com/dead3.pdf",
            CandKind::Pdf,
            Source::Doi,
            0,
        );
        crossref2.record_doi = Some(DOI.to_owned());
        let outcome = run_with_extra(&mut rig, snap(), vec![crossref, crossref2]);
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::NoPdfLink,
                handoff: hand()
            }
        );
        assert_eq!(
            requests(&rig).len(),
            3,
            "landing + two links; the translators took the other unit: {:?}",
            request_urls(&rig)
        );
    }

    // ------------------------------------------------------------------------------------ 3. OpenAthens

    #[test]
    fn inst_openathens_load_is_a_hidden_barrier_load_and_a_captured_pdf_is_saved() {
        let s = off_campus().on(is_type("browser_load"), browser_pdf(900, PDF, 7));
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, snap());
        match &outcome {
            Outcome::Saved {
                route,
                source,
                version,
                url,
                attachment_key,
            } => {
                assert_eq!(
                    (*route, source.as_str(), *version, attachment_key.as_str()),
                    (Route::Inst, "redirector", Version::Published, SAVED_KEY)
                );
                assert_eq!(
                    url, DOI_URL,
                    "the stored URL names the article, not the redirector"
                );
            }
            other => panic!("{other:?}"),
        }
        let load = rig
            .effects()
            .into_iter()
            .find(|e| matches!(e, Effect::BrowserLoad { .. }))
            .expect("one hidden load");
        let Effect::BrowserLoad {
            route,
            barrier,
            req,
            inspect,
            ..
        } = load
        else {
            panic!("not a load")
        };
        assert_eq!((route, barrier), (Route::Inst, true));
        let want = BrowserReq {
            url: redirector(),
            ua: Ua::Plain,
            capture_pdf: true,
            want_page: true,
            settle_ms: 3_000,
            timeout_ms: 60_000,
            hold_while: vec![UrlClassKind::OpenathensRedirector],
        };
        assert_eq!(req, want);
        assert!(matches!(
            inspect,
            InspectSpec::Page {
                route: Route::Inst,
                ..
            }
        ));
        assert_persist_before_barrier(&rig);
        assert_eq!(ledger(&rig), (1, 0));
        assert_eq!(
            session(&rig, Realm::Openathens),
            SessionState::SignedIn { verified: true }
        );
        assert_eq!(rig.count(|e| matches!(e, Effect::Release { handle: 7 })), 1);
        assert_eq!(
            rig.count(|e| matches!(e, Effect::LoginWindow { .. })),
            0,
            "a working session never opens a window"
        );
        assert!(rig.host.pending.is_empty());
    }

    #[test]
    fn inst_a_slow_family_gets_the_slow_browser_timeout() {
        let wiley = "10.1002/bjs.11646";
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                page(80, ARTICLE, PageClass::AccessDenied),
            )
            .on(is_type("browser_load"), browser_pdf(900, PDF, 7));
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, fixtures::item(1, wiley));
        assert_eq!(saved_source(&outcome), Some("redirector"), "{outcome:?}");
        assert_eq!(loads(&rig).first().map(|r| r.timeout_ms), Some(90_000));
    }

    #[test]
    fn inst_a_publisher_page_behind_the_redirector_is_extracted_and_its_pdf_fetched() {
        let s = off_campus()
            .on(
                is_type("browser_load"),
                browser_landing(900, ARTICLE, AccessHint::Unknown, vec![pdf_cand(PDF)], 5),
            )
            .on(url_has("full.pdf"), pdf(400, PDF, 6));
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, snap());
        match &outcome {
            Outcome::Saved {
                route, source, url, ..
            } => assert_eq!(
                (*route, source.as_str(), url.as_str()),
                (Route::Inst, "redirector", PDF)
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            request_urls(&rig),
            vec![DOI_URL.to_owned(), redirector(), PDF.to_owned()],
            "direct, redirector, PDF: three fetches"
        );
        assert_eq!(
            session(&rig, Realm::Openathens),
            SessionState::SignedIn { verified: true }
        );
        assert_persist_before_barrier(&rig);
        assert_eq!(ledger(&rig), (1, 0));
        // The page and the PDF are released once the route is done with them.
        assert_eq!(
            rig.count(|e| matches!(e, Effect::Release { handle: 5 | 6 })),
            2
        );
    }

    #[test]
    fn inst_a_working_session_that_gets_no_access_is_not_entitled() {
        let mut config = cfg();
        config.inst.use_ezproxy = false;
        let s = off_campus()
            .on(
                is_type("browser_load"),
                browser_landing(900, ARTICLE, AccessHint::NoAccess, Vec::new(), 5),
            )
            .on(
                is_type("run_translators"),
                Reply::ok(100, translators_ok(None)),
            );
        let mut rig = Rig::with_config(config, s);
        let outcome = run_one(&mut rig, snap());
        assert_eq!(outcome, Outcome::NotEntitled { handoff: hand() });
        assert_eq!(loads(&rig).len(), 1);
        assert_eq!(
            session(&rig, Realm::Openathens),
            SessionState::SignedIn { verified: true },
            "the session itself works"
        );
    }

    // ------------------------------------------------------------------------------- login: one window

    /// A small model of the sites behind the route for scenarios with several items: Alma holds everything,
    /// doi.org says "no access", the redirector bounces to the Entra sign-in until a window has been answered
    /// (when `window_works`) and then hands out the PDF.
    struct Web {
        session: Rc<Cell<bool>>,
        window_works: bool,
        window_ms: u64,
        load_ms: u64,
        /// Hidden loads whose URL contains the marker take that long instead.
        slow: Option<(&'static str, u64)>,
        handles: u32,
    }

    impl Web {
        fn new(session: &Rc<Cell<bool>>) -> Self {
            Self {
                session: Rc::clone(session),
                window_works: true,
                window_ms: 3_000,
                load_ms: 800,
                slow: None,
                handles: 100,
            }
        }
    }

    impl Responder for Web {
        fn respond(&mut self, op: &PendingOp, _now: u64, _wall: u64) -> Reply {
            self.handles += 1;
            let handle = self.handles;
            match &op.effect {
                Effect::Http { req, .. } if req.url.contains(ALMA_HOST) => alma(true),
                Effect::Http { req, .. } if req.url.starts_with("https://doi.org/") => {
                    page(80, &req.url, PageClass::AccessDenied)
                }
                Effect::BrowserLoad { req, .. } => {
                    let ms = self
                        .slow
                        .filter(|(marker, _)| req.url.contains(marker))
                        .map_or(self.load_ms, |(_, ms)| ms);
                    if self.session.get() {
                        browser_pdf(ms, &req.url, handle)
                    } else {
                        browser_page(ms, IDP, idp())
                    }
                }
                Effect::LoginWindow { .. } => {
                    if self.window_works {
                        self.session.set(true);
                    }
                    Reply::ok(self.window_ms, window_ok(LoginResult::Landed))
                }
                Effect::SavePdf { req, .. } => {
                    Reply::ok(100, save_ok(&format!("1/K{:04}", req.item)))
                }
                _ => Reply::err(10, OpErrKind::Network),
            }
        }
    }

    fn f1_doi(n: u32) -> String {
        const PREFIXES: [&str; 10] = [
            "10.1136", "10.1016", "10.1007", "10.1093", "10.1097", "10.1056", "10.1001", "10.1080",
            "10.1177", "10.1017",
        ];
        format!("{}/f1.{n}", PREFIXES[n as usize % PREFIXES.len()])
    }

    #[test]
    fn inst_f1_twenty_items_share_one_login_window() {
        let session = Rc::new(Cell::new(false));
        let mut farm = Farm::new(cfg(), Web::new(&session));
        for n in 1..=20 {
            farm.spawn(fixtures::item(n, &f1_doi(n)), true);
        }
        assert_eq!(farm.rig.run(), SimEnd::Idle);
        for n in 1..=20 {
            let outcome = farm.outcome(n).expect("every item ends");
            assert_eq!(
                saved_source(&outcome),
                Some("redirector"),
                "item {n}: {outcome:?}"
            );
        }
        assert_eq!(windows(&farm.rig).len(), 1, "ONE window for twenty items");
        let effects = farm.rig.effects();
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
            loads(&farm.rig).len(),
            21,
            "the prober twice, everybody else once"
        );
        assert_eq!(ledger(&farm.rig), (20, 0));
        assert!(farm.rig.host.pending.is_empty());
        assert_persist_before_barrier(&farm.rig);
    }

    #[test]
    fn inst_f2_an_expired_session_prompts_again_once_then_hands_off() {
        let session = Rc::new(Cell::new(false));
        let mut rig = Rig::new(Web::new(&session));
        let first = run_one(&mut rig, fixtures::item(1, "10.1136/f2.1"));
        assert_eq!(saved_source(&first), Some("redirector"), "{first:?}");
        assert_eq!(windows(&rig).len(), 1);

        session.set(false); // the cookies expired
        let second = run_one(&mut rig, fixtures::item(2, "10.1136/f2.2"));
        assert_eq!(saved_source(&second), Some("redirector"), "{second:?}");
        assert_eq!(windows(&rig).len(), 2, "the expiry costs one more prompt");

        session.set(false); // and again: no third window in one run
        let third = run_one(&mut rig, fixtures::item(3, "10.1136/f2.3"));
        assert_eq!(
            third,
            Outcome::NeedsUser {
                why: NeedsWhy::LoginIneffective,
                handoff: hand_for("10.1136/f2.3")
            }
        );
        assert_handed_off(&third, "10.1136/f2.3");
        assert_eq!(windows(&rig).len(), 2, "at most two prompts per run");
    }

    #[test]
    fn inst_an_item_that_bounced_while_another_signed_in_retries_without_a_prompt() {
        let session = Rc::new(Cell::new(false));
        let mut web = Web::new(&session);
        web.slow = Some(("10.1016", 20_000));
        let mut farm = Farm::new(cfg(), web);
        farm.spawn(fixtures::item(100, "10.1136/s.0"), true);
        farm.rig.run();
        assert_eq!(windows(&farm.rig).len(), 1, "the first sign-in");

        session.set(false); // expired; two items find out at the same time, one of them late
        farm.spawn(fixtures::item(1, "10.1136/s.1"), true);
        farm.spawn(fixtures::item(2, "10.1016/s.2"), true);
        farm.rig.run();
        for n in [100, 1, 2] {
            let outcome = farm.outcome(n).expect("done");
            assert_eq!(
                saved_source(&outcome),
                Some("redirector"),
                "item {n}: {outcome:?}"
            );
        }
        assert_eq!(
            windows(&farm.rig).len(),
            2,
            "the late item reloaded silently under the new epoch"
        );
        assert_eq!(
            loads(&farm.rig).len(),
            6,
            "every item: one bounce and one successful load"
        );
    }

    #[test]
    fn inst_f25_a_sign_in_that_does_not_take_effect_stops_the_prompts() {
        let mut rig = Rig::new(bouncing(Reply::ok(2_000, window_ok(LoginResult::Landed))));
        let want = Outcome::NeedsUser {
            why: NeedsWhy::LoginIneffective,
            handoff: hand(),
        };
        assert_eq!(run_one(&mut rig, snap()), want);
        assert_eq!(
            (windows(&rig).len(), loads(&rig).len()),
            (1, 2),
            "one window, one reload, then the hand-off"
        );
        assert_handed_off(&want, DOI);
        // Later items of the run do not prompt and do not load the redirector either.
        assert_eq!(run_one(&mut rig, fixtures::item(2, DOI)), want);
        assert_eq!((windows(&rig).len(), loads(&rig).len()), (1, 2));
    }

    #[test]
    fn inst_f3_a_conditional_access_refusal_is_final_for_everybody() {
        let mut refused = window_ok(LoginResult::CaBlocked);
        refused["ca_error"] = json!("AADSTS53000");
        let mut rig = Rig::new(bouncing(Reply::ok(1_500, refused)));
        let want = Outcome::NeedsUser {
            why: NeedsWhy::LoginRefused,
            handoff: hand(),
        };
        assert_eq!(run_one(&mut rig, snap()), want);
        assert_eq!(
            session(&rig, Realm::Openathens),
            SessionState::Refused {
                code: Some("AADSTS53000".into())
            }
        );
        assert_eq!(run_one(&mut rig, fixtures::item(2, DOI)), want);
        assert_eq!(
            (windows(&rig).len(), loads(&rig).len()),
            (1, 1),
            "no second window, no second load"
        );
    }

    #[test]
    fn inst_f24_a_window_left_alone_times_out_and_does_not_come_back() {
        let mut rig = Rig::new(bouncing(Reply::Never));
        let start = rig.host.now;
        let want = Outcome::NeedsUser {
            why: NeedsWhy::LoginTimeout,
            handoff: hand(),
        };
        assert_eq!(run_one(&mut rig, snap()), want);
        let waited = rig.host.now - start;
        assert!(
            (305_000..310_000).contains(&waited),
            "the 300 s window timeout plus the engine's 5 s grace: {waited}"
        );
        assert!(
            rig.count(|e| matches!(e, Effect::Cancel { .. })) >= 1,
            "the unanswered window is closed"
        );
        assert!(rig.host.pending.is_empty());
        // The run does not wait another five minutes for the next item.
        let before = rig.host.now;
        assert_eq!(run_one(&mut rig, fixtures::item(2, DOI)), want);
        assert!(rig.host.now - before < 60_000);
        assert_eq!(windows(&rig).len(), 1);
    }

    #[test]
    fn inst_f24_the_window_can_report_its_own_timeout() {
        let mut rig = Rig::new(bouncing(Reply::ok(
            300_000,
            window_ok(LoginResult::Timeout),
        )));
        assert_eq!(
            run_one(&mut rig, snap()),
            Outcome::NeedsUser {
                why: NeedsWhy::LoginTimeout,
                handoff: hand()
            }
        );
    }

    #[test]
    fn inst_a_declined_sign_in_is_final_for_the_run() {
        for result in [LoginResult::ClosedByUser, LoginResult::Handoff] {
            let mut rig = Rig::new(bouncing(Reply::ok(1_000, window_ok(result))));
            let want = Outcome::NeedsUser {
                why: NeedsWhy::LoginDeclined,
                handoff: hand(),
            };
            assert_eq!(run_one(&mut rig, snap()), want, "{result:?}");
            assert_eq!(
                run_one(&mut rig, fixtures::item(2, DOI)),
                want,
                "{result:?}: the next item does not ask again"
            );
            assert_eq!(
                (windows(&rig).len(), loads(&rig).len()),
                (1, 1),
                "{result:?}"
            );
        }
    }

    #[test]
    fn inst_hand_off_mode_and_non_interactive_runs_never_open_a_window() {
        let mut config = cfg();
        config.inst.login_mode = LoginMode::Handoff;
        let mut rig = Rig::with_config(
            config,
            bouncing(Reply::ok(1_000, window_ok(LoginResult::Landed))),
        );
        assert_eq!(
            run_one(&mut rig, snap()),
            Outcome::NeedsUser {
                why: NeedsWhy::HandoffRequested,
                handoff: hand()
            }
        );
        assert!(windows(&rig).is_empty());

        // Two non-interactive items: the first one's bounce must not leave the second waiting for a dead prober.
        let mut farm = Farm::new(
            cfg(),
            bouncing(Reply::ok(1_000, window_ok(LoginResult::Landed))),
        );
        farm.spawn(fixtures::item(1, "10.1136/ni.1"), false);
        farm.spawn(fixtures::item(2, "10.1016/ni.2"), false);
        let start = farm.rig.host.now;
        assert_eq!(farm.rig.run(), SimEnd::Idle);
        for n in [1, 2] {
            assert!(
                matches!(
                    farm.outcome(n),
                    Some(Outcome::NeedsUser {
                        why: NeedsWhy::HandoffRequested,
                        ..
                    })
                ),
                "item {n}: {:?}",
                farm.outcome(n)
            );
        }
        assert!(windows(&farm.rig).is_empty());
        assert!(
            farm.rig.host.now - start < 30_000,
            "nobody waited for the stale-probe timer"
        );
    }

    #[test]
    fn inst_a_probe_dropped_without_a_report_hands_the_realm_on() {
        let mut rig = Rig::new(script());
        let states = rig
            .exec(snap(), |cx| async move {
                let Admission::Proceed { epoch } = cx.session_admit(Realm::Openathens).await else {
                    return None;
                };
                let probe = Probe::new(&cx, Realm::Openathens, epoch);
                let read = || {
                    cx.shared
                        .with(|s| {
                            s.login
                                .realms
                                .get(&Realm::Openathens)
                                .map(|r| r.state.clone())
                        })
                        .flatten()
                };
                let during = read();
                drop(probe);
                Some((during, read()))
            })
            .flatten()
            .expect("admitted");
        assert!(
            matches!(states.0, Some(SessionState::Probing { .. })),
            "{:?}",
            states.0
        );
        assert_eq!(states.1, Some(SessionState::SignedOut));
    }

    // ----------------------------------------------------------------------- F4: "Please wait..." polling

    fn head_rule(e: &Effect) -> bool {
        matches!(e, Effect::Http { req, .. } if req.method == Method::Head)
    }

    fn waiting_world() -> ScriptResponder {
        off_campus().on(
            is_type("browser_load"),
            browser_page(
                500,
                "https://go.openathens.net/wait",
                PageClass::OpenathensWait,
            ),
        )
    }

    #[test]
    fn inst_f4_the_wait_page_is_polled_with_head_requests_and_then_reloaded() {
        let s = off_campus()
            .on_seq(
                is_type("browser_load"),
                vec![
                    browser_page(
                        500,
                        "https://go.openathens.net/wait",
                        PageClass::OpenathensWait,
                    ),
                    browser_pdf(900, PDF, 7),
                ],
            )
            .on_seq(
                head_rule,
                vec![
                    probe_reply(100, 200, PageClass::OpenathensWait, None),
                    probe_reply(100, 201, PageClass::OpenathensWait, None),
                ],
            );
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, snap());
        assert_eq!(saved_source(&outcome), Some("redirector"), "{outcome:?}");
        let polls = heads(&rig);
        assert_eq!(polls.len(), 2, "one 'still waiting', one 'ready'");
        for (_, req) in &polls {
            assert_eq!(
                (req.url.as_str(), req.follow_redirects),
                (redirector().as_str(), false),
                "a no-follow HEAD of the redirector URL"
            );
        }
        assert!(
            polls[1].0 - polls[0].0 >= policy::OPENATHENS_POLL_MS,
            "polls are {} ms apart at least",
            policy::OPENATHENS_POLL_MS
        );
        assert_eq!(
            loads(&rig).len(),
            2,
            "loaded again once the redirector had its answer"
        );
    }

    #[test]
    fn inst_f4_a_redirect_with_a_location_also_means_ready() {
        let s = off_campus()
            .on_seq(
                is_type("browser_load"),
                vec![
                    browser_page(
                        500,
                        "https://go.openathens.net/wait",
                        PageClass::OpenathensWait,
                    ),
                    browser_pdf(900, PDF, 7),
                ],
            )
            .on(
                head_rule,
                probe_reply(
                    100,
                    302,
                    PageClass::OpenathensWait,
                    Some("https://www.bmj.com/content/372/bmj.n71"),
                ),
            );
        let mut rig = Rig::new(s);
        assert_eq!(saved_source(&run_one(&mut rig, snap())), Some("redirector"));
        assert_eq!(heads(&rig).len(), 1);
    }

    #[test]
    fn inst_f4_a_wait_page_that_never_resolves_ends_in_a_hand_off() {
        let s = waiting_world().on(
            head_rule,
            probe_reply(100, 200, PageClass::OpenathensWait, None),
        );
        let mut rig = Rig::new(s);
        let start = rig.host.now;
        let outcome = run_one(&mut rig, snap());
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::HandoffRequested,
                handoff: hand()
            }
        );
        assert_eq!(loads(&rig).len(), 1, "no reload without an answer");
        let polls = heads(&rig).len();
        assert!(
            (8..=13).contains(&polls),
            "polled every 2.5 s for at most 30 s: {polls}"
        );
        assert!(rig.host.now - start < 45_000);
    }

    #[test]
    fn inst_f4_the_poll_can_learn_that_the_resource_is_unknown_or_the_organisation_wrong() {
        let mut rig = Rig::new(waiting_world().on(
            head_rule,
            probe_reply(100, 404, PageClass::OpenathensUnknownResource, None),
        ));
        assert_eq!(
            run_one(&mut rig, snap()),
            Outcome::NotEntitled { handoff: hand() }
        );

        let mut rig = Rig::new(waiting_world().on(
            head_rule,
            probe_reply(100, 403, PageClass::OpenathensBadOrg, None),
        ));
        assert_eq!(
            run_one(&mut rig, snap()),
            Outcome::NeedsUser {
                why: NeedsWhy::Misconfigured,
                handoff: hand()
            }
        );
        assert_eq!(
            session(&rig, Realm::Openathens),
            SessionState::Halted {
                why: HaltWhy::Misconfigured
            }
        );
    }

    // ------------------------------------------------------------------- redirector answers and failures

    #[test]
    fn inst_an_unknown_resource_is_not_entitled() {
        let s = off_campus().on(
            is_type("browser_load"),
            browser_page(
                500,
                "https://go.openathens.net/unknown",
                PageClass::OpenathensUnknownResource,
            ),
        );
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, snap());
        assert_eq!(outcome, Outcome::NotEntitled { handoff: hand() });
        assert_handed_off(&outcome, DOI);
        assert_eq!(
            session(&rig, Realm::Openathens),
            SessionState::SignedOut,
            "the probe was given back"
        );
    }

    #[test]
    fn inst_a_wrong_organisation_is_a_misconfiguration_for_every_item() {
        let s = off_campus().on(
            is_type("browser_load"),
            browser_page(
                500,
                "https://go.openathens.net/org",
                PageClass::OpenathensBadOrg,
            ),
        );
        let mut rig = Rig::new(s);
        let want = Outcome::NeedsUser {
            why: NeedsWhy::Misconfigured,
            handoff: hand(),
        };
        assert_eq!(run_one(&mut rig, snap()), want);
        assert_eq!(run_one(&mut rig, fixtures::item(2, DOI)), want);
        assert_eq!(loads(&rig).len(), 1, "the second item does not even try");
    }

    #[test]
    fn inst_passthrough_publishers_skip_the_redirector() {
        let lww = "10.1097/sla.0000000000005603";
        let page_url = "https://journals.lww.com/annalsofsurgery/fulltext/2022/09000/x.4.aspx";
        let mut config = cfg();
        config.inst.use_ezproxy = false;
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, page_url, AccessHint::NoAccess, Vec::new(), 3),
            )
            .on(
                is_type("run_translators"),
                Reply::ok(100, translators_ok(None)),
            );
        let mut rig = Rig::with_config(config, s);
        let outcome = run_one(&mut rig, fixtures::item(1, lww));
        assert_eq!(
            outcome,
            Outcome::NotEntitled {
                handoff: hand_for(lww)
            }
        );
        assert!(loads(&rig).is_empty(), "the redirector is a no-op for LWW");
    }

    #[test]
    fn inst_a_hidden_load_that_times_out_still_counts_its_reservation() {
        let mut rig = Rig::new(off_campus().on(is_type("browser_load"), Reply::Never));
        let start = rig.host.now;
        let outcome = run_one(&mut rig, snap());
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::HandoffRequested,
                handoff: hand()
            }
        );
        assert!(rig.host.now - start >= 60_000);
        assert_eq!(
            ledger(&rig),
            (1, 0),
            "the publisher may have served the PDF: the sent reservation is committed"
        );
        assert!(rig.count(|e| matches!(e, Effect::Cancel { .. })) >= 1);
        assert_eq!(session(&rig, Realm::Openathens), SessionState::SignedOut);
    }

    #[test]
    fn inst_a_hidden_load_that_fails_on_the_network_gives_the_reservation_back() {
        let mut rig =
            Rig::new(off_campus().on(is_type("browser_load"), Reply::err(50, OpErrKind::Network)));
        let outcome = run_one(&mut rig, snap());
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::NetworkDown,
                handoff: hand()
            }
        );
        assert_eq!(ledger(&rig), (0, 0), "nothing was transferred");
    }

    #[test]
    fn inst_bytes_that_are_not_a_pdf_are_neither_saved_nor_counted() {
        let not_pdf = Inspection::Pdf(PdfVerdict {
            is_pdf: true,
            junk_prefix: 0,
            has_eof: true,
            encrypted: false,
            looks_html: true,
            len: 80_000,
        });
        let s = off_campus().on(
            is_type("browser_load"),
            Reply::ok(500, browser_ok(PDF, &not_pdf, Some(body(7, 80_000)), None)),
        );
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, snap());
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::HandoffRequested,
                handoff: hand()
            }
        );
        assert_eq!(ledger(&rig), (0, 0));
        assert_eq!(rig.count(|e| matches!(e, Effect::Release { handle: 7 })), 1);
        assert_eq!(rig.count(|e| matches!(e, Effect::SavePdf { .. })), 0);
    }

    // ---------------------------------------------------------------------------------------- 5. EZproxy

    const SCIENCEDIRECT: &str =
        "https://www.sciencedirect.com/science/article/pii/S0039606021000001";
    const ELSEVIER_DOI: &str = "10.1016/j.surg.2021.01.001";

    fn ezproxy_entry(target: &str) -> String {
        pure::ezproxy_login_url(&cfg().institution, target)
            .expect("the test institution has an EZproxy")
    }

    fn redirector_load(e: &Effect) -> bool {
        matches!(e, Effect::BrowserLoad { req, .. } if req.url.contains("go.openathens.net"))
    }

    fn ezproxy_load(e: &Effect) -> bool {
        matches!(e, Effect::BrowserLoad { req, .. } if req.url.contains("login.kcl.idm.oclc.org"))
    }

    fn unknown_at_the_redirector() -> Reply {
        browser_page(
            500,
            "https://go.openathens.net/unknown",
            PageClass::OpenathensUnknownResource,
        )
    }

    /// An item whose own URL names the publisher page (so the publisher host is known before any page is read).
    fn item_at(doi: &str, url: &str) -> ItemSnapshot {
        let mut item = fixtures::item(1, doi);
        item.fields.url = url.to_owned();
        item
    }

    #[test]
    fn inst_ezproxy_runs_for_a_listed_host_through_the_login_qurl_entry() {
        let s = paywalled()
            .on(redirector_load, unknown_at_the_redirector())
            .on(ezproxy_load, browser_pdf(900, PDF, 7));
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, snap());
        match &outcome {
            Outcome::Saved {
                route, source, url, ..
            } => assert_eq!(
                (*route, source.as_str(), url.as_str()),
                (Route::Inst, "ezproxy", ARTICLE)
            ),
            other => panic!("{other:?}"),
        }
        let sent = loads(&rig);
        assert_eq!(
            sent.iter().map(|r| r.url.clone()).collect::<Vec<_>>(),
            vec![redirector(), ezproxy_entry(ARTICLE)],
            "OpenAthens first, then EZproxy for the page it saw"
        );
        assert_eq!(sent[1].hold_while, vec![UrlClassKind::EzproxyLogin]);
        assert_eq!(
            (sent[1].ua, sent[1].capture_pdf, sent[1].want_page),
            (Ua::Plain, true, true)
        );
        assert!(heads(&rig).is_empty(), "a listed host is not probed");
        assert_eq!(
            session(&rig, Realm::Ezproxy),
            SessionState::SignedIn { verified: true }
        );
        assert_persist_before_barrier(&rig);
    }

    #[test]
    fn inst_ezproxy_unknown_hosts_are_probed_once_and_the_answer_is_cached() {
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                page(80, SCIENCEDIRECT, PageClass::AccessDenied),
            )
            .on(redirector_load, unknown_at_the_redirector())
            .on(
                head_rule,
                probe_reply(100, 302, PageClass::EzproxyHostNotConfigured, None),
            );
        let mut rig = Rig::new(s);
        let first = run_one(&mut rig, item_at(ELSEVIER_DOI, SCIENCEDIRECT));
        assert_eq!(
            first,
            Outcome::NotEntitled {
                handoff: hand_for(ELSEVIER_DOI)
            }
        );
        let probes = heads(&rig);
        assert_eq!(probes.len(), 1);
        let (url, follow) = (probes[0].1.url.as_str(), probes[0].1.follow_redirects);
        assert_eq!(
            (url, follow),
            ("https://www-sciencedirect-com.kcl.idm.oclc.org/", false),
            "a no-follow HEAD of the proxified host"
        );
        assert_eq!(
            rig.exec(fixtures::item(9, ELSEVIER_DOI), |cx| async move {
                cx.ezproxy_known("www.sciencedirect.com")
            }),
            Some(Some(false))
        );
        assert!(rig.host.persisted.contains_key(&PersistKey::Ezproxy));

        let second_doi = "10.1016/j.surg.2021.01.002";
        let second = run_one(&mut rig, fixtures::item(2, second_doi));
        assert_eq!(
            second,
            Outcome::NotEntitled {
                handoff: hand_for(second_doi)
            }
        );
        assert_eq!(heads(&rig).len(), 1, "the answer is cached for seven days");
        assert_eq!(loads(&rig).len(), 2, "the redirector twice, EZproxy never");
    }

    #[test]
    fn inst_ezproxy_probe_that_finds_the_host_configured_leads_to_its_own_sign_in() {
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                page(80, SCIENCEDIRECT, PageClass::AccessDenied),
            )
            .on(redirector_load, unknown_at_the_redirector())
            .on(
                head_rule,
                probe_reply(
                    100,
                    302,
                    PageClass::EzproxyNeedLogin,
                    Some("https://login.kcl.idm.oclc.org/login?x"),
                ),
            )
            .on_seq(
                ezproxy_load,
                vec![
                    browser_page(
                        500,
                        "https://login.kcl.idm.oclc.org/login",
                        PageClass::EzproxyNeedLogin,
                    ),
                    browser_pdf(900, PDF, 7),
                ],
            )
            .on(
                is_type("login_window"),
                Reply::ok(2_000, window_ok(LoginResult::Landed)),
            );
        let mut rig = Rig::new(s);
        let doi_url = pure::doi_url(ELSEVIER_DOI);
        let outcome = run_one(&mut rig, item_at(ELSEVIER_DOI, SCIENCEDIRECT));
        assert_eq!(saved_source(&outcome), Some("ezproxy"), "{outcome:?}");
        let shown = windows(&rig);
        assert_eq!(shown.len(), 1);
        assert_eq!(
            (shown[0].realm, shown[0].kind, shown[0].entry_url.as_str()),
            (
                Realm::Ezproxy,
                LoginKind::Login,
                ezproxy_entry(&doi_url).as_str()
            )
        );
        assert_eq!(
            rig.exec(fixtures::item(9, ELSEVIER_DOI), |cx| async move {
                cx.ezproxy_known("www.sciencedirect.com")
            }),
            Some(Some(true))
        );
        assert_eq!(
            session(&rig, Realm::Ezproxy),
            SessionState::SignedIn { verified: true }
        );
    }

    #[test]
    fn inst_f5_an_ezproxy_that_rejects_a_listed_host_is_remembered() {
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                page(80, ARTICLE, PageClass::AccessDenied),
            )
            .on(redirector_load, unknown_at_the_redirector())
            .on(
                ezproxy_load,
                browser_page(
                    500,
                    "https://login.kcl.idm.oclc.org/bmj",
                    PageClass::EzproxyHostNotConfigured,
                ),
            );
        let mut rig = Rig::new(s);
        let want = Outcome::NotEntitled { handoff: hand() };
        assert_eq!(run_one(&mut rig, item_at(DOI, ARTICLE)), want);
        assert_eq!(loads(&rig).len(), 2, "redirector and EZproxy");
        assert_eq!(
            rig.exec(fixtures::item(9, DOI), |cx| async move {
                cx.ezproxy_known("www.bmj.com")
            }),
            Some(Some(false))
        );

        let mut again = item_at(DOI, ARTICLE);
        again.item = 2;
        assert_eq!(run_one(&mut rig, again), want);
        assert_eq!(
            loads(&rig).len(),
            3,
            "the proxy said no: the list no longer sends the next item there"
        );
    }

    #[test]
    fn inst_f5_an_ezproxy_suspend_page_halts_every_item() {
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                page(80, ARTICLE, PageClass::AccessDenied),
            )
            .on(redirector_load, unknown_at_the_redirector())
            .on(
                ezproxy_load,
                browser_page(
                    500,
                    "https://login.kcl.idm.oclc.org/suspend.htm",
                    PageClass::EzproxyError { suspend: true },
                ),
            );
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, item_at(DOI, ARTICLE));
        assert_eq!(outcome, Outcome::Halted { handoff: hand() });
        assert_handed_off(&outcome, DOI);
        assert!(halted(&mut rig));
        assert_eq!(
            session(&rig, Realm::Ezproxy),
            SessionState::Halted {
                why: HaltWhy::Suspended
            }
        );
        assert!(
            rig.effects().iter().any(|e| matches!(
                e,
                Effect::Persist {
                    key: PersistKey::Halts,
                    ..
                }
            )),
            "the halt is persisted"
        );

        let (http, browser) = (rig.http().len(), loads(&rig).len());
        let mut other = fixtures::item(2, "10.1016/j.x.1");
        other.fields.url = SCIENCEDIRECT.to_owned();
        assert_eq!(
            run_one(&mut rig, other),
            Outcome::Halted {
                handoff: hand_for("10.1016/j.x.1")
            }
        );
        assert_eq!(
            (rig.http().len(), loads(&rig).len()),
            (http, browser),
            "the other items send nothing"
        );
    }

    // ------------------------------------------------------------------------------------ 6. stop signals

    #[test]
    fn inst_f11_a_429_halts_the_route_and_is_not_retried() {
        for (retry_after_s, min_ms) in [(Some(120), 29 * 60_000), (Some(7_200), 7_100_000)] {
            let s = kcl().on(
                url_has("https://doi.org/"),
                page(80, ARTICLE, PageClass::RateLimited { retry_after_s }),
            );
            let mut rig = Rig::new(s);
            let outcome = run_one(&mut rig, snap());
            assert_eq!(outcome, Outcome::Halted { handoff: hand() });
            assert_handed_off(&outcome, DOI);
            assert_eq!(
                requests(&rig).len(),
                1,
                "no retry on the institutional route"
            );
            let (_, until) = rig
                .exec(snap(), |cx| async move { cx.inst_halted() })
                .flatten()
                .expect("halted");
            assert!(
                until > rig.host.wall + min_ms,
                "halted for max(Retry-After, 30 min): until {until}, now {}",
                rig.host.wall
            );

            let before = rig.http().len();
            assert_eq!(
                run_one(&mut rig, fixtures::item(2, DOI)),
                Outcome::Halted { handoff: hand() }
            );
            assert_eq!(rig.http().len(), before, "later items send nothing");
        }
    }

    #[test]
    fn inst_a_429_at_the_redirector_halts_the_route_too() {
        let s = off_campus().on(
            redirector_load,
            browser_page(
                500,
                "https://go.openathens.net/slow-down",
                PageClass::RateLimited {
                    retry_after_s: None,
                },
            ),
        );
        let mut rig = Rig::new(s);
        assert_eq!(
            run_one(&mut rig, snap()),
            Outcome::Halted { handoff: hand() }
        );
        assert!(halted(&mut rig));
        assert_eq!(loads(&rig).len(), 1);
        assert_eq!(ledger(&rig), (0, 0));
    }

    #[test]
    fn inst_licence_stop_text_halts_the_route() {
        let direct = kcl().on(
            url_has("https://doi.org/"),
            page(80, ARTICLE, PageClass::LicenceStop),
        );
        let browser = off_campus().on(
            redirector_load,
            browser_page(
                500,
                "https://go.openathens.net/stop",
                PageClass::LicenceStop,
            ),
        );
        for (name, s) in [("direct", direct), ("redirector", browser)] {
            let mut rig = Rig::new(s);
            let outcome = run_one(&mut rig, snap());
            assert_eq!(outcome, Outcome::Halted { handoff: hand() }, "{name}");
            assert_handed_off(&outcome, DOI);
            assert!(halted(&mut rig), "{name}");
        }
    }

    #[test]
    fn inst_three_hard_blocks_in_a_row_halt_the_route() {
        let s = kcl().on(
            url_has("https://doi.org/"),
            page(
                50,
                ARTICLE,
                PageClass::Blocked {
                    vendor: Vendor::Cloudflare,
                },
            ),
        );
        let mut rig = Rig::new(s);
        let first = run_one(&mut rig, fixtures::item(1, "10.1136/blk.1"));
        assert!(matches!(first, Outcome::Blocked { .. }), "{first:?}");
        assert_handed_off(&first, "10.1136/blk.1");
        assert!(!halted(&mut rig));

        // The blocked family is out for the time being; asking again sends nothing and does not count as a signal.
        let sent = requests(&rig).len();
        let same_family = run_one(&mut rig, fixtures::item(2, "10.1136/blk.2"));
        assert!(
            matches!(same_family, Outcome::Blocked { .. }),
            "{same_family:?}"
        );
        assert_eq!(requests(&rig).len(), sent);

        let second = run_one(&mut rig, fixtures::item(3, "10.1016/blk.3"));
        assert!(matches!(second, Outcome::Blocked { .. }), "{second:?}");
        assert!(!halted(&mut rig));
        let third = run_one(&mut rig, fixtures::item(4, "10.1007/blk.4"));
        assert_eq!(
            third,
            Outcome::Halted {
                handoff: hand_for("10.1007/blk.4")
            }
        );
        assert!(halted(&mut rig), "the third signal stops the route");
        let before = rig.http().len();
        assert_eq!(
            run_one(&mut rig, fixtures::item(5, "10.1093/blk.5")),
            Outcome::Halted {
                handoff: hand_for("10.1093/blk.5")
            }
        );
        assert_eq!(rig.http().len(), before);
    }

    const PDF1: &str = "https://www.bmj.com/content/bmj/372/bmj.n71.full1.pdf";
    const PDF2: &str = "https://www.bmj.com/content/bmj/372/bmj.n71.full2.pdf";

    fn challenged(interactive: bool) -> PageClass {
        PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive,
        }
    }

    fn pdf_get(e: &Effect) -> bool {
        matches!(e, Effect::Http { req, .. } if req.url.contains("full"))
    }

    fn pdf_load(e: &Effect) -> bool {
        matches!(e, Effect::BrowserLoad { req, .. } if req.url.contains("full"))
    }

    /// The landing page offers `urls`; each of them (on both tiers) answers with a bot check, and the redirector
    /// does not know the publisher.
    fn behind_a_bot_check(urls: &[&str], interactive: bool) -> ScriptResponder {
        let cands = urls.iter().map(|u| pdf_cand(u)).collect();
        kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, ARTICLE, AccessHint::Unknown, cands, 3),
            )
            .on(pdf_get, page(50, PDF1, challenged(interactive)))
            .on(pdf_load, browser_page(500, PDF1, challenged(interactive)))
            .on(redirector_load, unknown_at_the_redirector())
    }

    #[test]
    fn inst_a_second_challenge_on_one_host_halts_the_route() {
        let mut rig = Rig::new(behind_a_bot_check(&[PDF1, PDF2], false));
        let outcome = run_one(&mut rig, snap());
        assert_eq!(outcome, Outcome::Halted { handoff: hand() });
        assert_handed_off(&outcome, DOI);
        assert!(halted(&mut rig));
        assert_eq!(
            loads(&rig).len(),
            2,
            "each candidate went through the browser tier once"
        );
        assert!(windows(&rig).is_empty(), "the check was not interactive");
        assert_eq!(ledger(&rig), (0, 0));
    }

    #[test]
    fn inst_a_single_bot_check_that_nothing_clears_is_reported_as_a_challenge() {
        let mut rig = Rig::new(behind_a_bot_check(&[PDF1], false));
        let outcome = run_one(&mut rig, snap());
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::Challenge,
                handoff: hand()
            }
        );
        assert!(
            !halted(&mut rig),
            "one check is not a reason to stop everybody"
        );
    }

    #[test]
    fn inst_an_interactive_bot_check_opens_one_window_and_the_pdf_it_releases_is_saved() {
        let released = login_ok(
            LoginResult::Pdf,
            PDF1,
            &pdf_inspection(80_000),
            Some(body(9, 80_000)),
            None,
        );
        let mut rig = Rig::new(
            behind_a_bot_check(&[PDF1], true)
                .on(is_type("login_window"), Reply::ok(4_000, released)),
        );
        let outcome = run_one(&mut rig, snap());
        assert_eq!(saved_source(&outcome), Some("doi"), "{outcome:?}");
        let shown = windows(&rig);
        assert_eq!(shown.len(), 1);
        assert_eq!(
            (
                shown[0].kind,
                shown[0].entry_url.as_str(),
                shown[0].capture_pdf
            ),
            (LoginKind::Challenge, PDF1, true)
        );
        assert_eq!(ledger(&rig), (1, 0), "one download, counted once");
        assert_eq!(rig.count(|e| matches!(e, Effect::Release { handle: 9 })), 1);
    }

    #[test]
    fn inst_challenge_windows_can_be_switched_off() {
        let mut config = cfg();
        config.inst.challenge_windows = false;
        let mut rig = Rig::with_config(config, behind_a_bot_check(&[PDF1], true));
        let outcome = run_one(&mut rig, snap());
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::Challenge,
                handoff: hand()
            }
        );
        assert!(windows(&rig).is_empty());
    }

    // --------------------------------------------------------------------------------------- 7. group libraries

    fn group_item() -> ItemSnapshot {
        let mut item = snap();
        item.library.kind = LibraryKind::Group;
        item.library.name = "Lab group".into();
        item
    }

    /// The real fetching and extracting, a scripted answer to the group-library question, and a save that only
    /// records what it was given.
    struct Consent {
        answer: bool,
        asked: Cell<u32>,
        saved: RefCell<Vec<(Route, Source, bool)>>,
    }

    impl Consent {
        fn new(answer: bool) -> Rc<Self> {
            Rc::new(Self {
                answer,
                asked: Cell::new(0),
                saved: RefCell::new(Vec::new()),
            })
        }
    }

    impl Collab for Consent {
        fn fetch<'a>(
            &'a self,
            cx: &'a ItemCx,
            cand: &'a Candidate,
            pol: &'a FetchPolicy,
            pool: &'a mut CandidatePool,
        ) -> Fut<'a, FetchEnd> {
            <Real as Collab>::fetch(&Real, cx, cand, pol, pool)
        }

        fn extract<'a>(
            &'a self,
            cx: &'a ItemCx,
            page: Option<&'a Body>,
            url: &'a str,
            inspection: &'a PageInspection,
            pool: &'a mut CandidatePool,
        ) -> Fut<'a, LandingResult> {
            <Real as Collab>::extract(&Real, cx, page, url, inspection, pool)
        }

        fn save<'a>(&'a self, _cx: &'a ItemCx, blob: PdfBlob, _ids: &'a Ids) -> Fut<'a, Outcome> {
            self.saved
                .borrow_mut()
                .push((blob.route, blob.source, blob.ticket.is_some()));
            Box::pin(async move {
                let PdfBlob {
                    body,
                    verdict,
                    url,
                    version,
                    route,
                    source,
                    ticket,
                    ..
                } = blob;
                if let Some(ticket) = ticket {
                    ticket.commit(verdict.len);
                }
                drop(body);
                Outcome::Saved {
                    route,
                    source: crate::sched::source_name(source),
                    version,
                    url,
                    attachment_key: "1/FAKE".into(),
                }
            })
        }

        fn group_consent<'a>(&'a self, _cx: &'a ItemCx) -> Fut<'a, bool> {
            self.asked.set(self.asked.get() + 1);
            let answer = self.answer;
            Box::pin(async move { answer })
        }
    }

    fn with_group_policy(policy: GroupPolicy) -> EngineConfig {
        let mut config = cfg();
        config.inst.group_libraries = policy;
        config
    }

    #[test]
    fn inst_f20_a_group_library_with_policy_never_is_handed_off_before_any_publisher_request() {
        let mut rig = Rig::with_config(
            with_group_policy(GroupPolicy::Never),
            off_campus().on(redirector_load, browser_pdf(900, PDF, 7)),
        );
        let outcome = run_one(&mut rig, group_item());
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::GroupLibrary,
                handoff: hand()
            }
        );
        assert_handed_off(&outcome, DOI);
        assert!(
            requests(&rig).is_empty(),
            "no publisher request, so no licence use: {:?}",
            request_urls(&rig)
        );
        assert_eq!(ledger(&rig), (0, 0));
    }

    #[test]
    fn inst_f20_a_group_library_with_policy_ask_asks_once_and_obeys() {
        for (answer, saved) in [(false, false), (true, true)] {
            let mut farm = Farm::new(
                with_group_policy(GroupPolicy::Ask),
                off_campus().on(redirector_load, browser_pdf(900, PDF, 7)),
            );
            let pipe = Consent::new(answer);
            farm.spawn_with(group_item(), true, Rc::clone(&pipe) as Rc<dyn Collab>);
            farm.rig.run();
            assert_eq!(pipe.asked.get(), 1, "answer {answer}: asked exactly once");
            let outcome = farm.outcome(1).expect("done");
            if saved {
                assert_eq!(
                    outcome,
                    Outcome::Saved {
                        route: Route::Inst,
                        source: "redirector".into(),
                        version: Version::Published,
                        url: redirector(),
                        attachment_key: "1/FAKE".into()
                    }
                );
                assert_eq!(
                    *pipe.saved.borrow(),
                    vec![(Route::Inst, Source::Redirector, true)],
                    "the save gets the ticket of its download"
                );
            } else {
                assert_eq!(
                    outcome,
                    Outcome::NeedsUser {
                        why: NeedsWhy::GroupLibrary,
                        handoff: hand()
                    }
                );
                assert!(
                    requests(&farm.rig).is_empty(),
                    "denied before the first licence reservation"
                );
                assert_eq!(ledger(&farm.rig), (0, 0));
            }
        }
    }

    #[test]
    fn inst_f20_a_non_interactive_run_never_asks_and_treats_ask_as_never() {
        let mut farm = Farm::new(with_group_policy(GroupPolicy::Ask), off_campus());
        let pipe = Consent::new(true);
        farm.spawn_with(group_item(), false, Rc::clone(&pipe) as Rc<dyn Collab>);
        farm.rig.run();
        assert_eq!(pipe.asked.get(), 0);
        assert_eq!(
            farm.outcome(1),
            Some(Outcome::NeedsUser {
                why: NeedsWhy::GroupLibrary,
                handoff: hand()
            })
        );
    }

    #[test]
    fn inst_f20_a_group_library_with_policy_allow_goes_on() {
        let mut rig = Rig::with_config(
            with_group_policy(GroupPolicy::Allow),
            off_campus().on(redirector_load, browser_pdf(900, PDF, 7)),
        );
        let outcome = run_one(&mut rig, group_item());
        assert_eq!(saved_source(&outcome), Some("redirector"), "{outcome:?}");
    }

    // -------------------------------------------------------------------------------------------- caps

    #[test]
    fn inst_f26_a_spent_cap_defers_the_item_and_sends_no_pdf_request() {
        let with_cap = |hourly: u32, daily: u32, mb: u32| {
            let mut config = cfg();
            config.inst.hourly_cap = hourly;
            config.inst.daily_cap = daily;
            config.inst.mb_per_hour = mb;
            config
        };
        // The byte cap lets a first file through whatever its size; the file that is counted then fills the hour.
        let cases = [
            ("hourly", with_cap(1, 80, 300), CapKind::Hourly),
            ("daily", with_cap(30, 1, 300), CapKind::Daily),
            ("mb per hour", with_cap(30, 80, 1), CapKind::MbPerHour),
        ];
        for (name, config, kind) in cases {
            let s = kcl()
                .on(
                    url_has("https://doi.org/"),
                    landing(80, ARTICLE, AccessHint::Unknown, vec![pdf_cand(PDF)], 3),
                )
                .on(url_has("full.pdf"), pdf(400, PDF, 4));
            let mut rig = Rig::with_config(config, s);
            let first = run_one(&mut rig, snap());
            assert_eq!(saved_source(&first), Some("doi"), "{name}: {first:?}");
            let second = run_one(&mut rig, fixtures::item(2, DOI));
            assert_eq!(second, Outcome::Deferred { cap: kind }, "{name}");
            let pdf_gets = request_urls(&rig)
                .iter()
                .filter(|u| u.as_str() == PDF)
                .count();
            assert_eq!(
                pdf_gets, 1,
                "{name}: the deferred item never asked the publisher for the PDF"
            );
            assert_eq!(ledger(&rig), (1, 0), "{name}");
        }
    }

    #[test]
    fn inst_a_cap_is_checked_before_the_redirector_load() {
        let mut config = cfg();
        config.inst.hourly_cap = 0;
        let mut rig = Rig::with_config(
            config,
            off_campus().on(redirector_load, browser_pdf(900, PDF, 7)),
        );
        assert_eq!(
            run_one(&mut rig, snap()),
            Outcome::Deferred {
                cap: CapKind::Hourly
            }
        );
        assert!(
            loads(&rig).is_empty(),
            "no hidden load when the licence budget is spent"
        );
        assert_eq!(ledger(&rig), (0, 0));
    }

    // ----------------------------------------------------------------------------- spacing, hedging, cancel

    /// A publisher that always answers: the article page after `page_ms`, the PDF after `pdf_ms`.
    struct Publisher {
        page_ms: u64,
        pdf_ms: u64,
        handles: u32,
    }

    impl Responder for Publisher {
        fn respond(&mut self, op: &PendingOp, _now: u64, _wall: u64) -> Reply {
            self.handles += 1;
            let handle = self.handles;
            match &op.effect {
                Effect::Http { req, .. } if req.url.contains(ALMA_HOST) => alma(true),
                Effect::Http { req, .. } if req.url.starts_with("https://doi.org/") => landing(
                    self.page_ms,
                    ARTICLE,
                    AccessHint::Unknown,
                    vec![pdf_cand(PDF)],
                    handle,
                ),
                Effect::Http { req, .. } if req.url == PDF => pdf(self.pdf_ms, PDF, handle),
                Effect::SavePdf { req, .. } => {
                    Reply::ok(100, save_ok(&format!("1/K{:04}", req.item)))
                }
                _ => Reply::err(10, OpErrKind::Network),
            }
        }
    }

    #[test]
    fn inst_one_request_per_family_at_a_time_and_pdfs_are_spaced() {
        let (page_ms, pdf_ms) = (500, 3_000);
        let mut farm = Farm::new(
            cfg(),
            Publisher {
                page_ms,
                pdf_ms,
                handles: 0,
            },
        );
        // Same family (BMJ), same article: both items want the page and then the PDF from the same publisher.
        farm.spawn(fixtures::item(1, DOI), true);
        farm.spawn(fixtures::item(2, DOI), true);
        assert_eq!(farm.rig.run(), SimEnd::Idle);
        for n in [1, 2] {
            let outcome = farm.outcome(n).expect("done");
            assert_eq!(saved_source(&outcome), Some("doi"), "item {n}: {outcome:?}");
        }
        let sent = requests(&farm.rig);
        assert_eq!(sent.len(), 4, "a page and a PDF per item");
        for pair in sent.windows(2) {
            let ((at, first), (next_at, _)) = (&pair[0], &pair[1]);
            let (took, gap) = match first {
                Effect::Http {
                    retain: Retain::Pdf,
                    ..
                } => (pdf_ms, u64::from(policy::INST_PDF_GAP_MS)),
                _ => (page_ms, u64::from(policy::INST_PAGE_GAP_MS)),
            };
            assert!(
                *next_at >= at + took + gap,
                "the next request of the family started at {next_at}, after one at {at} that took {took} and needs a {gap} ms gap"
            );
        }
        assert_persist_before_barrier(&farm.rig);
    }

    #[test]
    fn inst_never_hedges_a_slow_institutional_pdf() {
        let mut farm = Farm::new(
            cfg(),
            Publisher {
                page_ms: 100,
                pdf_ms: 30_000,
                handles: 0,
            },
        );
        farm.spawn(snap(), true);
        farm.rig.host.run_until_idle(15_000).expect("run");
        let in_flight = farm
            .rig
            .host
            .pending_where(|e| matches!(e, Effect::Http { req, .. } if req.url == PDF));
        assert_eq!(
            in_flight.len(),
            1,
            "one PDF request after 15 s, while OA hedging would have started a second after 3 s"
        );
        assert_eq!(
            requests(&farm.rig).len(),
            2,
            "the page and the one PDF request: {:?}",
            request_urls(&farm.rig)
        );
        farm.rig.run();
        assert_eq!(saved_source(&farm.outcome(1).expect("done")), Some("doi"));
        assert_eq!(
            request_urls(&farm.rig)
                .iter()
                .filter(|u| u.as_str() == PDF)
                .count(),
            1
        );
    }

    #[test]
    fn inst_cancelling_mid_fetch_counts_the_sent_reservation() {
        let by_pdf = kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, ARTICLE, AccessHint::Unknown, vec![pdf_cand(PDF)], 3),
            )
            .on(url_has("full.pdf"), Reply::Never);
        let by_load = off_campus().on(redirector_load, Reply::Never);
        for (name, s) in [("PDF request", by_pdf), ("hidden load", by_load)] {
            let mut farm = Farm::new(cfg(), s);
            farm.spawn(snap(), true);
            farm.rig.host.run_until_idle(5_000).expect("run");
            assert_eq!(
                ledger(&farm.rig),
                (0, 1),
                "{name}: reserved and persisted before the request left"
            );
            assert_persist_before_barrier(&farm.rig);

            farm.cancel(1);
            let out = farm.rig.host.step(Vec::new()).expect("step");
            assert!(
                out.effects
                    .iter()
                    .any(|e| matches!(e, Effect::Cancel { .. })),
                "{name}: the request is cancelled in the same step"
            );
            assert_eq!(
                ledger(&farm.rig),
                (1, 0),
                "{name}: the request may have reached the publisher, so it counts"
            );
            assert!(farm.rig.host.pending.is_empty(), "{name}");
            assert_eq!(
                farm.outcome(1),
                None,
                "{name}: a cancelled item reports nothing itself"
            );
        }
    }

    // ------------------------------------------------------------------------------- hand-off on every failure

    #[test]
    fn inst_an_item_without_a_doi_hands_off_to_the_page_it_reached() {
        let s = kcl()
            .on(
                url_has("https://www.bmj.com/"),
                landing(80, ARTICLE, AccessHint::NoAccess, Vec::new(), 3),
            )
            .on(
                is_type("run_translators"),
                Reply::ok(100, translators_ok(None)),
            )
            .on(redirector_load, unknown_at_the_redirector())
            .on(
                ezproxy_load,
                browser_page(
                    500,
                    "https://login.kcl.idm.oclc.org/x",
                    PageClass::OpenathensUnknownResource,
                ),
            );
        let mut rig = Rig::new(s);
        let mut item = fixtures::item(1, "");
        item.fields.url =
            "https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71?utm_source=feed".into();
        let outcome = run_one(&mut rig, item);
        let want = handoff_with_landing(&cfg().institution, &Ids::default(), Some(ARTICLE));
        assert_eq!(
            outcome,
            Outcome::NotEntitled {
                handoff: want.clone()
            }
        );
        assert!(
            want.redirector_url.is_some() && want.ezproxy_url.is_some(),
            "{want:?}"
        );
        assert_eq!(
            (want.libkey_url, want.doi_url),
            (None, None),
            "no DOI, no LibKey or DOI link"
        );
    }

    #[test]
    fn inst_direct_access_can_be_switched_off() {
        let mut config = cfg();
        config.inst.try_direct = false;
        let mut rig = Rig::with_config(config, kcl().on(redirector_load, browser_pdf(900, PDF, 7)));
        let outcome = run_one(&mut rig, snap());
        assert_eq!(saved_source(&outcome), Some("redirector"), "{outcome:?}");
        assert_eq!(
            request_urls(&rig),
            vec![redirector()],
            "the redirector is the first and only request"
        );
    }

    #[test]
    fn inst_with_every_sign_in_route_off_a_denial_ends_in_a_hand_off() {
        let mut config = cfg();
        config.inst.use_openathens = false;
        config.inst.use_ezproxy = false;
        let mut rig = Rig::with_config(config, off_campus());
        let outcome = run_one(&mut rig, snap());
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::HandoffRequested,
                handoff: hand()
            }
        );
        assert!(loads(&rig).is_empty());
    }

    #[test]
    fn inst_the_route_shows_its_own_progress_row_and_traces_its_steps() {
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, ARTICLE, AccessHint::Unknown, vec![pdf_cand(PDF)], 3),
            )
            .on(url_has("full.pdf"), pdf(400, PDF, 4));
        let mut rig = Rig::new(s);
        let trace = rig
            .exec(snap(), |cx| async move {
                let outcome = drive(&cx, Vec::new(), &Real).await;
                (outcome, cx.take_trace())
            })
            .expect("done");
        assert_eq!(saved_source(&trace.0), Some("doi"));
        let steps: Vec<(String, String, String)> = trace
            .1
            .into_iter()
            .map(|t| (t.src, t.tier, t.class))
            .collect();
        let has = |src: &str, tier: &str, class: &str| {
            steps.contains(&(src.to_owned(), tier.to_owned(), class.to_owned()))
        };
        assert!(has("alma", "api", "held"), "{steps:?}");
        assert!(has("doi", "http", "landing"), "{steps:?}");
        assert!(has("extractor", "http", "pdf"), "{steps:?}");
        assert_eq!(
            steps
                .iter()
                .filter(|s| s.0 == "doi" && s.1 == "http")
                .count(),
            1,
            "each fetch is traced once: {steps:?}"
        );
        assert_eq!(
            rig.rows().first().map(String::as_str),
            Some(l10n::id::INSTITUTIONAL)
        );
    }

    // ----------------------------------------------------------------------- hand-off links, fetch codes

    #[test]
    fn inst_every_failure_carries_the_hand_off_links() {
        let no_link = kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, ARTICLE, AccessHint::Unknown, Vec::new(), 3),
            )
            .on(
                is_type("run_translators"),
                Reply::ok(100, translators_ok(None)),
            );
        let cases: Vec<(&str, ScriptResponder)> = vec![
            ("not held", script().on(url_has(ALMA_HOST), alma(false))),
            (
                "429",
                kcl()
                    .on(
                        url_has("https://doi.org/"),
                        page(
                            80,
                            ARTICLE,
                            PageClass::RateLimited {
                                retry_after_s: None,
                            },
                        ),
                    )
                    .on(redirector_load, unknown_at_the_redirector()),
            ),
            (
                "hard block",
                kcl().on(
                    url_has("https://doi.org/"),
                    page(
                        80,
                        ARTICLE,
                        PageClass::Blocked {
                            vendor: Vendor::Cloudflare,
                        },
                    ),
                ),
            ),
            (
                "licence stop",
                kcl().on(
                    url_has("https://doi.org/"),
                    page(80, ARTICLE, PageClass::LicenceStop),
                ),
            ),
            (
                "unknown resource",
                off_campus().on(redirector_load, unknown_at_the_redirector()),
            ),
            (
                "wrong organisation",
                off_campus().on(
                    redirector_load,
                    browser_page(
                        500,
                        "https://go.openathens.net/org",
                        PageClass::OpenathensBadOrg,
                    ),
                ),
            ),
            (
                "redirector timeout",
                off_campus().on(redirector_load, Reply::Never),
            ),
            (
                "network down",
                off_campus().on(redirector_load, Reply::err(20, OpErrKind::Network)),
            ),
            ("no pdf link", no_link),
            (
                "sign-in declined",
                bouncing(Reply::ok(500, window_ok(LoginResult::ClosedByUser))),
            ),
            (
                "sign-in ineffective",
                bouncing(Reply::ok(500, window_ok(LoginResult::Landed))),
            ),
            ("bot check", behind_a_bot_check(&[PDF1], false)),
        ];
        for (name, s) in cases {
            let mut rig = Rig::new(s);
            let outcome = run_one(&mut rig, snap());
            assert!(
                !matches!(outcome, Outcome::Saved { .. }),
                "{name}: {outcome:?}"
            );
            assert_handed_off(&outcome, DOI);
        }
    }

    #[test]
    fn inst_page_classes_of_the_login_routes_that_come_back_as_fetch_codes_are_understood() {
        // Straight from the HTTP tier (no browser): the pipeline reports these as `Failed(<code>)`.
        let suspended = kcl().on(
            url_has("https://doi.org/"),
            page(80, ARTICLE, PageClass::EzproxyError { suspend: true }),
        );
        let mut rig = Rig::new(suspended);
        assert_eq!(
            run_one(&mut rig, snap()),
            Outcome::Halted { handoff: hand() }
        );
        assert!(halted(&mut rig));

        let wrong_org = kcl().on(
            url_has("https://doi.org/"),
            page(80, ARTICLE, PageClass::OpenathensBadOrg),
        );
        let mut rig = Rig::new(wrong_org);
        assert_eq!(
            run_one(&mut rig, snap()),
            Outcome::NeedsUser {
                why: NeedsWhy::Misconfigured,
                handoff: hand()
            }
        );

        // "Please wait..." on a plain fetch is a timeout of the redirector, nothing to poll there.
        let waiting = kcl().on(
            url_has("https://doi.org/"),
            page(80, ARTICLE, PageClass::OpenathensWait),
        );
        let mut config = cfg();
        config.inst.use_openathens = false;
        let mut rig = Rig::with_config(config, waiting);
        assert_eq!(
            run_one(&mut rig, snap()),
            Outcome::NeedsUser {
                why: NeedsWhy::HandoffRequested,
                handoff: hand()
            }
        );
    }

    #[test]
    fn inst_a_save_that_the_library_refuses_still_counts_the_download() {
        // The PDF arrived: the ledger counts it whatever the save says (save.rs commits the ticket).
        let s = script()
            .on(url_has(ALMA_HOST), alma(true))
            .on(
                url_has("https://doi.org/"),
                page(80, ARTICLE, PageClass::AccessDenied),
            )
            .on(redirector_load, browser_pdf(900, PDF, 7))
            .on(
                is_type("save_pdf"),
                Reply::err(50, OpErrKind::AlreadyHasPdf),
            );
        let mut rig = Rig::new(s);
        let outcome = run_one(&mut rig, snap());
        assert_eq!(
            outcome,
            Outcome::Skipped {
                reason: SkipReason::PdfAddedMeanwhile
            }
        );
        assert_eq!(ledger(&rig), (1, 0));
    }

    // ------------------------------------------------------------- SAML, EZproxy for every host, configuration

    const OX_IDP: &str = "https://registry.shibboleth.ox.ac.uk/idp";
    const NATURE_DOI: &str = "10.1038/nature12373";
    const NATURE_DOI_URL: &str = "https://doi.org/10.1038/nature12373";
    const NATURE_URL: &str = "https://www.nature.com/articles/nature12373";
    const NATURE_PDF: &str = "https://www.nature.com/articles/nature12373.pdf";
    const SHIB_IDP: &str =
        "https://idp.shibboleth.ox.ac.uk/idp/profile/SAML2/Redirect/SSO?execution=e1s1";

    fn shib() -> PageClass {
        PageClass::IdpLogin {
            idp: Idp::Other,
            ca_error: None,
        }
    }

    /// An institution on the UK federation only: no OpenAthens domain, no EZproxy, no Alma.
    fn saml_only() -> EngineConfig {
        let mut c = cfg();
        c.institution.openathens_domain = String::new();
        c.institution.ezproxy = None;
        c.institution.ezproxy_hosts.clear();
        c.institution.alma = None;
        c.institution.passthrough_hosts.clear();
        c.institution.idp_hosts.clear();
        c.institution.saml_entity_id = Some(OX_IDP.into());
        c.inst.check_alma = false;
        c
    }

    /// An institution whose EZproxy proxies every host and that is not on OpenAthens at all.
    fn ezproxy_all_only() -> EngineConfig {
        let mut c = cfg();
        c.institution.openathens_domain = String::new();
        c.institution.ezproxy_hosts.clear();
        c.institution.ezproxy_all_hosts = true;
        c.institution.alma = None;
        c.institution.passthrough_hosts.clear();
        c.inst.check_alma = false;
        c
    }

    /// KCL plus a SAML identity provider: OpenAthens first, the publisher's own login behind it.
    fn kcl_with_saml() -> EngineConfig {
        let mut c = cfg();
        c.institution.saml_entity_id = Some(OX_IDP.into());
        c.inst.use_ezproxy = false;
        c
    }

    fn wayfless_of(target: &str) -> String {
        fulltext_core::wayfless::wayfless_url(None, OX_IDP, target).expect("a supported publisher")
    }

    fn nature_entry() -> String {
        wayfless_of(NATURE_URL)
    }

    fn saml_load(e: &Effect) -> bool {
        matches!(e, Effect::BrowserLoad { req, .. } if req.url.contains("sp.nature.com/saml/login"))
    }

    /// doi.org says "no access" (off campus), saving works.
    fn nature_off_campus() -> ScriptResponder {
        script()
            .on(is_type("save_pdf"), Reply::ok(100, save_ok(SAVED_KEY)))
            .on(
                url_has("https://doi.org/"),
                page(80, NATURE_URL, PageClass::AccessDenied),
            )
    }

    fn effect_urls(rig: &Rig) -> Vec<String> {
        rig.effects()
            .iter()
            .filter_map(|e| fixtures::effect_url(e).map(str::to_owned))
            .collect()
    }

    fn assert_no_openathens_or_proxy(rig: &Rig) {
        for url in effect_urls(rig) {
            assert!(
                !url.contains("openathens")
                    && !url.contains("/redirector/")
                    && !url.contains("qurl="),
                "a route that is not configured was used: {url}"
            );
        }
    }

    #[test]
    fn inst_route_configuration_needs_at_least_one_usable_route() {
        let usable = |f: &dyn Fn(&mut EngineConfig)| {
            let mut c = cfg();
            f(&mut c);
            has_usable_route(&c)
        };
        assert!(usable(&|_| {}));
        // Nothing but a blank OpenAthens domain.
        assert!(!usable(&|c| {
            c.inst.try_direct = false;
            c.institution.openathens_domain = "  ".into();
            c.institution.ezproxy = None;
        }));
        // Everything configured but switched off.
        assert!(!usable(&|c| {
            c.inst.try_direct = false;
            c.inst.use_openathens = false;
            c.inst.use_ezproxy = false;
            c.inst.use_saml = false;
            c.institution.saml_entity_id = Some(OX_IDP.into());
        }));
        // Each route alone is enough.
        assert!(usable(&|c| {
            c.inst.try_direct = false;
            c.inst.use_ezproxy = false;
        }));
        assert!(usable(&|c| {
            c.inst.try_direct = false;
            c.inst.use_openathens = false;
        }));
        assert!(usable(&|c| {
            c.inst.try_direct = false;
            c.inst.use_openathens = false;
            c.inst.use_ezproxy = false;
            c.institution.saml_entity_id = Some(OX_IDP.into());
        }));
        assert!(usable(&|c| {
            c.inst.use_openathens = false;
            c.inst.use_ezproxy = false;
        }));
        // A malformed entityID is no route.
        assert!(!usable(&|c| {
            c.inst.try_direct = false;
            c.inst.use_openathens = false;
            c.inst.use_ezproxy = false;
            c.institution.saml_entity_id = Some("not an entity id".into());
        }));
        // An EZproxy without a login host is no route.
        assert!(!usable(&|c| {
            c.inst.try_direct = false;
            c.inst.use_openathens = false;
            c.institution.ezproxy = Some(Ezproxy {
                login_host: " ".into(),
                suffix: "x.example".into(),
            });
        }));
    }

    #[test]
    fn inst_a_configuration_with_nothing_usable_is_misconfigured_before_any_request() {
        let mut config = cfg();
        config.inst.try_direct = false;
        config.institution.openathens_domain = String::new();
        config.institution.ezproxy = None;
        config.institution.saml_entity_id = None;
        let mut rig = Rig::with_config(config.clone(), kcl());
        let outcome = run_one(&mut rig, snap());
        assert_eq!(
            outcome,
            Outcome::NeedsUser {
                why: NeedsWhy::Misconfigured,
                handoff: handoff(&config.institution, &ids_of(DOI)),
            }
        );
        assert!(
            rig.effects()
                .iter()
                .all(|e| fixtures::effect_url(e).is_none()),
            "no request of any kind: {:?}",
            effect_urls(&rig)
        );
        // The row says so with the existing "settings need attention" text.
        assert_eq!(
            l10n::outcome_row(&outcome).l10n,
            "zotero-fulltext-status-misconfigured"
        );
        // The hand-off of such an institution has no redirector, EZproxy or WAYFless link.
        let Outcome::NeedsUser { handoff: h, .. } = outcome else {
            panic!("not a hand-off")
        };
        assert_eq!(
            (h.redirector_url, h.ezproxy_url, h.wayfless_url),
            (None, None, None)
        );
        assert_eq!(h.doi_url.as_deref(), Some(DOI_URL));

        // Direct access alone is a route: the item is tried and ends as an ordinary hand-off.
        let mut direct_only = config;
        direct_only.inst.try_direct = true;
        let mut rig = Rig::with_config(direct_only, off_campus());
        let outcome = run_one(&mut rig, snap());
        assert!(
            matches!(
                outcome,
                Outcome::NeedsUser {
                    why: NeedsWhy::HandoffRequested,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(request_urls(&rig), vec![DOI_URL.to_owned()]);
        assert_no_openathens_or_proxy(&rig);
    }

    #[test]
    fn inst_saml_only_institution_loads_the_wayfless_link_and_builds_no_redirector_url() {
        let s = nature_off_campus().on(is_type("browser_load"), browser_pdf(900, NATURE_PDF, 7));
        let mut rig = Rig::with_config(saml_only(), s);
        let outcome = run_one(&mut rig, fixtures::item(1, NATURE_DOI));
        match &outcome {
            Outcome::Saved {
                route, source, url, ..
            } => assert_eq!(
                (*route, source.as_str(), url.as_str()),
                (Route::Inst, "saml", NATURE_DOI_URL),
                "the stored URL names the article, not the sign-in link"
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            request_urls(&rig),
            vec![NATURE_DOI_URL.to_owned(), nature_entry()],
            "direct, then the WAYFless link"
        );
        let sent = loads(&rig);
        assert_eq!(sent.len(), 1);
        let want = BrowserReq {
            url: nature_entry(),
            ua: Ua::Plain,
            capture_pdf: true,
            want_page: true,
            settle_ms: 3_000,
            timeout_ms: 60_000,
            hold_while: vec![UrlClassKind::PublisherSsoHop],
        };
        assert_eq!(sent[0], want);
        assert!(
            nature_entry().starts_with(
                "https://sp.nature.com/saml/login?idp=https%3A%2F%2Fregistry.shibboleth.ox.ac.uk%2Fidp&targetUrl="
            ),
            "{}",
            nature_entry()
        );
        assert_eq!(
            session(&rig, Realm::Saml),
            SessionState::SignedIn { verified: true }
        );
        assert_eq!(session(&rig, Realm::Openathens), SessionState::SignedOut);
        assert_persist_before_barrier(&rig);
        assert_eq!(ledger(&rig), (1, 0));
        assert_eq!(rig.count(|e| matches!(e, Effect::LoginWindow { .. })), 0);
        assert_no_openathens_or_proxy(&rig);
        assert!(rig.host.pending.is_empty());
    }

    #[test]
    fn inst_saml_signs_in_through_one_window_in_its_own_realm() {
        let s = nature_off_campus()
            .on_seq(
                is_type("browser_load"),
                vec![
                    browser_page(500, SHIB_IDP, shib()),
                    browser_pdf(900, NATURE_PDF, 7),
                ],
            )
            .on(
                is_type("login_window"),
                Reply::ok(2_000, window_ok(LoginResult::Landed)),
            );
        let mut rig = Rig::with_config(saml_only(), s);
        let outcome = run_one(&mut rig, fixtures::item(1, NATURE_DOI));
        assert_eq!(saved_source(&outcome), Some("saml"), "{outcome:?}");
        let shown = windows(&rig);
        assert_eq!(shown.len(), 1);
        assert_eq!(
            (shown[0].realm, shown[0].kind, shown[0].entry_url.as_str()),
            (Realm::Saml, LoginKind::Login, nature_entry().as_str()),
            "the window opens on the WAYFless link, in the SAML realm"
        );
        assert!(shown[0].capture_pdf);
        assert_eq!(loads(&rig).len(), 2, "the bounce, then the retry");
        assert_eq!(
            session(&rig, Realm::Saml),
            SessionState::SignedIn { verified: true }
        );
        assert_eq!(session(&rig, Realm::Openathens), SessionState::SignedOut);
        assert_eq!(session(&rig, Realm::Ezproxy), SessionState::SignedOut);
        assert_persist_before_barrier(&rig);
        assert_eq!(ledger(&rig), (1, 0));
        assert_no_openathens_or_proxy(&rig);
    }

    /// The publisher sites for several items at once: the WAYFless link bounces to the Shibboleth IdP until a
    /// window has been answered, then hands out the PDF.
    struct Federation {
        session: Rc<Cell<bool>>,
        handles: u32,
    }

    impl Responder for Federation {
        fn respond(&mut self, op: &PendingOp, _now: u64, _wall: u64) -> Reply {
            self.handles += 1;
            let handle = self.handles;
            match &op.effect {
                Effect::Http { req, .. } if req.url.starts_with("https://doi.org/") => {
                    page(80, &req.url, PageClass::AccessDenied)
                }
                Effect::BrowserLoad { req, .. } => {
                    if self.session.get() {
                        browser_pdf(800, &req.url, handle)
                    } else {
                        browser_page(800, SHIB_IDP, shib())
                    }
                }
                Effect::LoginWindow { .. } => {
                    self.session.set(true);
                    Reply::ok(3_000, window_ok(LoginResult::Landed))
                }
                Effect::SavePdf { req, .. } => {
                    Reply::ok(100, save_ok(&format!("1/K{:04}", req.item)))
                }
                _ => Reply::err(10, OpErrKind::Network),
            }
        }
    }

    #[test]
    fn inst_saml_items_of_several_publishers_share_one_window() {
        let session = Rc::new(Cell::new(false));
        let mut farm = Farm::new(
            saml_only(),
            Federation {
                session: Rc::clone(&session),
                handles: 100,
            },
        );
        let dois = [
            "10.1038/saml.1",
            "10.1007/saml.2",
            "10.1002/saml.3",
            "10.1080/saml.4",
            "10.1177/saml.5",
        ];
        for (n, doi) in (1..).zip(dois) {
            farm.spawn(fixtures::item(n, doi), true);
        }
        assert_eq!(farm.rig.run(), SimEnd::Idle);
        for n in 1..=5 {
            let outcome = farm.outcome(n).expect("every item ends");
            assert_eq!(
                saved_source(&outcome),
                Some("saml"),
                "item {n}: {outcome:?}"
            );
        }
        let shown = windows(&farm.rig);
        assert_eq!(shown.len(), 1, "ONE window for five items");
        assert_eq!(shown[0].realm, Realm::Saml);
        assert_eq!(
            loads(&farm.rig).len(),
            6,
            "the prober twice, everybody else once"
        );
        let hosts: BTreeSet<String> = loads(&farm.rig)
            .iter()
            .filter_map(|r| pure::host_of(&r.url))
            .collect();
        assert_eq!(
            hosts,
            BTreeSet::from([
                "sp.nature.com".to_owned(),
                "fsso.springer.com".to_owned(),
                "onlinelibrary.wiley.com".to_owned(),
                "www.tandfonline.com".to_owned(),
                "journals.sagepub.com".to_owned(),
            ]),
            "each publisher's own platform form"
        );
        assert_eq!(ledger(&farm.rig), (5, 0));
        assert_persist_before_barrier(&farm.rig);
        assert!(farm.rig.host.pending.is_empty());
        assert_no_openathens_or_proxy(&farm.rig);
    }

    #[test]
    fn inst_saml_declined_sign_in_hands_off_with_the_wayfless_link() {
        let s = nature_off_campus()
            .on(is_type("browser_load"), browser_page(500, SHIB_IDP, shib()))
            .on(
                is_type("login_window"),
                Reply::ok(1_000, window_ok(LoginResult::ClosedByUser)),
            );
        let config = saml_only();
        let mut rig = Rig::with_config(config.clone(), s);
        let outcome = run_one(&mut rig, fixtures::item(1, NATURE_DOI));
        let Outcome::NeedsUser {
            why: NeedsWhy::LoginDeclined,
            handoff: h,
        } = &outcome
        else {
            panic!("{outcome:?}")
        };
        assert_eq!(h.wayfless_url.as_deref(), Some(nature_entry().as_str()));
        assert_eq!(h.doi_url.as_deref(), Some(NATURE_DOI_URL));
        assert_eq!((&h.redirector_url, &h.ezproxy_url), (&None, &None));
        assert_eq!(*h, handoff(&config.institution, &ids_of(NATURE_DOI)));
        // The row offers the link as an "open in your browser" button.
        let row = l10n::outcome_row(&outcome);
        assert!(
            row.actions.contains(&RowAction::OpenBrowser {
                url: nature_entry(),
                l10n: "zotero-fulltext-action-open-wayfless".into()
            }),
            "{:?}",
            row.actions
        );
    }

    #[test]
    fn inst_saml_skips_publishers_without_a_wayfless_form() {
        // LWW: the redirector passes it through unchanged and no WAYFless form exists.
        let s = kcl().on(
            url_has("https://doi.org/"),
            page(80, "https://journals.lww.com/x", PageClass::AccessDenied),
        );
        let config = saml_only();
        let mut rig = Rig::with_config(config.clone(), s);
        let lww = "10.1097/sla.0000000000005603";
        let outcome = run_one(&mut rig, fixtures::item(1, lww));
        assert_eq!(
            outcome,
            Outcome::NotEntitled {
                handoff: handoff(&config.institution, &ids_of(lww))
            }
        );
        assert!(loads(&rig).is_empty(), "no sign-in is attempted");
        assert_eq!(rig.count(|e| matches!(e, Effect::LoginWindow { .. })), 0);
        let Outcome::NotEntitled { handoff: h } = outcome else {
            panic!("hand-off")
        };
        assert_eq!(h.wayfless_url, None);
    }

    #[test]
    fn inst_openathens_that_does_not_know_the_publisher_falls_back_to_saml() {
        let s = nature_off_campus()
            .on(redirector_load, unknown_at_the_redirector())
            .on(saml_load, browser_pdf(900, NATURE_PDF, 7));
        let config = kcl_with_saml();
        let mut rig = Rig::with_config(config.clone(), s);
        let outcome = run_one(&mut rig, fixtures::item(1, NATURE_DOI));
        assert_eq!(saved_source(&outcome), Some("saml"), "{outcome:?}");
        let redirector_entry = pure::redirector_url(&config.institution, NATURE_DOI_URL)
            .expect("KCL has an OpenAthens domain");
        assert_eq!(
            loads(&rig)
                .iter()
                .map(|r| r.url.clone())
                .collect::<Vec<_>>(),
            vec![redirector_entry, nature_entry()],
            "OpenAthens first, then the publisher's own login"
        );
        assert_eq!(
            request_urls(&rig).first().map(String::as_str),
            Some(NATURE_DOI_URL),
            "direct access came first of all"
        );
        assert_eq!(
            session(&rig, Realm::Saml),
            SessionState::SignedIn { verified: true }
        );
        assert_persist_before_barrier(&rig);
        assert_eq!(ledger(&rig), (1, 0));
    }

    #[test]
    fn inst_a_working_openathens_session_that_is_refused_does_not_try_saml_too() {
        let s = nature_off_campus()
            .on(
                redirector_load,
                browser_landing(900, NATURE_URL, AccessHint::NoAccess, Vec::new(), 5),
            )
            .on(
                is_type("run_translators"),
                Reply::ok(100, translators_ok(None)),
            )
            .on(saml_load, browser_pdf(900, NATURE_PDF, 7));
        let config = kcl_with_saml();
        let mut rig = Rig::with_config(config.clone(), s);
        let outcome = run_one(&mut rig, fixtures::item(1, NATURE_DOI));
        let Outcome::NotEntitled { handoff: h } = &outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(
            loads(&rig).len(),
            1,
            "the same institution would say no again"
        );
        assert!(h.wayfless_url.is_some(), "the link is still offered: {h:?}");
        assert_eq!(session(&rig, Realm::Saml), SessionState::SignedOut);
    }

    #[test]
    fn inst_saml_is_off_with_use_saml_false_or_without_an_entity_id() {
        for mutate in [
            (|c: &mut EngineConfig| c.inst.use_saml = false) as fn(&mut EngineConfig),
            |c| c.institution.saml_entity_id = None,
            |c| c.institution.saml_entity_id = Some("   ".into()),
        ] {
            let mut config = kcl_with_saml();
            mutate(&mut config);
            let s = nature_off_campus().on(redirector_load, unknown_at_the_redirector());
            let mut rig = Rig::with_config(config, s);
            let outcome = run_one(&mut rig, fixtures::item(1, NATURE_DOI));
            assert!(
                matches!(outcome, Outcome::NotEntitled { .. }),
                "{outcome:?}"
            );
            assert_eq!(loads(&rig).len(), 1, "only the redirector");
            assert!(
                !effect_urls(&rig)
                    .iter()
                    .any(|u| u.contains("sp.nature.com"))
            );
        }
    }

    #[test]
    fn inst_saml_prefers_the_page_that_was_reached_over_the_doi() {
        // OUP has no DOI-only WAYFless form; the page doi.org reached supplies the article URL.
        let oup_doi = "10.1093/bjs/znab001";
        let oup_page = "https://academic.oup.com/bjs/article/108/1/1/6065";
        let s = kcl()
            .on(
                url_has("https://doi.org/"),
                landing(80, oup_page, AccessHint::NoAccess, Vec::new(), 3),
            )
            .on(
                is_type("run_translators"),
                Reply::ok(100, translators_ok(None)),
            )
            .on(
                is_type("browser_load"),
                browser_pdf(900, "https://academic.oup.com/bjs/article-pdf/1.pdf", 7),
            );
        let mut config = saml_only();
        config.institution.alma = None;
        let mut rig = Rig::with_config(config, s);
        let outcome = run_one(&mut rig, fixtures::item(1, oup_doi));
        assert_eq!(saved_source(&outcome), Some("saml"), "{outcome:?}");
        assert_eq!(
            loads(&rig).first().map(|r| r.url.clone()),
            Some(wayfless_of(oup_page))
        );
    }

    // ---- EZproxy for every host

    fn ezproxy_all_entry(target: &str) -> String {
        pure::ezproxy_login_url(&ezproxy_all_only().institution, target)
            .expect("the institution has an EZproxy")
    }

    #[test]
    fn inst_ezproxy_only_institution_tries_any_host_and_builds_no_redirector_url() {
        // Nothing is listed, the host is unknown (doi.org stands in for the DOI), the probe cannot tell: try it.
        let s = nature_off_campus().on(ezproxy_load, browser_pdf(900, NATURE_PDF, 7));
        let mut rig = Rig::with_config(ezproxy_all_only(), s);
        let outcome = run_one(&mut rig, fixtures::item(1, NATURE_DOI));
        assert_eq!(saved_source(&outcome), Some("ezproxy"), "{outcome:?}");
        assert_eq!(
            loads(&rig)
                .iter()
                .map(|r| r.url.clone())
                .collect::<Vec<_>>(),
            vec![ezproxy_all_entry(NATURE_DOI_URL)]
        );
        assert_eq!(
            heads(&rig).len(),
            1,
            "one probe of the proxified host: {:?}",
            heads(&rig)
        );
        assert_eq!(
            session(&rig, Realm::Ezproxy),
            SessionState::SignedIn { verified: true }
        );
        assert_eq!(session(&rig, Realm::Openathens), SessionState::SignedOut);
        assert_persist_before_barrier(&rig);
        for url in effect_urls(&rig) {
            assert!(!url.contains("openathens"), "{url}");
        }
    }

    #[test]
    fn inst_ezproxy_all_hosts_learns_from_the_probe_and_skips_unconfigured_hosts() {
        let s = nature_off_campus()
            .on(
                head_rule,
                probe_reply(100, 302, PageClass::EzproxyHostNotConfigured, None),
            )
            .on(ezproxy_load, browser_pdf(900, NATURE_PDF, 7));
        let mut rig = Rig::with_config(ezproxy_all_only(), s);
        let want = |doi: &str| Outcome::NotEntitled {
            handoff: handoff(&ezproxy_all_only().institution, &ids_of(doi)),
        };
        assert_eq!(
            run_one(&mut rig, fixtures::item(1, NATURE_DOI)),
            want(NATURE_DOI)
        );
        assert_eq!(heads(&rig).len(), 1);
        assert!(loads(&rig).is_empty(), "the proxy does not know the host");
        assert_eq!(
            rig.exec(fixtures::item(9, NATURE_DOI), |cx| async move {
                cx.ezproxy_known("doi.org")
            }),
            Some(Some(false))
        );
        assert!(rig.host.persisted.contains_key(&PersistKey::Ezproxy));
        // The next item does not ask again.
        let other = "10.1038/nature99999";
        assert_eq!(run_one(&mut rig, fixtures::item(2, other)), want(other));
        assert_eq!(heads(&rig).len(), 1, "cached for seven days");
        assert!(loads(&rig).is_empty());
    }

    #[test]
    fn inst_ezproxy_all_hosts_learns_from_the_proxy_needhost_page() {
        // The probe cannot tell; the load ends on the proxy's own "cannot serve this address" page.
        let needhost = browser_page(
            500,
            "https://login.kcl.idm.oclc.org/login",
            PageClass::EzproxyError { suspend: false },
        );
        let s = nature_off_campus().on(ezproxy_load, needhost);
        let mut rig = Rig::with_config(ezproxy_all_only(), s);
        let first = run_one(&mut rig, fixtures::item(1, NATURE_DOI));
        assert!(matches!(first, Outcome::NotEntitled { .. }), "{first:?}");
        assert_eq!(loads(&rig).len(), 1);
        assert_eq!(
            rig.exec(fixtures::item(9, NATURE_DOI), |cx| async move {
                cx.ezproxy_known("doi.org")
            }),
            Some(Some(false)),
            "learnt false from the page"
        );
        let second = run_one(&mut rig, fixtures::item(2, "10.1038/nature99999"));
        assert!(matches!(second, Outcome::NotEntitled { .. }), "{second:?}");
        assert_eq!(loads(&rig).len(), 1, "skipped next time");
        assert_eq!(heads(&rig).len(), 1, "and not probed again either");
    }

    #[test]
    fn inst_ezproxy_without_all_hosts_still_skips_a_host_nobody_listed() {
        let mut config = ezproxy_all_only();
        config.institution.ezproxy_all_hosts = false;
        let s = nature_off_campus().on(ezproxy_load, browser_pdf(900, NATURE_PDF, 7));
        let mut rig = Rig::with_config(config, s);
        let outcome = run_one(&mut rig, fixtures::item(1, NATURE_DOI));
        assert!(
            matches!(
                outcome,
                Outcome::NeedsUser { .. } | Outcome::NotEntitled { .. }
            ),
            "{outcome:?}"
        );
        assert!(
            loads(&rig).is_empty(),
            "the old rule: a listed or confirmed host only"
        );
    }

    #[test]
    fn inst_ezproxy_all_hosts_remembers_a_host_the_proxy_served() {
        let mut link = pdf_cand(NATURE_PDF);
        link.record_doi = Some(NATURE_DOI.to_owned());
        let s = nature_off_campus()
            .on(
                ezproxy_load,
                browser_landing(900, NATURE_URL, AccessHint::Unknown, vec![link], 5),
            )
            .on(url_has("nature12373.pdf"), pdf(400, NATURE_PDF, 6));
        let mut rig = Rig::with_config(ezproxy_all_only(), s);
        let outcome = run_one(&mut rig, fixtures::item(1, NATURE_DOI));
        assert_eq!(saved_source(&outcome), Some("ezproxy"), "{outcome:?}");
        assert_eq!(
            rig.exec(fixtures::item(9, NATURE_DOI), |cx| async move {
                cx.ezproxy_known("doi.org")
            }),
            Some(Some(true))
        );
    }
}
