//! Invariants checked after every simulated step (see `sim::Invariant`). One struct per property so a failure
//! names the property; `SimHost` adds the virtual time and the last 30 effects to the message.
//!
//! The set (`standard()` returns all of them; every scenario runs them):
//! * `HostConcurrency`: never more than 1 in-flight institutional request per publisher family (gate key) and
//!   never more than 1 in-flight OA request per OA host; at most 2 institutional requests overall. "Request"
//!   means a page or PDF fetch (`InspectSpec::Page` / `Pdf`, over HTTP or in a hidden browser); API lookups, the
//!   Alma check, EZproxy probes and OpenAthens polls have their own gates and are not counted.
//! * `PdfSpacing`: two institutional PDF fetches of one family start at least `INST_PDF_GAP_MS` apart.
//! * `LedgerCaps`: committed + reserved entries within the hour (and day) never exceed the clamped caps, in the
//!   live ledger and in every persisted copy; every institutional `barrier: true` fetch is preceded in the effect
//!   log by its own `Persist { Ledger, flush: true }`.
//! * `OpsResolvedOnce`: every op id is completed or cancelled exactly once, never both, never twice; at the end
//!   no op is pending. (`LoginUpdate` ops are exempt: the simulator completes them at once and the engine reports
//!   them in `ignored`.) A completion that crosses an engine `Cancel` is fine: it is reported in `ignored`.
//! * `NoBodyLeak`: every `BodyRef` handle that appeared in a completion is released (or consumed by a save
//!   that was followed by a release) by the end of the scenario; no handle is released twice. Handles of
//!   completions the engine ignored are released by JS and need no `Release`.
//! * `NoStall`: after a step, live tasks > 0 implies something can still happen (op in flight, timer, user
//!   wait, ready task); `Stats.stalled` is false and `Stats.lost_wakes` is 0.
//! * `OneLoginWindow`: at most one `LoginWindow` op pending at any time (login and challenge windows share one
//!   gate), also within a step: a window is never opened before the previous one is closed.
//! * `CancelCompleteness`: once an item has a terminal row, or a run has finished, none of its ops is still
//!   pending. This is what `CancelRun` promises (the same step contains `Cancel` for every op the run owned,
//!   saving items excepted: their op is over when their row is terminal) and it also covers hedge losers.
//!   Sign-in windows are exempt: they belong to the engine, not to the item that happened to open them.
//! * `RunAlwaysFinishes`: every run that shows up in the effects ends with exactly one `RunFinished`, nothing is
//!   emitted for a run after that, and the report is consistent (each item once, counts add up).
use crate::proto::*;
use crate::sched::hosts::{InstKey, host_of, oa_host_key};
use crate::sched::ledger::Caps;
use crate::sched::policy::{self, DAY_MS, HOUR_MS};
use crate::sim::{Invariant, SimHost, effect_op};
use fulltext_core::model::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// All invariants above.
pub(crate) fn standard() -> Vec<Box<dyn Invariant>> {
    vec![
        Box::new(HostConcurrency),
        Box::new(PdfSpacing::default()),
        Box::new(LedgerCaps::default()),
        Box::new(OpsResolvedOnce::default()),
        Box::new(NoBodyLeak::default()),
        Box::new(NoStall),
        Box::new(OneLoginWindow::default()),
        Box::new(CancelCompleteness),
        Box::new(RunAlwaysFinishes::default()),
    ]
}

// ------------------------------------------------------------------------------------------------ helpers

/// A page or PDF fetch that goes through a host gate: which route, which gate key, whether it fetches a PDF.
struct Fetch {
    route: Route,
    key: String,
    pdf: bool,
}

/// `Some` for the requests the host gates are about (see the module docs).
fn gated_fetch(effect: &Effect) -> Option<Fetch> {
    let (route, url, inspect) = match effect {
        Effect::Http {
            route,
            req,
            inspect,
            ..
        } => (*route, req.url.as_str(), inspect),
        Effect::BrowserLoad {
            route,
            req,
            inspect,
            ..
        } => (*route, req.url.as_str(), inspect),
        _ => return None,
    };
    let (requested, pdf) = match inspect {
        InspectSpec::Page { requested, .. } => (requested, false),
        InspectSpec::Pdf { requested, .. } => (requested, true),
        InspectSpec::Api { .. } | InspectSpec::Probe { .. } | InspectSpec::None => return None,
    };
    let key = match route {
        // The family of the item's DOI when it has one (the entry hop is doi.org, not the publisher).
        Route::Inst => match requested.doi.as_deref().filter(|d| !d.is_empty()) {
            Some(doi) => InstKey::for_doi(doi).0,
            None => InstKey::for_host(&host_of(url)?).0,
        },
        Route::Oa => oa_host_key(url)?,
    };
    Some(Fetch { route, key, pdf })
}

/// `(run, item)` an op belongs to. `item` is `None` for run-level ops (the confirm prompts). Sign-in windows
/// belong to the engine and have no owner here.
fn op_owner(effect: &Effect) -> Option<(RunId, Option<ItemId>)> {
    match effect {
        Effect::Http { run, item, .. }
        | Effect::BrowserLoad { run, item, .. }
        | Effect::RunTranslators { run, item, .. }
        | Effect::ZoteroOa { run, item, .. }
        | Effect::SavePdf { run, item, .. } => Some((*run, Some(*item))),
        Effect::LoginWindow { run, item, req, .. } if req.kind == LoginKind::Challenge => {
            Some((*run, Some(*item)))
        }
        Effect::Confirm { run, .. } => Some((*run, None)),
        _ => None,
    }
}

/// The run an effect is labelled with (0 = none).
fn run_label(effect: &Effect) -> Option<RunId> {
    match effect {
        Effect::Http { run, .. }
        | Effect::BrowserLoad { run, .. }
        | Effect::RunTranslators { run, .. }
        | Effect::ZoteroOa { run, .. }
        | Effect::SavePdf { run, .. }
        | Effect::Confirm { run, .. }
        | Effect::Progress { run, .. }
        | Effect::Trace { run, .. } => Some(*run).filter(|r| *r != 0),
        // A sign-in window carries the run of whoever opened it, but belongs to the engine.
        Effect::LoginWindow { .. }
        | Effect::LoginUpdate { .. }
        | Effect::Cancel { .. }
        | Effect::Release { .. }
        | Effect::Persist { .. }
        | Effect::RunFinished { .. }
        | Effect::Log { .. } => None,
    }
}

