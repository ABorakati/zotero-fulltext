//! fulltext-engine::proto - the JS <-> engine message protocol (protocol version 1).
//! Same derive conventions as model.rs. All JSON keys are snake_case.
use crate::model::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROTOCOL: u32 = 1;

macro_rules! plain_enum {
    ($(#[$m:meta])* $name:ident { $($v:ident),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($v),+ }
    };
}

// ------------------------------------------------------------------ input
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StepIn {
    pub now: u64,                       // monotonic ms (JS clock), never decreases
    pub wall: u64,                      // Date.now()
    #[serde(default)] pub commands: Vec<Command>,
    #[serde(default)] pub completions: Vec<CompletionMsg>,
}

plain_enum!(Mode { Full, OaOnly });
plain_enum!(UserActionKind { SignInNow, SkipSignIn, KeepWaiting, RetrySignIn });

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResumeHint { pub item: ItemId, pub inst_blocked: bool }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Configure { protocol: u32, config: Box<EngineConfig>, persisted: Persisted, seed: u64 },
    StartRun {
        run: RunId, mode: Mode, interactive: bool,
        items: Vec<ItemSnapshot>,
        #[serde(default)] resume: Vec<ResumeHint>,
    },
    CancelRun { run: RunId },
    CancelItem { run: RunId, item: ItemId },
    UserAction { run: RunId, kind: UserActionKind },
    DumpState,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompletionMsg {
    pub op: OpId,
    #[serde(default)] pub ok: Option<Value>,
    #[serde(default)] pub err: Option<OpErr>,
}
plain_enum!(OpErrKind {
    Timeout, Network, Cancelled, TooLarge, Tls, Unsupported, Offline,
    AlreadyHasPdf, NotEditable, ParentGone, Io, Internal
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpErr {
    pub kind: OpErrKind,
    #[serde(default)] pub message: String,
    #[serde(default)] pub after_ms: Option<u32>,
    #[serde(default)] pub status: Option<u16>,
}

// ------------------------------------------------------------------ configuration (from prefs, JS builds it)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OaConfig {
    pub openalex: bool, pub openalex_key: String,
    pub europepmc: bool, pub biorxiv: bool, pub pmc_s3: bool,
    pub europepmc_get_pdf: bool, pub medrxiv_early: bool, pub arxiv: bool,
    pub zotero_oa: bool,
    pub unpaywall_email: String, pub semantic_scholar_key: String, pub core_key: String,
    pub version_order: Vec<Version>,
}
plain_enum!(LoginMode { Window, Handoff });
plain_enum!(GroupPolicy { Ask, Never, Allow });
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstPrefs {
    pub enabled: bool, pub try_direct: bool, pub use_openathens: bool, pub use_ezproxy: bool, pub check_alma: bool,
    pub login_mode: LoginMode, pub auto_open_login: bool, pub login_timeout_sec: u32,
    pub group_libraries: GroupPolicy,
    pub confirm_above: u32, pub batch_max: u32,
    pub hourly_cap: u32, pub daily_cap: u32, pub mb_per_hour: u32, pub min_gap_sec: u32,
    pub challenge_windows: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPrefs { pub item_concurrency: u32, pub hedge: bool, pub pdf_max_mb: u32 }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ezproxy { pub login_host: String, pub suffix: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alma { pub base: String, pub inst: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Institution {
    pub name: String,
    pub openathens_domain: String,
    #[serde(default)] pub ezproxy: Option<Ezproxy>,
    #[serde(default)] pub ezproxy_hosts: Vec<String>,
    #[serde(default)] pub alma: Option<Alma>,
    #[serde(default)] pub libkey_library_id: Option<u32>,
    #[serde(default)] pub idp_hosts: Vec<String>,
    #[serde(default)] pub entra_tenant: Option<String>,
    #[serde(default)] pub passthrough_hosts: Vec<String>,
    #[serde(default)] pub warmup_url: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineConfig {
    pub oa: OaConfig,
    pub inst: InstPrefs,
    pub run: RunPrefs,
    pub institution: Institution,
    #[serde(default)] pub endpoint_map: Vec<(String, String)>, // [real_origin, fake_origin], harness only
    #[serde(default)] pub trace: bool,
}

// ------------------------------------------------------------------ output
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepOut {
    pub effects: Vec<Effect>,
    pub next_timer: Option<u64>,        // earliest virtual deadline (JS monotonic ms), if any
    pub more: bool,                     // poll budget exhausted: call fx_step again soon
    pub ignored: Vec<OpId>,             // completions for unknown/cancelled ops
    pub stats: Stats,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub tasks: u32, pub ops_inflight: u32, pub timers: u32, pub waiting_user: u32,
    pub polls: u32, pub lost_wakes: u32, pub stalled: bool, pub mem_hint_kb: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyRef { pub handle: u32, pub len: u64 }

plain_enum!(Retain { None, Page, Pdf });
plain_enum!(Method { Get, Head, Post });
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpReq {
    pub method: Method,
    pub url: String,
    #[serde(default)] pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub body: Option<String>,
    pub ua: Ua,
    pub timeout_ms: u32,
    pub follow_redirects: bool,
    pub max_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserReq {
    pub url: String,
    pub ua: Ua,
    pub capture_pdf: bool,
    pub want_page: bool,             // read documentHTML (PageData) when no PDF was captured
    pub settle_ms: u32,              // URL stable this long => settled
    pub timeout_ms: u32,
    #[serde(default)] pub hold_while: Vec<UrlClassKind>, // do not treat as settled while url class is in this set
}
plain_enum!(LoginKind { Login, Challenge });
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginReq {
    pub kind: LoginKind,
    pub realm: Realm,
    pub entry_url: String,
    pub item_title: String,
    pub waiting: u32,
    pub timeout_ms: u32,
    pub capture_pdf: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReq {
    pub item: ItemId,
    pub body: BodyRef,
    pub trim_prefix: u32,
    pub version: Version,
    pub url_to_store: String,
    pub route: Route,
    pub source: String,
}
plain_enum!(ConfirmKind { LargeBatch, GroupLibrary });
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmReq { pub kind: ConfirmKind, pub n: u32, pub cap: u32, pub library: String }

plain_enum!(RowState { Queued, Working, Waiting, Done, Attention, Failed, Skipped, Cancelled });
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowAction {
    OpenBrowser { url: String, l10n: String },
    AttachFile,
    SignIn,
    Retry,
    CancelItem,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressRow {
    pub state: RowState,
    pub l10n: String,
    #[serde(default)] pub args: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub route: Option<Route>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub actions: Vec<RowAction>,
}
plain_enum!(PersistKey { Ledger, Halts, Ezproxy, Alma });
plain_enum!(LogLevel { Debug, Info, Warn, Error });

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Effect {
    Http {
        op: OpId, run: RunId, item: ItemId, route: Route,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")] barrier: bool,
        req: HttpReq, retain: Retain, inspect: InspectSpec,
    },
    BrowserLoad {
        op: OpId, run: RunId, item: ItemId, route: Route,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")] barrier: bool,
        req: BrowserReq, inspect: InspectSpec,
    },
    LoginWindow { op: OpId, run: RunId, item: ItemId, req: LoginReq },
    LoginUpdate { op: OpId, waiting: u32 },
    RunTranslators { op: OpId, run: RunId, item: ItemId, page: BodyRef, url: String },
    ZoteroOa { op: OpId, run: RunId, item: ItemId, doi: String },
    SavePdf { op: OpId, run: RunId, item: ItemId, req: SaveReq },
    Confirm { op: OpId, run: RunId, req: ConfirmReq },
    Cancel { op: OpId },
    Release { handle: u32 },
    Progress { run: RunId, item: ItemId, row: ProgressRow },
    Persist { key: PersistKey, value: Value, flush: bool },
    RunFinished { run: RunId, report: RunReport },
    Log { level: LogLevel, msg: String },
    Trace { run: RunId, item: ItemId, ev: Value },
}

// ------------------------------------------------------------------ completion payloads (the `ok` value per effect type)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpOk {
    pub status: u16,
    pub final_url: String,
    #[serde(default)] pub headers: BTreeMap<String, String>, // lower-case names; whitelist in DESIGN.md
    pub bytes: u64,
    pub ms: u32,
    #[serde(default)] pub body: Option<BodyRef>,
    pub inspect: Inspection,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrowserOk {
    pub final_url: String,
    #[serde(default)] pub status: Option<u16>,
    pub ms: u32,
    #[serde(default)] pub pdf: Option<BodyRef>,
    #[serde(default)] pub page: Option<BodyRef>,
    #[serde(default)] pub url_trail: Vec<String>,   // hosts+paths visited, query stripped, max 12
    pub inspect: Inspection,
}
plain_enum!(LoginResult { Landed, Pdf, ClosedByUser, Handoff, Timeout, CaBlocked });
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoginOk {
    pub result: LoginResult,
    pub final_url: String,
    #[serde(default)] pub ca_error: Option<String>,
    #[serde(default)] pub pdf: Option<BodyRef>,
    #[serde(default)] pub page: Option<BodyRef>,
    pub inspect: Inspection,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslatorHit { pub url: String, pub mime: String, #[serde(default)] pub title: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslatorsOk { pub found: Option<TranslatorHit>, pub ms: u32 }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoteroOaHit { pub url: String, #[serde(default)] pub page_url: Option<String>, #[serde(default)] pub version: Option<String> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveOk { pub attachment_key: String, pub filename: String }
plain_enum!(ConfirmChoice { Yes, OaOnly, Cancel });
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmOk { pub choice: ConfirmChoice }

/// Result envelope of every wasm JSON export.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Envelope<T> {
    Ok(T),
    Err(ErrBody),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrBody { pub code: String, pub message: String }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn step_in_start_run() {
        let j = json!({
          "now": 52311, "wall": 1792496400123u64,
          "commands": [{
            "type": "start_run", "run": 3, "mode": "full", "interactive": true,
            "items": [{
              "item": 1201, "key": "1/Q7ZK2M4P",
              "library": {"id": 1, "kind": "user", "files_editable": true, "name": "My Library"},
              "regular": true, "feed": false, "item_type": "journalArticle",
              "title": "Cold-stored whole blood in trauma resuscitation",
              "fields": {"doi": "10.1097/SLA.0000000000005603", "pmid": "", "pmcid": "", "url": "", "archive_id": "", "extra": ""},
              "pdf_attachments": 0, "best_attachment": {"kind": "none", "exists": null}
            }]
          }],
          "completions": [
            {"op": 118, "ok": {"status": 200, "final_url": "https://api.openalex.org/works/doi:10.1371/journal.pmed.1000097",
              "headers": {"content-type": "application/json"}, "bytes": 48211, "ms": 640, "body": null,
              "inspect": {"expect": "api", "source": "openalex", "status": "found",
                "learned": {"pmcid": "PMC2707599", "pmid": "19621072"},
                "candidates": [{"url": "https://journals.plos.org/plosmedicine/article?id=10.1371/journal.pmed.1000097",
                  "kind": "landing", "source": "openalex", "version": "published", "license": "cc-by",
                  "host_kind": "publisher", "record_doi": "10.1371/journal.pmed.1000097"}]}}},
            {"op": 121, "err": {"kind": "timeout", "after_ms": 9000}}
          ]
        });
        let s: StepIn = serde_json::from_value(j).unwrap();
        assert_eq!(s.commands.len(), 1);
        let ok: HttpOk = serde_json::from_value(s.completions[0].ok.clone().unwrap()).unwrap();
        assert!(matches!(ok.inspect, Inspection::Api(_)));
        assert_eq!(s.completions[1].err.as_ref().unwrap().kind, OpErrKind::Timeout);
    }

    #[test]
    fn step_out_roundtrip() {
        let out = StepOut {
            effects: vec![
                Effect::Progress { run: 3, item: 1201, row: ProgressRow {
                    state: RowState::Working, l10n: "zotero-fulltext-status-searching-oa".into(),
                    args: BTreeMap::new(), route: Some(Route::Oa), actions: vec![] } },
                Effect::Http { op: 122, run: 3, item: 1201, route: Route::Oa, barrier: false,
                    req: HttpReq { method: Method::Get, url: "https://pmc-oa-opendata.s3.amazonaws.com/metadata/PMC2707599.1.json".into(),
                        headers: BTreeMap::from([("Accept".into(), "application/json".into())]), body: None, ua: Ua::Default,
                        timeout_ms: 11000, follow_redirects: true, max_bytes: 1_048_576 },
                    retain: Retain::None,
                    inspect: InspectSpec::Api { source: Source::PmcS3, requested: Ids::default() } },
                Effect::Cancel { op: 117 },
            ],
            next_timer: Some(53311), more: false, ignored: vec![], stats: Stats::default(),
        };
        let s = serde_json::to_string(&Envelope::Ok(&out)).unwrap();
        assert!(s.starts_with("{\"ok\":{"));
        let back: Envelope<StepOut> = serde_json::from_str(&s).unwrap();
        assert_eq!(back, Envelope::Ok(out));
        println!("{s}");
    }

    #[test]
    fn effect_examples_parse() {
        let browser = json!({"type":"browser_load","op":131,"run":3,"item":1201,"route":"inst","barrier":true,
          "req":{"url":"https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1097%2FSLA.0000000000005603",
            "ua":"plain","capture_pdf":true,"want_page":true,"settle_ms":3000,"timeout_ms":60000,
            "hold_while":["openathens_redirector"]},
          "inspect":{"expect":"page","requested":{"doi":"10.1097/sla.0000000000005603","rxiv":false},"route":"inst"}});
        let e: Effect = serde_json::from_value(browser).unwrap();
        assert!(matches!(e, Effect::BrowserLoad { barrier: true, .. }));
        let login = json!({"type":"login_window","op":132,"run":3,"item":1201,
          "req":{"kind":"login","realm":"openathens","entry_url":"https://go.openathens.net/redirector/kcl.ac.uk?url=x",
            "item_title":"Cold-stored whole blood","waiting":3,"timeout_ms":300000,"capture_pdf":true}});
        assert!(matches!(serde_json::from_value::<Effect>(login).unwrap(), Effect::LoginWindow { .. }));
        let save = json!({"type":"save_pdf","op":140,"run":3,"item":1201,"req":{"item":1201,"body":{"handle":9,"len":1834567},
          "trim_prefix":0,"version":"published","url_to_store":"https://doi.org/10.1097/SLA.0000000000005603","route":"inst","source":"lww"}});
        assert!(matches!(serde_json::from_value::<Effect>(save).unwrap(), Effect::SavePdf { .. }));
        let confirm = json!({"type":"confirm","op":5,"run":3,"req":{"kind":"large_batch","n":34,"cap":20,"library":""}});
        assert!(matches!(serde_json::from_value::<Effect>(confirm).unwrap(), Effect::Confirm { .. }));
        let persist = json!({"type":"persist","key":"ledger","value":{"committed":[[1792500000000u64,1834567]],"reserved":[]},"flush":true});
        assert!(matches!(serde_json::from_value::<Effect>(persist).unwrap(), Effect::Persist { flush: true, .. }));
        let bok = json!({"final_url":"https://journals.lww.com/annalsofsurgery/fulltext/2022/09000/x.4.aspx","ms":8100,
          "pdf":{"handle":9,"len":1834567},"url_trail":["go.openathens.net/redirector/kcl.ac.uk","journals.lww.com/annalsofsurgery/fulltext"],
          "inspect":{"expect":"pdf","is_pdf":true,"junk_prefix":0,"has_eof":true,"encrypted":false,"looks_html":false,"len":1834567}});
        let b: BrowserOk = serde_json::from_value(bok).unwrap();
        assert!(matches!(b.inspect, Inspection::Pdf(_)));
        let lok = json!({"result":"landed","final_url":"https://journals.lww.com/x","inspect":{"expect":"none"}});
        let l: LoginOk = serde_json::from_value(lok).unwrap();
        assert_eq!(l.result, LoginResult::Landed);
        let meta = json!({"spec":{"expect":"page","requested":{"doi":"10.1097/sla.0000000000005603"},"route":"oa"},
          "resp":{"status":200,"final_url":"https://journals.lww.com/annalsofsurgery/fulltext/x.aspx",
            "headers":{"content-type":"text/html; charset=utf-8","server":"cloudflare"},"body_len":231044}});
        let m: InspectMeta = serde_json::from_value(meta).unwrap();
        assert_eq!(m.resp.body_len, 231044);
        let cfg = json!({"type":"configure","protocol":1,"seed":2718281828u64,"persisted":{},
          "config":{"oa":{"openalex":true,"openalex_key":"","europepmc":true,"biorxiv":true,"pmc_s3":true,
            "europepmc_get_pdf":true,"medrxiv_early":true,"arxiv":true,"zotero_oa":true,"unpaywall_email":"",
            "semantic_scholar_key":"","core_key":"","version_order":["published","accepted","submitted"]},
           "inst":{"enabled":true,"try_direct":true,"use_openathens":true,"use_ezproxy":true,"check_alma":true,
            "login_mode":"window","auto_open_login":true,"login_timeout_sec":300,"group_libraries":"ask",
            "confirm_above":10,"batch_max":20,"hourly_cap":30,"daily_cap":80,"mb_per_hour":300,"min_gap_sec":5,"challenge_windows":true},
           "run":{"item_concurrency":4,"hedge":true,"pdf_max_mb":100},
           "institution":{"name":"King's College London","openathens_domain":"kcl.ac.uk",
             "ezproxy":{"login_host":"login.kcl.idm.oclc.org","suffix":"kcl.idm.oclc.org"},
             "ezproxy_hosts":["doi.org","www.bmj.com"],"alma":{"base":"https://librarysearch.kcl.ac.uk","inst":"44KCL_INST"},
             "libkey_library_id":286,"idp_hosts":["login.microsoftonline.com"],
             "entra_tenant":"8370cf14-16f3-4c16-b83c-724071654356","passthrough_hosts":["journals.lww.com"],"warmup_url":""}}});
        let c: Command = serde_json::from_value(cfg).unwrap();
        assert!(matches!(c, Command::Configure { protocol: 1, .. }));
        let done = json!({"type":"run_finished","run":3,"report":{"run":3,"counts":{"saved":7,"skipped":3,"not_found":1,"needs_user":2,"blocked":0,"deferred":0,"cancelled":0,"failed":0},
          "items":[{"item":1201,"key":"1/Q7ZK2M4P","outcome":{"kind":"saved","route":"oa","source":"pmc_s3","version":"published","url":"https://doi.org/10.1371/x","attachment_key":"1/ABCD1234"},
            "trace":[{"src":"openalex","tier":"api","class":"found","ms":512}]}],
          "ledger":{"hour":0,"hour_cap":30,"day":0,"day_cap":80,"mb_hour":0,"mb_hour_cap":300}}});
        assert!(matches!(serde_json::from_value::<Effect>(done).unwrap(), Effect::RunFinished { .. }));
        let ls = serde_json::to_value(&LoginState::Refused { code: Some("AADSTS53000".into()) }).unwrap();
        assert_eq!(ls, json!({"state":"refused","code":"AADSTS53000"}));
    }

    #[test]
    fn outcome_and_inspection_shapes() {
        let o = Outcome::NeedsUser { why: NeedsWhy::LoginDeclined, handoff: Handoff {
            redirector_url: Some("https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646".into()),
            libkey_url: Some("https://libkey.io/libraries/286/10.1002/bjs.11646".into()), ..Default::default() } };
        println!("{}", serde_json::to_string(&o).unwrap());
        let p = Inspection::Page(PageInspection {
            class: PageClass::Challenge { vendor: Vendor::Cloudflare, interactive: false },
            page_ids: PageIds::default(), doi_match: None, access: AccessHint::Unknown,
            candidates: vec![], rejected: vec![], rule: None });
        println!("{}", serde_json::to_string(&p).unwrap());
        let j = serde_json::to_value(&Inspection::None).unwrap();
        assert_eq!(j, json!({"expect": "none"}));
        let c = serde_json::to_value(&PageClass::Landing).unwrap();
        assert_eq!(c, json!({"kind": "landing"}));
        let r = serde_json::to_value(&Inspection::Probe { class: PageClass::OpenathensWait }).unwrap();
        assert_eq!(r, json!({"expect": "probe", "class": {"kind": "openathens_wait"}}));
    }
}
