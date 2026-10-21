//! Fluent ids and `ProgressRow` builders. The engine speaks only ids + args; JS formats them
//! (`Zotero.ftl.formatValueSync(id, args)`), so every id here must exist in
//! plugin/locale/en-US/zotero-fulltext.ftl (owned by the plugin area; keep this list and the ftl in step).
//!
//! Naming: `zotero-fulltext-status-*` are row texts, `zotero-fulltext-action-*` are row-action labels.
use crate::proto::{ProgressRow, RowAction, RowState};
use fulltext_core::model::{
    CapKind, Handoff, NeedsWhy, NotFoundReason, Outcome, Route, SkipReason,
};
use std::collections::BTreeMap;

/// Row text ids.
pub(crate) mod id {
    pub const QUEUED: &str = "zotero-fulltext-status-queued";
    pub const WAITING_CONFIRM: &str = "zotero-fulltext-status-waiting-confirm";
    pub const CHECKING: &str = "zotero-fulltext-status-checking";
    pub const SEARCHING_OA: &str = "zotero-fulltext-status-searching-oa";
    /// args: `source` (a `Source` snake_case name), `host`.
    pub const DOWNLOADING: &str = "zotero-fulltext-status-downloading";
    /// args: `host`, `seconds` (licence pacing or per-host gap).
    #[cfg(test)]
    pub const WAITING_HOST: &str = "zotero-fulltext-status-waiting-host";
    pub const WAITING_LOGIN: &str = "zotero-fulltext-status-waiting-login";
    #[cfg(test)]
    pub const SIGNING_IN: &str = "zotero-fulltext-status-signing-in";
    pub const INSTITUTIONAL: &str = "zotero-fulltext-status-institutional";
    pub const CHALLENGE: &str = "zotero-fulltext-status-challenge";
    pub const SAVING: &str = "zotero-fulltext-status-saving";
    pub const NETWORK_DOWN: &str = "zotero-fulltext-status-network-down";
    // Terminal rows (one per Outcome variant, see `outcome_row`).
    pub const SAVED_OA: &str = "zotero-fulltext-status-saved-oa";
    pub const SAVED_INST: &str = "zotero-fulltext-status-saved-inst";
    pub const SKIPPED_HAS_PDF: &str = "zotero-fulltext-status-skipped-has-pdf";
    pub const SKIPPED_NOT_DOWNLOADED: &str = "zotero-fulltext-status-skipped-not-downloaded";
    pub const SKIPPED_READ_ONLY: &str = "zotero-fulltext-status-not-editable";
    pub const SKIPPED_NOT_REGULAR: &str = "zotero-fulltext-status-skipped-not-regular";
    pub const SKIPPED_IN_PROGRESS: &str = "zotero-fulltext-status-skipped-in-progress";
    pub const NO_IDENTIFIER: &str = "zotero-fulltext-status-no-identifier";
    pub const NOT_FOUND: &str = "zotero-fulltext-status-not-found";
    pub const NOT_FOUND_OA_ONLY: &str = "zotero-fulltext-status-not-found-oa-only";
    pub const NOT_ENTITLED: &str = "zotero-fulltext-status-not-entitled";
    #[cfg(test)]
    pub const NEEDS_LOGIN: &str = "zotero-fulltext-status-needs-login";
    pub const LOGIN_DECLINED: &str = "zotero-fulltext-status-login-declined";
    pub const LOGIN_TIMEOUT: &str = "zotero-fulltext-status-login-timeout";
    /// args: `code` (e.g. AADSTS53000).
    pub const LOGIN_REFUSED: &str = "zotero-fulltext-status-login-refused";
    pub const LOGIN_INEFFECTIVE: &str = "zotero-fulltext-status-login-ineffective";
    pub const MISCONFIGURED: &str = "zotero-fulltext-status-misconfigured";
    pub const NEEDS_USER: &str = "zotero-fulltext-status-needs-user";
    pub const NO_PDF_LINK: &str = "zotero-fulltext-status-no-pdf-link";
    pub const GROUP_LIBRARY: &str = "zotero-fulltext-status-group-library";
    pub const ENGINE_RESTARTED: &str = "zotero-fulltext-status-engine-restarted";
    /// args: `host`.
    pub const BLOCKED: &str = "zotero-fulltext-status-blocked";
    /// args: `cap` (`hourly` | `daily` | `mb_per_hour` | `batch`).
    pub const CAP_REACHED: &str = "zotero-fulltext-status-cap-reached";
    pub const HALTED: &str = "zotero-fulltext-status-halted";
    pub const CANCELLED: &str = "zotero-fulltext-status-cancelled";
    pub const TIMED_OUT: &str = "zotero-fulltext-status-timed-out";
    /// args: `code`.
    pub const FAILED: &str = "zotero-fulltext-status-failed";
}

