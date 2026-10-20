//! Fetching a candidate URL through the tiers, shared by the OA path (`attempt_oa`) and the institutional route
//! (`fetch_candidate` with `FetchPolicy { route: Inst, .. }`, called from inst.rs).
//!
//! # Tier ladder for one URL (research/SUMMARY (d), judges' failure catalogue F6-F13, F30)
//! 1. HTTP (`cx.http`, UA from `cx.ua_for(host)`; Plain once a host was seen challenging; timeout from
//!    `cx.timeout_for(class, host)`, slow hosts get the long profile). Gate first: OA -> `acquire_oa_host`;
//!    Inst -> `reserve_licence` (PDF only) then `acquire_inst(key, Page|Pdf)`; `barrier: true` whenever a licence
//!    reservation was made for the fetch (the ledger `Persist` is pushed before the request, in the same step).
//! 2. Classify by the `Inspection` JS attached: `Pdf` verdict ok -> done; `Page` with `Landing`/`Html` ->
//!    `FetchEnd::Landing` (caller extracts); `Redirect`/`ReaderShell { rewrite }` -> follow (at most 3 hops, one
//!    reader rewrite, never a URL visited before); `Challenge` -> tier 3; `Blocked` -> `FetchEnd::Blocked` (OA: the
//!    host is walled); `RateLimited` -> OA: one retry after Retry-After <= 10 s, Inst: `halt_inst`;
//!    `AccessDenied`/`NotFound` -> terminal for this candidate; non-PDF bytes on a PDF URL -> the body was
//!    re-classified by core, never loop.
//! 3. Browser (`cx.acquire_browser()` + `cx.browser_load` with `capture_pdf`, plain UA) when allowed and the class is
//!    `Challenge` or the candidate `needs_browser`. The host is pinned to the plain UA from the first challenge on.
//! 4. Visible challenge window (`cx.challenge_window`) only when `allow_challenge_window`, `interactive`, and
//!    `inst.challenge_windows`; the user acts, we never solve CAPTCHAs. A challenge that survives every allowed tier
//!    counts as one challenge outcome for the host (`note_challenge`); two wall it.
//!
//! Every tried URL (query included) goes into `pool.mark_tried`; a page already visited is never re-extracted.
//!
//! # Hedging (OA only, `RunPrefs.hedge`)
//! `attempt_oa` starts attempt k+1 when attempt k has not answered after `HEDGE_AFTER_MS` (`..SLOW_MS` on slow
//! hosts) and k+1 is on a different host key; at most 2 attempts in flight; first verified PDF wins, the loser is
//! dropped (-> `Cancel`). Same-host candidates are never hedged. Institutional attempts are NEVER hedged.
//!
//! # Response shape (core decision 4)
//! The `Inspection` describes what the bytes ARE, not what the spec expected: a `Page` spec can come back as
//! `Inspection::Pdf` (a landing URL that served a PDF) and a `Pdf` spec as `Inspection::Page` (a challenge, login
//! or landing page). Always match on the returned variant. `Inspection::Failed { .. }` and `Inspection::None` are
//! "unusable response": count the candidate as failed, never loop.
//!
//! # Verification
//! A candidate "wins" only with `Inspection::Pdf(v)` where `save::validate(&v)` is ok and the byte count is not
//! below the announced `Content-Length` (a truncated download is retried once on the OA route and refused on the
//! institutional route). A PDF whose record DOI disagrees is rejected before it is fetched (see landing.rs).
//!
//! # Offline
//! `OpErrKind::Offline` pauses the fetch (row `NETWORK_DOWN`), probes by repeating the request every 15 s and gives
//! up after 2 minutes with `FetchEnd::Failed(fail::OFFLINE)`.
//!
//! # Codes of `FetchEnd::Failed`
//! The skeleton's `FetchEnd` has no variant for page classes only the institutional route understands; they come
//! out as `Failed(<code>)` with the stable codes in [`fail`].
use crate::cx::ItemCx;
use crate::l10n::{self, id};
use crate::ops::{Body, BrowserResp, HttpResp, LoginResp, OpResult, TimedOut};
use crate::pipeline::landing;
use crate::pipeline::pool::CandidatePool;
use crate::pipeline::{pure, requests, save};
use crate::proto::*;
use crate::sched::hosts::{HaltReason, is_slow_host, oa_host_key};
use crate::sched::policy::{self, FetchClass};
use crate::sched::{FetchKind, InstKey, LedgerTicket, Permit, Refused, source_name};
use fulltext_core::model::*;
use futures_util::StreamExt;
use futures_util::future::LocalBoxFuture;
use futures_util::stream::FuturesUnordered;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;

/// Stable `FetchEnd::Failed` codes (the institutional route matches on them).
pub(crate) mod fail {
    /// The proxied host is not configured at the EZproxy (`EzproxyHostNotConfigured`).
    pub const EZPROXY_NOT_CONFIGURED: &str = "ezproxy_host_not_configured";
    /// The EZproxy suspended the account (`EzproxyError { suspend: true }`); the route halts for a day.
    pub const EZPROXY_SUSPENDED: &str = "ezproxy_suspended";
    /// Any other EZproxy error page.
    pub const EZPROXY_ERROR: &str = "ezproxy_error";
    /// OpenAthens "Please wait..." (poll with `requests::probe`, or let the browser settle).
    pub const OPENATHENS_WAIT: &str = "openathens_wait";
    /// The OpenAthens redirector does not know the target resource.
    pub const OPENATHENS_UNKNOWN_RESOURCE: &str = "openathens_unknown_resource";
    /// The OpenAthens organisation is wrong (HTTP 403 at the redirector).
    pub const OPENATHENS_BAD_ORG: &str = "openathens_bad_org";
    /// The network was down for the whole pause (2 minutes).
    pub const OFFLINE: &str = "offline";
    /// The candidate URL is not an http(s) URL.
    pub const BAD_URL: &str = "bad_url";
    /// JS answered without the body the engine asked to retain.
    pub const NO_BODY: &str = "no_body";
    /// The op was cancelled on the JS side.
    pub const CANCELLED: &str = "cancelled";
}

/// Follow-ups (meta refresh, reader rewrite) one fetch may make.
const MAX_FOLLOWS: u32 = 3;
/// Candidates one OA phase starts at most (landing pages count).
const OA_ATTEMPT_CAP: u32 = 8;
/// Largest landing page the engine asks JS to retain.
const PAGE_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Longest `Retry-After` an OA fetch honours.
const RETRY_AFTER_CAP_S: u32 = 10;
/// How long a hidden browser must see a stable URL before the page counts as settled.
const BROWSER_SETTLE_MS: u32 = 3_000;

/// A PDF that passed the byte-level check, ready for `save::save_pdf_blob`. Dropping it releases the body.
pub(crate) struct PdfBlob {
    pub body: Body,
    pub verdict: PdfVerdict,
    /// The candidate URL that produced it.
    pub url: String,
    #[allow(dead_code)] // kept so the struct mirrors its source; nothing reads it yet
    pub final_url: String,
    pub landing_url: Option<String>,
    pub source: Source,
    pub version: Version,
    pub route: Route,
    /// Licence ticket of an institutional fetch, resolved by `save` (commit on save) or dropped (counted).
    pub ticket: Option<LedgerTicket>,
}

/// A reached HTML page that is not a PDF: the caller extracts candidates (landing.rs) or reads the class.
pub(crate) struct LandingPage {
    pub url: String,
    pub final_url: String,
    pub page: Option<Body>,
    pub inspection: PageInspection,
}

/// What an institutional or OA fetch must follow.
#[derive(Clone, Debug)]
pub(crate) struct FetchPolicy {
    pub route: Route,
    /// Publisher family for institutional gating (`None` on the OA route).
    pub inst_key: Option<InstKey>,
    pub allow_browser: bool,
    pub allow_challenge_window: bool,
}

impl FetchPolicy {
    pub(crate) fn oa() -> Self {
        Self {
            route: Route::Oa,
            inst_key: None,
            allow_browser: true,
            allow_challenge_window: false,
        }
    }

    pub(crate) fn inst(key: InstKey) -> Self {
        Self {
            route: Route::Inst,
            inst_key: Some(key),
            allow_browser: true,
            allow_challenge_window: true,
        }
    }
}

/// How one candidate fetch ended.
pub(crate) enum FetchEnd {
    Pdf(PdfBlob),
    Landing(LandingPage),
    /// Bytes were not a usable PDF and the body classified as nothing useful.
    NotPdf,
    AccessDenied,
    NotFound,
    /// Still challenged after the tiers this policy allows.
    Challenge {
        host: String,
        interactive: bool,
    },
    Blocked {
        host: String,
    },
    RateLimited {
        retry_after_s: Option<u32>,
    },
    /// Landed on an IdP / EZproxy login page: the session is missing (inst only).
    NeedsLogin {
        #[allow(dead_code)] // kept so the struct mirrors its source; nothing reads it yet
        realm: Realm,
        #[allow(dead_code)] // kept so the struct mirrors its source; nothing reads it yet
        final_url: String,
    },
    /// The publisher's licence-stop / suspend page (inst only): the caller halts the route.
    LicenceStop,
    Timeout,
    Network,
    TooLarge,
    /// A gate refused (halt, cap, skipped host).
    Refused(Refused),
    /// Anything else; the code is one of [`fail`] or the snake_case name of an `OpErrKind`.
    Failed(String),
}

impl FetchEnd {
    /// A short stable name, for traces.
    fn name(&self) -> &'static str {
        match self {
            FetchEnd::Pdf(_) => "pdf",
            FetchEnd::Landing(_) => "landing",
            FetchEnd::NotPdf => "not_pdf",
            FetchEnd::AccessDenied => "access_denied",
            FetchEnd::NotFound => "not_found",
            FetchEnd::Challenge { .. } => "challenge",
            FetchEnd::Blocked { .. } => "blocked",
            FetchEnd::RateLimited { .. } => "rate_limited",
            FetchEnd::NeedsLogin { .. } => "needs_login",
            FetchEnd::LicenceStop => "licence_stop",
            FetchEnd::Timeout => "timeout",
            FetchEnd::Network => "network",
            FetchEnd::TooLarge => "too_large",
            FetchEnd::Refused(_) => "refused",
            FetchEnd::Failed(_) => "failed",
        }
    }
}

// ------------------------------------------------------------------------------------------------ one fetch

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Http,
    Browser,
}

impl Tier {
    fn name(self) -> &'static str {
        match self {
            Tier::Http => "http",
            Tier::Browser => "browser",
        }
    }
}

/// What one network step learned. Created and consumed within one loop turn, so the size gap between `Done` and the
/// small variants costs nothing worth a `Box`.
#[allow(clippy::large_enum_variant)]
enum Step {
    Done(FetchEnd),
    /// Fetch this URL next, on the same tier (meta refresh, reader rewrite).
    Follow(String),
    /// Repeat the same request (the one allowed retry).
    Retry,
    /// A bot check: escalate or give up.
    Challenge {
        interactive: bool,
    },
}

/// A response from any tier, in one shape.
struct Got {
    status: u16,
    final_url: String,
    headers: BTreeMap<String, String>,
    bytes: u64,
    inspect: Inspection,
    pdf: Option<Body>,
    page: Option<Body>,
}

