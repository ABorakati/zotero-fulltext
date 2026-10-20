//! fulltext-core::model - data shared by the pure core, the engine and the JS host.
//! Conventions: every type derives Clone, Debug, PartialEq, Serialize, Deserialize; fieldless enums also Copy, Eq,
//! PartialOrd, Ord, Hash; every enum has #[serde(rename_all = "snake_case")]; JS-supplied structs use
//! #[serde(default)] on every optional field. (Derives are omitted from the listings in DESIGN.md.)
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type ItemId = u32;
pub type RunId = u32;
pub type OpId = u64;

macro_rules! plain_enum {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $v:ident),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($(#[$vm])* $v),+ }
    };
}

plain_enum!(Route { Oa, Inst });
plain_enum!(Source {
    Openalex, Europepmc, Biorxiv, PmcS3, Arxiv, Unpaywall, ZoteroOa, SemanticScholar, Core,
    Alma, ItemUrl, Doi, Translator, Extractor, Redirector, Ezproxy
});
plain_enum!(Version { Published, Accepted, Submitted, Unknown });
plain_enum!(HostKind { Publisher, Repository, Pmc, Preprint, Aggregator, Other });
plain_enum!(CandKind { Pdf, Landing });
plain_enum!(Ua { Default, Plain });
plain_enum!(Realm { Openathens, Ezproxy });
plain_enum!(Vendor { Cloudflare, Akamai, Pmc, Thieme, Sciencedirect, Other });
plain_enum!(Idp { Entra, Openathens, Kcl, Other });
plain_enum!(AccessHint { Unknown, Granted, NoAccess });
plain_enum!(ApiStatus { Found, NotFound, Empty, Throttled, Unparseable });
plain_enum!(UrlClassKind { OpenathensRedirector, IdpLogin, EzproxyLogin, PublisherSsoHop, Other });

impl Default for Version { fn default() -> Self { Version::Unknown } }
impl Default for HostKind { fn default() -> Self { HostKind::Other } }
impl Default for AccessHint { fn default() -> Self { AccessHint::Unknown } }

// ------------------------------------------------------------------ identifiers
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ids {
    #[serde(default)] pub doi: Option<String>,    // lower-case, bare ("10.1371/journal.pmed.1000097")
    #[serde(default)] pub pmid: Option<String>,   // digits
    #[serde(default)] pub pmcid: Option<String>,  // "PMC2707599"
    #[serde(default)] pub arxiv: Option<String>,  // "1706.03762" | "cs/0112017", no version suffix
    #[serde(default)] pub rxiv: bool,             // doi starts with 10.1101/ (bioRxiv or medRxiv)
    #[serde(default)] pub url: Option<String>,    // the item's own URL (http(s) only)
    #[serde(default)] pub sources: BTreeMap<String, String>, // id name -> where it came from
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnedIds {
    #[serde(default)] pub doi: Option<String>,
    #[serde(default)] pub pmid: Option<String>,
    #[serde(default)] pub pmcid: Option<String>,
    #[serde(default)] pub arxiv: Option<String>,
}

plain_enum!(LibraryKind { User, Group, Feed });
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryInfo {
    pub id: u32,
    pub kind: LibraryKind,
    pub files_editable: bool,
    #[serde(default)] pub name: String,
}
plain_enum!(AttachKind { None, Pdf, Other });
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BestAttachment {
    pub kind: AttachKind,
    #[serde(default)] pub exists: Option<bool>,   // Some(false) = synced but not downloaded
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemFields {
    #[serde(default)] pub doi: String,
    #[serde(default)] pub pmid: String,
    #[serde(default)] pub pmcid: String,
    #[serde(default)] pub url: String,
    #[serde(default)] pub archive_id: String,
    #[serde(default)] pub extra: String,
}
/// What JS reads from a Zotero.Item before a run (cheap field reads only).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemSnapshot {
    pub item: ItemId,
    pub key: String,                  // "<libraryID>/<itemKey>"
    pub library: LibraryInfo,
    pub regular: bool,
    pub feed: bool,
    pub item_type: String,
    pub title: String,
    pub fields: ItemFields,
    pub pdf_attachments: u32,
    pub best_attachment: BestAttachment,
}

// ------------------------------------------------------------------ candidates
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub url: String,
    pub kind: CandKind,
    pub source: Source,
    #[serde(default)] pub version: Version,
    #[serde(default)] pub license: Option<String>,
    #[serde(default)] pub host_kind: HostKind,
    #[serde(default)] pub record_doi: Option<String>,
    #[serde(default)] pub landing_url: Option<String>,
    #[serde(default)] pub signal: Option<String>,
    #[serde(default)] pub score: i32,
    #[serde(default)] pub tier: u8,
    #[serde(default)] pub needs_browser: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiInspection {
    pub source: Source,
    pub status: ApiStatus,
    #[serde(default)] pub learned: LearnedIds,
    #[serde(default)] pub candidates: Vec<Candidate>,
    #[serde(default)] pub oa_status: Option<String>,
    #[serde(default)] pub note: Option<String>,
    #[serde(default)] pub retry_after_s: Option<u32>,
}

