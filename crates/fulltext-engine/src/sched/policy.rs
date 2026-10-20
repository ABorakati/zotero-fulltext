//! Every number that is policy (limits, gaps, timeouts), in one place. Licence-derived ceilings are hard
//! constants: user prefs may only LOWER them (`min(pref, CEIL_*)`). Sources: docs/design/engine.md section 5,
//! research/04 section 8.2 (licence), research/05 section 6 (OA politeness), research/08 section 3.6 (slow hosts).
use fulltext_core::model::Source;

// ---- engine sizing
pub(crate) const MAX_TASKS: u32 = 256;
/// Polls per `Engine::step` before `StepOut.more = true`.
pub(crate) const POLL_BUDGET: u32 = 256;
pub(crate) const ITEM_CONCURRENCY_DEFAULT: u32 = 4;
pub(crate) const ITEM_CONCURRENCY_MAX: u32 = 6;

// ---- institutional route (licence-derived)
/// In flight across all institutional hosts.
pub(crate) const INST_GLOBAL_CONCURRENCY: u64 = 2;
/// In flight per publisher family / host.
pub(crate) const INST_HOST_CONCURRENCY: u64 = 1;
/// Minimum gap after a page fetch from the same publisher.
pub(crate) const INST_PAGE_GAP_MS: u32 = 2_000;
/// Minimum gap after a PDF fetch from the same publisher, plus uniform jitter in `0..=INST_PDF_JITTER_MS`.
pub(crate) const INST_PDF_GAP_MS: u32 = 5_000;
pub(crate) const INST_PDF_JITTER_MS: u32 = 3_000;
/// Fetches (page, PDF link, PDF, +1) one item may spend on the institutional route; a browser load counts 1.
pub(crate) const INST_FETCH_BUDGET_PER_ITEM: u32 = 4;
/// Ledger ceilings; `InstPrefs` may only lower them.
pub(crate) const CEIL_HOURLY: u32 = 30;
pub(crate) const CEIL_DAILY: u32 = 80;
pub(crate) const CEIL_MB_PER_HOUR: u32 = 300;
/// Institutional items per run; the rest run OA-only (`Outcome::Deferred { cap: Batch }`).
pub(crate) const CEIL_BATCH_MAX: u32 = 20;
/// Bytes assumed for a reservation when the real size is unknown (a typical article PDF).
pub(crate) const RESERVE_BYTES_DEFAULT: u64 = 4 * 1024 * 1024;
/// A reservation that was never committed or rolled back and is older than this (engine restarted, crash) is
/// counted as committed when the ledger is loaded: the ledger must never under-count.
pub(crate) const RESERVATION_STALE_MS: u64 = 10 * 60_000;
pub(crate) const HOUR_MS: u64 = 3_600_000;
pub(crate) const DAY_MS: u64 = 24 * HOUR_MS;

// ---- halts (stop signals)
/// 429 / suspend page / challenge loop: halt the institutional route for `max(Retry-After, this)`.
pub(crate) const HALT_MIN_MS: u64 = 30 * 60_000;
/// EZproxy "suspended" page: longer cooldown.
pub(crate) const SUSPEND_COOLDOWN_MS: u64 = DAY_MS;
/// Challenge outcomes on one host before the host is walled for the run.
pub(crate) const CHALLENGES_TO_WALL_HOST: u32 = 2;
/// Max sign-in prompts per realm per run (expiry mid-batch re-prompts once).
pub(crate) const MAX_PROMPTS_PER_REALM: u32 = 2;
/// A caller parked behind a probe takes the probe over when the prober has not reported for this long (its item
/// was cancelled or its load hung). Longer than the longest hidden-browser load (90 s) plus inspection.
pub(crate) const PROBE_STALE_MS: u64 = 150_000;
/// The engine-side login timer is the window timeout (`InstPrefs.login_timeout_sec`) plus this grace, so JS's own
/// `Timeout` result (which closes the window cleanly) normally wins and the engine timer is only a backstop.
pub(crate) const LOGIN_ENGINE_GRACE_MS: u64 = 5_000;

// ---- knowledge caches (TTL in wall ms)
pub(crate) const EZPROXY_TTL_MS: u64 = 7 * DAY_MS;
pub(crate) const ALMA_TTL_MS: u64 = DAY_MS;