impl Got {
    fn from_http(r: HttpResp) -> Got {
        let HttpResp {
            status,
            final_url,
            headers,
            bytes,
            inspect,
            body,
            ..
        } = r;
        // The one retained body is the PDF when the bytes were a PDF, else the page.
        let (pdf, page) = if matches!(inspect, Inspection::Pdf(_)) {
            (body, None)
        } else {
            (None, body)
        };
        Got {
            status,
            final_url,
            headers,
            bytes,
            inspect,
            pdf,
            page,
        }
    }

    fn from_browser(r: BrowserResp) -> Got {
        let BrowserResp {
            final_url,
            status,
            inspect,
            pdf,
            page,
            ..
        } = r;
        let bytes = pdf.as_ref().map_or(0, Body::len);
        Got {
            status: status.unwrap_or(200),
            final_url,
            headers: BTreeMap::new(),
            bytes,
            inspect,
            pdf,
            page,
        }
    }

    fn from_login(r: LoginResp) -> Got {
        let LoginResp {
            final_url,
            inspect,
            pdf,
            page,
            ..
        } = r;
        let bytes = pdf.as_ref().map_or(0, Body::len);
        Got {
            status: 200,
            final_url,
            headers: BTreeMap::new(),
            bytes,
            inspect,
            pdf,
            page,
        }
    }

    /// Did the host answer like a working server (for the OA host's failure counter)?
    fn host_answered(&self) -> bool {
        if self.status == 429 || self.status >= 500 {
            return false;
        }
        match &self.inspect {
            Inspection::Page(p) => !matches!(
                p.class,
                PageClass::Blocked { .. }
                    | PageClass::Challenge { .. }
                    | PageClass::RateLimited { .. }
            ),
            Inspection::Failed { .. } => false,
            Inspection::Pdf(_)
            | Inspection::Api(_)
            | Inspection::Probe { .. }
            | Inspection::None => true,
        }
    }
}

/// Why a request produced no response.
enum Net {
    TimedOut,
    Op(crate::proto::OpErr),
}

/// The snake_case wire name of a plain enum.
fn wire_name<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Map an op error to the end it stands for.
fn end_for_op(e: &crate::proto::OpErr) -> FetchEnd {
    match e.kind {
        OpErrKind::Timeout => FetchEnd::Timeout,
        OpErrKind::Network | OpErrKind::Tls => FetchEnd::Network,
        OpErrKind::TooLarge => FetchEnd::TooLarge,
        OpErrKind::Offline => FetchEnd::Failed(fail::OFFLINE.to_owned()),
        OpErrKind::Cancelled => FetchEnd::Failed(fail::CANCELLED.to_owned()),
        OpErrKind::Unsupported
        | OpErrKind::AlreadyHasPdf
        | OpErrKind::NotEditable
        | OpErrKind::ParentGone
        | OpErrKind::Io
        | OpErrKind::Internal => FetchEnd::Failed(wire_name(&e.kind)),
    }
}

/// Feed the OA host's circuit breaker. A network that is down (or an op we cancelled ourselves) says nothing about
/// the host, so it counts as neither success nor failure.
fn report_host(permit: &mut Permit, got: &Result<Got, Net>) {
    match got {
        Ok(g) => permit.report(g.host_answered()),
        Err(Net::Op(e)) if matches!(e.kind, OpErrKind::Offline | OpErrKind::Cancelled) => {}
        Err(Net::Op(_) | Net::TimedOut) => permit.report(false),
    }
}

fn net_end(n: Net) -> FetchEnd {
    match n {
        Net::TimedOut => FetchEnd::Timeout,
        Net::Op(e) => end_for_op(&e),
    }
}

/// Show the NETWORK_DOWN row and wait one probe interval; `false` once the give-up time is used up.
async fn pause_for_network(cx: &ItemCx, waited: &mut u64) -> bool {
    if *waited >= policy::NETWORK_DOWN_GIVE_UP_MS {
        return false;
    }
    cx.progress(l10n::row(RowState::Waiting, id::NETWORK_DOWN));
    cx.sleep(policy::NETWORK_PROBE_EVERY_MS).await;
    *waited = waited.saturating_add(policy::NETWORK_PROBE_EVERY_MS);
    true
}

/// Run `make()` against the deadline; on `Offline` pause and run it again (see the module docs).
async fn with_offline_retry<T, F, Fut>(cx: &ItemCx, timeout_ms: u64, make: F) -> Result<T, Net>
where
    F: Fn() -> Fut,
    Fut: Future<Output = OpResult<T>>,
{
    let mut waited = 0u64;
    loop {
        match cx.timeout(timeout_ms, make()).await {
            Err(TimedOut) => return Err(Net::TimedOut),
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(e))
                if e.kind == OpErrKind::Offline && pause_for_network(cx, &mut waited).await => {}
            Ok(Err(e)) => return Err(Net::Op(e)),
        }
    }
}

/// State of one `fetch_candidate` call.
struct Fetch<'a> {
    cx: &'a ItemCx,
    cand: &'a Candidate,
    pol: &'a FetchPolicy,
    /// Identifiers a response must match.
    requested: &'a Ids,
    /// Licence reservation of an institutional PDF fetch; moves into the `PdfBlob` on success.
    ticket: Option<LedgerTicket>,
    /// `canonical_key` of every URL this fetch has requested.
    visited: BTreeSet<String>,
    /// URLs requested after the first (the caller marks them tried).
    trail: Vec<String>,
    /// The one allowed retry (rate limit, truncated download) was used.
    retried: bool,
    /// The one allowed reader-shell rewrite was used.
    rewrote: bool,
    /// The bytes are expected to be a PDF (the candidate says so, or a reader shell was rewritten to one).
    expect_pdf: bool,
    last_tier: Tier,
}

impl<'a> Fetch<'a> {
    fn new(cx: &'a ItemCx, cand: &'a Candidate, pol: &'a FetchPolicy, requested: &'a Ids) -> Self {
        Self {
            cx,
            cand,
            pol,
            requested,
            ticket: None,
            visited: BTreeSet::new(),
            trail: Vec::new(),
            retried: false,
            rewrote: false,
            expect_pdf: cand.kind == CandKind::Pdf,
            last_tier: Tier::Http,
        }
    }

    fn is_inst(&self) -> bool {
        self.pol.route == Route::Inst
    }

    fn can_retry(&self) -> bool {
        !self.retried && !self.is_inst()
    }

    fn inst_key(&self, host: &str) -> InstKey {
        self.pol
            .inst_key
            .clone()
            .unwrap_or_else(|| InstKey::for_host(host))
    }

    fn inspect_spec(&self) -> InspectSpec {
        let (requested, route) = (self.requested.clone(), self.pol.route);
        if self.expect_pdf {
            InspectSpec::Pdf { requested, route }
        } else {
            InspectSpec::Page { requested, route }
        }
    }

    fn pdf_cap(&self) -> u64 {
        u64::from(self.cx.cfg.run.pdf_max_mb).saturating_mul(1024 * 1024)
    }

    /// Reserve the licence for an institutional PDF fetch (once per candidate).
    fn reserve(&mut self) -> Result<(), Refused> {
        if self.is_inst() && self.expect_pdf && self.ticket.is_none() {
            self.ticket = Some(self.cx.reserve_licence(policy::RESERVE_BYTES_DEFAULT)?);
        }
        Ok(())
    }

    /// The "Downloading from <host>" row.
    fn announce(&self, host: &str) {
        let row = l10n::row(RowState::Working, id::DOWNLOADING)
            .arg("source", source_name(self.cand.source))
            .arg("host", host)
            .on_route(self.pol.route);
        self.cx.progress(row);
    }

    /// Step 1: one plain HTTP request. Gates, request, host bookkeeping; the permits are gone when this returns.
    async fn http_request(&mut self, url: &str, host: &str) -> Result<Got, FetchEnd> {
        let cx = self.cx;
        self.reserve().map_err(FetchEnd::Refused)?;
        let kind = if self.expect_pdf {
            FetchKind::Pdf
        } else {
            FetchKind::Page
        };
        let inst_permit = if self.is_inst() {
            Some(
                cx.acquire_inst(&self.inst_key(host), kind)
                    .await
                    .map_err(FetchEnd::Refused)?,
            )
        } else {
            None
        };
        let mut host_permit = if self.is_inst() {
            None
        } else {
            Some(
                cx.acquire_oa_host(&oa_host_key(url).unwrap_or_else(|| host.to_owned()))
                    .await
                    .map_err(FetchEnd::Refused)?,
            )
        };

        let class = if self.expect_pdf {
            FetchClass::Pdf
        } else {
            FetchClass::Landing
        };
        let timeout_ms = cx.timeout_for(class, host);
        let max_bytes = if self.expect_pdf {
            self.pdf_cap()
        } else {
            PAGE_MAX_BYTES
        };
        let req = requests::get(url, cx.ua_for(host), timeout_ms, max_bytes);
        let retain = if self.expect_pdf {
            Retain::Pdf
        } else {
            Retain::Page
        };
        let spec = self.inspect_spec();
        let (route, barrier) = (self.pol.route, self.ticket.is_some());
        if let Some(t) = self.ticket.as_mut() {
            t.mark_sent();
        }
        self.announce(host);
        let res = with_offline_retry(cx, u64::from(timeout_ms), || {
            cx.http(route, barrier, req.clone(), retain, spec.clone())
        })
        .await;
        let got = res.map(Got::from_http);
        if let Some(p) = host_permit.as_mut() {
            report_host(p, &got);
        }
        drop(inst_permit);
        drop(host_permit);
        got.map_err(net_end)
    }

    /// Step 3: the same URL in a hidden browser (plain UA, PDF capture).
    async fn browser_request(&mut self, url: &str, host: &str) -> Result<Got, FetchEnd> {
        let cx = self.cx;
        self.reserve().map_err(FetchEnd::Refused)?;
        let kind = if self.expect_pdf {
            FetchKind::Pdf
        } else {
            FetchKind::Page
        };
        // Gate order (sched/acquire.rs): institutional slot, then browser; OA host last (short, taken alone).
        let inst_permit = if self.is_inst() {
            Some(
                cx.acquire_inst(&self.inst_key(host), kind)
                    .await
                    .map_err(FetchEnd::Refused)?,
            )
        } else {
            None
        };
        let browser = cx.acquire_browser().await.map_err(FetchEnd::Refused)?;
        let mut host_permit = if self.is_inst() {
            None
        } else {
            Some(
                cx.acquire_oa_host(&oa_host_key(url).unwrap_or_else(|| host.to_owned()))
                    .await
                    .map_err(FetchEnd::Refused)?,
            )
        };

        cx.pin_plain_ua(host);
        let timeout_ms = cx.timeout_for(FetchClass::BrowserLoad, host);
        let hold_while = if self.is_inst() {
            vec![
                UrlClassKind::OpenathensRedirector,
                UrlClassKind::PublisherSsoHop,
            ]
        } else {
            Vec::new()
        };
        let req = BrowserReq {
            url: url.to_owned(),
            ua: Ua::Plain,
            capture_pdf: true,
            want_page: true,
            settle_ms: BROWSER_SETTLE_MS,
            timeout_ms,
            hold_while,
        };
        let spec = self.inspect_spec();
        let (route, barrier) = (self.pol.route, self.ticket.is_some());
        if let Some(t) = self.ticket.as_mut() {
            t.mark_sent();
        }
        self.announce(host);
        let res = with_offline_retry(cx, u64::from(timeout_ms), || {
            cx.browser_load(route, barrier, req.clone(), spec.clone())
        })
        .await;
        let got = res.map(Got::from_browser);
        if let Some(p) = host_permit.as_mut() {
            report_host(p, &got);
        }
        drop(host_permit);
        drop(browser);
        drop(inst_permit);
        got.map_err(net_end)
    }

