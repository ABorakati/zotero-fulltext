//! Builders shared by all engine tests: configs, item snapshots, canned completion payloads.
use crate::proto::*;
use crate::sim::{PendingOp, Reply, Responder};
use fulltext_core::model::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// KCL-like config with every route on and the licence defaults from research/04 (matches the JSON example in
/// proto.rs tests).
pub(crate) fn test_config() -> EngineConfig {
    EngineConfig {
        oa: OaConfig {
            openalex: true,
            openalex_key: String::new(),
            europepmc: true,
            biorxiv: true,
            pmc_s3: true,
            europepmc_get_pdf: true,
            medrxiv_early: true,
            arxiv: true,
            zotero_oa: true,
            unpaywall_email: String::new(),
            semantic_scholar_key: String::new(),
            core_key: String::new(),
            version_order: vec![Version::Published, Version::Accepted, Version::Submitted],
        },
        inst: InstPrefs {
            enabled: true,
            try_direct: true,
            use_openathens: true,
            use_ezproxy: true,
            check_alma: true,
            login_mode: LoginMode::Window,
            auto_open_login: true,
            login_timeout_sec: 300,
            group_libraries: GroupPolicy::Ask,
            confirm_above: 10,
            batch_max: 20,
            hourly_cap: 30,
            daily_cap: 80,
            mb_per_hour: 300,
            min_gap_sec: 5,
            challenge_windows: true,
        },
        run: RunPrefs {
            item_concurrency: 4,
            hedge: true,
            pdf_max_mb: 100,
        },
        institution: Institution {
            name: "King's College London".into(),
            openathens_domain: "kcl.ac.uk".into(),
            ezproxy: Some(Ezproxy {
                login_host: "login.kcl.idm.oclc.org".into(),
                suffix: "kcl.idm.oclc.org".into(),
            }),
            ezproxy_hosts: vec!["doi.org".into(), "www.bmj.com".into()],
            alma: Some(Alma {
                base: "https://librarysearch.kcl.ac.uk".into(),
                inst: "44KCL_INST".into(),
            }),
            libkey_library_id: Some(286),
            idp_hosts: vec!["login.microsoftonline.com".into()],
            entra_tenant: Some("8370cf14-16f3-4c16-b83c-724071654356".into()),
            passthrough_hosts: vec!["journals.lww.com".into()],
            warmup_url: String::new(),
        },
        endpoint_map: Vec::new(),
        trace: true,
    }
}

/// A regular journal article in the user library without any attachment.
pub(crate) fn item(id: ItemId, doi: &str) -> ItemSnapshot {
    ItemSnapshot {
        item: id,
        key: format!("1/ITEM{id:04}"),
        library: LibraryInfo {
            id: 1,
            kind: LibraryKind::User,
            files_editable: true,
            name: "My Library".into(),
        },
        regular: true,
        feed: false,
        item_type: "journalArticle".into(),
        title: format!("Article {id}"),
        fields: ItemFields {
            doi: doi.to_owned(),
            ..ItemFields::default()
        },
        pdf_attachments: 0,
        best_attachment: BestAttachment {
            kind: AttachKind::None,
            exists: None,
        },
    }
}

/// `HttpOk` JSON for a response with the given inspection.
pub(crate) fn http_ok(
    status: u16,
    final_url: &str,
    inspect: &Inspection,
    body: Option<BodyRef>,
) -> Value {
    let ok = HttpOk {
        status,
        final_url: final_url.to_owned(),
        headers: BTreeMap::new(),
        bytes: body.map(|b| b.len).unwrap_or(0),
        ms: 50,
        body,
        inspect: inspect.clone(),
    };
    serde_json::to_value(ok).unwrap_or(Value::Null)
}

/// `BrowserOk` JSON: the page the hidden browser ended on, with the PDF and/or page body it captured.
pub(crate) fn browser_ok(
    final_url: &str,
    inspect: &Inspection,
    pdf: Option<BodyRef>,
    page: Option<BodyRef>,
) -> Value {
    let ok = BrowserOk {
        final_url: final_url.to_owned(),
        status: Some(200),
        ms: 800,
        pdf,
        page,
        url_trail: vec![final_url.to_owned()],
        inspect: inspect.clone(),
    };
    serde_json::to_value(ok).unwrap_or(Value::Null)
}

/// `LoginOk` JSON: how a login / challenge window ended.
pub(crate) fn login_ok(
    result: LoginResult,
    final_url: &str,
    inspect: &Inspection,
    pdf: Option<BodyRef>,
    page: Option<BodyRef>,
) -> Value {
    let ok = LoginOk {
        result,
        final_url: final_url.to_owned(),
        ca_error: None,
        pdf,
        page,
        inspect: inspect.clone(),
    };
    serde_json::to_value(ok).unwrap_or(Value::Null)
}