// ---- OA phase
pub(crate) const OA_HOST_CONCURRENCY: u64 = 1;
pub(crate) const OA_HOST_GAP_MS: u32 = 1_000;
/// Consecutive failures after which an OA publisher host is skipped for the rest of the run.
pub(crate) const OA_HOST_FAIL_LIMIT: u32 = 5;
/// A walled OA host (5 failures in a row, or 2 challenges) gets another chance this long after it was walled
/// (monotonic ms). Without it a Wi-Fi outage would skip every host until Zotero restarts.
pub(crate) const HOST_WALL_TTL_MS: u64 = 30 * 60_000;
/// Consecutive failures reported on permits of one OA API (`Permit::report(false)`) that switch it off.
pub(crate) const API_FAIL_LIMIT: u32 = 3;
/// How long an OA API stays switched off (circuit breaker, `Cx::disable_api`), in virtual ms.
pub(crate) const API_COOLDOWN_MS: u64 = 5 * 60_000;

// ---- shared resources
pub(crate) const BROWSER_SLOTS: u64 = 2;
pub(crate) const SAVE_SLOTS: u64 = 1;
/// Retained-body budget (JS may stream to disk; this still bounds handles held at once).
pub(crate) const BYTES_BUDGET: u64 = 192 * 1024 * 1024;
#[cfg(test)]
pub(crate) const BYTES_MIN_RESERVE: u64 = 16 * 1024 * 1024;

// ---- pipeline timing (virtual ms)
pub(crate) const DISCOVERY_BUDGET_MS: u64 = 12_000;
/// After a strong candidate appears, wait this long for better ones, then stop discovery.
pub(crate) const STRONG_SETTLE_MS: u64 = 1_500;
/// OA only: start the next attempt (different host) when the current one has not answered by then.
pub(crate) const HEDGE_AFTER_MS: u64 = 3_000;
pub(crate) const HEDGE_AFTER_SLOW_MS: u64 = 8_000;
/// How long to wait for Zotero translators on a landing page before going on with the Rust candidates.
pub(crate) const TRANSLATOR_WAIT_MS: u64 = 8_000;
/// OpenAthens "Please wait..." HEAD poll.
pub(crate) const OPENATHENS_POLL_MS: u64 = 2_500;
pub(crate) const OPENATHENS_POLL_MAX_MS: u64 = 30_000;
/// Offline handling: pause new fetches, probe every 15 s for 2 min, then fail.
pub(crate) const NETWORK_PROBE_EVERY_MS: u64 = 15_000;
pub(crate) const NETWORK_DOWN_GIVE_UP_MS: u64 = 120_000;

/// Hosts whose landing pages / PDFs are known to take 25-60 s (research/08 section 3.6); matched by suffix.
pub(crate) const SLOW_HOST_SUFFIXES: &[&str] = &["onlinelibrary.wiley.com", "wiley.com"];

/// Item concurrency from the pref: clamped to `1..=ITEM_CONCURRENCY_MAX`.
pub(crate) fn clamp_item_concurrency(pref: u32) -> u64 {
    u64::from(pref.clamp(1, ITEM_CONCURRENCY_MAX))
}

/// Gap after an institutional PDF fetch from one publisher family: the licence-derived floor, or the user's
/// `InstPrefs.min_gap_sec` when that is larger (prefs may only RAISE gaps). The 0-3 s jitter is added per release.
pub(crate) fn pdf_gap_ms(min_gap_sec: u32) -> u32 {
    INST_PDF_GAP_MS.max(min_gap_sec.saturating_mul(1000))
}

/// What a timeout is for. Each class has a normal and a slow-host value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FetchClass {
    OaApi,
    Landing,
    Pdf,
    Probe,
    BrowserLoad,
    #[allow(dead_code)] // reserved: never constructed yet
    Translators,
    Challenge,
    /// Whole item, excluding time parked in the login gate.
    Item,
}