    /// Step 4: the visible window for a human check (never solved by us).
    async fn window_request(&mut self, url: &str) -> Result<Got, FetchEnd> {
        let cx = self.cx;
        self.reserve().map_err(FetchEnd::Refused)?;
        self.cx
            .progress(l10n::row(RowState::Waiting, id::CHALLENGE));
        let req = LoginReq {
            kind: LoginKind::Challenge,
            realm: Realm::Openathens,
            entry_url: url.to_owned(),
            item_title: cx.snap.title.clone(),
            waiting: 1,
            timeout_ms: policy::timeout_ms(FetchClass::Challenge, false),
            capture_pdf: true,
        };
        if let Some(t) = self.ticket.as_mut() {
            t.mark_sent();
        }
        match cx.challenge_window(req).await {
            Ok(resp) => Ok(Got::from_login(resp)),
            // Declined or closed: the check stays unanswered.
            Err(_) => Err(FetchEnd::Challenge {
                host: pure::host_of(url).unwrap_or_default(),
                interactive: true,
            }),
        }
    }

    /// One network step on `tier` for `url`.
    async fn step(&mut self, tier: Tier, url: &str) -> Step {
        let Some(host) = pure::host_of(url) else {
            return Step::Done(FetchEnd::Failed(fail::BAD_URL.to_owned()));
        };
        self.last_tier = tier;
        let got = match tier {
            Tier::Http => self.http_request(url, &host).await,
            Tier::Browser => self.browser_request(url, &host).await,
        };
        match got {
            Ok(got) => self.judge(got, url, &host).await,
            Err(end) => Step::Done(end),
        }
    }

    /// Classify a response (the single place that turns an `Inspection` into a `Step`).
    async fn judge(&mut self, got: Got, url: &str, host: &str) -> Step {
        let Got {
            status,
            final_url,
            headers,
            bytes,
            inspect,
            pdf,
            page,
        } = got;
        match inspect {
            Inspection::Pdf(verdict) => self.judge_pdf(verdict, pdf, &final_url, &headers, bytes),
            Inspection::Page(p) => {
                self.judge_page(p, page, url, &final_url, host, status)
                    .await
            }
            Inspection::Failed { reason } => {
                self.cx
                    .trace_event(json!({ "ev": "inspect_failed", "url": url, "reason": reason }));
                Step::Done(FetchEnd::NotPdf)
            }
            Inspection::Api(_) | Inspection::Probe { .. } | Inspection::None => {
                Step::Done(FetchEnd::NotPdf)
            }
        }
    }

    fn judge_pdf(
        &mut self,
        verdict: PdfVerdict,
        body: Option<Body>,
        final_url: &str,
        headers: &BTreeMap<String, String>,
        bytes: u64,
    ) -> Step {
        if save::validate(&verdict).is_err() {
            return Step::Done(FetchEnd::NotPdf);
        }
        let Some(body) = body else {
            return Step::Done(FetchEnd::Failed(fail::NO_BODY.to_owned()));
        };
        let announced = headers
            .get("content-length")
            .and_then(|v| v.trim().parse::<u64>().ok());
        if announced.is_some_and(|expected| bytes < expected) {
            // Cut short: the bytes look like a PDF but the transfer did not finish.
            drop(body);
            return if self.can_retry() {
                self.retried = true;
                Step::Retry
            } else {
                Step::Done(FetchEnd::Network)
            };
        }
        Step::Done(FetchEnd::Pdf(PdfBlob {
            body,
            verdict,
            url: self.cand.url.clone(),
            final_url: final_url.to_owned(),
            landing_url: self.cand.landing_url.clone(),
            source: self.cand.source,
            version: self.cand.version,
            route: self.pol.route,
            ticket: self.ticket.take(),
        }))
    }

    async fn judge_page(
        &mut self,
        p: PageInspection,
        page: Option<Body>,
        url: &str,
        final_url: &str,
        host: &str,
        status: u16,
    ) -> Step {
        let cx = self.cx;
        let inst = self.is_inst();
        let host_key = oa_host_key(url).unwrap_or_else(|| host.to_owned());
        // Matched on a copy so that `p` stays whole for the arms that hand it on.
        match p.class.clone() {
            PageClass::Landing | PageClass::Html => {
                let landing = LandingPage {
                    url: url.to_owned(),
                    final_url: final_url.to_owned(),
                    page,
                    inspection: p,
                };
                Step::Done(FetchEnd::Landing(landing))
            }
            PageClass::Redirect { next } => Step::Follow(next),
            PageClass::ReaderShell { rewrite } => match rewrite {
                Some(next) if !self.rewrote => {
                    self.rewrote = true;
                    self.expect_pdf = true;
                    Step::Follow(next)
                }
                _ if p.candidates.is_empty() => Step::Done(FetchEnd::NotPdf),
                _ => Step::Done(FetchEnd::Landing(LandingPage {
                    url: url.to_owned(),
                    final_url: final_url.to_owned(),
                    page,
                    inspection: p,
                })),
            },
            PageClass::Challenge { interactive, .. } => Step::Challenge { interactive },
            PageClass::Blocked { .. } => {
                if !inst {
                    cx.wall_host(&host_key, "blocked");
                }
                Step::Done(FetchEnd::Blocked { host: host_key })
            }
            PageClass::RateLimited { retry_after_s } => {
                if inst {
                    cx.halt_inst(HaltReason::RateLimited, retry_after_s);
                    return Step::Done(FetchEnd::RateLimited { retry_after_s });
                }
                if self.can_retry() && retry_after_s.is_none_or(|s| s <= RETRY_AFTER_CAP_S) {
                    self.retried = true;
                    let wait_s = retry_after_s.unwrap_or(2).min(RETRY_AFTER_CAP_S);
                    cx.sleep(u64::from(wait_s) * 1_000).await;
                    return Step::Retry;
                }
                Step::Done(FetchEnd::RateLimited { retry_after_s })
            }
            PageClass::NotFound => Step::Done(FetchEnd::NotFound),
            PageClass::AccessDenied => Step::Done(FetchEnd::AccessDenied),
            PageClass::LicenceStop => Step::Done(if inst {
                FetchEnd::LicenceStop
            } else {
                FetchEnd::AccessDenied
            }),
            PageClass::EzproxyNeedLogin
            | PageClass::IdpLogin { .. }
            | PageClass::SamlAutoPost
            | PageClass::PublisherSsoHop => {
                if inst {
                    let realm = if pure::is_ezproxy_url(&cx.cfg.institution, final_url)
                        || pure::is_ezproxy_url(&cx.cfg.institution, url)
                    {
                        Realm::Ezproxy
                    } else {
                        Realm::Openathens
                    };
                    Step::Done(FetchEnd::NeedsLogin {
                        realm,
                        final_url: final_url.to_owned(),
                    })
                } else {
                    // An open-access host that wants a sign-in is a host without free access.
                    Step::Done(FetchEnd::AccessDenied)
                }
            }
            PageClass::OpenathensWait => {
                Step::Done(FetchEnd::Failed(fail::OPENATHENS_WAIT.to_owned()))
            }
            PageClass::OpenathensUnknownResource => Step::Done(FetchEnd::Failed(
                fail::OPENATHENS_UNKNOWN_RESOURCE.to_owned(),
            )),
            PageClass::OpenathensBadOrg => {
                Step::Done(FetchEnd::Failed(fail::OPENATHENS_BAD_ORG.to_owned()))
            }
            PageClass::EzproxyHostNotConfigured => {
                Step::Done(FetchEnd::Failed(fail::EZPROXY_NOT_CONFIGURED.to_owned()))
            }
            PageClass::EzproxyError { suspend } => Step::Done(FetchEnd::Failed(
                if suspend {
                    fail::EZPROXY_SUSPENDED
                } else {
                    fail::EZPROXY_ERROR
                }
                .to_owned(),
            )),
            // A page whose class says "PDF" but whose bytes core did not accept as one.
            PageClass::Pdf { .. } => {
                cx.trace_event(
                    json!({ "ev": "pdf_class_without_pdf", "url": url, "status": status }),
                );
                Step::Done(FetchEnd::NotPdf)
            }
        }
    }

    /// A challenge that no allowed tier cleared: count it against the host.
    fn challenged(&self, host: &str, interactive: bool) -> FetchEnd {
        let walled = self.cx.note_challenge(host);
        self.cx
            .trace_event(json!({ "ev": "challenge", "host": host, "walled": walled }));
        FetchEnd::Challenge {
            host: host.to_owned(),
            interactive,
        }
    }

    /// What to do about a bot check seen on `tier`.
    async fn on_challenge(
        &mut self,
        tier: Tier,
        url: &str,
        host: &str,
        interactive: bool,
    ) -> Result<Tier, FetchEnd> {
        let cx = self.cx;
        // cf_clearance is bound to the UA: from the first challenge on, this host only sees the plain one.
        cx.pin_plain_ua(host);
        match tier {
            Tier::Http if self.pol.allow_browser => Ok(Tier::Browser),
            Tier::Http => Err(self.challenged(host, interactive)),
            Tier::Browser => {
                let may_ask = interactive
                    && self.pol.allow_challenge_window
                    && cx.interactive
                    && cx.cfg.inst.challenge_windows;
                if !may_ask {
                    return Err(self.challenged(host, interactive));
                }
                let got = self.window_request(url).await?;
                match self.judge(got, url, host).await {
                    Step::Done(end) => Err(end),
                    Step::Challenge { interactive } => Err(self.challenged(host, interactive)),
                    Step::Follow(_) | Step::Retry => Err(FetchEnd::NotPdf),
                }
            }
        }
    }

    /// The tier ladder for the candidate.
    async fn run(&mut self) -> FetchEnd {
        let mut url = self.cand.url.clone();
        let mut tier = if self.cand.needs_browser && self.pol.allow_browser {
            Tier::Browser
        } else {
            Tier::Http
        };
        let mut follows = 0u32;
        self.visited.insert(pure::canonical_key(&url));
        loop {
            match self.step(tier, &url).await {
                Step::Done(end) => return end,
                Step::Retry => {}
                Step::Follow(next) => {
                    follows += 1;
                    if follows > MAX_FOLLOWS || !self.visited.insert(pure::canonical_key(&next)) {
                        return FetchEnd::NotPdf;
                    }
                    self.trail.push(next.clone());
                    url = next;
                }
                Step::Challenge { interactive } => {
                    let host = pure::host_of(&url).unwrap_or_default();
                    match self.on_challenge(tier, &url, &host, interactive).await {
                        Ok(next_tier) => tier = next_tier,
                        Err(end) => return end,
                    }
                }
            }
        }
    }
}

/// Run one fetch to its end: `(end, URLs requested after the first)`. Resolves the licence ticket (a non-PDF end
/// gives the reservation back) and records the trace step. Does not touch the pool, so several can run at once.
async fn fetch_inner(
    cx: &ItemCx,
    cand: &Candidate,
    pol: &FetchPolicy,
    requested: &Ids,
) -> (FetchEnd, Vec<String>) {
    let started = cx.now();
    let mut fetch = Fetch::new(cx, cand, pol, requested);
    let end = fetch.run().await;
    if !matches!(end, FetchEnd::Pdf(_))
        && let Some(ticket) = fetch.ticket.take()
    {
        ticket.rollback();
    }
    let ms = u32::try_from(cx.now().saturating_sub(started)).unwrap_or(u32::MAX);
    cx.trace_step(
        &source_name(cand.source),
        fetch.last_tier.name(),
        end.name(),
        ms,
    );
    (end, fetch.trail)
}