// ------------------------------------------------------------------ page classification
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PageClass {
    Pdf { junk_prefix: u32 },
    Landing,
    Html,
    Redirect { next: String },
    ReaderShell { rewrite: Option<String> },
    Challenge { vendor: Vendor, interactive: bool },
    Blocked { vendor: Vendor },
    RateLimited { retry_after_s: Option<u32> },
    NotFound,
    AccessDenied,
    EzproxyNeedLogin,
    EzproxyHostNotConfigured,
    EzproxyError { suspend: bool },
    IdpLogin { idp: Idp, ca_error: Option<String> },
    SamlAutoPost,
    OpenathensWait,
    OpenathensUnknownResource,
    OpenathensBadOrg,
    PublisherSsoHop,
    LicenceStop,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageIds {
    #[serde(default)] pub doi: Option<String>,
    #[serde(default)] pub pii: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejected { pub url: String, pub why: String }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageInspection {
    pub class: PageClass,
    #[serde(default)] pub page_ids: PageIds,
    #[serde(default)] pub doi_match: Option<bool>,
    #[serde(default)] pub access: AccessHint,
    #[serde(default)] pub candidates: Vec<Candidate>,
    #[serde(default)] pub rejected: Vec<Rejected>,
    #[serde(default)] pub rule: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfVerdict {
    pub is_pdf: bool,
    pub junk_prefix: u32,   // bytes before "%PDF-" (0 = clean)
    pub has_eof: bool,      // "%%EOF" in the tail
    pub encrypted: bool,
    pub looks_html: bool,
    pub len: u64,
}

/// What the pure instance returns for any response (JS never interprets it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "expect", rename_all = "snake_case")]
pub enum Inspection {
    Api(ApiInspection),
    Page(PageInspection),
    Pdf(PdfVerdict),
    Probe { class: PageClass },
    Failed { reason: String },
    None,
}

/// What the engine asks the pure instance to do with a response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "expect", rename_all = "snake_case")]
pub enum InspectSpec {
    Api { source: Source, requested: Ids },
    Page { requested: Ids, route: Route },
    Pdf { requested: Ids, route: Route },
    Probe { route: Route },
    None,
}

// ------------------------------------------------------------------ outcomes and reports
plain_enum!(SkipReason { HasPdf, PdfNotDownloaded, ReadOnly, NotRegular, Feed, InProgressElsewhere, PdfAddedMeanwhile });
plain_enum!(NotFoundReason { NoOaCopy, OaOnly, NotHeld, Timeout, NoPdfLink, AccessDenied });
plain_enum!(NeedsWhy {
    LoginDeclined, LoginTimeout, LoginRefused, LoginIneffective, Misconfigured, Challenge, NoPdfLink,
    EngineRestarted, HandoffRequested, GroupLibrary, NetworkDown
});
plain_enum!(CapKind { Hourly, Daily, MbPerHour, Batch });
plain_enum!(CancelledBy { User, ItemDeleted, Shutdown, ParentGone });

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub redirector_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub ezproxy_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub libkey_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub doi_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Saved { route: Route, source: String, version: Version, url: String, attachment_key: String },
    Skipped { reason: SkipReason },
    NoIdentifier,
    NotFound { reason: NotFoundReason },
    NotEntitled { handoff: Handoff },
    NeedsUser { why: NeedsWhy, handoff: Handoff },
    Blocked { host: String, handoff: Handoff },
    Deferred { cap: CapKind },
    Halted { handoff: Handoff },
    Cancelled { by: CancelledBy },
    Failed { code: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStep { pub src: String, pub tier: String, pub class: String, pub ms: u32 }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemReport { pub item: ItemId, pub key: String, pub outcome: Outcome, #[serde(default)] pub trace: Vec<TraceStep> }
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportCounts { pub saved: u32, pub skipped: u32, pub not_found: u32, pub needs_user: u32, pub blocked: u32, pub deferred: u32, pub cancelled: u32, pub failed: u32 }
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerUsage { pub hour: u32, pub hour_cap: u32, pub day: u32, pub day_cap: u32, pub mb_hour: u32, pub mb_hour_cap: u32 }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub run: RunId,
    pub counts: ReportCounts,
    pub items: Vec<ItemReport>,
    pub ledger: LedgerUsage,
    #[serde(default)] pub halted: Option<String>,
    #[serde(default)] pub engine_restarts: u32,
}

/// Response facts JS gives the pure instance next to the body bytes (fx_inspect meta.resp).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RespMeta {
    pub status: u16,
    pub final_url: String,
    #[serde(default)] pub headers: BTreeMap<String, String>, // lower-case names
    #[serde(default)] pub body_len: u64,                      // full length of the response body
    #[serde(default)] pub head_len: Option<u32>,              // set when body = head ++ tail (binary staging)
    #[serde(default)] pub truncated: bool,                    // text body was cut at 2 MiB
}
/// fx_inspect meta.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InspectMeta { pub spec: InspectSpec, pub resp: RespMeta }

/// login_state() result for the login/challenge window poll loop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoginState { Pending, ChallengeShown, Landed, Refused { code: Option<String> } }

// ------------------------------------------------------------------ persisted engine state (state.json "engine" part)
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation { pub id: u64, pub wall_ms: u64, pub bytes: u64 }
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerState {
    #[serde(default)] pub committed: Vec<(u64, u64)>,   // (wall_ms, bytes), newest last, pruned to 24 h
    #[serde(default)] pub reserved: Vec<Reservation>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaltState {
    #[serde(default)] pub inst_until: u64,              // wall_ms; 0 = not halted
    #[serde(default)] pub reason: Option<String>,
    #[serde(default)] pub hosts: BTreeMap<String, u64>, // gate_key -> blocked-until wall_ms
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConf { pub configured: bool, pub at: u64 }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlmaEntry { pub held: bool, pub at: u64 }
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Persisted {
    #[serde(default)] pub ledger: LedgerState,
    #[serde(default)] pub halts: HaltState,
    #[serde(default)] pub ezproxy: BTreeMap<String, HostConf>,
    #[serde(default)] pub alma: BTreeMap<String, AlmaEntry>,
}