/// Row-action label ids, used as `RowAction::OpenBrowser { l10n, .. }`. (`AttachFile`, `SignIn`, `Retry`,
/// `CancelItem` carry no id: JS labels them itself.)
pub(crate) mod action {
    #[cfg(test)]
    pub const OPEN_BROWSER: &str = "zotero-fulltext-action-open-browser";
    pub const OPEN_LIBKEY: &str = "zotero-fulltext-action-open-libkey";
    pub const OPEN_REDIRECTOR: &str = "zotero-fulltext-action-open-redirector";
    /// The publisher's SAML / Shibboleth "institutional login" link (`Handoff.wayfless_url`).
    pub const OPEN_WAYFLESS: &str = "zotero-fulltext-action-open-wayfless";
    pub const OPEN_EZPROXY: &str = "zotero-fulltext-action-open-ezproxy";
    pub const OPEN_DOI: &str = "zotero-fulltext-action-open-doi";
}

/// A bare row.
pub(crate) fn row(state: RowState, l10n: &str) -> ProgressRow {
    ProgressRow {
        state,
        l10n: l10n.to_owned(),
        args: BTreeMap::new(),
        route: None,
        actions: Vec::new(),
    }
}

impl ProgressRow {
    /// Builder: add one Fluent argument.
    pub(crate) fn arg(mut self, key: &str, value: impl ToString) -> Self {
        self.args.insert(key.to_owned(), value.to_string());
        self
    }

    /// Builder: tag the row with the route that produced it.
    pub(crate) fn on_route(mut self, route: Route) -> Self {
        self.route = Some(route);
        self
    }

    /// Builder: add a row action button.
    pub(crate) fn with_action(mut self, action: RowAction) -> Self {
        self.actions.push(action);
        self
    }
}

/// Which hand-off links become "open in browser" buttons, in the order they are offered.
#[derive(Clone, Copy)]
enum Link {
    Redirector,
    Wayfless,
    Ezproxy,
    Libkey,
    Doi,
}

/// All five links, in the order a user is most likely to want them (the institution's sign-in links first).
const ALL_LINKS: [Link; 5] = [
    Link::Redirector,
    Link::Wayfless,
    Link::Ezproxy,
    Link::Libkey,
    Link::Doi,
];

/// One `OpenBrowser` action per available link in `which`.
fn open_actions(handoff: &Handoff, which: &[Link]) -> Vec<RowAction> {
    which
        .iter()
        .filter_map(|link| {
            let (url, label) = match link {
                Link::Redirector => (&handoff.redirector_url, action::OPEN_REDIRECTOR),
                Link::Wayfless => (&handoff.wayfless_url, action::OPEN_WAYFLESS),
                Link::Ezproxy => (&handoff.ezproxy_url, action::OPEN_EZPROXY),
                Link::Libkey => (&handoff.libkey_url, action::OPEN_LIBKEY),
                Link::Doi => (&handoff.doi_url, action::OPEN_DOI),
            };
            url.as_ref().map(|url| RowAction::OpenBrowser {
                url: url.clone(),
                l10n: label.to_owned(),
            })
        })
        .collect()
}

/// The wire name of a licence cap, as the FTL select expects it.
fn cap_name(cap: CapKind) -> &'static str {
    match cap {
        CapKind::Hourly => "hourly",
        CapKind::Daily => "daily",
        CapKind::MbPerHour => "mb_per_hour",
        CapKind::Batch => "batch",
    }
}

/// An attention row with the given actions.
fn attention(l10n: &str, actions: Vec<RowAction>) -> ProgressRow {
    actions
        .into_iter()
        .fold(row(RowState::Attention, l10n), ProgressRow::with_action)
}