/// Fetch one candidate through the tier ladder (see the module docs). Marks the URL (and every URL it followed)
/// as tried in `pool`.
pub(crate) async fn fetch_candidate(
    cx: &ItemCx,
    cand: &Candidate,
    pol: &FetchPolicy,
    pool: &mut CandidatePool,
) -> FetchEnd {
    pool.mark_tried(&cand.url);
    let requested = pool.requested_for(cand);
    let (end, trail) = fetch_inner(cx, cand, pol, &requested).await;
    for url in &trail {
        pool.mark_tried(url);
    }
    end
}

// ------------------------------------------------------------------------------------------- the OA phase

/// How the OA attempt phase ended.
pub(crate) enum OaEnd {
    Pdf(PdfBlob),
    /// Nothing worked. Flags feed the outcome mapping (`NotFound` reason, inst-phase eligibility).
    Exhausted {
        saw_access_denied: bool,
        #[allow(dead_code)] // kept so the struct mirrors its source; nothing reads it yet
        saw_challenge: bool,
        saw_timeout: bool,
        #[allow(dead_code)] // kept so the struct mirrors its source; nothing reads it yet
        tried: u32,
    },
    /// The network was down for the whole pause and nothing else answered.
    Offline,
    /// The engine is going away.
    ShuttingDown,
}

/// An OA fetch in flight.
struct Flight {
    cand: Candidate,
    end: FetchEnd,
    trail: Vec<String>,
}

/// Bookkeeping of a running flight.
struct Live {
    url: String,
    host_key: String,
    started: u64,
    delay: u64,
    /// When a second attempt may start (`u64::MAX`: nothing left to hedge to).
    hedge_at: u64,
}

/// The best untried candidate this phase may fetch on a host that is not in `avoid`.
fn next_candidate(cx: &ItemCx, pool: &CandidatePool, avoid: &[String]) -> Option<Candidate> {
    pool.ranked(&cx.cfg.oa).into_iter().find(|c| {
        // Crossref links and Alma entries name where an institution's copy lives: the institutional phase's job.
        !matches!(c.source, Source::Doi | Source::Alma)
            && oa_host_key(&c.url)
                .is_some_and(|key| !cx.host_skipped(&key) && !avoid.contains(&key))
    })
}