/// Body handles in a completion payload (same keys as the reactor looks at).
fn handles(msg: &CompletionMsg) -> Vec<u32> {
    let Some(ok) = &msg.ok else { return Vec::new() };
    ["body", "pdf", "page"]
        .iter()
        .filter_map(|key| ok.get(*key)?.get("handle")?.as_u64())
        .filter_map(|h| u32::try_from(h).ok())
        .collect()
}

fn cancelled_ops(out: &StepOut) -> BTreeSet<OpId> {
    out.effects
        .iter()
        .filter_map(|e| match e {
            Effect::Cancel { op } => Some(*op),
            _ => None,
        })
        .collect()
}

fn list(ops: &[OpId]) -> String {
    ops.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

// ------------------------------------------------------------------------------------------ HostConcurrency

/// See the module docs.
pub(crate) struct HostConcurrency;

impl Invariant for HostConcurrency {
    fn name(&self) -> &'static str {
        "HostConcurrency"
    }

    fn check(&mut self, host: &SimHost, _out: &StepOut) -> Result<(), String> {
        let mut inst_total: Vec<OpId> = Vec::new();
        let mut inst: BTreeMap<String, Vec<OpId>> = BTreeMap::new();
        let mut oa: BTreeMap<String, Vec<OpId>> = BTreeMap::new();
        for (op, pending) in &host.pending {
            let Some(fetch) = gated_fetch(&pending.effect) else {
                continue;
            };
            match fetch.route {
                Route::Inst => {
                    inst_total.push(*op);
                    inst.entry(fetch.key).or_default().push(*op);
                }
                Route::Oa => oa.entry(fetch.key).or_default().push(*op),
            }
        }
        if let Some((family, ops)) = inst.iter().find(|(_, ops)| ops.len() > 1) {
            return Err(format!(
                "{} institutional requests in flight for publisher family {family} (ops {})",
                ops.len(),
                list(ops)
            ));
        }
        if let Some((key, ops)) = oa.iter().find(|(_, ops)| ops.len() > 1) {
            return Err(format!(
                "{} open-access requests in flight for host {key} (ops {})",
                ops.len(),
                list(ops)
            ));
        }
        let global = usize::try_from(policy::INST_GLOBAL_CONCURRENCY).unwrap_or(usize::MAX);
        if inst_total.len() > global {
            return Err(format!(
                "{} institutional requests in flight, at most {global} allowed (ops {})",
                inst_total.len(),
                list(&inst_total)
            ));
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------------------------- PdfSpacing

/// See the module docs.
#[derive(Default)]
pub(crate) struct PdfSpacing {
    last_start: BTreeMap<String, u64>,
}

impl Invariant for PdfSpacing {
    fn name(&self) -> &'static str {
        "PdfSpacing"
    }

    fn check(&mut self, host: &SimHost, out: &StepOut) -> Result<(), String> {
        let gap = u64::from(policy::INST_PDF_GAP_MS);
        for effect in &out.effects {
            let Some(fetch) = gated_fetch(effect).filter(|f| f.route == Route::Inst && f.pdf)
            else {
                continue;
            };
            if let Some(prev) = self.last_start.get(&fetch.key)
                && host.now.saturating_sub(*prev) < gap
            {
                return Err(format!(
                    "PDF fetch for {} at {} ms, only {} ms after the previous one (minimum {gap} ms)",
                    fetch.key,
                    host.now,
                    host.now.saturating_sub(*prev)
                ));
            }
            self.last_start.insert(fetch.key, host.now);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------------ LedgerCaps

/// See the module docs.
#[derive(Default)]
pub(crate) struct LedgerCaps {
    /// `Persist { Ledger, flush: true }` effects seen so far.
    flushed: u32,
    /// Institutional `barrier: true` fetches seen so far.
    barriers: u32,
}

/// Entries inside the windows must fit the caps.
fn check_ledger(state: &LedgerState, wall: u64, caps: Caps, what: &str) -> Result<(), String> {
    let entries = state
        .committed
        .iter()
        .copied()
        .chain(state.reserved.iter().map(|r| (r.wall_ms, r.bytes)));
    let (mut hour, mut day, mut bytes) = (0u64, 0u64, 0u64);
    for (at, size) in entries {
        if at.saturating_add(DAY_MS) > wall {
            day += 1;
        }
        if at.saturating_add(HOUR_MS) > wall {
            hour += 1;
            bytes = bytes.saturating_add(size);
        }
    }
    let byte_cap = u64::from(caps.mb_per_hour).saturating_mul(1_000_000);
    if hour > u64::from(caps.hourly) {
        return Err(format!(
            "{what}: {hour} entries in the last hour, cap {}",
            caps.hourly
        ));
    }
    if day > u64::from(caps.daily) {
        return Err(format!(
            "{what}: {day} entries in the last 24 hours, cap {}",
            caps.daily
        ));
    }
    if bytes > byte_cap {
        return Err(format!(
            "{what}: {bytes} bytes in the last hour, cap {byte_cap}"
        ));
    }
    Ok(())
}

impl Invariant for LedgerCaps {
    fn name(&self) -> &'static str {
        "LedgerCaps"
    }

    fn check(&mut self, host: &SimHost, out: &StepOut) -> Result<(), String> {
        let live = host
            .engine
            .shared()
            .with(|s| (s.sched.ledger.state().clone(), s.sched.ledger.caps()));
        let Some((state, caps)) = live else {
            return Err("engine state was borrowed".into());
        };
        check_ledger(&state, host.wall, caps, "ledger")?;
        for effect in &out.effects {
            match effect {
                Effect::Persist {
                    key: PersistKey::Ledger,
                    value,
                    flush,
                } => {
                    let persisted: LedgerState = serde_json::from_value(value.clone())
                        .map_err(|e| format!("persisted ledger does not parse: {e}"))?;
                    check_ledger(&persisted, host.wall, caps, "persisted ledger")?;
                    if *flush {
                        self.flushed += 1;
                    }
                }
                Effect::Http {
                    route: Route::Inst,
                    barrier: true,
                    ..
                }
                | Effect::BrowserLoad {
                    route: Route::Inst,
                    barrier: true,
                    ..
                } => {
                    self.barriers += 1;
                    if self.barriers > self.flushed {
                        return Err(format!(
                            "institutional fetch #{} sent with barrier before its reservation was persisted ({} flushed ledger writes so far)",
                            self.barriers, self.flushed
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------ OpsResolvedOnce

/// How an op ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
    Pending,
    Completed,
    Cancelled,
}

/// See the module docs.
#[derive(Default)]
pub(crate) struct OpsResolvedOnce {
    ops: BTreeMap<OpId, Fate>,
    /// `LoginUpdate` ids: completed by JS at once, reported in `ignored`.
    updates: BTreeSet<OpId>,
    /// Completions scheduled as of the last check: op -> due time.
    due: BTreeMap<OpId, u64>,
}

impl Invariant for OpsResolvedOnce {
    fn name(&self) -> &'static str {
        "OpsResolvedOnce"
    }

    fn check(&mut self, host: &SimHost, out: &StepOut) -> Result<(), String> {
        // A completion that was due when the step began was delivered at its start, before the engine acted on
        // its commands. The engine either consumed it (the op is completed) or had already dropped the op and
        // reports it in `ignored` (the answer crossed the `Cancel`).
        let scheduled: BTreeMap<OpId, u64> = host
            .scheduled
            .iter()
            .map(|(at, op, _)| (*op, *at))
            .collect();
        let delivered: Vec<OpId> = self
            .due
            .iter()
            .filter(|(op, at)| !scheduled.contains_key(op) && **at <= host.now)
            .map(|(op, _)| *op)
            .collect();
        self.due = scheduled;
        for op in delivered {
            if !out.ignored.contains(&op)
                && let Some(fate) = self.ops.get_mut(&op)
                && *fate == Fate::Pending
            {
                *fate = Fate::Completed;
            }
        }
        for effect in &out.effects {
            if let Effect::LoginUpdate { op, .. } = effect {
                self.updates.insert(*op);
            } else if let Some(op) = effect_op(effect) {
                if self.ops.insert(op, Fate::Pending).is_some() {
                    return Err(format!("op {op} was issued twice"));
                }
            } else if let Effect::Cancel { op } = effect {
                match self.ops.get(op).copied() {
                    None => return Err(format!("Cancel for op {op}, which was never issued")),
                    Some(Fate::Completed) => {
                        return Err(format!("Cancel for op {op}, which had already completed"));
                    }
                    Some(Fate::Cancelled) => return Err(format!("op {op} was cancelled twice")),
                    Some(Fate::Pending) => {
                        self.ops.insert(*op, Fate::Cancelled);
                    }
                }
            }
        }
        for op in &out.ignored {
            if !self.updates.contains(op) && self.ops.get(op) != Some(&Fate::Cancelled) {
                return Err(format!(
                    "a completion for op {op} reached the engine, which has no such op (state {:?})",
                    self.ops.get(op)
                ));
            }
        }
        Ok(())
    }

    fn finish(&mut self, host: &SimHost) -> Result<(), String> {
        let open: Vec<OpId> = host.pending.keys().copied().collect();
        if open.is_empty() {
            Ok(())
        } else {
            Err(format!("ops still pending at the end: {}", list(&open)))
        }
    }
}

// ------------------------------------------------------------------------------------------------ NoBodyLeak

/// See the module docs.
#[derive(Default)]
pub(crate) struct NoBodyLeak {
    /// Completions scheduled as of the last check: op -> (due time, body handles they carry).
    offered: BTreeMap<OpId, (u64, Vec<u32>)>,
    /// Handle -> how many deliveries still wait for their `Release`.
    live: BTreeMap<u32, u32>,
}

impl Invariant for NoBodyLeak {
    fn name(&self) -> &'static str {
        "NoBodyLeak"
    }

    fn check(&mut self, host: &SimHost, out: &StepOut) -> Result<(), String> {
        let scheduled: BTreeMap<OpId, (u64, Vec<u32>)> = host
            .scheduled
            .iter()
            .map(|(at, op, msg)| (*op, (*at, handles(msg))))
            .collect();
        // A completion that left the schedule while due was delivered; one that left before it was due was
        // removed by the engine's `Cancel`. A delivered completion the engine ignored (it crossed a `Cancel`)
        // is released by JS, the others are the engine's to release.
        for (op, (due, hs)) in std::mem::replace(&mut self.offered, scheduled) {
            if self.offered.contains_key(&op) || due > host.now || out.ignored.contains(&op) {
                continue;
            }
            for h in hs {
                *self.live.entry(h).or_insert(0) += 1;
            }
        }
        for effect in &out.effects {
            if let Effect::Release { handle } = effect {
                match self.live.get_mut(handle) {
                    Some(n) if *n > 1 => *n -= 1,
                    Some(_) => {
                        self.live.remove(handle);
                    }
                    None => {
                        return Err(format!(
                            "handle {handle} was released but no delivered body holds it (released twice, or never delivered)"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self, _host: &SimHost) -> Result<(), String> {
        if self.live.is_empty() {
            return Ok(());
        }
        let leaked: Vec<String> = self
            .live
            .iter()
            .map(|(h, n)| format!("{h} (x{n})"))
            .collect();
        Err(format!(
            "body handles never released: {}",
            leaked.join(", ")
        ))
    }
}

// ----------------------------------------------------------------------------------------------------- NoStall

/// See the module docs.
pub(crate) struct NoStall;

impl Invariant for NoStall {
    fn name(&self) -> &'static str {
        "NoStall"
    }

    fn check(&mut self, _host: &SimHost, out: &StepOut) -> Result<(), String> {
        let s = &out.stats;
        if s.stalled {
            return Err(format!(
                "the engine reports a stall ({} tasks, nothing in flight)",
                s.tasks
            ));
        }
        if s.lost_wakes != 0 {
            return Err(format!(
                "{} lost wake-ups (a task only moved because of the stall sweep)",
                s.lost_wakes
            ));
        }
        if s.tasks > 0 && s.ops_inflight == 0 && s.timers == 0 && s.waiting_user == 0 && !out.more {
            return Err(format!(
                "{} tasks alive but no op, timer, user wait or ready task: nothing can happen",
                s.tasks
            ));
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------------------------- OneLoginWindow

/// See the module docs.
#[derive(Default)]
pub(crate) struct OneLoginWindow {
    open: BTreeSet<OpId>,
}

impl Invariant for OneLoginWindow {
    fn name(&self) -> &'static str {
        "OneLoginWindow"
    }

    fn check(&mut self, host: &SimHost, out: &StepOut) -> Result<(), String> {
        let cancelled = cancelled_ops(out);
        // Windows the user closed (answered) before this step's effects were produced.
        self.open
            .retain(|op| host.pending.contains_key(op) || cancelled.contains(op));
        for effect in &out.effects {
            match effect {
                Effect::LoginWindow { op, .. } => {
                    self.open.insert(*op);
                    if self.open.len() > 1 {
                        return Err(format!(
                            "windows {} are open at once",
                            list(&self.open.iter().copied().collect::<Vec<_>>())
                        ));
                    }
                }
                Effect::Cancel { op } => {
                    self.open.remove(op);
                }
                _ => {}
            }
        }
        self.open.retain(|op| host.pending.contains_key(op));
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------ CancelCompleteness

/// See the module docs.
pub(crate) struct CancelCompleteness;

fn terminal(state: RowState) -> bool {
    matches!(
        state,
        RowState::Done
            | RowState::Attention
            | RowState::Failed
            | RowState::Skipped
            | RowState::Cancelled
    )
}

impl Invariant for CancelCompleteness {
    fn name(&self) -> &'static str {
        "CancelCompleteness"
    }

    fn check(&mut self, host: &SimHost, out: &StepOut) -> Result<(), String> {
        for effect in &out.effects {
            let (run, item) = match effect {
                Effect::Progress { run, item, row } if terminal(row.state) => (*run, Some(*item)),
                Effect::RunFinished { run, .. } => (*run, None),
                _ => continue,
            };
            let stuck: Vec<OpId> = host
                .pending
                .iter()
                .filter(|(_, p)| {
                    op_owner(&p.effect)
                        .is_some_and(|(r, i)| r == run && (item.is_none() || i == item))
                })
                .map(|(op, _)| *op)
                .collect();
            if !stuck.is_empty() {
                let who = item.map_or_else(
                    || format!("run {run} finished"),
                    |i| format!("item {i} of run {run} reached a terminal row"),
                );
                return Err(format!(
                    "{who}, but ops {} are still pending (no Cancel in this step)",
                    list(&stuck)
                ));
            }
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------------------- RunAlwaysFinishes

/// See the module docs.
#[derive(Default)]
pub(crate) struct RunAlwaysFinishes {
    seen: BTreeSet<RunId>,
    finished: BTreeSet<RunId>,
}

fn check_report(report: &RunReport) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    if let Some(dup) = report
        .items
        .iter()
        .map(|i| i.item)
        .find(|id| !ids.insert(*id))
    {
        return Err(format!(
            "run {}: item {dup} appears twice in the report",
            report.run
        ));
    }
    let c = &report.counts;
    let total = c.saved
        + c.skipped
        + c.not_found
        + c.needs_user
        + c.blocked
        + c.deferred
        + c.cancelled
        + c.failed;
    if usize::try_from(total).ok() != Some(report.items.len()) {
        return Err(format!(
            "run {}: counts add up to {total} but the report lists {} items",
            report.run,
            report.items.len()
        ));
    }
    Ok(())
}

impl Invariant for RunAlwaysFinishes {
    fn name(&self) -> &'static str {
        "RunAlwaysFinishes"
    }

    fn check(&mut self, _host: &SimHost, out: &StepOut) -> Result<(), String> {
        for effect in &out.effects {
            if let Some(run) = run_label(effect) {
                if self.finished.contains(&run) {
                    return Err(format!(
                        "run {run} produced an effect after its RunFinished"
                    ));
                }
                self.seen.insert(run);
            }
            if let Effect::RunFinished { run, report } = effect {
                if !self.finished.insert(*run) {
                    return Err(format!("run {run} finished twice"));
                }
                check_report(report)?;
            }
        }
        Ok(())
    }

    fn finish(&mut self, _host: &SimHost) -> Result<(), String> {
        let open: Vec<String> = self
            .seen
            .difference(&self.finished)
            .map(ToString::to_string)
            .collect();
        if open.is_empty() {
            Ok(())
        } else {
            Err(format!("runs without a RunFinished: {}", open.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::{Cx, LocalFuture, Owner};
    use crate::l10n;
    use crate::sim::fixtures::{
        ScriptResponder, body, browser_req, http_ok, http_req, login_req, pdf_inspection, save_ok,
        test_config,
    };
    use crate::sim::{Reply, SimEnd};
    use serde_json::json;
    use std::future::Future;

    fn quiet() -> ScriptResponder {
        ScriptResponder::new().fallback(Reply::Never)
    }

    /// A configured host that checks every invariant.
    fn host(responder: ScriptResponder) -> SimHost {
        isolated(standard(), responder)
    }

    /// A configured host that checks only `invariants` (so `finish` tests need not finish synthetic runs).
    fn isolated(invariants: Vec<Box<dyn Invariant>>, responder: ScriptResponder) -> SimHost {
        let mut host = SimHost::new(Box::new(responder)).with_invariants(invariants);
        host.configure(test_config(), Persisted::default(), 7)
            .expect("configure");
        host
    }

    fn boxed(f: impl Future<Output = ()> + 'static) -> LocalFuture {
        Box::pin(f)
    }

    fn run_task(host: &mut SimHost, owner: Owner, make: impl FnOnce(Cx) -> LocalFuture) {
        host.engine.spawn_test(owner, make).expect("spawn");
    }

    fn system(host: &mut SimHost, make: impl FnOnce(Cx) -> LocalFuture) {
        run_task(host, Owner::System, make);
    }

    fn item_task(
        host: &mut SimHost,
        run: RunId,
        item: ItemId,
        make: impl FnOnce(Cx) -> LocalFuture,
    ) {
        run_task(host, Owner::Item { run, item }, make);
    }

    fn pdf_spec(doi: &str, route: Route) -> InspectSpec {
        InspectSpec::Pdf {
            requested: Ids {
                doi: Some(doi.to_owned()),
                ..Ids::default()
            },
            route,
        }
    }

    fn page_spec(url_only: bool, route: Route) -> InspectSpec {
        let requested = if url_only {
            Ids::default()
        } else {
            Ids {
                doi: Some("10.1002/x".into()),
                ..Ids::default()
            }
        };
        InspectSpec::Page { requested, route }
    }

    /// The error of a failing run, or panic when the run went through.
    fn failure(host: &mut SimHost) -> String {
        host.run_until_idle(600_000)
            .expect_err("an invariant must fail")
            .message
    }

    fn name_of(message: &str) -> &str {
        message.split(':').next().unwrap_or("")
    }

    // ---- HostConcurrency

    #[test]
    fn two_institutional_requests_to_one_family_are_caught() {
        let mut h = host(quiet());
        for n in 0..2 {
            item_task(&mut h, 1, n, |cx| {
                boxed(async move {
                    let _ = cx
                        .http(
                            Route::Inst,
                            false,
                            http_req("https://onlinelibrary.wiley.com/doi/10.1002/x"),
                            Retain::None,
                            pdf_spec("10.1002/x", Route::Inst),
                        )
                        .await;
                })
            });
        }
        let msg = failure(&mut h);
        assert_eq!(name_of(&msg), "HostConcurrency");
        assert!(msg.contains("publisher family wiley"), "{msg}");
        assert!(
            msg.contains("last effects"),
            "the failure carries the effect tail: {msg}"
        );
    }

    #[test]
    fn two_families_run_side_by_side_but_a_third_request_exceeds_the_global_limit() {
        let fetch = |url: &'static str, doi: &'static str| {
            move |cx: Cx| {
                boxed(async move {
                    let _ = cx
                        .http(
                            Route::Inst,
                            false,
                            http_req(url),
                            Retain::None,
                            pdf_spec(doi, Route::Inst),
                        )
                        .await;
                })
            }
        };
        let mut h = host(quiet());
        item_task(
            &mut h,
            1,
            1,
            fetch("https://onlinelibrary.wiley.com/a", "10.1002/a"),
        );
        item_task(&mut h, 1, 2, fetch("https://www.bmj.com/b", "10.1136/b"));
        assert_eq!(
            h.run_until_idle(1_000),
            Ok(SimEnd::Idle),
            "two in flight are fine"
        );
        item_task(
            &mut h,
            1,
            3,
            fetch("https://jamanetwork.com/c", "10.1001/c"),
        );
        let msg = failure(&mut h);
        assert!(
            msg.contains("3 institutional requests in flight, at most 2"),
            "{msg}"
        );
    }

    #[test]
    fn api_lookups_and_probes_are_not_page_fetches() {
        let mut h = host(quiet());
        for n in 0..4 {
            item_task(&mut h, 1, n, |cx| {
                boxed(async move {
                    let spec = InspectSpec::Api {
                        source: Source::Alma,
                        requested: Ids::default(),
                    };
                    let _ = cx
                        .http(
                            Route::Inst,
                            false,
                            http_req("https://librarysearch.kcl.ac.uk/x"),
                            Retain::None,
                            spec,
                        )
                        .await;
                })
            });
        }
        assert_eq!(h.run_until_idle(1_000), Ok(SimEnd::Idle));
    }

    #[test]
    fn two_open_access_requests_to_one_host_are_caught_but_two_hosts_are_fine() {
        let go = |url: &'static str| {
            move |cx: Cx| {
                boxed(async move {
                    let _ = cx
                        .http(
                            Route::Oa,
                            false,
                            http_req(url),
                            Retain::Pdf,
                            pdf_spec("10.1371/x", Route::Oa),
                        )
                        .await;
                })
            }
        };
        let mut h = host(quiet());
        item_task(&mut h, 1, 1, go("https://www.pub-a.example.org/1.pdf"));
        item_task(&mut h, 1, 2, go("https://pub-b.example.org/2.pdf"));
        assert_eq!(h.run_until_idle(1_000), Ok(SimEnd::Idle));
        item_task(&mut h, 1, 3, go("https://pub-a.example.org/3.pdf"));
        let msg = failure(&mut h);
        assert!(
            msg.contains("host pub-a.example.org"),
            "www. and bare host are one host: {msg}"
        );
    }

    #[test]
    fn requests_without_a_doi_use_the_host_family() {
        let mut h = host(quiet());
        for (n, url) in ["https://onlinelibrary.wiley.com/a", "https://wiley.com/b"]
            .into_iter()
            .enumerate()
        {
            item_task(&mut h, 1, u32::try_from(n).unwrap_or(0), move |cx| {
                boxed(async move {
                    let _ = cx
                        .http(
                            Route::Inst,
                            false,
                            http_req(url),
                            Retain::None,
                            page_spec(true, Route::Inst),
                        )
                        .await;
                })
            });
        }
        assert!(failure(&mut h).contains("family wiley"));
    }

    // ---- PdfSpacing

    #[test]
    fn pdf_fetches_of_one_family_must_be_five_seconds_apart() {
        let two_fetches = |gap: u64| {
            let answer = quiet().on(
                is_pdf_fetch(),
                Reply::ok(
                    100,
                    http_ok(200, "https://x/a.pdf", &pdf_inspection(5_000), None),
                ),
            );
            let mut h = host(answer);
            item_task(&mut h, 1, 1, move |cx| {
                boxed(async move {
                    let first = pdf_spec("10.1002/x", Route::Inst);
                    let _ = cx
                        .http(
                            Route::Inst,
                            false,
                            http_req("https://onlinelibrary.wiley.com/a.pdf"),
                            Retain::None,
                            first,
                        )
                        .await;
                    cx.sleep(gap).await;
                    let second = pdf_spec("10.1002/y", Route::Inst);
                    let _ = cx
                        .http(
                            Route::Inst,
                            false,
                            http_req("https://onlinelibrary.wiley.com/b.pdf"),
                            Retain::None,
                            second,
                        )
                        .await;
                })
            });
            h
        };
        let mut h = two_fetches(1_000);
        let msg = failure(&mut h);
        assert_eq!(name_of(&msg), "PdfSpacing");
        assert!(msg.contains("minimum 5000 ms"), "{msg}");

        let mut ok = two_fetches(5_000);
        assert_eq!(ok.run_until_idle(60_000), Ok(SimEnd::Idle));
    }

    #[test]
    fn other_families_and_page_fetches_do_not_count_for_spacing() {
        let mut h = host(quiet().on(
            |e| matches!(e, Effect::Http { .. }),
            Reply::ok(10, http_ok(200, "https://x/", &Inspection::None, None)),
        ));
        item_task(&mut h, 1, 1, |cx| {
            boxed(async move {
                let _ = cx
                    .http(
                        Route::Inst,
                        false,
                        http_req("https://onlinelibrary.wiley.com/a.pdf"),
                        Retain::None,
                        pdf_spec("10.1002/x", Route::Inst),
                    )
                    .await;
                let _ = cx
                    .http(
                        Route::Inst,
                        false,
                        http_req("https://onlinelibrary.wiley.com/page"),
                        Retain::None,
                        page_spec(false, Route::Inst),
                    )
                    .await;
                let _ = cx
                    .http(
                        Route::Inst,
                        false,
                        http_req("https://www.bmj.com/b.pdf"),
                        Retain::None,
                        pdf_spec("10.1136/y", Route::Inst),
                    )
                    .await;
                let _ = cx
                    .http(
                        Route::Oa,
                        false,
                        http_req("https://pub.example.org/c.pdf"),
                        Retain::None,
                        pdf_spec("10.1371/z", Route::Oa),
                    )
                    .await;
            })
        });
        assert_eq!(h.run_until_idle(10_000), Ok(SimEnd::Idle));
    }

    fn is_pdf_fetch() -> impl Fn(&Effect) -> bool + 'static {
        |e| gated_fetch(e).is_some_and(|f| f.pdf)
    }

    // ---- LedgerCaps

    fn ledger_with(entries: usize, wall: u64) -> Value {
        let committed: Vec<(u64, u64)> = (0..entries)
            .map(|i| (wall - 1_000 - u64::try_from(i).unwrap_or(0), 1_000))
            .collect();
        json!({ "committed": committed, "reserved": [] })
    }

    #[test]
    fn a_persisted_ledger_over_the_hourly_cap_is_caught() {
        let mut h = host(quiet());
        let wall = h.wall;
        system(&mut h, move |cx| {
            boxed(async move {
                cx.emit(Effect::Persist {
                    key: PersistKey::Ledger,
                    value: ledger_with(30, wall),
                    flush: true,
                });
                cx.emit(Effect::Persist {
                    key: PersistKey::Ledger,
                    value: ledger_with(31, wall),
                    flush: true,
                });
            })
        });
        let msg = failure(&mut h);
        assert_eq!(name_of(&msg), "LedgerCaps");
        assert!(msg.contains("31 entries in the last hour, cap 30"), "{msg}");
    }

    #[test]
    fn a_ledger_state_over_the_byte_cap_or_garbage_is_caught() {
        let mut h = host(quiet());
        let wall = h.wall;
        system(&mut h, move |cx| {
            boxed(async move {
                cx.emit(Effect::Persist {
                    key: PersistKey::Ledger,
                    value: json!({ "committed": [[wall - 5, 301_000_000u64]], "reserved": [] }),
                    flush: false,
                });
            })
        });
        assert!(failure(&mut h).contains("bytes in the last hour"));
        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move {
                cx.emit(Effect::Persist {
                    key: PersistKey::Ledger,
                    value: json!("nonsense"),
                    flush: false,
                })
            })
        });
        assert!(failure(&mut h).contains("does not parse"));
    }

    #[test]
    fn a_barrier_fetch_needs_its_own_flushed_reservation_first() {
        let fetch = |cx: Cx| {
            boxed(async move {
                let _ = cx
                    .http(
                        Route::Inst,
                        true,
                        http_req("https://onlinelibrary.wiley.com/a.pdf"),
                        Retain::Pdf,
                        pdf_spec("10.1002/x", Route::Inst),
                    )
                    .await;
            })
        };
        // without any Persist
        let mut h = host(quiet());
        item_task(&mut h, 1, 1, fetch);
        let msg = failure(&mut h);
        assert!(
            msg.contains("barrier before its reservation was persisted"),
            "{msg}"
        );
        // with the Persist after it in the same step
        let mut h = host(quiet());
        item_task(&mut h, 1, 1, fetch);
        system(&mut h, |cx| {
            boxed(async move {
                cx.emit(Effect::Persist {
                    key: PersistKey::Ledger,
                    value: json!({}),
                    flush: true,
                })
            })
        });
        assert!(
            failure(&mut h).contains("barrier"),
            "order matters: the persist came second"
        );
        // persist first: fine, and a second barrier fetch needs a second persist
        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move {
                cx.emit(Effect::Persist {
                    key: PersistKey::Ledger,
                    value: json!({}),
                    flush: true,
                })
            })
        });
        item_task(&mut h, 1, 1, fetch);
        assert_eq!(h.run_until_idle(1_000), Ok(SimEnd::Idle));
        item_task(&mut h, 1, 2, |cx| {
            boxed(async move {
                let spec = InspectSpec::Page {
                    requested: Ids {
                        doi: Some("10.1136/y".into()),
                        ..Ids::default()
                    },
                    route: Route::Inst,
                };
                let _ = cx
                    .browser_load(
                        Route::Inst,
                        true,
                        browser_req("https://go.openathens.net/x"),
                        spec,
                    )
                    .await;
            })
        });
        assert!(failure(&mut h).contains("fetch #2"));
    }

    #[test]
    fn the_live_ledger_is_checked_too() {
        let mut h = host(quiet());
        let wall = h.wall;
        h.engine.shared().with(|s| {
            let entries: Vec<(u64, u64)> = (0..40).map(|i| (wall - 1_000 - i, 1_000)).collect();
            s.sched.ledger = crate::sched::Ledger::new(
                LedgerState {
                    committed: entries,
                    reserved: Vec::new(),
                },
                Caps {
                    hourly: 30,
                    daily: 80,
                    mb_per_hour: 300,
                },
            );
        });
        let msg = h
            .step(Vec::new())
            .expect_err("the ledger already holds more than the cap")
            .message;
        assert!(msg.contains("ledger: 40 entries in the last hour"), "{msg}");
    }

    // ---- OpsResolvedOnce

    #[test]
    fn cancelling_an_unknown_or_finished_op_is_caught() {
        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move { cx.emit(Effect::Cancel { op: 99 }) })
        });
        let msg = failure(&mut h);
        assert_eq!(name_of(&msg), "OpsResolvedOnce");
        assert!(msg.contains("never issued"));

        let mut h = host(quiet().on(
            |e| matches!(e, Effect::ZoteroOa { .. }),
            Reply::ok(10, json!([])),
        ));
        system(&mut h, |cx| {
            boxed(async move {
                let _ = cx.zotero_oa("10.1/x").await;
                cx.emit(Effect::Cancel { op: 1 });
            })
        });
        assert!(failure(&mut h).contains("already completed"));
    }

    #[test]
    fn cancelling_twice_and_issuing_twice_are_caught() {
        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move {
                let _ = cx.timeout(10, cx.zotero_oa("10.1/x")).await;
                cx.emit(Effect::Cancel { op: 1 });
            })
        });
        assert!(
            failure(&mut h).contains("cancelled twice"),
            "the timeout already cancelled op 1"
        );
    }

    #[test]
    fn a_completion_that_crosses_a_cancel_is_fine_and_a_stray_one_is_not() {
        // The answer is due in the very step in which the item is dropped.
        let answer = quiet().on(
            |e| matches!(e, Effect::ZoteroOa { .. }),
            Reply::ok(100, json!([])),
        );
        let mut h = isolated(vec![Box::new(OpsResolvedOnce::default())], answer);
        item_task(&mut h, 1, 1, |cx| {
            boxed(async move {
                let _ = cx.zotero_oa("10.1/x").await;
            })
        });
        h.step(Vec::new()).expect("issue");
        h.advance(100);
        let dropped = h.engine.exec_mut().cancel_where(|_| true);
        assert_eq!(dropped.len(), 1);
        h.step(Vec::new())
            .expect("a crossing completion is reported in `ignored` and tolerated");
        assert_eq!(h.ignored, vec![1]);
        h.finish().expect("nothing left");

        let mut h = host(quiet());
        h.scheduled.push((
            h.now,
            77,
            CompletionMsg {
                op: 77,
                ok: Some(json!({})),
                err: None,
            },
        ));
        let msg = h
            .step(Vec::new())
            .expect_err("a completion for an op nobody issued")
            .message;
        assert!(msg.contains("no such op"), "{msg}");
    }

    #[test]
    fn ops_still_pending_at_the_end_fail_finish() {
        let mut h = isolated(vec![Box::new(OpsResolvedOnce::default())], quiet());
        system(&mut h, |cx| {
            boxed(async move {
                let _ = cx.zotero_oa("10.1/x").await;
            })
        });
        h.run_until_idle(100).expect("run");
        assert!(
            h.finish()
                .expect_err("pending op")
                .message
                .contains("ops still pending at the end: 1")
        );
    }

    // ---- NoBodyLeak

    fn pdf_reply(handle: u32) -> Reply {
        Reply::ok(
            10,
            http_ok(
                200,
                "https://x/a.pdf",
                &pdf_inspection(5_000),
                Some(body(handle, 5_000)),
            ),
        )
    }

    #[test]
    fn bodies_that_are_released_do_not_leak_and_repeated_handle_numbers_are_counted() {
        let mut h = isolated(
            vec![Box::new(NoBodyLeak::default())],
            quiet().on(|e| matches!(e, Effect::Http { .. }), pdf_reply(7)),
        );
        for n in 0..3 {
            item_task(&mut h, 1, n, |cx| {
                boxed(async move {
                    let resp = cx
                        .http(
                            Route::Oa,
                            false,
                            http_req(&format!("https://h{n}.example.org/a.pdf")),
                            Retain::Pdf,
                            pdf_spec("10.1371/x", Route::Oa),
                        )
                        .await;
                    drop(resp);
                })
            });
        }
        assert_eq!(h.run_until_idle(1_000), Ok(SimEnd::Idle));
        h.finish().expect("three deliveries, three releases");
    }

    #[test]
    fn a_body_that_is_never_released_is_a_leak() {
        let mut h = isolated(
            vec![Box::new(NoBodyLeak::default())],
            quiet().on(|e| matches!(e, Effect::Http { .. }), pdf_reply(7)),
        );
        system(&mut h, |cx| {
            boxed(async move {
                let resp = cx
                    .http(
                        Route::Oa,
                        false,
                        http_req("https://a.example.org/a.pdf"),
                        Retain::Pdf,
                        pdf_spec("10.1371/x", Route::Oa),
                    )
                    .await;
                std::mem::forget(resp);
            })
        });
        h.run_until_idle(1_000).expect("run");
        let msg = h.finish().expect_err("leak").message;
        assert!(
            msg.starts_with("NoBodyLeak: body handles never released: 7"),
            "{msg}"
        );
    }

    #[test]
    fn a_double_or_unknown_release_is_caught() {
        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move { cx.emit(Effect::Release { handle: 5 }) })
        });
        let msg = failure(&mut h);
        assert_eq!(name_of(&msg), "NoBodyLeak");
        assert!(msg.contains("handle 5"), "{msg}");
    }

    #[test]
    fn bodies_of_ignored_completions_are_released_by_js_not_the_engine() {
        let invariants: Vec<Box<dyn Invariant>> = vec![
            Box::new(NoBodyLeak::default()),
            Box::new(OpsResolvedOnce::default()),
        ];
        let mut h = isolated(
            invariants,
            quiet().on(|e| matches!(e, Effect::Http { .. }), pdf_reply(7)),
        );
        item_task(&mut h, 1, 1, |cx| {
            boxed(async move {
                let _ = cx
                    .http(
                        Route::Oa,
                        false,
                        http_req("https://a.example.org/a.pdf"),
                        Retain::Pdf,
                        pdf_spec("10.1371/x", Route::Oa),
                    )
                    .await;
            })
        });
        h.step(Vec::new()).expect("issue");
        h.advance(10);
        h.engine.exec_mut().cancel_where(|_| true);
        h.step(Vec::new()).expect("completion crosses the cancel");
        assert_eq!(h.ignored, vec![1]);
        h.finish()
            .expect("no Release is owed for an ignored completion");
    }

    #[test]
    fn a_save_followed_by_a_release_is_balanced() {
        let responder = quiet()
            .on(|e| matches!(e, Effect::Http { .. }), pdf_reply(7))
            .on(is_save(), Reply::ok(10, save_ok("1/K")));
        let mut h = isolated(vec![Box::new(NoBodyLeak::default())], responder);
        item_task(&mut h, 1, 1, |cx| {
            boxed(async move {
                if let Ok(resp) = cx
                    .http(
                        Route::Oa,
                        false,
                        http_req("https://a.example.org/a.pdf"),
                        Retain::Pdf,
                        pdf_spec("10.1371/x", Route::Oa),
                    )
                    .await
                    && let Some(b) = resp.body.as_ref()
                {
                    let _ = cx
                        .save_pdf(SaveReq {
                            item: 1,
                            body: b.body_ref(),
                            trim_prefix: 0,
                            version: Version::Published,
                            url_to_store: "u".into(),
                            route: Route::Oa,
                            source: "x".into(),
                        })
                        .await;
                }
            })
        });
        h.run_until_idle(1_000).expect("run");
        h.finish().expect("released after the save");
    }

    fn is_save() -> impl Fn(&Effect) -> bool + 'static {
        |e| matches!(e, Effect::SavePdf { .. })
    }

    // ---- NoStall

    #[test]
    fn a_task_that_never_registers_a_wake_trips_no_stall() {
        let mut h = host(quiet());
        system(&mut h, |_| {
            boxed(std::future::poll_fn(|_| std::task::Poll::<()>::Pending))
        });
        let msg = failure(&mut h);
        assert_eq!(name_of(&msg), "NoStall");
        assert!(msg.contains("stall"), "{msg}");
    }

    #[test]
    fn waiting_for_an_op_or_a_timer_is_not_a_stall() {
        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move {
                let _ = cx.zotero_oa("10.1/x").await;
            })
        });
        system(&mut h, |cx| boxed(async move { cx.sleep(50).await }));
        assert_eq!(h.run_until_idle(1_000), Ok(SimEnd::Idle));
    }

    // ---- OneLoginWindow

    #[test]
    fn two_windows_at_once_are_caught_even_when_the_first_is_cancelled_afterwards() {
        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move {
                cx.sleep(100).await;
                // the first window is dropped by its timeout only after this one opened (timers fire oldest first)
                let _ = cx
                    .login_window(login_req("https://go.openathens.net/b"))
                    .await;
            })
        });
        system(&mut h, |cx| {
            boxed(async move {
                let _ = cx
                    .timeout(
                        100,
                        cx.login_window(login_req("https://go.openathens.net/a")),
                    )
                    .await;
            })
        });
        let msg = failure(&mut h);
        assert_eq!(name_of(&msg), "OneLoginWindow");
        assert!(msg.contains("open at once"), "{msg}");
    }

    #[test]
    fn a_window_that_closes_before_the_next_one_opens_is_fine() {
        let mut h = host(quiet().on(
            |e| matches!(e, Effect::LoginWindow { .. }),
            Reply::ok(50, login_answer()),
        ));
        system(&mut h, |cx| {
            boxed(async move {
                let _ = cx
                    .login_window(login_req("https://go.openathens.net/a"))
                    .await;
                let _ = cx
                    .login_window(login_req("https://go.openathens.net/b"))
                    .await;
            })
        });
        assert_eq!(h.run_until_idle(10_000), Ok(SimEnd::Idle));
    }

    fn login_answer() -> Value {
        crate::sim::fixtures::login_ok(
            LoginResult::Landed,
            "https://x/",
            &Inspection::None,
            None,
            None,
        )
    }

    // ---- CancelCompleteness

    #[test]
    fn an_item_with_a_terminal_row_and_a_pending_op_is_caught() {
        let mut h = host(quiet());
        item_task(&mut h, 1, 1, |cx| {
            boxed(async move {
                let _ = cx.zotero_oa("10.1/x").await;
            })
        });
        system(&mut h, |cx| {
            boxed(async move {
                cx.sleep(10).await;
                let row = l10n::outcome_row(&Outcome::Failed { code: "x".into() });
                cx.emit(Effect::Progress {
                    run: 1,
                    item: 1,
                    row,
                });
            })
        });
        let msg = failure(&mut h);
        assert_eq!(name_of(&msg), "CancelCompleteness");
        assert!(
            msg.contains("item 1 of run 1 reached a terminal row, but ops 1 are still pending"),
            "{msg}"
        );
    }

    #[test]
    fn a_finished_run_with_a_pending_prompt_is_caught_but_sign_in_windows_are_exempt() {
        let report = || RunReport {
            run: 2,
            counts: ReportCounts::default(),
            items: Vec::new(),
            ledger: LedgerUsage::default(),
            halted: None,
            engine_restarts: 0,
        };
        let mut h = host(quiet());
        run_task(&mut h, Owner::Run(2), |cx| {
            boxed(async move {
                let _ = cx
                    .confirm(ConfirmReq {
                        kind: ConfirmKind::LargeBatch,
                        n: 20,
                        cap: 20,
                        library: String::new(),
                    })
                    .await;
            })
        });
        system(&mut h, move |cx| {
            boxed(async move {
                cx.sleep(10).await;
                cx.emit(Effect::RunFinished {
                    run: 2,
                    report: report(),
                });
            })
        });
        assert!(failure(&mut h).contains("run 2 finished, but ops 1 are still pending"));

        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move {
                let _ = cx
                    .with_ids(2, 9)
                    .login_window(login_req("https://go.openathens.net/a"))
                    .await;
            })
        });
        system(&mut h, move |cx| {
            boxed(async move {
                cx.sleep(10).await;
                cx.emit(Effect::RunFinished {
                    run: 2,
                    report: report(),
                });
            })
        });
        assert_eq!(
            h.run_until_idle(100),
            Ok(SimEnd::Idle),
            "the sign-in window belongs to the engine, not to run 2"
        );
    }

    // ---- RunAlwaysFinishes

    fn report_of(run: RunId, items: &[ItemId], saved: u32) -> RunReport {
        RunReport {
            run,
            counts: ReportCounts {
                saved,
                ..ReportCounts::default()
            },
            items: items
                .iter()
                .map(|i| ItemReport {
                    item: *i,
                    key: format!("1/K{i}"),
                    outcome: Outcome::NoIdentifier,
                    trace: Vec::new(),
                })
                .collect(),
            ledger: LedgerUsage::default(),
            halted: None,
            engine_restarts: 0,
        }
    }

    #[test]
    fn a_second_run_finished_and_effects_after_it_are_caught() {
        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move {
                cx.emit(Effect::RunFinished {
                    run: 3,
                    report: report_of(3, &[], 0),
                });
                cx.sleep(10).await;
                cx.emit(Effect::RunFinished {
                    run: 3,
                    report: report_of(3, &[], 0),
                });
            })
        });
        let msg = failure(&mut h);
        assert_eq!(name_of(&msg), "RunAlwaysFinishes");
        assert!(msg.contains("run 3 finished twice"), "{msg}");

        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move {
                cx.emit(Effect::RunFinished {
                    run: 3,
                    report: report_of(3, &[], 0),
                });
                cx.sleep(10).await;
                cx.emit(Effect::Progress {
                    run: 3,
                    item: 1,
                    row: l10n::row(RowState::Working, l10n::id::CHECKING),
                });
            })
        });
        assert!(failure(&mut h).contains("after its RunFinished"));
    }

    #[test]
    fn an_inconsistent_report_and_a_run_that_never_finishes_are_caught() {
        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move {
                cx.emit(Effect::RunFinished {
                    run: 4,
                    report: report_of(4, &[1, 1], 0),
                })
            })
        });
        assert!(failure(&mut h).contains("appears twice"));
        let mut h = host(quiet());
        system(&mut h, |cx| {
            boxed(async move {
                cx.emit(Effect::RunFinished {
                    run: 4,
                    report: report_of(4, &[1, 2], 1),
                })
            })
        });
        assert!(failure(&mut h).contains("counts add up to 1 but the report lists 2 items"));

        let mut h = isolated(vec![Box::new(RunAlwaysFinishes::default())], quiet());
        system(&mut h, |cx| {
            boxed(async move {
                cx.emit(Effect::Progress {
                    run: 8,
                    item: 1,
                    row: l10n::row(RowState::Queued, l10n::id::QUEUED),
                })
            })
        });
        h.run_until_idle(100).expect("run");
        assert!(
            h.finish()
                .expect_err("run 8 never finished")
                .message
                .contains("runs without a RunFinished: 8")
        );
    }

    #[test]
    fn standard_has_every_invariant_once() {
        let names: Vec<&str> = standard().iter().map(|i| i.name()).collect();
        assert_eq!(
            names,
            vec![
                "HostConcurrency",
                "PdfSpacing",
                "LedgerCaps",
                "OpsResolvedOnce",
                "NoBodyLeak",
                "NoStall",
                "OneLoginWindow",
                "CancelCompleteness",
                "RunAlwaysFinishes"
            ]
        );
    }
}