/// `TranslatorsOk` JSON; `found` is the `(url, mime)` of the attachment the translators discovered, if any.
pub(crate) fn translators_ok(found: Option<(&str, &str)>) -> Value {
    let found = found.map(|(url, mime)| TranslatorHit {
        url: url.to_owned(),
        mime: mime.to_owned(),
        title: String::new(),
    });
    serde_json::to_value(TranslatorsOk { found, ms: 120 }).unwrap_or(Value::Null)
}

/// A verified-PDF inspection.
pub(crate) fn pdf_inspection(len: u64) -> Inspection {
    Inspection::Pdf(PdfVerdict {
        is_pdf: true,
        junk_prefix: 0,
        has_eof: true,
        encrypted: false,
        looks_html: false,
        len,
    })
}

/// An API answer with candidates (`status: found`, nothing learned).
pub(crate) fn api_found(source: Source, candidates: Vec<Candidate>) -> Inspection {
    Inspection::Api(ApiInspection {
        source,
        status: ApiStatus::Found,
        learned: LearnedIds::default(),
        candidates,
        oa_status: None,
        note: None,
        retry_after_s: None,
    })
}

/// A page inspection of the given class with candidates (DOI matches, access unknown).
pub(crate) fn page_inspection(class: PageClass, candidates: Vec<Candidate>) -> Inspection {
    Inspection::Page(PageInspection {
        class,
        page_ids: PageIds::default(),
        doi_match: Some(true),
        access: AccessHint::Unknown,
        candidates,
        rejected: Vec::new(),
        rule: None,
    })
}

/// A body handle JS retains.
pub(crate) fn body(handle: u32, len: u64) -> BodyRef {
    BodyRef { handle, len }
}

/// A ranked candidate (published version, publisher host, `score` 0).
pub(crate) fn candidate(url: &str, kind: CandKind, source: Source, tier: u8) -> Candidate {
    Candidate {
        url: url.to_owned(),
        kind,
        source,
        version: Version::Published,
        license: None,
        host_kind: HostKind::Publisher,
        record_doi: None,
        landing_url: None,
        signal: None,
        score: 0,
        tier,
        needs_browser: false,
    }
}

/// A plain GET with the default UA (what the pipeline sends first).
pub(crate) fn http_req(url: &str) -> HttpReq {
    HttpReq {
        method: Method::Get,
        url: url.to_owned(),
        headers: BTreeMap::new(),
        body: None,
        ua: Ua::Default,
        timeout_ms: 10_000,
        follow_redirects: true,
        max_bytes: 1_048_576,
    }
}

/// A hidden-browser load that captures PDF or page and settles after 3 s.
pub(crate) fn browser_req(url: &str) -> BrowserReq {
    BrowserReq {
        url: url.to_owned(),
        ua: Ua::Plain,
        capture_pdf: true,
        want_page: true,
        settle_ms: 3_000,
        timeout_ms: 60_000,
        hold_while: vec![UrlClassKind::OpenathensRedirector],
    }
}

/// A login window request for the OpenAthens realm (five-minute timeout, one waiting item).
pub(crate) fn login_req(entry_url: &str) -> LoginReq {
    LoginReq {
        kind: LoginKind::Login,
        realm: Realm::Openathens,
        entry_url: entry_url.to_owned(),
        item_title: "Article".to_owned(),
        waiting: 1,
        timeout_ms: 300_000,
        capture_pdf: true,
    }
}

/// `OpErr` shorthand.
pub(crate) fn op_err(kind: OpErrKind) -> OpErr {
    OpErr {
        kind,
        message: String::new(),
        after_ms: None,
        status: None,
    }
}

/// `SaveOk` JSON.
pub(crate) fn save_ok(key: &str) -> Value {
    json!({ "attachment_key": key, "filename": "Article.pdf" })
}

// ------------------------------------------------------------------------------------------ scripted answers

/// The URL an effect is about (`Http`, `BrowserLoad`, `LoginWindow.entry_url`, `RunTranslators`), if any.
pub(crate) fn effect_url(e: &Effect) -> Option<&str> {
    match e {
        Effect::Http { req, .. } => Some(&req.url),
        Effect::BrowserLoad { req, .. } => Some(&req.url),
        Effect::LoginWindow { req, .. } => Some(&req.entry_url),
        Effect::RunTranslators { url, .. } => Some(url),
        _ => None,
    }
}