fn skipped(reason: SkipReason) -> ProgressRow {
    let l10n = match reason {
        SkipReason::HasPdf | SkipReason::PdfAddedMeanwhile => id::SKIPPED_HAS_PDF,
        SkipReason::PdfNotDownloaded => id::SKIPPED_NOT_DOWNLOADED,
        SkipReason::ReadOnly => id::SKIPPED_READ_ONLY,
        SkipReason::NotRegular | SkipReason::Feed => id::SKIPPED_NOT_REGULAR,
        SkipReason::InProgressElsewhere => id::SKIPPED_IN_PROGRESS,
    };
    row(RowState::Skipped, l10n)
}

fn not_found(reason: NotFoundReason) -> ProgressRow {
    match reason {
        NotFoundReason::OaOnly => attention(
            id::NOT_FOUND_OA_ONLY,
            vec![RowAction::Retry, RowAction::AttachFile],
        ),
        NotFoundReason::NoPdfLink => attention(id::NO_PDF_LINK, vec![RowAction::AttachFile]),
        NotFoundReason::Timeout => attention(id::TIMED_OUT, vec![RowAction::AttachFile]),
        NotFoundReason::NoOaCopy | NotFoundReason::NotHeld | NotFoundReason::AccessDenied => {
            attention(id::NOT_FOUND, vec![RowAction::AttachFile])
        }
    }
}

fn needs_user(why: NeedsWhy, handoff: &Handoff) -> ProgressRow {
    let (l10n, first) = match why {
        NeedsWhy::LoginDeclined => (id::LOGIN_DECLINED, vec![RowAction::SignIn]),
        NeedsWhy::LoginTimeout => (id::LOGIN_TIMEOUT, Vec::new()),
        NeedsWhy::LoginRefused => (id::LOGIN_REFUSED, Vec::new()),
        NeedsWhy::LoginIneffective => (id::LOGIN_INEFFECTIVE, Vec::new()),
        NeedsWhy::Misconfigured => (id::MISCONFIGURED, Vec::new()),
        NeedsWhy::Challenge => (id::CHALLENGE, Vec::new()),
        NeedsWhy::NoPdfLink => (id::NO_PDF_LINK, Vec::new()),
        NeedsWhy::EngineRestarted => (id::ENGINE_RESTARTED, Vec::new()),
        NeedsWhy::HandoffRequested => (id::NEEDS_USER, Vec::new()),
        NeedsWhy::GroupLibrary => (id::GROUP_LIBRARY, Vec::new()),
        NeedsWhy::NetworkDown => (id::NETWORK_DOWN, Vec::new()),
    };
    let mut actions = first;
    actions.extend(open_actions(handoff, &ALL_LINKS));
    actions.push(RowAction::AttachFile);
    actions.push(RowAction::Retry);
    let row = attention(l10n, actions);
    match why {
        // The outcome keeps no IdP error code; the row names the kind of refusal instead.
        NeedsWhy::LoginRefused => row.arg("code", "refused"),
        _ => row,
    }
}