/// Engine-side timeout in ms (JS adds a slightly longer backstop). The login window timeout comes from
/// `InstPrefs.login_timeout_sec`, not from here.
pub(crate) fn timeout_ms(class: FetchClass, slow: bool) -> u32 {
    match (class, slow) {
        (FetchClass::OaApi, _) => 9_000,
        (FetchClass::Landing, false) => 20_000,
        (FetchClass::Landing, true) => 60_000,
        (FetchClass::Pdf, false) => 90_000,
        (FetchClass::Pdf, true) => 120_000,
        (FetchClass::Probe, _) => 8_000,
        (FetchClass::BrowserLoad, false) => 60_000,
        (FetchClass::BrowserLoad, true) => 90_000,
        (FetchClass::Translators, _) => 8_000,
        (FetchClass::Challenge, _) => 120_000,
        (FetchClass::Item, _) => 360_000,
    }
}

/// Politeness limits of one OA API: token bucket (`burst` tokens, one refilled per `refill_ms`) and the
/// concurrent-request cap. Values from research/05 section 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ApiLimits {
    pub concurrency: u64,
    pub burst: u32,
    pub refill_ms: u32,
}

pub(crate) fn api_limits(src: Source) -> ApiLimits {
    let (concurrency, burst, refill_ms) = match src {
        Source::Openalex => (4, 10, 100),
        Source::Europepmc => (2, 5, 200),
        Source::Biorxiv => (1, 1, 1_000),
        Source::PmcS3 => (3, 5, 200),
        Source::Arxiv => (1, 1, 3_000),
        Source::Unpaywall => (4, 10, 100),
        Source::ZoteroOa => (2, 2, 500),
        Source::SemanticScholar => (1, 1, 1_000),
        Source::Core => (1, 1, 6_000),
        Source::Alma => (1, 1, 1_000),
        Source::ItemUrl
        | Source::Doi
        | Source::Translator
        | Source::Extractor
        | Source::Redirector
        | Source::Ezproxy => (4, 10, 100),
    };
    ApiLimits {
        concurrency,
        burst,
        refill_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_only_raise_the_pdf_gap() {
        assert_eq!(pdf_gap_ms(0), INST_PDF_GAP_MS);
        assert_eq!(pdf_gap_ms(1), INST_PDF_GAP_MS);
        assert_eq!(pdf_gap_ms(5), 5_000);
        assert_eq!(pdf_gap_ms(12), 12_000);
        assert_eq!(pdf_gap_ms(u32::MAX), u32::MAX);
    }

    #[test]
    fn item_concurrency_is_clamped() {
        assert_eq!(clamp_item_concurrency(0), 1);
        assert_eq!(clamp_item_concurrency(4), 4);
        assert_eq!(clamp_item_concurrency(99), u64::from(ITEM_CONCURRENCY_MAX));
    }

    #[test]
    fn slow_hosts_get_the_long_profile() {
        assert_eq!(timeout_ms(FetchClass::Landing, false), 20_000);
        assert_eq!(timeout_ms(FetchClass::Landing, true), 60_000);
        assert_eq!(timeout_ms(FetchClass::Pdf, true), 120_000);
        for class in [
            FetchClass::OaApi,
            FetchClass::Landing,
            FetchClass::Pdf,
            FetchClass::Probe,
            FetchClass::BrowserLoad,
        ] {
            assert!(
                timeout_ms(class, true) >= timeout_ms(class, false),
                "{class:?}"
            );
        }
    }

    #[test]
    fn every_api_has_usable_limits() {
        use Source::*;
        for src in [
            Openalex,
            Europepmc,
            Biorxiv,
            PmcS3,
            Arxiv,
            Unpaywall,
            ZoteroOa,
            SemanticScholar,
            Core,
            Alma,
            ItemUrl,
            Doi,
        ] {
            let l = api_limits(src);
            assert!(
                l.concurrency >= 1 && l.burst >= 1 && l.refill_ms >= 1,
                "{src:?}"
            );
        }
    }

    #[test]
    fn licence_ceilings_match_the_research() {
        assert_eq!(
            (CEIL_HOURLY, CEIL_DAILY, CEIL_MB_PER_HOUR, CEIL_BATCH_MAX),
            (30, 80, 300, 20)
        );
        assert_eq!((INST_GLOBAL_CONCURRENCY, INST_HOST_CONCURRENCY), (2, 1));
        const { assert!(HALT_MIN_MS < SUSPEND_COOLDOWN_MS) };
    }
}