/// Matcher: the effect's URL contains `needle`.
pub(crate) fn url_has(needle: &'static str) -> impl Fn(&Effect) -> bool + 'static {
    move |e| effect_url(e).is_some_and(|u| u.contains(needle))
}

/// Matcher: any effect of the given wire type (`"http"`, `"browser_load"`, `"login_window"`, `"save_pdf"`, ...).
pub(crate) fn is_type(ty: &'static str) -> impl Fn(&Effect) -> bool + 'static {
    move |e| {
        serde_json::to_value(e)
            .ok()
            .and_then(|v| v.get("type").and_then(Value::as_str).map(|t| t == ty))
            .unwrap_or(false)
    }
}

struct Rule {
    matcher: Box<dyn Fn(&Effect) -> bool>,
    /// Answers in order; the last one repeats.
    replies: Vec<Reply>,
    used: usize,
}

/// A `Responder` made of rules: the first rule whose matcher accepts the effect answers (replies are consumed
/// in order, the last repeats); no match -> the fallback (default: network error after 10 ms).
pub(crate) struct ScriptResponder {
    rules: Vec<Rule>,
    fallback: Reply,
}

impl ScriptResponder {
    pub(crate) fn new() -> Self {
        Self {
            rules: Vec::new(),
            fallback: Reply::Err {
                after_ms: 10,
                err: op_err(OpErrKind::Network),
            },
        }
    }

    /// Answer matching effects with `reply` (every time).
    pub(crate) fn on(self, matcher: impl Fn(&Effect) -> bool + 'static, reply: Reply) -> Self {
        self.on_seq(matcher, vec![reply])
    }

    /// Answer matching effects with `replies` in order (the last repeats).
    pub(crate) fn on_seq(
        mut self,
        matcher: impl Fn(&Effect) -> bool + 'static,
        replies: Vec<Reply>,
    ) -> Self {
        self.rules.push(Rule {
            matcher: Box::new(matcher),
            replies,
            used: 0,
        });
        self
    }

    pub(crate) fn fallback(mut self, reply: Reply) -> Self {
        self.fallback = reply;
        self
    }
}

impl Responder for ScriptResponder {
    fn respond(&mut self, op: &PendingOp, _now: u64, _wall: u64) -> Reply {
        for rule in &mut self.rules {
            if (rule.matcher)(&op.effect) {
                let at = rule.used.min(rule.replies.len().saturating_sub(1));
                rule.used += 1;
                if let Some(r) = rule.replies.get(at) {
                    return r.clone();
                }
            }
        }
        self.fallback.clone()
    }
}

impl Reply {
    /// Complete with `value` after `after_ms`.
    pub(crate) fn ok(after_ms: u64, value: Value) -> Reply {
        Reply::Ok { after_ms, value }
    }

    /// Complete with an error of `kind` after `after_ms`.
    pub(crate) fn err(after_ms: u64, kind: OpErrKind) -> Reply {
        Reply::Err {
            after_ms,
            err: op_err(kind),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_effect(url: &str) -> Effect {
        Effect::Http {
            op: 1,
            run: 1,
            item: 1,
            route: Route::Oa,
            barrier: false,
            req: http_req(url),
            retain: Retain::None,
            inspect: InspectSpec::None,
        }
    }

    fn pending(effect: Effect) -> PendingOp {
        PendingOp {
            effect,
            issued_at: 0,
        }
    }

    #[test]
    fn completion_builders_round_trip_through_the_wire_types() {
        let pdf = pdf_inspection(1234);
        let h: HttpOk =
            serde_json::from_value(http_ok(200, "https://x/a.pdf", &pdf, Some(body(3, 1234))))
                .expect("HttpOk");
        assert_eq!(
            (h.status, h.bytes, h.body),
            (
                200,
                1234,
                Some(BodyRef {
                    handle: 3,
                    len: 1234
                })
            )
        );
        assert_eq!(h.inspect, pdf);

        let b: BrowserOk = serde_json::from_value(browser_ok(
            "https://x/b",
            &Inspection::None,
            Some(body(4, 9)),
            None,
        ))
        .expect("BrowserOk");
        assert_eq!(
            (b.final_url.as_str(), b.pdf, b.page),
            ("https://x/b", Some(body(4, 9)), None)
        );

        let l: LoginOk = serde_json::from_value(login_ok(
            LoginResult::Landed,
            "https://x/c",
            &Inspection::None,
            None,
            Some(body(5, 7)),
        ))
        .expect("LoginOk");
        assert_eq!((l.result, l.page), (LoginResult::Landed, Some(body(5, 7))));

        let t: TranslatorsOk =
            serde_json::from_value(translators_ok(Some(("https://x/t.pdf", "application/pdf"))))
                .expect("TranslatorsOk");
        assert_eq!(t.found.map(|f| f.mime), Some("application/pdf".to_owned()));
        let none: TranslatorsOk =
            serde_json::from_value(translators_ok(None)).expect("TranslatorsOk");
        assert!(none.found.is_none());

        let s: SaveOk = serde_json::from_value(save_ok("1/ABCD")).expect("SaveOk");
        assert_eq!(s.attachment_key, "1/ABCD");
    }

    #[test]
    fn inspection_builders_make_what_the_pipeline_expects() {
        let c = candidate("https://x/a.pdf", CandKind::Pdf, Source::PmcS3, 1);
        assert_eq!(
            (c.tier, c.kind, c.source, c.version),
            (1, CandKind::Pdf, Source::PmcS3, Version::Published)
        );
        match api_found(Source::Openalex, vec![c.clone()]) {
            Inspection::Api(a) => assert_eq!(
                (a.source, a.status, a.candidates),
                (Source::Openalex, ApiStatus::Found, vec![c.clone()])
            ),
            other => panic!("{other:?}"),
        }
        match page_inspection(PageClass::Landing, vec![c.clone()]) {
            Inspection::Page(p) => assert_eq!(
                (p.class, p.candidates.len(), p.doi_match),
                (PageClass::Landing, 1, Some(true))
            ),
            other => panic!("{other:?}"),
        }
        // They survive the JSON the host attaches to completions.
        let j = serde_json::to_value(api_found(Source::Doi, vec![c])).expect("json");
        assert_eq!(j["expect"], "api");
    }

    #[test]
    fn request_builders_are_sane() {
        let h = http_req("https://example.org/x");
        assert_eq!(
            (h.method, h.ua, h.follow_redirects),
            (Method::Get, Ua::Default, true)
        );
        let b = browser_req("https://example.org/x");
        assert!(b.capture_pdf && b.want_page);
        let l = login_req("https://go.openathens.net/redirector/kcl.ac.uk");
        assert_eq!(
            (l.kind, l.realm, l.waiting),
            (LoginKind::Login, Realm::Openathens, 1)
        );
        assert_eq!(item(7, "10.1/x").fields.doi, "10.1/x");
        assert_eq!(item(7, "10.1/x").key, "1/ITEM0007");
        assert!(test_config().inst.enabled);
    }

    #[test]
    fn matchers_look_at_url_and_wire_type() {
        let e = http_effect("https://api.openalex.org/works/doi:10.1/x");
        assert_eq!(
            effect_url(&e),
            Some("https://api.openalex.org/works/doi:10.1/x")
        );
        assert!(url_has("openalex")(&e) && !url_has("europepmc")(&e));
        assert!(is_type("http")(&e) && !is_type("browser_load")(&e));
        assert_eq!(effect_url(&Effect::Cancel { op: 1 }), None);
        assert!(!url_has("x")(&Effect::Release { handle: 1 }));
        assert!(is_type("cancel")(&Effect::Cancel { op: 1 }));
    }

    #[test]
    fn script_responder_picks_the_first_matching_rule_and_walks_its_replies() {
        let mut r = ScriptResponder::new()
            .on_seq(
                url_has("openalex"),
                vec![
                    Reply::err(1, OpErrKind::Timeout),
                    Reply::ok(2, json!("second")),
                ],
            )
            .on(is_type("http"), Reply::ok(3, json!("any http")))
            .fallback(Reply::Never);
        let oa = pending(http_effect("https://api.openalex.org/works/x"));
        let other = pending(http_effect("https://example.org/"));
        let save = pending(Effect::Cancel { op: 1 });

        assert!(matches!(
            r.respond(&oa, 0, 0),
            Reply::Err { after_ms: 1, .. }
        ));
        assert!(matches!(
            r.respond(&oa, 0, 0),
            Reply::Ok { after_ms: 2, .. }
        ));
        assert!(
            matches!(r.respond(&oa, 0, 0), Reply::Ok { after_ms: 2, .. }),
            "the last reply repeats"
        );
        assert!(
            matches!(r.respond(&other, 0, 0), Reply::Ok { after_ms: 3, .. }),
            "falls through to the next rule"
        );
        assert!(
            matches!(r.respond(&save, 0, 0), Reply::Never),
            "no rule: the fallback"
        );
    }

    #[test]
    fn default_fallback_is_a_fast_network_error() {
        let mut r = ScriptResponder::new();
        match r.respond(&pending(http_effect("https://example.org/")), 0, 0) {
            Reply::Err { after_ms, err } => {
                assert_eq!((after_ms, err.kind), (10, OpErrKind::Network))
            }
            other => panic!("{other:?}"),
        }
    }
}