/// The terminal row for an outcome, including actions (OpenBrowser for hand-offs, AttachFile, SignIn, Retry).
/// Mapping table lives in docs/wp/engine-pipeline.md. The `match` has no wildcard: a new `Outcome` variant must
/// get a row before the crate compiles.
pub(crate) fn outcome_row(outcome: &Outcome) -> ProgressRow {
    match outcome {
        Outcome::Saved { route, source, .. } => {
            let l10n = match route {
                Route::Oa => id::SAVED_OA,
                Route::Inst => id::SAVED_INST,
            };
            row(RowState::Done, l10n)
                .arg("source", source)
                .on_route(*route)
        }
        Outcome::Skipped { reason } => skipped(*reason),
        Outcome::NoIdentifier => attention(id::NO_IDENTIFIER, vec![RowAction::AttachFile]),
        Outcome::NotFound { reason } => not_found(*reason),
        Outcome::NotEntitled { handoff } => {
            let mut actions = open_actions(handoff, &[Link::Libkey, Link::Doi]);
            actions.push(RowAction::AttachFile);
            attention(id::NOT_ENTITLED, actions)
        }
        Outcome::NeedsUser { why, handoff } => needs_user(*why, handoff),
        Outcome::Blocked { host, handoff } => {
            let mut actions = open_actions(handoff, &ALL_LINKS);
            actions.push(RowAction::AttachFile);
            attention(id::BLOCKED, actions).arg("host", host)
        }
        Outcome::Deferred { cap } => {
            attention(id::CAP_REACHED, vec![RowAction::Retry]).arg("cap", cap_name(*cap))
        }
        Outcome::Halted { handoff } => {
            let mut actions = open_actions(handoff, &ALL_LINKS);
            actions.push(RowAction::Retry);
            attention(id::HALTED, actions)
        }
        Outcome::Cancelled { .. } => row(RowState::Cancelled, id::CANCELLED),
        Outcome::Failed { code } => row(RowState::Failed, id::FAILED)
            .arg("code", code)
            .with_action(RowAction::Retry),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fulltext_core::model::*;

    fn full_handoff() -> Handoff {
        Handoff {
            redirector_url: Some("https://go.openathens.net/redirector/kcl.ac.uk?url=x".into()),
            ezproxy_url: Some("https://login.kcl.idm.oclc.org/login?qurl=x".into()),
            libkey_url: Some("https://libkey.io/libraries/286/10.1/x".into()),
            doi_url: Some("https://doi.org/10.1/x".into()),
            wayfless_url: None,
        }
    }

    fn open(url: &str, l10n: &str) -> RowAction {
        RowAction::OpenBrowser {
            url: url.into(),
            l10n: l10n.into(),
        }
    }

    fn all_open_actions() -> Vec<RowAction> {
        vec![
            open(
                "https://go.openathens.net/redirector/kcl.ac.uk?url=x",
                action::OPEN_REDIRECTOR,
            ),
            open(
                "https://login.kcl.idm.oclc.org/login?qurl=x",
                action::OPEN_EZPROXY,
            ),
            open(
                "https://libkey.io/libraries/286/10.1/x",
                action::OPEN_LIBKEY,
            ),
            open("https://doi.org/10.1/x", action::OPEN_DOI),
        ]
    }

    /// One sample per `Outcome` variant. The exhaustive `match` makes a new variant a compile error here.
    fn one_of_each() -> Vec<Outcome> {
        let samples = vec![
            Outcome::Saved {
                route: Route::Oa,
                source: "pmc_s3".into(),
                version: Version::Published,
                url: "https://doi.org/10.1/x".into(),
                attachment_key: "1/ABCD".into(),
            },
            Outcome::Skipped {
                reason: SkipReason::HasPdf,
            },
            Outcome::NoIdentifier,
            Outcome::NotFound {
                reason: NotFoundReason::NoOaCopy,
            },
            Outcome::NotEntitled {
                handoff: full_handoff(),
            },
            Outcome::NeedsUser {
                why: NeedsWhy::LoginDeclined,
                handoff: full_handoff(),
            },
            Outcome::Blocked {
                host: "www.sciencedirect.com".into(),
                handoff: full_handoff(),
            },
            Outcome::Deferred {
                cap: CapKind::Hourly,
            },
            Outcome::Halted {
                handoff: full_handoff(),
            },
            Outcome::Cancelled {
                by: CancelledBy::User,
            },
            Outcome::Failed { code: "io".into() },
        ];
        for s in &samples {
            match s {
                Outcome::Saved { .. }
                | Outcome::Skipped { .. }
                | Outcome::NoIdentifier
                | Outcome::NotFound { .. }
                | Outcome::NotEntitled { .. }
                | Outcome::NeedsUser { .. }
                | Outcome::Blocked { .. }
                | Outcome::Deferred { .. }
                | Outcome::Halted { .. }
                | Outcome::Cancelled { .. }
                | Outcome::Failed { .. } => {}
            }
        }
        samples
    }

    #[test]
    fn saved_rows_name_the_route_and_the_source() {
        let oa = outcome_row(&one_of_each()[0]);
        assert_eq!(
            (oa.state, oa.l10n.as_str(), oa.route),
            (RowState::Done, id::SAVED_OA, Some(Route::Oa))
        );
        assert_eq!(oa.args.get("source").map(String::as_str), Some("pmc_s3"));
        assert!(oa.actions.is_empty());
        let inst = outcome_row(&Outcome::Saved {
            route: Route::Inst,
            source: "lww".into(),
            version: Version::Published,
            url: "https://doi.org/10.1/x".into(),
            attachment_key: "1/K".into(),
        });
        assert_eq!(
            (inst.state, inst.l10n.as_str(), inst.route),
            (RowState::Done, id::SAVED_INST, Some(Route::Inst))
        );
    }

    #[test]
    fn skipped_rows() {
        let cases = [
            (SkipReason::HasPdf, id::SKIPPED_HAS_PDF),
            (SkipReason::PdfAddedMeanwhile, id::SKIPPED_HAS_PDF),
            (SkipReason::PdfNotDownloaded, id::SKIPPED_NOT_DOWNLOADED),
            (SkipReason::ReadOnly, id::SKIPPED_READ_ONLY),
            (SkipReason::NotRegular, id::SKIPPED_NOT_REGULAR),
            (SkipReason::Feed, id::SKIPPED_NOT_REGULAR),
            (SkipReason::InProgressElsewhere, id::SKIPPED_IN_PROGRESS),
        ];
        for (reason, want) in cases {
            let r = outcome_row(&Outcome::Skipped { reason });
            assert_eq!(
                (r.state, r.l10n.as_str()),
                (RowState::Skipped, want),
                "{reason:?}"
            );
            assert!(r.actions.is_empty() && r.args.is_empty(), "{reason:?}");
        }
    }

    #[test]
    fn not_found_rows() {
        let attach = vec![RowAction::AttachFile];
        let cases = [
            (
                NotFoundReason::OaOnly,
                id::NOT_FOUND_OA_ONLY,
                vec![RowAction::Retry, RowAction::AttachFile],
            ),
            (NotFoundReason::NoPdfLink, id::NO_PDF_LINK, attach.clone()),
            (NotFoundReason::NoOaCopy, id::NOT_FOUND, attach.clone()),
            (NotFoundReason::NotHeld, id::NOT_FOUND, attach.clone()),
            (NotFoundReason::AccessDenied, id::NOT_FOUND, attach.clone()),
            (NotFoundReason::Timeout, id::TIMED_OUT, attach),
        ];
        for (reason, want, actions) in cases {
            let r = outcome_row(&Outcome::NotFound { reason });
            assert_eq!(
                (r.state, r.l10n.as_str()),
                (RowState::Attention, want),
                "{reason:?}"
            );
            assert_eq!(r.actions, actions, "{reason:?}");
        }
        let r = outcome_row(&Outcome::NoIdentifier);
        assert_eq!(
            (r.state, r.l10n.as_str(), r.actions),
            (
                RowState::Attention,
                id::NO_IDENTIFIER,
                vec![RowAction::AttachFile]
            )
        );
    }

    #[test]
    fn not_entitled_offers_libkey_and_the_doi_only() {
        let r = outcome_row(&Outcome::NotEntitled {
            handoff: full_handoff(),
        });
        assert_eq!(
            (r.state, r.l10n.as_str()),
            (RowState::Attention, id::NOT_ENTITLED)
        );
        assert_eq!(
            r.actions,
            vec![
                open(
                    "https://libkey.io/libraries/286/10.1/x",
                    action::OPEN_LIBKEY
                ),
                open("https://doi.org/10.1/x", action::OPEN_DOI),
                RowAction::AttachFile
            ]
        );
        let bare = outcome_row(&Outcome::NotEntitled {
            handoff: Handoff::default(),
        });
        assert_eq!(
            bare.actions,
            vec![RowAction::AttachFile],
            "no links, no buttons"
        );
    }

    #[test]
    fn needs_user_rows_cover_every_reason() {
        let cases = [
            (NeedsWhy::LoginDeclined, id::LOGIN_DECLINED),
            (NeedsWhy::LoginTimeout, id::LOGIN_TIMEOUT),
            (NeedsWhy::LoginRefused, id::LOGIN_REFUSED),
            (NeedsWhy::LoginIneffective, id::LOGIN_INEFFECTIVE),
            (NeedsWhy::Misconfigured, id::MISCONFIGURED),
            (NeedsWhy::Challenge, id::CHALLENGE),
            (NeedsWhy::NoPdfLink, id::NO_PDF_LINK),
            (NeedsWhy::EngineRestarted, id::ENGINE_RESTARTED),
            (NeedsWhy::HandoffRequested, id::NEEDS_USER),
            (NeedsWhy::GroupLibrary, id::GROUP_LIBRARY),
            (NeedsWhy::NetworkDown, id::NETWORK_DOWN),
        ];
        for (why, want) in cases {
            let r = outcome_row(&Outcome::NeedsUser {
                why,
                handoff: full_handoff(),
            });
            assert_eq!(
                (r.state, r.l10n.as_str()),
                (RowState::Attention, want),
                "{why:?}"
            );
            let mut expect = Vec::new();
            if why == NeedsWhy::LoginDeclined {
                expect.push(RowAction::SignIn);
            }
            expect.extend(all_open_actions());
            expect.push(RowAction::AttachFile);
            expect.push(RowAction::Retry);
            assert_eq!(r.actions, expect, "{why:?}");
        }
        let refused = outcome_row(&Outcome::NeedsUser {
            why: NeedsWhy::LoginRefused,
            handoff: Handoff::default(),
        });
        assert!(
            refused.args.contains_key("code"),
            "the FTL message interpolates $code"
        );
    }

    #[test]
    fn blocked_deferred_halted_cancelled_failed() {
        let b = outcome_row(&Outcome::Blocked {
            host: "www.sciencedirect.com".into(),
            handoff: full_handoff(),
        });
        assert_eq!(
            (b.state, b.l10n.as_str()),
            (RowState::Attention, id::BLOCKED)
        );
        assert_eq!(
            b.args.get("host").map(String::as_str),
            Some("www.sciencedirect.com")
        );
        let mut want = all_open_actions();
        want.push(RowAction::AttachFile);
        assert_eq!(b.actions, want);

        for (cap, name) in [
            (CapKind::Hourly, "hourly"),
            (CapKind::Daily, "daily"),
            (CapKind::MbPerHour, "mb_per_hour"),
            (CapKind::Batch, "batch"),
        ] {
            let d = outcome_row(&Outcome::Deferred { cap });
            assert_eq!(
                (d.state, d.l10n.as_str()),
                (RowState::Attention, id::CAP_REACHED)
            );
            assert_eq!(d.args.get("cap").map(String::as_str), Some(name));
            assert_eq!(d.actions, vec![RowAction::Retry]);
        }

        let h = outcome_row(&Outcome::Halted {
            handoff: full_handoff(),
        });
        assert_eq!(
            (h.state, h.l10n.as_str()),
            (RowState::Attention, id::HALTED)
        );
        let mut want = all_open_actions();
        want.push(RowAction::Retry);
        assert_eq!(h.actions, want);

        for by in [
            CancelledBy::User,
            CancelledBy::ItemDeleted,
            CancelledBy::Shutdown,
            CancelledBy::ParentGone,
        ] {
            let c = outcome_row(&Outcome::Cancelled { by });
            assert_eq!(
                (c.state, c.l10n.as_str()),
                (RowState::Cancelled, id::CANCELLED)
            );
            assert!(c.actions.is_empty());
        }

        let f = outcome_row(&Outcome::Failed { code: "io".into() });
        assert_eq!((f.state, f.l10n.as_str()), (RowState::Failed, id::FAILED));
        assert_eq!(f.args.get("code").map(String::as_str), Some("io"));
        assert_eq!(f.actions, vec![RowAction::Retry]);
    }

    #[test]
    fn handoff_links_that_do_not_exist_are_skipped() {
        let only_doi = Handoff {
            doi_url: Some("https://doi.org/10.1/x".into()),
            ..Handoff::default()
        };
        let r = outcome_row(&Outcome::Halted { handoff: only_doi });
        assert_eq!(
            r.actions,
            vec![
                open("https://doi.org/10.1/x", action::OPEN_DOI),
                RowAction::Retry
            ]
        );
    }

    #[test]
    fn the_wayfless_link_is_offered_after_the_redirector_and_before_the_proxy() {
        let handoff = Handoff {
            wayfless_url: Some("https://sp.nature.com/saml/login?idp=x".into()),
            ..full_handoff()
        };
        let r = outcome_row(&Outcome::NeedsUser {
            why: NeedsWhy::HandoffRequested,
            handoff: handoff.clone(),
        });
        let labels: Vec<&str> = r
            .actions
            .iter()
            .filter_map(|a| match a {
                RowAction::OpenBrowser { l10n, .. } => Some(l10n.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                action::OPEN_REDIRECTOR,
                action::OPEN_WAYFLESS,
                action::OPEN_EZPROXY,
                action::OPEN_LIBKEY,
                action::OPEN_DOI
            ]
        );
        // Without a redirector (a SAML-only institution) it comes first.
        let saml_only = Handoff {
            redirector_url: None,
            ezproxy_url: None,
            libkey_url: None,
            ..handoff
        };
        let r = outcome_row(&Outcome::Halted { handoff: saml_only });
        assert_eq!(
            r.actions.first(),
            Some(&open(
                "https://sp.nature.com/saml/login?idp=x",
                action::OPEN_WAYFLESS
            ))
        );
        assert_eq!(
            action::OPEN_WAYFLESS,
            "zotero-fulltext-action-open-wayfless"
        );
    }

    #[test]
    fn every_outcome_gets_a_row_with_a_known_id() {
        let known = known_ids();
        for o in one_of_each() {
            let r = outcome_row(&o);
            assert!(known.contains(&r.l10n.as_str()), "{o:?} -> {}", r.l10n);
            for a in &r.actions {
                if let RowAction::OpenBrowser { url, l10n } = a {
                    assert!(url.starts_with("https://"), "{url}");
                    assert!(known.contains(&l10n.as_str()), "{l10n}");
                }
            }
        }
    }

    /// Every id of this file (the `id` and `action` modules).
    fn known_ids() -> Vec<&'static str> {
        vec![
            id::QUEUED,
            id::WAITING_CONFIRM,
            id::CHECKING,
            id::SEARCHING_OA,
            id::DOWNLOADING,
            id::WAITING_HOST,
            id::WAITING_LOGIN,
            id::SIGNING_IN,
            id::INSTITUTIONAL,
            id::CHALLENGE,
            id::SAVING,
            id::NETWORK_DOWN,
            id::SAVED_OA,
            id::SAVED_INST,
            id::SKIPPED_HAS_PDF,
            id::SKIPPED_NOT_DOWNLOADED,
            id::SKIPPED_READ_ONLY,
            id::SKIPPED_NOT_REGULAR,
            id::SKIPPED_IN_PROGRESS,
            id::NO_IDENTIFIER,
            id::NOT_FOUND,
            id::NOT_FOUND_OA_ONLY,
            id::NOT_ENTITLED,
            id::NEEDS_LOGIN,
            id::LOGIN_DECLINED,
            id::LOGIN_TIMEOUT,
            id::LOGIN_REFUSED,
            id::LOGIN_INEFFECTIVE,
            id::MISCONFIGURED,
            id::NEEDS_USER,
            id::NO_PDF_LINK,
            id::GROUP_LIBRARY,
            id::ENGINE_RESTARTED,
            id::BLOCKED,
            id::CAP_REACHED,
            id::HALTED,
            id::CANCELLED,
            id::TIMED_OUT,
            id::FAILED,
            action::OPEN_BROWSER,
            action::OPEN_LIBKEY,
            action::OPEN_REDIRECTOR,
            action::OPEN_EZPROXY,
            action::OPEN_DOI,
        ]
    }

    /// The plugin's FTL must define every id (tests/js/ftl.test.mjs checks the other direction). Skipped when the
    /// plugin folder is not part of the checkout.
    #[test]
    fn every_id_has_a_message_in_the_plugin_ftl() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../plugin/locale/en-US/zotero-fulltext.ftl"
        );
        let Ok(ftl) = std::fs::read_to_string(path) else {
            return;
        };
        for known in known_ids() {
            let defined = ftl.lines().any(|line| {
                line.trim_end().starts_with(known)
                    && line.trim_start_matches(known).trim_start().starts_with('=')
            });
            assert!(defined, "{known} is not defined in zotero-fulltext.ftl");
        }
    }

    #[test]
    fn row_builders_compose() {
        let r = row(RowState::Working, id::DOWNLOADING)
            .arg("host", "pmc.ncbi.nlm.nih.gov")
            .arg("n", 3)
            .on_route(Route::Oa)
            .with_action(RowAction::CancelItem);
        assert_eq!(r.args.len(), 2);
        assert_eq!(r.args.get("n").map(String::as_str), Some("3"));
        assert_eq!(
            (r.route, r.actions),
            (Some(Route::Oa), vec![RowAction::CancelItem])
        );
    }
}