/// Try the ranked OA candidates (sequential with OA hedging, see module docs). Landing pages are extracted
/// through `landing::extract_landing` and their candidates appended to the pool.
pub(crate) async fn attempt_oa(cx: &ItemCx, pool: &mut CandidatePool) -> OaEnd {
    let pol = FetchPolicy::oa();
    let hedge = cx.cfg.run.hedge;
    let mut flights: FuturesUnordered<LocalBoxFuture<'_, Flight>> = FuturesUnordered::new();
    let mut live: Vec<Live> = Vec::new();
    let (mut started, mut saw_denied, mut saw_challenge, mut saw_timeout, mut saw_offline) =
        (0u32, false, false, false, false);

    loop {
        // 1. Start work: the first attempt, or the hedge once the first one is slow.
        let wanted = if live.is_empty() {
            Some(Vec::new())
        } else if hedge && live.len() == 1 && live.first().is_some_and(|l| cx.now() >= l.hedge_at) {
            Some(live.iter().map(|l| l.host_key.clone()).collect())
        } else {
            None
        };
        if let Some(avoid) = wanted
            && started < OA_ATTEMPT_CAP
        {
            match next_candidate(cx, pool, &avoid) {
                Some(cand) => {
                    started += 1;
                    pool.mark_tried(&cand.url);
                    let requested = pool.requested_for(&cand);
                    let host_key = oa_host_key(&cand.url).unwrap_or_default();
                    let delay = if is_slow_host(&host_key) {
                        policy::HEDGE_AFTER_SLOW_MS
                    } else {
                        policy::HEDGE_AFTER_MS
                    };
                    let now = cx.now();
                    live.push(Live {
                        url: cand.url.clone(),
                        host_key,
                        started: now,
                        delay,
                        hedge_at: now.saturating_add(delay),
                    });
                    let pol = &pol;
                    flights.push(Box::pin(async move {
                        let (end, trail) = fetch_inner(cx, &cand, pol, &requested).await;
                        Flight { cand, end, trail }
                    }));
                }
                None => {
                    // Nothing to hedge to until the pool grows.
                    if let Some(l) = live.first_mut() {
                        l.hedge_at = u64::MAX;
                    }
                }
            }
        }
        if live.is_empty() {
            break;
        }

        // 2. Wait for an answer, or for the moment to hedge.
        let hedge_at = live
            .first()
            .map(|l| l.hedge_at)
            .filter(|at| hedge && live.len() == 1 && started < OA_ATTEMPT_CAP && *at != u64::MAX);
        let mut hedge_sleep = hedge_at.map(|at| cx.sleep_until(at));
        let event = poll_fn(|task| {
            if let Poll::Ready(done) = flights.poll_next_unpin(task) {
                return Poll::Ready(Some(done));
            }
            if let Some(s) = hedge_sleep.as_mut()
                && Pin::new(s).poll(task).is_ready()
            {
                return Poll::Ready(None);
            }
            Poll::Pending
        })
        .await;
        let Some(done) = event else { continue };
        let Some(flight) = done else { continue };

        // 3. Take the answer.
        live.retain(|l| l.url != flight.cand.url);
        for url in &flight.trail {
            pool.mark_tried(url);
        }
        let Flight { cand, end, .. } = flight;
        match end {
            FetchEnd::Pdf(blob) => return OaEnd::Pdf(blob),
            FetchEnd::Landing(page) => {
                let url = if page.final_url.is_empty() {
                    page.url.as_str()
                } else {
                    page.final_url.as_str()
                };
                let res = landing::extract_landing(
                    cx,
                    page.page.as_ref(),
                    url,
                    &page.inspection,
                    pool,
                    Route::Oa,
                )
                .await;
                let fresh = res.candidates.iter().any(|c| !pool.was_tried(&c.url));
                // A PDF URL that only gave a page, with nothing new on it, is a publisher saying "no".
                if !fresh && (cand.kind == CandKind::Pdf || res.access == AccessHint::NoAccess) {
                    saw_denied = true;
                }
            }
            FetchEnd::AccessDenied | FetchEnd::Blocked { .. } => saw_denied = true,
            FetchEnd::Challenge { .. } => saw_challenge = true,
            FetchEnd::Timeout => saw_timeout = true,
            FetchEnd::Refused(Refused::ShuttingDown) => return OaEnd::ShuttingDown,
            FetchEnd::Failed(code) if code == fail::OFFLINE => saw_offline = true,
            FetchEnd::NotPdf
            | FetchEnd::NotFound
            | FetchEnd::RateLimited { .. }
            | FetchEnd::NeedsLogin { .. }
            | FetchEnd::LicenceStop
            | FetchEnd::Network
            | FetchEnd::TooLarge
            | FetchEnd::Refused(_)
            | FetchEnd::Failed(_) => {}
        }
        // The pool may have grown and the survivors' clocks restart from their own start.
        for l in &mut live {
            l.hedge_at = l.started.saturating_add(l.delay);
        }
    }

    if saw_offline {
        return OaEnd::Offline;
    }
    OaEnd::Exhausted {
        saw_access_denied: saw_denied,
        saw_challenge,
        saw_timeout,
        tried: started,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::testkit::{Rig, script};
    use crate::sim::Reply;
    use crate::sim::fixtures::{
        self, body, browser_ok, candidate, http_ok, is_type, page_inspection, pdf_inspection,
        url_has,
    };

    const PDF_A: &str = "https://pub-a.example.org/a.pdf";
    const PDF_B: &str = "https://pub-b.example.org/b.pdf";
    const PMC: &str = "https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf";

    fn snap() -> ItemSnapshot {
        fixtures::item(1, "10.1234/x")
    }

    fn ids() -> Ids {
        Ids {
            doi: Some("10.1234/x".into()),
            ..Ids::default()
        }
    }

    fn pdf_cand(url: &str) -> Candidate {
        candidate(url, CandKind::Pdf, Source::Openalex, 0)
    }

    fn landing_cand(url: &str) -> Candidate {
        candidate(url, CandKind::Landing, Source::Openalex, 0)
    }

    fn pdf_reply(after: u64, url: &str, handle: u32) -> Reply {
        Reply::ok(
            after,
            http_ok(
                200,
                url,
                &pdf_inspection(50_000),
                Some(body(handle, 50_000)),
            ),
        )
    }

    fn page_reply(after: u64, url: &str, class: PageClass) -> Reply {
        Reply::ok(
            after,
            http_ok(200, url, &page_inspection(class, vec![]), None),
        )
    }

    fn challenge() -> PageClass {
        PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive: false,
        }
    }

    /// Fetch `cand` once on a fresh pool and describe the end.
    fn fetch(
        rig: &mut Rig,
        cand: Candidate,
        pol: FetchPolicy,
    ) -> Option<(FetchEnd, CandidatePool)> {
        rig.exec(snap(), move |cx| async move {
            let mut pool = CandidatePool::new(ids());
            let end = fetch_candidate(&cx, &cand, &pol, &mut pool).await;
            (end, pool)
        })
    }

    fn describe(end: &FetchEnd) -> String {
        match end {
            FetchEnd::Failed(code) => format!("failed:{code}"),
            other => other.name().to_owned(),
        }
    }

    fn count_http(rig: &Rig) -> usize {
        rig.count(|e| matches!(e, Effect::Http { .. }))
    }

    // ------------------------------------------------------------------------------------ one candidate

    #[test]
    fn a_pdf_candidate_is_fetched_with_the_pdf_limits_and_wins() {
        let mut rig = Rig::new(script().on(url_has("a.pdf"), pdf_reply(80, PDF_A, 7)));
        let (end, pool) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        assert!(
            matches!(&end, FetchEnd::Pdf(b) if b.url == PDF_A && b.route == Route::Oa && b.ticket.is_none()),
            "{}",
            describe(&end)
        );
        assert!(pool.was_tried(PDF_A));
        let sent = rig.effects().into_iter().find_map(|e| match e {
            Effect::Http {
                req,
                retain,
                inspect,
                route,
                barrier,
                ..
            } => Some((req, retain, inspect, route, barrier)),
            _ => None,
        });
        let (req, retain, inspect, route, barrier) = sent.expect("one http effect");
        assert_eq!(
            (
                req.max_bytes,
                req.timeout_ms,
                req.ua,
                retain,
                route,
                barrier
            ),
            (
                100 * 1024 * 1024,
                90_000,
                Ua::Default,
                Retain::Pdf,
                Route::Oa,
                false
            )
        );
        assert!(matches!(
            inspect,
            InspectSpec::Pdf {
                route: Route::Oa,
                ..
            }
        ));
        assert!(rig.rows().contains(&id::DOWNLOADING.to_owned()));
        // The body belongs to the caller now; it is released when the blob (and with it the Body) is dropped.
        assert_eq!(rig.count(|e| matches!(e, Effect::Release { handle: 7 })), 0);
        drop(end);
        rig.flush();
        assert_eq!(rig.count(|e| matches!(e, Effect::Release { handle: 7 })), 1);
        assert!(rig.host.pending.is_empty());
    }

    #[test]
    fn landing_pages_use_the_page_limits() {
        let mut rig = Rig::new(script().on(
            url_has("article"),
            Reply::ok(
                50,
                http_ok(
                    200,
                    "https://pub-a.example.org/article",
                    &page_inspection(PageClass::Landing, vec![]),
                    Some(body(3, 900)),
                ),
            ),
        ));
        let (end, _) = fetch(
            &mut rig,
            landing_cand("https://pub-a.example.org/article"),
            FetchPolicy::oa(),
        )
        .expect("done");
        let FetchEnd::Landing(page) = end else {
            panic!("expected a landing page")
        };
        assert_eq!(
            (page.url.as_str(), page.page.as_ref().map(Body::len)),
            ("https://pub-a.example.org/article", Some(900))
        );
        let (req, retain, inspect) = rig
            .effects()
            .into_iter()
            .find_map(|e| match e {
                Effect::Http {
                    req,
                    retain,
                    inspect,
                    ..
                } => Some((req, retain, inspect)),
                _ => None,
            })
            .expect("http");
        assert_eq!(
            (req.max_bytes, req.timeout_ms, retain),
            (4 * 1024 * 1024, 20_000, Retain::Page)
        );
        assert!(matches!(inspect, InspectSpec::Page { .. }));
    }

    #[test]
    fn slow_hosts_get_the_long_timeouts() {
        let mut rig = Rig::new(script().on(url_has("wiley"), Reply::Never));
        let start = rig.host.now;
        let (end, _) = fetch(
            &mut rig,
            landing_cand("https://onlinelibrary.wiley.com/doi/10.1002/x"),
            FetchPolicy::oa(),
        )
        .expect("done");
        assert!(matches!(end, FetchEnd::Timeout));
        assert_eq!(
            rig.host.now,
            start + 60_000,
            "landing profile of a slow host"
        );
        let req = rig.effects().into_iter().find_map(|e| match e {
            Effect::Http { req, .. } => Some(req),
            _ => None,
        });
        assert_eq!(req.map(|r| r.timeout_ms), Some(60_000));
        assert_eq!(
            rig.count(|e| matches!(e, Effect::Cancel { .. })),
            1,
            "the engine gives up and cancels the request"
        );
        let mut rig = Rig::new(script().on(url_has("wiley"), Reply::Never));
        let start = rig.host.now;
        let (end, _) = fetch(
            &mut rig,
            pdf_cand("https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/x.pdf"),
            FetchPolicy::oa(),
        )
        .expect("done");
        assert!(matches!(end, FetchEnd::Timeout));
        assert_eq!(rig.host.now, start + 120_000, "PDF profile of a slow host");
    }

    #[test]
    fn op_errors_map_to_their_ends() {
        let cases = [
            (OpErrKind::Timeout, "timeout"),
            (OpErrKind::Network, "network"),
            (OpErrKind::Tls, "network"),
            (OpErrKind::TooLarge, "too_large"),
            (OpErrKind::Cancelled, "failed:cancelled"),
            (OpErrKind::Io, "failed:io"),
        ];
        for (kind, want) in cases {
            let mut rig = Rig::new(script().on(url_has("a.pdf"), Reply::err(10, kind)));
            let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
            assert_eq!(describe(&end), want, "{kind:?}");
        }
    }

    // ------------------------------------------------------------------------------ classification

    #[test]
    fn page_classes_map_to_ends() {
        let cases: Vec<(PageClass, &str)> = vec![
            (PageClass::NotFound, "not_found"),
            (PageClass::AccessDenied, "access_denied"),
            (PageClass::LicenceStop, "access_denied"),
            (PageClass::EzproxyNeedLogin, "access_denied"),
            (
                PageClass::IdpLogin {
                    idp: Idp::Entra,
                    ca_error: None,
                },
                "access_denied",
            ),
            (PageClass::SamlAutoPost, "access_denied"),
            (PageClass::PublisherSsoHop, "access_denied"),
            (PageClass::OpenathensWait, "failed:openathens_wait"),
            (
                PageClass::OpenathensUnknownResource,
                "failed:openathens_unknown_resource",
            ),
            (PageClass::OpenathensBadOrg, "failed:openathens_bad_org"),
            (
                PageClass::EzproxyHostNotConfigured,
                "failed:ezproxy_host_not_configured",
            ),
            (
                PageClass::EzproxyError { suspend: true },
                "failed:ezproxy_suspended",
            ),
            (
                PageClass::EzproxyError { suspend: false },
                "failed:ezproxy_error",
            ),
            (PageClass::Pdf { junk_prefix: 0 }, "not_pdf"),
            (PageClass::ReaderShell { rewrite: None }, "not_pdf"),
        ];
        for (class, want) in cases {
            let mut rig =
                Rig::new(script().on(url_has("a.pdf"), page_reply(10, PDF_A, class.clone())));
            let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
            assert_eq!(describe(&end), want, "{class:?}");
        }
    }

    #[test]
    fn institutional_login_pages_say_which_realm() {
        let pol = FetchPolicy::inst(InstKey("elsevier".into()));
        let proxied = "https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71.full.pdf";
        let mut rig = Rig::new(script().on(
            url_has("bmj"),
            page_reply(
                10,
                "https://login.kcl.idm.oclc.org/login?qurl=x",
                PageClass::EzproxyNeedLogin,
            ),
        ));
        let (end, _) = fetch(&mut rig, pdf_cand(proxied), pol.clone()).expect("done");
        assert!(
            matches!(&end, FetchEnd::NeedsLogin { realm: Realm::Ezproxy, final_url } if final_url.contains("login.kcl.idm")),
            "{}",
            describe(&end)
        );

        let mut rig = Rig::new(script().on(
            url_has("sciencedirect"),
            page_reply(
                10,
                "https://login.microsoftonline.com/t/saml2",
                PageClass::IdpLogin {
                    idp: Idp::Entra,
                    ca_error: None,
                },
            ),
        ));
        let (end, _) = fetch(
            &mut rig,
            landing_cand("https://www.sciencedirect.com/science/article/pii/S1"),
            pol,
        )
        .expect("done");
        assert!(matches!(
            end,
            FetchEnd::NeedsLogin {
                realm: Realm::Openathens,
                ..
            }
        ));
    }

    #[test]
    fn non_pdf_and_unusable_bytes_are_rejected() {
        let not_pdf = Inspection::Pdf(PdfVerdict {
            is_pdf: false,
            junk_prefix: 0,
            has_eof: false,
            encrypted: false,
            looks_html: false,
            len: 5_000,
        });
        let html_pdf = Inspection::Pdf(PdfVerdict {
            is_pdf: true,
            junk_prefix: 0,
            has_eof: true,
            encrypted: false,
            looks_html: true,
            len: 5_000,
        });
        let tiny = Inspection::Pdf(PdfVerdict {
            is_pdf: true,
            junk_prefix: 0,
            has_eof: true,
            encrypted: false,
            looks_html: false,
            len: 12,
        });
        for (name, inspection) in [
            ("not a pdf", not_pdf),
            ("html polyglot", html_pdf),
            ("tiny", tiny),
            (
                "failed",
                Inspection::Failed {
                    reason: "body_too_large".into(),
                },
            ),
            ("none", Inspection::None),
        ] {
            let mut rig = Rig::new(script().on(
                url_has("a.pdf"),
                Reply::ok(10, http_ok(200, PDF_A, &inspection, Some(body(4, 5_000)))),
            ));
            let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
            assert_eq!(describe(&end), "not_pdf", "{name}");
            assert_eq!(count_http(&rig), 1, "{name}: never loops");
            assert_eq!(
                rig.count(|e| matches!(e, Effect::Release { handle: 4 })),
                1,
                "{name}: the body is released"
            );
        }
    }

    #[test]
    fn a_missing_body_is_a_failure_not_a_pdf() {
        let mut rig = Rig::new(script().on(
            url_has("a.pdf"),
            Reply::ok(10, http_ok(200, PDF_A, &pdf_inspection(50_000), None)),
        ));
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        assert_eq!(describe(&end), "failed:no_body");
    }

    #[test]
    fn a_truncated_download_is_retried_once_and_then_refused() {
        let mut headers = BTreeMap::new();
        headers.insert("content-length".to_owned(), "50000".to_owned());
        let truncated = |handle: u32| {
            let mut ok = http_ok(
                200,
                PDF_A,
                &pdf_inspection(20_000),
                Some(body(handle, 20_000)),
            );
            ok["headers"] = serde_json::to_value(&headers).expect("headers");
            ok["bytes"] = json!(20_000);
            Reply::ok(10, ok)
        };
        let mut complete = http_ok(200, PDF_A, &pdf_inspection(50_000), Some(body(9, 50_000)));
        complete["headers"] = serde_json::to_value(&headers).expect("headers");
        complete["bytes"] = json!(50_000);

        // cut short twice: refused after the single retry
        let mut rig = Rig::new(script().on_seq(url_has("a.pdf"), vec![truncated(5), truncated(6)]));
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        assert_eq!(describe(&end), "network");
        assert_eq!(count_http(&rig), 2);
        assert_eq!(
            rig.count(|e| matches!(e, Effect::Release { handle: 5 | 6 })),
            2,
            "both partial bodies are released"
        );

        // cut short once, whole the second time
        let mut rig = Rig::new(script().on_seq(
            url_has("a.pdf"),
            vec![truncated(5), Reply::ok(10, complete)],
        ));
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        assert!(matches!(end, FetchEnd::Pdf(b) if b.verdict.len == 50_000));
        assert_eq!(count_http(&rig), 2);

        // the institutional route never retries
        let mut rig = Rig::new(script().on_seq(url_has("a.pdf"), vec![truncated(5), truncated(6)]));
        let (end, _) = fetch(
            &mut rig,
            pdf_cand(PDF_A),
            FetchPolicy::inst(InstKey("pub-a".into())),
        )
        .expect("done");
        assert_eq!(describe(&end), "network");
        assert_eq!(count_http(&rig), 1);
    }

    // ------------------------------------------------------------------------------- landing pages

    #[test]
    fn a_pdf_url_that_serves_a_landing_page_comes_back_as_a_landing() {
        let landing = page_inspection(
            PageClass::Landing,
            vec![pdf_cand("https://pub-a.example.org/real.pdf")],
        );
        let mut rig = Rig::new(script().on(
            url_has("a.pdf"),
            Reply::ok(
                10,
                http_ok(
                    200,
                    "https://pub-a.example.org/article",
                    &landing,
                    Some(body(8, 400)),
                ),
            ),
        ));
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        let FetchEnd::Landing(page) = end else {
            panic!("expected a landing page")
        };
        assert_eq!(page.inspection.candidates.len(), 1);
        assert_eq!(page.final_url, "https://pub-a.example.org/article");
    }

    #[test]
    fn redirects_are_followed_a_few_times_and_never_in_circles() {
        // 1 -> 2 -> 3 -> 4 is three follow-ups: allowed
        let redirect = |from: &str, to: &str| {
            page_reply(
                5,
                from,
                PageClass::Redirect {
                    next: to.to_owned(),
                },
            )
        };
        let s = script()
            .on(
                url_has("org/1"),
                redirect("https://pub-a.example.org/1", "https://pub-a.example.org/2"),
            )
            .on(
                url_has("org/2"),
                redirect("https://pub-a.example.org/2", "https://pub-a.example.org/3"),
            )
            .on(
                url_has("org/3"),
                redirect("https://pub-a.example.org/3", "https://pub-a.example.org/4"),
            )
            .on(
                url_has("org/4"),
                pdf_reply(5, "https://pub-a.example.org/4", 2),
            );
        let mut rig = Rig::new(s);
        let (end, pool) = fetch(
            &mut rig,
            landing_cand("https://pub-a.example.org/1"),
            FetchPolicy::oa(),
        )
        .expect("done");
        assert!(matches!(end, FetchEnd::Pdf(_)), "{}", describe(&end));
        assert!(
            pool.was_tried("https://pub-a.example.org/4"),
            "followed URLs are marked tried"
        );

        // a loop: 1 -> 2 -> 1
        let s = script()
            .on(
                url_has("org/1"),
                redirect("https://pub-a.example.org/1", "https://pub-a.example.org/2"),
            )
            .on(
                url_has("org/2"),
                redirect("https://pub-a.example.org/2", "https://pub-a.example.org/1"),
            );
        let mut rig = Rig::new(s);
        let (end, _) = fetch(
            &mut rig,
            landing_cand("https://pub-a.example.org/1"),
            FetchPolicy::oa(),
        )
        .expect("done");
        assert_eq!(describe(&end), "not_pdf");
        assert_eq!(count_http(&rig), 2, "the visited set stops the loop");
    }

    #[test]
    fn a_reader_shell_is_rewritten_once() {
        let shell = |rewrite: &str| {
            page_reply(
                5,
                "https://www.tandfonline.com/doi/epdf/10.1/x",
                PageClass::ReaderShell {
                    rewrite: Some(rewrite.to_owned()),
                },
            )
        };
        let s = script()
            .on(
                url_has("/doi/epdf/"),
                shell("https://www.tandfonline.com/doi/pdf/10.1/x"),
            )
            .on(
                url_has("/doi/pdf/"),
                pdf_reply(5, "https://www.tandfonline.com/doi/pdf/10.1/x", 3),
            );
        let mut rig = Rig::new(s);
        let (end, _) = fetch(
            &mut rig,
            landing_cand("https://www.tandfonline.com/doi/epdf/10.1/x"),
            FetchPolicy::oa(),
        )
        .expect("done");
        assert!(matches!(end, FetchEnd::Pdf(_)));
        let specs: Vec<bool> = rig
            .effects()
            .into_iter()
            .filter_map(|e| match e {
                Effect::Http { inspect, .. } => Some(matches!(inspect, InspectSpec::Pdf { .. })),
                _ => None,
            })
            .collect();
        assert_eq!(
            specs,
            vec![false, true],
            "the rewritten URL is expected to be a PDF"
        );

        // the rewrite is itself a shell: no second rewrite
        let s = script()
            .on(
                url_has("/doi/epdf/"),
                shell("https://www.tandfonline.com/doi/reader/10.1/x"),
            )
            .on(
                url_has("/doi/reader/"),
                page_reply(
                    5,
                    "https://www.tandfonline.com/doi/reader/10.1/x",
                    PageClass::ReaderShell {
                        rewrite: Some("https://www.tandfonline.com/doi/pdf/10.1/y".into()),
                    },
                ),
            );
        let mut rig = Rig::new(s);
        let (end, _) = fetch(
            &mut rig,
            landing_cand("https://www.tandfonline.com/doi/epdf/10.1/x"),
            FetchPolicy::oa(),
        )
        .expect("done");
        assert_eq!(describe(&end), "not_pdf");
        assert_eq!(count_http(&rig), 2);
    }

    // ----------------------------------------------------------------------------------- challenges

    #[test]
    fn a_challenge_goes_to_the_browser_tier_with_the_plain_ua_pinned() {
        let browser = browser_ok(PDF_A, &pdf_inspection(50_000), Some(body(11, 50_000)), None);
        let s = script()
            .on(is_type("http"), page_reply(40, PDF_A, challenge()))
            .on(is_type("browser_load"), Reply::ok(900, browser));
        let mut rig = Rig::new(s);
        let (end, pool, pinned) = rig
            .exec(snap(), |cx| async move {
                let mut pool = CandidatePool::new(ids());
                let cand = pdf_cand(PDF_A);
                let end = fetch_candidate(&cx, &cand, &FetchPolicy::oa(), &mut pool).await;
                let pinned = cx.ua_for("pub-a.example.org");
                (end, pool, pinned)
            })
            .expect("done");
        assert_eq!(pinned, Ua::Plain, "the host is pinned to the plain UA");
        assert!(matches!(end, FetchEnd::Pdf(b) if b.url == PDF_A));
        assert!(pool.was_tried(PDF_A));
        let effects = rig.effects();
        let kinds: Vec<&str> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Http { .. } => Some("http"),
                Effect::BrowserLoad { .. } => Some("browser"),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec!["http", "browser"]);
        let load = effects.iter().find_map(|e| match e {
            Effect::BrowserLoad {
                req,
                route,
                barrier,
                ..
            } => Some((req.clone(), *route, *barrier)),
            _ => None,
        });
        let (req, route, barrier) = load.expect("browser load");
        assert_eq!(
            (req.ua, req.capture_pdf, req.want_page, route, barrier),
            (Ua::Plain, true, true, Route::Oa, false)
        );
        assert_eq!(req.timeout_ms, 60_000);
    }

    #[test]
    fn once_a_host_challenged_later_requests_use_the_plain_ua() {
        let s = script()
            .on(
                is_type("browser_load"),
                Reply::ok(
                    100,
                    browser_ok(PDF_A, &pdf_inspection(50_000), Some(body(11, 50_000)), None),
                ),
            )
            .on(
                url_has("first"),
                page_reply(5, "https://pub-a.example.org/first.pdf", challenge()),
            )
            .on(
                url_has("second"),
                pdf_reply(5, "https://pub-a.example.org/second.pdf", 12),
            );
        let mut rig = Rig::new(s);
        let ends = rig
            .exec(snap(), |cx| async move {
                let mut pool = CandidatePool::new(ids());
                let a = fetch_candidate(
                    &cx,
                    &pdf_cand("https://pub-a.example.org/first.pdf"),
                    &FetchPolicy::oa(),
                    &mut pool,
                )
                .await;
                let b = fetch_candidate(
                    &cx,
                    &pdf_cand("https://pub-a.example.org/second.pdf"),
                    &FetchPolicy::oa(),
                    &mut pool,
                )
                .await;
                (matches!(a, FetchEnd::Pdf(_)), matches!(b, FetchEnd::Pdf(_)))
            })
            .expect("done");
        assert_eq!(ends, (true, true));
        let uas: Vec<Ua> = rig.http().into_iter().map(|(_, ua, _)| ua).collect();
        assert_eq!(
            uas,
            vec![Ua::Default, Ua::Plain],
            "the second HTTP request is already plain"
        );
    }

    #[test]
    fn a_challenge_the_browser_cannot_clear_is_reported_and_two_wall_the_host() {
        let s = script()
            .on(
                is_type("http"),
                page_reply(5, "https://pub-a.example.org/x", challenge()),
            )
            .on(
                is_type("browser_load"),
                Reply::ok(
                    100,
                    browser_ok(
                        "https://pub-a.example.org/x",
                        &page_inspection(challenge(), vec![]),
                        None,
                        None,
                    ),
                ),
            );
        let mut rig = Rig::new(s);
        let (first, second, walled_after_two, third) = rig
            .exec(snap(), |cx| async move {
                let mut pool = CandidatePool::new(ids());
                let pol = FetchPolicy::oa();
                let a = fetch_candidate(
                    &cx,
                    &pdf_cand("https://pub-a.example.org/1.pdf"),
                    &pol,
                    &mut pool,
                )
                .await;
                let walled_after_one = cx.host_skipped("pub-a.example.org");
                let b = fetch_candidate(
                    &cx,
                    &pdf_cand("https://pub-a.example.org/2.pdf"),
                    &pol,
                    &mut pool,
                )
                .await;
                let walled = cx.host_skipped("pub-a.example.org");
                let c = fetch_candidate(
                    &cx,
                    &pdf_cand("https://pub-a.example.org/3.pdf"),
                    &pol,
                    &mut pool,
                )
                .await;
                assert!(!walled_after_one);
                (describe(&a), describe(&b), walled, describe(&c))
            })
            .expect("done");
        assert_eq!(
            (first.as_str(), second.as_str()),
            ("challenge", "challenge")
        );
        assert!(walled_after_two, "two challenge outcomes wall the host");
        assert_eq!(third, "refused", "a walled host is not contacted again");
        assert_eq!(rig.count(|e| matches!(e, Effect::BrowserLoad { .. })), 2);
    }

    #[test]
    fn with_the_browser_off_a_challenge_ends_the_fetch() {
        let mut rig = Rig::new(script().on(is_type("http"), page_reply(5, PDF_A, challenge())));
        let pol = FetchPolicy {
            allow_browser: false,
            ..FetchPolicy::oa()
        };
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), pol).expect("done");
        assert!(
            matches!(&end, FetchEnd::Challenge { host, interactive: false } if host == "pub-a.example.org")
        );
        assert_eq!(rig.count(|e| matches!(e, Effect::BrowserLoad { .. })), 0);
    }

    #[test]
    fn needs_browser_candidates_skip_the_http_tier() {
        let browser = browser_ok(PDF_A, &pdf_inspection(50_000), Some(body(11, 50_000)), None);
        let mut rig = Rig::new(script().on(is_type("browser_load"), Reply::ok(500, browser)));
        let mut cand = pdf_cand(PDF_A);
        cand.needs_browser = true;
        let (end, _) = fetch(&mut rig, cand, FetchPolicy::oa()).expect("done");
        assert!(matches!(end, FetchEnd::Pdf(_)));
        assert_eq!(count_http(&rig), 0);
        assert_eq!(rig.count(|e| matches!(e, Effect::BrowserLoad { .. })), 1);
    }

    #[test]
    fn an_interactive_challenge_opens_a_window_only_when_allowed() {
        let interactive = PageClass::Challenge {
            vendor: Vendor::Cloudflare,
            interactive: true,
        };
        let load = browser_ok(
            "https://pub-a.example.org/x.pdf",
            &page_inspection(interactive.clone(), vec![]),
            None,
            None,
        );
        let window = fixtures::login_ok(
            LoginResult::Pdf,
            PDF_A,
            &pdf_inspection(50_000),
            Some(body(21, 50_000)),
            None,
        );
        let s = || {
            script()
                .on(is_type("http"), page_reply(5, PDF_A, interactive.clone()))
                .on(is_type("browser_load"), Reply::ok(50, load.clone()))
                .on(is_type("login_window"), Reply::ok(4_000, window.clone()))
        };

        // OA policy: no window
        let mut rig = Rig::new(s());
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        assert_eq!(describe(&end), "challenge");
        assert_eq!(rig.count(|e| matches!(e, Effect::LoginWindow { .. })), 0);

        // institutional policy: the window runs and its PDF wins
        let mut rig = Rig::new(s());
        let (end, _) = fetch(
            &mut rig,
            pdf_cand(PDF_A),
            FetchPolicy::inst(InstKey("pub-a".into())),
        )
        .expect("done");
        assert!(
            matches!(&end, FetchEnd::Pdf(b) if b.ticket.is_some()),
            "{}",
            describe(&end)
        );
        let win = rig.effects().into_iter().find_map(|e| match e {
            Effect::LoginWindow { req, .. } => Some(req),
            _ => None,
        });
        let win = win.expect("window");
        assert_eq!(
            (win.kind, win.entry_url.as_str(), win.capture_pdf),
            (LoginKind::Challenge, PDF_A, true)
        );

        // ... unless the pref turned challenge windows off
        let mut cfg = fixtures::test_config();
        cfg.inst.challenge_windows = false;
        let mut rig = Rig::with_config(cfg, s());
        let (end, _) = fetch(
            &mut rig,
            pdf_cand(PDF_A),
            FetchPolicy::inst(InstKey("pub-a".into())),
        )
        .expect("done");
        assert_eq!(describe(&end), "challenge");
        assert_eq!(rig.count(|e| matches!(e, Effect::LoginWindow { .. })), 0);
    }

    // ------------------------------------------------------------------- blocks and rate limits

    #[test]
    fn a_hard_block_walls_the_host_on_the_oa_route() {
        let s = script().on(
            is_type("http"),
            page_reply(
                5,
                PDF_A,
                PageClass::Blocked {
                    vendor: Vendor::Cloudflare,
                },
            ),
        );
        let mut rig = Rig::new(s);
        let (first, walled, second) = rig
            .exec(snap(), |cx| async move {
                let mut pool = CandidatePool::new(ids());
                let a = fetch_candidate(&cx, &pdf_cand(PDF_A), &FetchPolicy::oa(), &mut pool).await;
                let walled = cx.host_skipped("pub-a.example.org");
                let b = fetch_candidate(
                    &cx,
                    &pdf_cand("https://pub-a.example.org/other.pdf"),
                    &FetchPolicy::oa(),
                    &mut pool,
                )
                .await;
                (describe(&a), walled, describe(&b))
            })
            .expect("done");
        assert_eq!(
            (first.as_str(), walled, second.as_str()),
            ("blocked", true, "refused")
        );
        assert_eq!(count_http(&rig), 1);
    }

    #[test]
    fn rate_limits_retry_once_on_the_oa_route() {
        let limited = |after| {
            page_reply(
                10,
                PDF_A,
                PageClass::RateLimited {
                    retry_after_s: after,
                },
            )
        };
        let mut rig = Rig::new(script().on_seq(
            is_type("http"),
            vec![limited(Some(4)), pdf_reply(10, PDF_A, 3)],
        ));
        let start = rig.host.now;
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        assert!(matches!(end, FetchEnd::Pdf(_)));
        let times: Vec<u64> = rig
            .timeline()
            .into_iter()
            .filter(|(_, e)| matches!(e, Effect::Http { .. }))
            .map(|(at, _)| at)
            .collect();
        // The answer takes 10 ms; the retry waits the 4 s Retry-After without holding the host gate.
        assert_eq!(times, vec![start, start + 10 + 4_000]);
    }

    #[test]
    fn a_second_rate_limit_or_a_long_retry_after_gives_up() {
        let limited = |after| {
            page_reply(
                10,
                PDF_A,
                PageClass::RateLimited {
                    retry_after_s: after,
                },
            )
        };
        let mut rig = Rig::new(script().on(is_type("http"), limited(Some(2))));
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        assert!(matches!(
            end,
            FetchEnd::RateLimited {
                retry_after_s: Some(2)
            }
        ));
        assert_eq!(count_http(&rig), 2);

        let mut rig = Rig::new(script().on(is_type("http"), limited(Some(60))));
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        assert!(matches!(
            end,
            FetchEnd::RateLimited {
                retry_after_s: Some(60)
            }
        ));
        assert_eq!(count_http(&rig), 1, "60 s is beyond the cap: no retry");
    }

    #[test]
    fn on_the_institutional_route_a_429_halts_instead_of_retrying() {
        let mut rig = Rig::new(script().on(
            is_type("http"),
            page_reply(
                10,
                PDF_A,
                PageClass::RateLimited {
                    retry_after_s: Some(120),
                },
            ),
        ));
        let (end, _) = fetch(
            &mut rig,
            pdf_cand(PDF_A),
            FetchPolicy::inst(InstKey("pub-a".into())),
        )
        .expect("done");
        assert!(matches!(
            end,
            FetchEnd::RateLimited {
                retry_after_s: Some(120)
            }
        ));
        assert_eq!(count_http(&rig), 1);
        let halted = rig.effects().into_iter().any(|e| {
            matches!(
                e,
                Effect::Persist {
                    key: PersistKey::Halts,
                    flush: true,
                    ..
                }
            )
        });
        assert!(halted, "the halt is persisted");
        // the ticket of the PDF fetch was given back
        assert_eq!(
            rig.host
                .persisted
                .get(&PersistKey::Ledger)
                .and_then(|v| v["reserved"].as_array())
                .map(Vec::len),
            Some(0)
        );
    }

    // ------------------------------------------------------------------------ institutional gating

    #[test]
    fn an_institutional_pdf_reserves_the_licence_before_the_barrier_fetch() {
        let mut rig = Rig::new(script().on(is_type("http"), pdf_reply(500, PDF_A, 3)));
        let (end, _) = fetch(
            &mut rig,
            pdf_cand(PDF_A),
            FetchPolicy::inst(InstKey("pub-a".into())),
        )
        .expect("done");
        let FetchEnd::Pdf(blob) = end else {
            panic!("expected a pdf")
        };
        assert!(
            blob.ticket.is_some(),
            "the reservation travels with the blob"
        );
        assert_eq!(blob.route, Route::Inst);
        let effects = rig.effects();
        let persist = effects.iter().position(|e| {
            matches!(
                e,
                Effect::Persist {
                    key: PersistKey::Ledger,
                    flush: true,
                    ..
                }
            )
        });
        let http = effects.iter().position(|e| {
            matches!(
                e,
                Effect::Http {
                    barrier: true,
                    route: Route::Inst,
                    ..
                }
            )
        });
        assert!(
            persist.is_some() && http.is_some() && persist < http,
            "Persist(ledger) before the barrier fetch: {persist:?} {http:?}"
        );
        let reserved = rig
            .host
            .persisted
            .get(&PersistKey::Ledger)
            .and_then(|v| v["reserved"].as_array())
            .map(Vec::len);
        assert_eq!(reserved, Some(1));
    }

    #[test]
    fn a_non_pdf_answer_gives_the_reservation_back_and_pages_reserve_nothing() {
        let mut rig = Rig::new(script().on(
            is_type("http"),
            page_reply(10, PDF_A, PageClass::AccessDenied),
        ));
        let (end, _) = fetch(
            &mut rig,
            pdf_cand(PDF_A),
            FetchPolicy::inst(InstKey("pub-a".into())),
        )
        .expect("done");
        assert_eq!(describe(&end), "access_denied");
        assert_eq!(
            rig.host
                .persisted
                .get(&PersistKey::Ledger)
                .and_then(|v| v["reserved"].as_array())
                .map(Vec::len),
            Some(0),
            "rolled back"
        );
        assert_eq!(
            rig.host
                .persisted
                .get(&PersistKey::Ledger)
                .and_then(|v| v["committed"].as_array())
                .map(Vec::len),
            Some(0),
            "never counted"
        );

        let mut rig = Rig::new(script().on(
            is_type("http"),
            Reply::ok(
                10,
                http_ok(
                    200,
                    "https://pub-a.example.org/article",
                    &page_inspection(PageClass::Landing, vec![]),
                    None,
                ),
            ),
        ));
        let (end, _) = fetch(
            &mut rig,
            landing_cand("https://pub-a.example.org/article"),
            FetchPolicy::inst(InstKey("pub-a".into())),
        )
        .expect("done");
        assert_eq!(describe(&end), "landing");
        assert!(
            !rig.host.persisted.contains_key(&PersistKey::Ledger),
            "a page fetch touches no ledger"
        );
        assert!(rig.effects().iter().any(|e| matches!(
            e,
            Effect::Http {
                barrier: false,
                route: Route::Inst,
                ..
            }
        )));
    }

    #[test]
    fn the_licence_cap_refuses_before_any_request() {
        let mut cfg = fixtures::test_config();
        cfg.inst.hourly_cap = 1;
        let mut rig = Rig::with_config(cfg, script().on(is_type("http"), pdf_reply(50, PDF_A, 3)));
        let (first, second) = rig
            .exec(snap(), |cx| async move {
                let mut pool = CandidatePool::new(ids());
                let pol = FetchPolicy::inst(InstKey("pub-a".into()));
                let a = fetch_candidate(&cx, &pdf_cand(PDF_A), &pol, &mut pool).await;
                let a_is_pdf = matches!(a, FetchEnd::Pdf(_));
                // keep the first blob (and its ticket) alive while the second fetch asks
                let b = fetch_candidate(&cx, &pdf_cand(PDF_B), &pol, &mut pool).await;
                drop(a);
                (a_is_pdf, describe(&b))
            })
            .expect("done");
        assert!(first);
        assert_eq!(second, "refused");
        assert_eq!(count_http(&rig), 1);
    }

    #[test]
    fn an_institutional_halt_refuses_without_a_request() {
        let mut rig = Rig::new(script().on(is_type("http"), pdf_reply(50, PDF_A, 3)));
        let end = rig
            .exec(snap(), |cx| async move {
                cx.halt_inst(HaltReason::RateLimited, None);
                let mut pool = CandidatePool::new(ids());
                let end = fetch_candidate(
                    &cx,
                    &pdf_cand(PDF_A),
                    &FetchPolicy::inst(InstKey("pub-a".into())),
                    &mut pool,
                )
                .await;
                matches!(end, FetchEnd::Refused(Refused::InstHalted { .. }))
            })
            .expect("done");
        assert!(end);
        assert_eq!(count_http(&rig), 0);
    }

    // ------------------------------------------------------------------------------------- offline

    #[test]
    fn offline_pauses_with_a_row_and_resumes_when_the_network_is_back() {
        let s = script().on_seq(
            is_type("http"),
            vec![
                Reply::err(5, OpErrKind::Offline),
                Reply::err(5, OpErrKind::Offline),
                pdf_reply(5, PDF_A, 3),
            ],
        );
        let mut rig = Rig::new(s);
        let start = rig.host.now;
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        assert!(matches!(end, FetchEnd::Pdf(_)));
        assert_eq!(count_http(&rig), 3);
        assert!(rig.rows().iter().any(|r| r == id::NETWORK_DOWN));
        assert!(rig.host.now >= start + 2 * policy::NETWORK_PROBE_EVERY_MS);
    }

    #[test]
    fn offline_gives_up_after_two_minutes() {
        let mut rig = Rig::new(script().on(is_type("http"), Reply::err(5, OpErrKind::Offline)));
        let start = rig.host.now;
        let (end, _) = fetch(&mut rig, pdf_cand(PDF_A), FetchPolicy::oa()).expect("done");
        assert_eq!(describe(&end), "failed:offline");
        let waited = rig.host.now - start;
        assert!(
            (policy::NETWORK_DOWN_GIVE_UP_MS..policy::NETWORK_DOWN_GIVE_UP_MS + 60_000)
                .contains(&waited),
            "{waited}"
        );
        assert_eq!(
            count_http(&rig),
            9,
            "the first try and one probe every 15 s for two minutes"
        );
    }

    // -------------------------------------------------------------------------------- attempt_oa

    fn pool_of(cands: Vec<Candidate>) -> CandidatePool {
        let mut pool = CandidatePool::new(ids());
        for c in cands {
            pool.add(c);
        }
        pool
    }

    fn attempt(rig: &mut Rig, pool: CandidatePool) -> Option<(OaEnd, CandidatePool)> {
        rig.exec(snap(), move |cx| async move {
            let mut pool = pool;
            let end = attempt_oa(&cx, &mut pool).await;
            (end, pool)
        })
    }

    fn pub_pdf(url: &str) -> Candidate {
        let mut c = pdf_cand(url);
        c.host_kind = HostKind::Publisher;
        c
    }

    #[test]
    fn the_best_candidate_wins_and_the_rest_is_not_touched() {
        let s = script()
            .on(url_has("pmc-oa-opendata"), pdf_reply(60, PMC, 3))
            .on(url_has("a.pdf"), pdf_reply(60, PDF_A, 4));
        let mut rig = Rig::new(s);
        let pool = pool_of(vec![
            pub_pdf(PDF_A),
            candidate(PMC, CandKind::Pdf, Source::PmcS3, 0),
        ]);
        let (end, _) = attempt(&mut rig, pool).expect("done");
        assert!(matches!(&end, OaEnd::Pdf(b) if b.url == PMC && b.source == Source::PmcS3));
        assert_eq!(
            rig.http().len(),
            1,
            "no hedge: the first answered within 3 s"
        );
    }

    #[test]
    fn the_hedge_starts_after_three_seconds_on_a_different_host_and_the_loser_is_cancelled() {
        // A answers after 10 s; B (another host) after 100 ms.
        let s = script()
            .on(url_has("pub-a"), pdf_reply(10_000, PDF_A, 4))
            .on(url_has("pub-b"), pdf_reply(100, PDF_B, 5));
        let mut rig = Rig::new(s);
        let pool = pool_of(vec![pub_pdf(PDF_A), pub_pdf(PDF_B)]);
        let start = rig.host.now;
        let (end, _) = attempt(&mut rig, pool).expect("done");
        assert!(
            matches!(&end, OaEnd::Pdf(b) if b.url == PDF_B),
            "the hedge won"
        );
        let http: Vec<(u64, String)> = rig
            .timeline()
            .into_iter()
            .filter_map(|(at, e)| match e {
                Effect::Http { req, .. } => Some((at - start, req.url)),
                _ => None,
            })
            .collect();
        assert_eq!(http.len(), 2);
        assert_eq!(http[0].0, 0);
        assert_eq!(
            http[1],
            (policy::HEDGE_AFTER_MS, PDF_B.to_owned()),
            "second attempt at exactly 3 s"
        );
        // the loser is cancelled in the step that returned the winner
        assert_eq!(rig.count(|e| matches!(e, Effect::Cancel { .. })), 1);
        assert!(rig.host.pending.is_empty());
    }

    #[test]
    fn candidates_on_the_same_host_are_not_hedged() {
        let a = "https://pub-a.example.org/a.pdf";
        let a2 = "https://pub-a.example.org/a2.pdf";
        let s = script()
            .on(url_has("/a.pdf"), pdf_reply(5_000, a, 4))
            .on(url_has("/a2.pdf"), pdf_reply(100, a2, 5));
        let mut rig = Rig::new(s);
        let pool = pool_of(vec![pub_pdf(a), pub_pdf(a2)]);
        let (end, _) = attempt(&mut rig, pool).expect("done");
        assert!(matches!(&end, OaEnd::Pdf(b) if b.url == a));
        assert_eq!(rig.http().len(), 1, "same host key: wait for the first");
    }

    #[test]
    fn slow_hosts_hedge_after_eight_seconds() {
        let wiley = "https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/x.pdf?download=true";
        let s = script()
            .on(url_has("wiley"), pdf_reply(30_000, wiley, 4))
            .on(url_has("pub-b"), pdf_reply(100, PDF_B, 5));
        let mut rig = Rig::new(s);
        let pool = pool_of(vec![pub_pdf(wiley), pub_pdf(PDF_B)]);
        let start = rig.host.now;
        let (end, _) = attempt(&mut rig, pool).expect("done");
        assert!(matches!(&end, OaEnd::Pdf(b) if b.url == PDF_B));
        let second = rig
            .timeline()
            .into_iter()
            .filter(|(_, e)| matches!(e, Effect::Http { .. }))
            .nth(1)
            .map(|(at, _)| at - start);
        assert_eq!(second, Some(policy::HEDGE_AFTER_SLOW_MS));
    }

    #[test]
    fn hedging_can_be_switched_off() {
        let mut cfg = fixtures::test_config();
        cfg.run.hedge = false;
        let s = script()
            .on(url_has("pub-a"), pdf_reply(9_000, PDF_A, 4))
            .on(url_has("pub-b"), pdf_reply(100, PDF_B, 5));
        let mut rig = Rig::with_config(cfg, s);
        let (end, _) =
            attempt(&mut rig, pool_of(vec![pub_pdf(PDF_A), pub_pdf(PDF_B)])).expect("done");
        assert!(matches!(&end, OaEnd::Pdf(b) if b.url == PDF_A));
        assert_eq!(rig.http().len(), 1);
    }

    #[test]
    fn a_failed_first_attempt_moves_on_without_waiting_for_the_hedge() {
        let s = script()
            .on(
                url_has("pub-a"),
                page_reply(100, PDF_A, PageClass::NotFound),
            )
            .on(url_has("pub-b"), pdf_reply(100, PDF_B, 5));
        let mut rig = Rig::new(s);
        let start = rig.host.now;
        let (end, _) =
            attempt(&mut rig, pool_of(vec![pub_pdf(PDF_A), pub_pdf(PDF_B)])).expect("done");
        assert!(matches!(&end, OaEnd::Pdf(b) if b.url == PDF_B));
        assert!(
            rig.host.now - start < policy::HEDGE_AFTER_MS,
            "sequential fallback, not a hedge"
        );
    }

    #[test]
    fn a_landing_page_feeds_the_pool_and_the_next_candidate_wins() {
        let landing = page_inspection(
            PageClass::Landing,
            vec![pdf_cand("https://pub-a.example.org/real.pdf")],
        );
        let s = script()
            .on(
                url_has("/article"),
                Reply::ok(
                    100,
                    http_ok(
                        200,
                        "https://pub-a.example.org/article",
                        &landing,
                        Some(body(8, 400)),
                    ),
                ),
            )
            .on(
                is_type("run_translators"),
                Reply::ok(50, fixtures::translators_ok(None)),
            )
            .on(
                url_has("real.pdf"),
                pdf_reply(100, "https://pub-a.example.org/real.pdf", 9),
            );
        let mut rig = Rig::new(s);
        let pool = pool_of(vec![landing_cand("https://pub-a.example.org/article")]);
        let (end, pool) = attempt(&mut rig, pool).expect("done");
        assert!(matches!(&end, OaEnd::Pdf(b) if b.url == "https://pub-a.example.org/real.pdf"));
        assert!(
            pool.was_tried("https://pub-a.example.org/article")
                && pool.was_tried("https://pub-a.example.org/real.pdf")
        );
        assert_eq!(
            rig.count(|e| matches!(e, Effect::RunTranslators { .. })),
            1,
            "translators ran on the page"
        );
        assert_eq!(
            rig.count(|e| matches!(e, Effect::Release { handle: 8 })),
            1,
            "the page body was released"
        );
    }

    #[test]
    fn exhaustion_reports_why() {
        // access denied
        let mut rig = Rig::new(script().on(
            is_type("http"),
            page_reply(10, PDF_A, PageClass::AccessDenied),
        ));
        let (end, _) = attempt(&mut rig, pool_of(vec![pub_pdf(PDF_A)])).expect("done");
        assert!(matches!(
            end,
            OaEnd::Exhausted {
                saw_access_denied: true,
                saw_challenge: false,
                saw_timeout: false,
                tried: 1
            }
        ));
        // timeout
        let mut rig = Rig::new(script().on(is_type("http"), Reply::err(10, OpErrKind::Timeout)));
        let (end, _) = attempt(&mut rig, pool_of(vec![pub_pdf(PDF_A)])).expect("done");
        assert!(matches!(
            end,
            OaEnd::Exhausted {
                saw_timeout: true,
                saw_access_denied: false,
                ..
            }
        ));
        // a PDF URL that only gave a page without anything new
        let landing = page_inspection(PageClass::Landing, vec![]);
        let mut rig = Rig::new(script().on(
            is_type("http"),
            Reply::ok(
                10,
                http_ok(200, "https://pub-a.example.org/art", &landing, None),
            ),
        ));
        let (end, _) = attempt(&mut rig, pool_of(vec![pub_pdf(PDF_A)])).expect("done");
        assert!(
            matches!(
                end,
                OaEnd::Exhausted {
                    saw_access_denied: true,
                    ..
                }
            ),
            "a PDF URL that serves an empty page is a refusal"
        );
        // nothing to try
        let mut rig = Rig::new(script());
        let (end, _) = attempt(&mut rig, pool_of(vec![])).expect("done");
        assert!(matches!(
            end,
            OaEnd::Exhausted {
                tried: 0,
                saw_access_denied: false,
                saw_challenge: false,
                saw_timeout: false
            }
        ));
        // a challenge that survives the browser
        let s = script()
            .on(is_type("http"), page_reply(5, PDF_A, challenge()))
            .on(
                is_type("browser_load"),
                Reply::ok(
                    50,
                    browser_ok(PDF_A, &page_inspection(challenge(), vec![]), None, None),
                ),
            );
        let mut rig = Rig::new(s);
        let (end, _) = attempt(&mut rig, pool_of(vec![pub_pdf(PDF_A)])).expect("done");
        assert!(matches!(
            end,
            OaEnd::Exhausted {
                saw_challenge: true,
                ..
            }
        ));
    }

    #[test]
    fn crossref_candidates_are_skipped_but_stay_in_the_pool() {
        let mut crossref = pub_pdf("https://pub-c.example.org/c.pdf");
        crossref.source = Source::Doi;
        let mut rig = Rig::new(script().on(is_type("http"), pdf_reply(10, PDF_A, 3)));
        let (end, pool) = attempt(&mut rig, pool_of(vec![crossref.clone()])).expect("done");
        assert!(matches!(end, OaEnd::Exhausted { tried: 0, .. }));
        assert!(!pool.was_tried(&crossref.url));
        assert_eq!(
            pool.ranked(&fixtures::test_config().oa).len(),
            1,
            "the institutional phase can still use it"
        );
        assert_eq!(count_http(&rig), 0);
    }

    #[test]
    fn at_most_eight_candidates_are_tried() {
        let cands: Vec<Candidate> = (0..12)
            .map(|i| pub_pdf(&format!("https://pub-{i}.example.org/{i}.pdf")))
            .collect();
        let mut rig = Rig::new(script().on(
            is_type("http"),
            page_reply(10, "https://x.example.org/", PageClass::NotFound),
        ));
        let (end, _) = attempt(&mut rig, pool_of(cands)).expect("done");
        assert!(matches!(end, OaEnd::Exhausted { tried: 8, .. }));
        assert_eq!(rig.http().len(), 8);
    }

    #[test]
    fn network_down_for_good_is_reported_as_offline() {
        let mut rig = Rig::new(script().on(is_type("http"), Reply::err(5, OpErrKind::Offline)));
        let (end, _) = attempt(&mut rig, pool_of(vec![pub_pdf(PDF_A)])).expect("done");
        assert!(matches!(end, OaEnd::Offline));
    }

    #[test]
    fn unparseable_urls_fail_fast_without_a_request() {
        let mut rig = Rig::new(script());
        let (end, _) = fetch(&mut rig, landing_cand("not a url"), FetchPolicy::oa()).expect("done");
        assert_eq!(describe(&end), "failed:bad_url");
        assert_eq!(rig.http().len(), 0);
    }
}
