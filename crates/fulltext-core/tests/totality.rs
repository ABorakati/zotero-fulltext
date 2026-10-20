//! No panic, ever (`docs/wp/core-integration.md` section 2).
//!
//! The shipped wasm is built with `panic = "abort"`: one panic in `inspect` or `login_state` traps the instance and
//! the JS host has to throw it away. So every public entry point must be total. This file feeds them everything the
//! recorded fixtures can be turned into (truncated, bit-flipped, cut, filled, re-encoded), response metadata that no
//! server would send, and hostile synthetic bodies, and requires for every call that it
//!
//! 1. does not panic,
//! 2. returns a value that serialises to JSON and parses back equal, and
//! 3. finishes within [`LIMIT`] (debug build).
//!
//! The mutation streams come from a seeded xorshift generator, so a failure names a case that can be replayed.
//! Mutated bodies run through a sample of the specs (the natural one for the fixture plus random others); the
//! original bodies run through every spec.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;

use fulltext_core::inspect::{inspect, login_state};
use fulltext_core::model::{
    Ids, InspectMeta, InspectSpec, Inspection, LoginState, RespMeta, Route, Source,
};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Per call, debug build.
const LIMIT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------------- generator

/// xorshift64: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

// ---------------------------------------------------------------------------------------------------- specs

/// Every `Source`; the exhaustive `match` below fails to compile when a variant is added without being listed here.
const ALL_SOURCES: [Source; 16] = [
    Source::Openalex,
    Source::Europepmc,
    Source::Biorxiv,
    Source::PmcS3,
    Source::Arxiv,
    Source::Unpaywall,
    Source::ZoteroOa,
    Source::SemanticScholar,
    Source::Core,
    Source::Alma,
    Source::ItemUrl,
    Source::Doi,
    Source::Translator,
    Source::Extractor,
    Source::Redirector,
    Source::Ezproxy,
];

fn source_index(s: Source) -> usize {
    match s {
        Source::Openalex => 0,
        Source::Europepmc => 1,
        Source::Biorxiv => 2,
        Source::PmcS3 => 3,
        Source::Arxiv => 4,
        Source::Unpaywall => 5,
        Source::ZoteroOa => 6,
        Source::SemanticScholar => 7,
        Source::Core => 8,
        Source::Alma => 9,
        Source::ItemUrl => 10,
        Source::Doi => 11,
        Source::Translator => 12,
        Source::Extractor => 13,
        Source::Redirector => 14,
        Source::Ezproxy => 15,
    }
}

#[test]
fn all_sources_are_listed() {
    for (i, s) in ALL_SOURCES.iter().enumerate() {
        assert_eq!(source_index(*s), i);
    }
}

fn populated_ids() -> Ids {
    Ids {
        doi: Some("10.1371/journal.pmed.1000097".into()),
        pmid: Some("19621072".into()),
        pmcid: Some("PMC2707599".into()),
        arxiv: Some("1706.03762".into()),
        rxiv: true,
        url: Some("https://doi.org/10.1371/journal.pmed.1000097".into()),
        sources: BTreeMap::from([("doi".to_owned(), "field".to_owned())]),
    }
}

/// Identifiers no normaliser would produce: the engine validates them, but the pure instance must not rely on that.
fn hostile_ids() -> Ids {
    Ids {
        doi: Some("\u{0}10.1/\u{202e}x".repeat(5000)),
        pmid: Some("９９９".into()),
        pmcid: Some(String::new()),
        arxiv: Some("../../etc/passwd".into()),
        rxiv: false,
        url: Some("not a url".into()),
        sources: BTreeMap::from([(String::new(), "\u{0}".to_owned())]),
    }
}

/// The 24 specs: 16 `Api` sources, `Page` / `Pdf` / `Probe` on both routes with different requests, and `None`.
fn all_specs() -> Vec<(String, InspectSpec)> {
    let mut specs: Vec<(String, InspectSpec)> = ALL_SOURCES
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let requested = if i % 2 == 0 {
                Ids::default()
            } else {
                populated_ids()
            };
            (
                format!("api/{s:?}"),
                InspectSpec::Api {
                    source: *s,
                    requested,
                },
            )
        })
        .collect();
    specs.extend([
        (
            "page/oa".to_owned(),
            InspectSpec::Page {
                requested: populated_ids(),
                route: Route::Oa,
            },
        ),
        (
            "page/inst".to_owned(),
            InspectSpec::Page {
                requested: Ids::default(),
                route: Route::Inst,
            },
        ),
        (
            "page/hostile-ids".to_owned(),
            InspectSpec::Page {
                requested: hostile_ids(),
                route: Route::Oa,
            },
        ),
        (
            "pdf/oa".to_owned(),
            InspectSpec::Pdf {
                requested: Ids::default(),
                route: Route::Oa,
            },
        ),
        (
            "pdf/inst".to_owned(),
            InspectSpec::Pdf {
                requested: populated_ids(),
                route: Route::Inst,
            },
        ),
        (
            "probe/oa".to_owned(),
            InspectSpec::Probe { route: Route::Oa },
        ),
        (
            "probe/inst".to_owned(),
            InspectSpec::Probe { route: Route::Inst },
        ),
        ("none".to_owned(), InspectSpec::None),
    ]);
    specs
}

// ---------------------------------------------------------------------------------------------------- checking

/// What a call may take for an input of `len` bytes: [`LIMIT`] flat, plus a linear allowance for the part of the body
/// the analysis reads (it never reads past the 2 MiB text cap).
///
/// The allowance exists because of a debug-build artefact, not a defect: html5gum's tokenizer is generic code that is
/// compiled with this crate at opt-level 0, and it costs 1 to 2 microseconds per token on pathological input. Measured
/// alone on this machine, 2 MiB of `<` (two million error tokens) or of NULs takes 3 to 5 s in a debug build and
/// 0.2 to 0.35 s in the release profile that ships; 200 000 nested `<div>` take 1 s and 60 ms. Under the load of
/// parallel test threads (and other builds on the same machine) the debug numbers grow four- to five-fold (up to
/// 10 microseconds per byte were seen), hence 20 microseconds per byte. Fixtures and their mutations are far below
/// 256 KiB and so keep the strict 2 s; a super-linear blow-up on a big body would still exceed any linear allowance
/// by orders of magnitude, which is what this check is for.
fn budget(len: usize) -> Duration {
    const TEXT_CAP: usize = 2 * 1024 * 1024;
    const PER_BYTE: Duration = Duration::from_nanos(20_000);
    let big = len.saturating_sub(256 * 1024).min(TEXT_CAP);
    LIMIT + PER_BYTE * u32::try_from(big).unwrap_or(u32::MAX)
}

#[derive(Default)]
struct Stats {
    calls: usize,
    worst: Duration,
    worst_name: String,
    /// Calls over their [`budget`]; collected so one run lists every offender instead of stopping at the first.
    slow: Vec<String>,
}

impl Stats {
    fn note(&mut self, name: &str, body_len: usize, took: Duration) {
        self.calls += 1;
        if took > self.worst {
            self.worst = took;
            self.worst_name = format!("{name} (body {body_len} bytes)");
        }
        if took >= budget(body_len) {
            self.slow.push(format!(
                "{name} (body {body_len} bytes): {took:?}, budget {:?}",
                budget(body_len)
            ));
        }
    }

    fn merge(&mut self, other: Stats) {
        self.calls += other.calls;
        if other.worst > self.worst {
            self.worst = other.worst;
            self.worst_name = other.worst_name;
        }
        self.slow.extend(other.slow);
    }

    /// Print the summary and fail if any call was over its time budget.
    fn report(&self, what: &str) {
        println!(
            "{what}: {} calls, slowest {:?} ({})",
            self.calls, self.worst, self.worst_name
        );
        assert!(
            self.slow.is_empty(),
            "{what}: {} call(s) over budget:\n  {}",
            self.slow.len(),
            self.slow.join("\n  ")
        );
    }
}

/// Run `work` over `items` on up to 6 threads, each with its own [`Stats`]; a panic in any worker fails the test with
/// that worker's message.
fn parallel<T: Sync>(items: &[T], work: impl Fn(&T, &mut Stats) + Sync) -> Stats {
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .clamp(1, 6)
        .min(items.len().max(1));
    let parts: Vec<Stats> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut stats = Stats::default();
                    while let Some(item) = items.get(next.fetch_add(1, Ordering::SeqCst)) {
                        work(item, &mut stats);
                    }
                    stats
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    let mut all = Stats::default();
    for part in parts {
        all.merge(part);
    }
    all
}

/// One call under all three requirements.
fn check(stats: &mut Stats, name: &str, spec: &InspectSpec, resp: &RespMeta, body: &[u8]) {
    let meta = InspectMeta {
        spec: spec.clone(),
        resp: resp.clone(),
    };
    let started = Instant::now();
    let answer = catch_unwind(AssertUnwindSafe(|| inspect(&meta, body)));
    let took = started.elapsed();
    let answer = match answer {
        Ok(a) => a,
        Err(_) => panic!(
            "PANIC in inspect: {name} (body {} bytes, status {}, url {:.80})",
            body.len(),
            resp.status,
            resp.final_url
        ),
    };
    stats.note(name, body.len(), took);
    let json = serde_json::to_vec(&answer)
        .unwrap_or_else(|e| panic!("{name}: answer does not serialise: {e}"));
    let back: Inspection = serde_json::from_slice(&json)
        .unwrap_or_else(|e| panic!("{name}: answer does not parse back: {e}"));
    assert_eq!(back, answer, "{name}: answer changed on the wire");
}

fn check_login(stats: &mut Stats, name: &str, url: &str, html: Option<&str>) {
    let started = Instant::now();
    let state = catch_unwind(AssertUnwindSafe(|| login_state(url, html)));
    let took = started.elapsed();
    let state = match state {
        Ok(s) => s,
        Err(_) => panic!("PANIC in login_state: {name}"),
    };
    stats.note(name, html.map_or(0, str::len).max(url.len()), took);
    let json = serde_json::to_string(&state).unwrap();
    assert_eq!(
        serde_json::from_str::<LoginState>(&json).unwrap(),
        state,
        "{name}"
    );
}

// ---------------------------------------------------------------------------------------------------- cases

struct Case {
    name: String,
    resp: RespMeta,
    body: Vec<u8>,
    /// The specs this input is really meant for (always run, also on mutations).
    natural: Vec<(String, InspectSpec)>,
}

fn named<'a>(specs: &'a [(String, InspectSpec)], name: &str) -> &'a (String, InspectSpec) {
    specs
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no spec {name}"))
}

fn html_resp(url: &str) -> RespMeta {
    RespMeta {
        status: 200,
        final_url: url.to_owned(),
        headers: BTreeMap::from([(
            "content-type".to_owned(),
            "text/html; charset=utf-8".to_owned(),
        )]),
        ..Default::default()
    }
}

/// Recorded pages: the 31 of `fixtures/html`, the three OA-side walls, and the hand-made pages of `tests/data/page`
/// each served from the hosts whose rules they exercise.
fn html_cases() -> Vec<Case> {
    let specs = all_specs();
    let natural: Vec<(String, InspectSpec)> = ["page/oa", "pdf/inst", "probe/oa"]
        .iter()
        .map(|n| named(&specs, n).clone())
        .collect();
    let mut files: Vec<String> = common::html_expected()
        .iter()
        .map(|e| e["file"].as_str().unwrap().to_owned())
        .collect();
    files.extend(
        [
            "pmc-web__pdf-pow-interstitial.html",
            "europepmc-web__pdf-render-cloudflare-challenge.html",
            "medrxiv-web__pdf-403-block.html",
        ]
        .map(String::from),
    );
    let mut cases: Vec<Case> = files
        .iter()
        .map(|f| {
            let (resp, body) = common::html_fixture(f);
            Case {
                name: f.clone(),
                resp,
                body,
                natural: natural.clone(),
            }
        })
        .collect();
    let dir = common::root().join("crates/fulltext-core/tests/data/page");
    let hosts = [
        "https://example.org/x",
        "https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/oauth2/v2.0/authorize",
        "https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2F10.1%2Fx",
        "https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1%2Fx",
    ];
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    assert!(names.len() >= 14, "{} hand-made pages", names.len());
    for path in names {
        let body = std::fs::read(&path).unwrap();
        for host in hosts {
            cases.push(Case {
                name: format!("{}@{}", path.file_name().unwrap().to_string_lossy(), host),
                resp: html_resp(host),
                body: body.clone(),
                natural: natural.clone(),
            });
        }
    }
    cases
}

fn source_of(file: &str) -> Source {
    match file.split("__").next().unwrap_or_default() {
        "openalex" | "openalex-content" => Source::Openalex,
        "europepmc" | "europepmc-web" => Source::Europepmc,
        "biorxiv-api" | "medrxiv-web" => Source::Biorxiv,
        "pmc-s3" | "ncbi-idconv" | "ncbi-pmc-oa" | "pmc-web" => Source::PmcS3,
        "arxiv" => Source::Arxiv,
        "unpaywall" => Source::Unpaywall,
        "semanticscholar" => Source::SemanticScholar,
        "core" | "doaj" => Source::Core,
        "zotero-oa" => Source::ZoteroOa,
        _ => Source::Doi,
    }
}

/// Every file of `fixtures/oa/manifest.json` (also the ones no parser owns: they must still not hurt any spec),
/// plus the hand-made Alma responses.
fn oa_cases() -> Vec<Case> {
    let specs = all_specs();
    let api = |s: Source| named(&specs, &format!("api/{s:?}")).clone();
    let mut cases: Vec<Case> = common::oa_manifest()
        .iter()
        .map(|e| {
            let file = e["file"].as_str().unwrap().to_owned();
            let (resp, body) = common::oa_fixture(&file);
            let natural = vec![api(source_of(&file))];
            Case {
                name: file,
                resp,
                body,
                natural,
            }
        })
        .collect();
    assert!(cases.len() >= 75, "{} OA fixtures", cases.len());
    let dir = common::root().join("crates/fulltext-core/tests/data/oa");
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    for path in names {
        let body = std::fs::read(&path).unwrap();
        let mut resp = html_resp("https://librarysearch.kcl.ac.uk/view/delivery/44KCL_INST/cto");
        resp.headers
            .insert("content-type".to_owned(), "application/xml".to_owned());
        cases.push(Case {
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            resp,
            body,
            natural: vec![api(Source::Alma)],
        });
    }
    cases
}

// ---------------------------------------------------------------------------------------------------- body mutations

/// The mutated bodies of the brief: truncations, 10 single-byte flips, 10 range deletions, three fills, UTF-16LE.
fn body_mutations(body: &[u8], rng: &mut Rng) -> Vec<(String, Vec<u8>)> {
    let len = body.len();
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for n in [0, 1, 7, 100, len / 2, len.saturating_sub(1)] {
        out.push((format!("truncated to {n}"), body[..n.min(len)].to_vec()));
    }
    const INTERESTING: [u8; 12] = [
        0, b'<', b'>', b'"', b'\'', b'&', b'/', b'=', b'{', 0xff, 0x80, 0xc3,
    ];
    for i in 0..10 {
        let mut b = body.to_vec();
        if !b.is_empty() {
            let at = rng.below(b.len());
            let old = b[at];
            b[at] = if rng.below(2) == 0 {
                *rng.pick(&INTERESTING)
            } else {
                old ^ (1 + rng.below(255) as u8)
            };
            out.push((format!("flip #{i} at {at}"), b));
        }
    }
    for i in 0..10 {
        let mut b = body.to_vec();
        if !b.is_empty() {
            let start = rng.below(b.len());
            let n = 1 + rng.below((b.len() - start).clamp(1, (len / 2).max(1)));
            b.drain(start..(start + n).min(len));
            out.push((format!("delete #{i} {start}+{n}"), b));
        }
    }
    for (label, fill) in [("NUL", 0u8), ("0xFF", 0xff), ("<", b'<')] {
        out.push((format!("all {label}"), vec![fill; len.max(1)]));
        let mut half = body.to_vec();
        for x in half.iter_mut().skip(len / 2) {
            *x = fill;
        }
        out.push((format!("{label} from the middle"), half));
    }
    let mut utf16 = vec![0xff, 0xfe];
    for unit in String::from_utf8_lossy(body).encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    out.push(("UTF-16LE with BOM".to_owned(), utf16));
    out
}

/// Run `case` and its mutations: the original through every spec, each mutation through the natural specs plus
/// `extra` random ones.
fn run_bodies(case: &Case, specs: &[(String, InspectSpec)], extra: usize, stats: &mut Stats) {
    let mut rng = Rng::new(hash(&case.name));
    for (spec_name, spec) in specs {
        check(
            stats,
            &format!("{} | {spec_name} | original", case.name),
            spec,
            &case.resp,
            &case.body,
        );
    }
    for (label, body) in body_mutations(&case.body, &mut rng) {
        let mut chosen: Vec<&(String, InspectSpec)> = case.natural.iter().collect();
        for _ in 0..extra {
            chosen.push(rng.pick(specs));
        }
        for (spec_name, spec) in chosen {
            let mut resp = case.resp.clone();
            resp.body_len = body.len() as u64;
            check(
                stats,
                &format!("{} | {spec_name} | {label}", case.name),
                spec,
                &resp,
                &body,
            );
        }
    }
}

#[test]
fn recorded_pages_survive_every_body_mutation() {
    let specs = all_specs();
    parallel(&html_cases(), |case, stats| {
        run_bodies(case, &specs, 2, stats)
    })
    .report("html bodies");
}

#[test]
fn recorded_api_responses_survive_every_body_mutation() {
    let specs = all_specs();
    parallel(&oa_cases(), |case, stats| {
        run_bodies(case, &specs, 2, stats)
    })
    .report("api bodies");
}

// ---------------------------------------------------------------------------------------------------- response mutations

fn huge(n: usize) -> String {
    "a".repeat(n)
}

/// Response metadata no sane server sends, each applied to the unchanged body.
fn resp_mutations(base: &RespMeta, body_len: usize) -> Vec<(String, RespMeta)> {
    let mut out: Vec<(String, RespMeta)> = Vec::new();
    let mut with = |label: String, f: &dyn Fn(&mut RespMeta)| {
        let mut r = base.clone();
        f(&mut r);
        out.push((label, r));
    };
    for status in [
        0u16,
        99,
        100,
        199,
        204,
        301,
        304,
        399,
        400,
        451,
        599,
        999,
        u16::MAX,
    ] {
        with(format!("status {status}"), &|r| r.status = status);
    }
    let long_path = format!("https://example.org/{}", huge(100_000));
    let long_host = format!("https://{}com/", "a.".repeat(50_000));
    let many_labels = format!("https://{}/", vec!["x"; 200].join("."));
    for url in [
        "",
        "not a url",
        "::::",
        "http://",
        "https://[",
        "https://[::1",
        "javascript:alert(1)",
        "data:text/html,<p>x",
        "file:///etc/passwd",
        "ftp://example.org/x",
        "https://\u{0}/",
        "https://exa mple.org/",
        "https://example.org:99999/",
        "HTTPS://EXAMPLE.ORG/%00%ff",
        "https://xn--/",
        "https://Xe\u{e9}.com/",
        "https://X\u{e9}.com/",
        "https://\u{fffd}.example/",
        "//example.org/x",
        "/relative/only",
        "https://login.kcl.idm.oclc.org",
        "https://login.kcl.idm.oclc.org/login?url=%ff",
        long_path.as_str(),
        long_host.as_str(),
        many_labels.as_str(),
    ] {
        with(format!("final_url {:.40}", url), &|r| {
            r.final_url = url.to_owned()
        });
    }
    for n in [
        0u64,
        1,
        (body_len / 2) as u64,
        (body_len * 3 + 10) as u64,
        u64::from(u32::MAX),
        u64::MAX,
    ] {
        with(format!("body_len {n}"), &|r| r.body_len = n);
    }
    for h in [
        0u32,
        1,
        (body_len + 1) as u32,
        (body_len * 2) as u32,
        u32::MAX,
    ] {
        with(format!("head_len {h}"), &|r| r.head_len = Some(h));
    }
    with("truncated".to_owned(), &|r| r.truncated = true);
    with("head_len and truncated".to_owned(), &|r| {
        r.head_len = Some(4096);
        r.truncated = true;
        r.body_len = 10_000_000_000;
    });
    for name in [
        "content-type",
        "location",
        "server",
        "retry-after",
        "cf-mitigated",
        "akamai-grn",
        "content-length",
        "content-disposition",
        "x-anything",
    ] {
        with(format!("{name}: 100 KB"), &|r| {
            r.headers.insert(name.to_owned(), huge(100_000));
        });
        with(format!("{name}: control characters"), &|r| {
            r.headers.insert(
                name.to_owned(),
                "\u{0}\r\n\u{7f}\u{ff}%00 ;;; \"'<>".to_owned(),
            );
        });
        with(format!("{name}: empty"), &|r| {
            r.headers.insert(name.to_owned(), String::new());
        });
    }
    with("UPPER-CASE header names".to_owned(), &|r| {
        r.headers = r
            .headers
            .iter()
            .map(|(k, v)| (k.to_uppercase(), v.clone()))
            .collect();
        r.headers
            .insert("CF-MITIGATED".to_owned(), "challenge".to_owned());
        r.headers
            .insert("Content-Type".to_owned(), "application/pdf".to_owned());
    });
    with("10 000 headers".to_owned(), &|r| {
        for i in 0..10_000 {
            r.headers.insert(format!("x-h-{i}"), "v".to_owned());
        }
    });
    with("PDF content type, 3xx, Location 100 KB".to_owned(), &|r| {
        r.status = 302;
        r.headers
            .insert("content-type".to_owned(), "application/pdf".to_owned());
        r.headers
            .insert("location".to_owned(), format!("/{}", huge(100_000)));
    });
    with("Retry-After overflow".to_owned(), &|r| {
        r.status = 429;
        r.headers
            .insert("retry-after".to_owned(), "99999999999999999999".to_owned());
    });
    out
}

fn run_responses(case: &Case, specs: &[(String, InspectSpec)], stats: &mut Stats) {
    let mut rng = Rng::new(hash(&case.name) ^ 0xabcd);
    for (label, resp) in resp_mutations(&case.resp, case.body.len()) {
        let mut chosen: Vec<&(String, InspectSpec)> = case.natural.iter().collect();
        chosen.push(rng.pick(specs));
        for (spec_name, spec) in chosen {
            check(
                stats,
                &format!("{} | {spec_name} | {label}", case.name),
                spec,
                &resp,
                &case.body,
            );
        }
    }
}

#[test]
fn recorded_pages_survive_hostile_response_metadata() {
    let specs = all_specs();
    parallel(&html_cases(), |case, stats| {
        run_responses(case, &specs, stats)
    })
    .report("html metadata");
}

#[test]
fn recorded_api_responses_survive_hostile_response_metadata() {
    let specs = all_specs();
    parallel(&oa_cases(), |case, stats| {
        run_responses(case, &specs, stats)
    })
    .report("api metadata");
}

// ---------------------------------------------------------------------------------------------------- hostile bodies

const MIB: usize = 1024 * 1024;

fn hostile_bodies() -> Vec<(&'static str, Vec<u8>)> {
    let nested = format!("<html><body>{}", "<div>".repeat(200_000));
    let attr = format!(
        "<html><body><a href=\"{}\">x</a><meta name=\"citation_pdf_url\" content=\"{}\"></body></html>",
        "h".repeat(MIB),
        "c".repeat(MIB)
    );
    let mut surrogates = b"<html><body><p>".to_vec();
    for _ in 0..50_000 {
        // U+D800 encoded as UTF-8 (CESU-style): not valid UTF-8, a lone surrogate.
        surrogates.extend_from_slice(&[0xed, 0xa0, 0x80]);
        surrogates.extend_from_slice(&[0xed, 0xbf, 0xbf]);
    }
    vec![
        ("2 MiB of <", vec![b'<'; 2 * MIB]),
        (
            "2 MiB of <a href=",
            b"<a href=".iter().copied().cycle().take(2 * MIB).collect(),
        ),
        ("200 000 nested <div>", nested.into_bytes()),
        ("1 MiB attribute values", attr.into_bytes()),
        ("2 MiB of NULs", vec![0; 2 * MIB]),
        (
            "5 MiB body (over the 2 MiB text cap)",
            b"<p>text <a href=\"/x.pdf\">pdf</a></p>\n"
                .iter()
                .copied()
                .cycle()
                .take(5 * MIB)
                .collect(),
        ),
        ("lone surrogates as invalid UTF-8", surrogates),
        (
            "a million &amp; entities",
            "&amp;".repeat(1_000_000).into_bytes(),
        ),
        ("a million bare &", vec![b'&'; 1_000_000]),
        (
            "a million numeric entities",
            "&#x10FFFF;&#0;&#xD800;&#99999999;"
                .repeat(40_000)
                .into_bytes(),
        ),
        ("deep JSON arrays", "[".repeat(300_000).into_bytes()),
        ("deep JSON objects", "{\"a\":".repeat(100_000).into_bytes()),
        (
            "unterminated comment and tag",
            format!("<!--{}", "x".repeat(2 * MIB)).into_bytes(),
        ),
        (
            "unterminated script",
            format!("<script>{}", "if(a<b){}".repeat(200_000)).into_bytes(),
        ),
        (
            "meta refresh storm",
            "<meta http-equiv=\"refresh\" content=\"0;url=/a\">"
                .repeat(30_000)
                .into_bytes(),
        ),
        (
            "a million anchors",
            "<a href=\"/a.pdf\">PDF</a>".repeat(60_000).into_bytes(),
        ),
        (
            "PDF magic everywhere",
            "%PDF-%%EOF".repeat(200_000).into_bytes(),
        ),
        ("empty", Vec::new()),
    ]
}

/// Bodies above this size are tokenised for seconds in a debug build, so they get fewer spec / host combinations.
const HEAVY: usize = 512 * 1024;

#[test]
fn hostile_synthetic_bodies_are_survived_by_every_spec() {
    let specs = all_specs();
    let pick = |names: &[&str]| -> Vec<(String, InspectSpec)> {
        names.iter().map(|n| named(&specs, n).clone()).collect()
    };
    // Light bodies go through every spec; heavy ones (each page analysis of which takes seconds in a debug build)
    // through one spec per code path: JSON, XML, page analysis, classification only, none.
    let heavy_specs = pick(&["api/Openalex", "api/Alma", "page/oa", "probe/inst", "none"]);
    let hosts = [
        "https://login.kcl.idm.oclc.org/login",
        "https://login.microsoftonline.com/x/saml2",
        "https://www-nature-com.kcl.idm.oclc.org/articles/x",
    ];
    parallel(&hostile_bodies(), |(name, body), stats| {
        let heavy = body.len() > HEAVY;
        let resp = RespMeta {
            body_len: body.len() as u64,
            ..html_resp("https://pub.example.org/article/1")
        };
        for (spec_name, spec) in if heavy { &heavy_specs } else { &specs } {
            check(stats, &format!("{name} | {spec_name}"), spec, &resp, body);
        }
        // The same bytes pretending to be a PDF, staged as head ++ tail.
        let mut pdf = resp.clone();
        pdf.headers
            .insert("content-type".to_owned(), "application/pdf".to_owned());
        pdf.head_len = Some(u32::try_from(1024.min(body.len())).unwrap_or(0));
        for (spec_name, spec) in pick(&["probe/oa", "page/oa", "pdf/oa"])
            .iter()
            .take(if heavy { 1 } else { 3 })
        {
            check(
                stats,
                &format!("{name} | {spec_name} | as pdf"),
                spec,
                &pdf,
                body,
            );
        }
        // Behind the proxy and an IdP the classifier takes other branches (light bodies only).
        if !heavy {
            for url in hosts {
                let mut r = resp.clone();
                r.final_url = url.to_owned();
                for (spec_name, spec) in pick(&["page/inst", "probe/oa"]) {
                    check(
                        stats,
                        &format!("{name} | {spec_name} | {url}"),
                        &spec,
                        &r,
                        body,
                    );
                }
            }
        }
    })
    .report("hostile bodies");
}

#[test]
fn login_state_is_total_too() {
    let urls = [
        "",
        "https://login.microsoftonline.com/x",
        "https://go.openathens.net/redirector/kcl.ac.uk?url=%ff",
        "https://login.kcl.idm.oclc.org/login?qurl=",
        "not a url",
        "https://Xe\u{e9}.com/",
        "javascript:void(0)",
    ];
    let long_url = format!(
        "https://login.kcl.idm.oclc.org/login?qurl={}",
        "a".repeat(MIB)
    );
    let mut htmls: Vec<(String, String)> = vec![
        ("empty".into(), String::new()),
        ("lt".into(), "<".repeat(2 * MIB)),
        ("nested".into(), "<div>".repeat(200_000)),
        ("amp".into(), "&amp;".repeat(500_000)),
        ("nul".into(), "\u{0}".repeat(MIB)),
        ("non-ascii".into(), "日本語 💡 café ".repeat(100_000)),
        ("5 MiB".into(), "<p>x</p>".repeat(700_000)),
    ];
    for case in html_cases().into_iter().take(40) {
        htmls.push((case.name, String::from_utf8_lossy(&case.body).into_owned()));
    }
    let urls: Vec<String> = urls
        .iter()
        .map(|s| (*s).to_owned())
        .chain(std::iter::once(long_url))
        .collect();
    // One work item per document; the huge documents (seconds each in a debug build) see one SSO-looking URL, the rest all.
    parallel(&htmls, |(name, html), stats| {
        let heavy = html.len() > HEAVY;
        for url in
            urls.iter()
                .skip(if heavy { 1 } else { 0 })
                .take(if heavy { 1 } else { urls.len() })
        {
            check_login(stats, &format!("{:.40} | {name}", url), url, Some(html));
        }
    })
    .report("login_state with documents");
    parallel(&urls, |url, stats| {
        check_login(stats, &format!("{url:.40} | none"), url, None)
    })
    .report("login_state without document");
}

// ---------------------------------------------------------------------------------------------------- JSON robustness

/// A minimal valid `InspectMeta` in the shape JS sends it.
const VALID: &str = r#"{"spec":{"expect":"page","requested":{"rxiv":false},"route":"oa"},"resp":{"status":200,"final_url":"https://example.org/"}}"#;

fn parse(bytes: &[u8]) -> Result<InspectMeta, serde_json::Error> {
    catch_unwind(AssertUnwindSafe(|| {
        serde_json::from_slice::<InspectMeta>(bytes)
    }))
    .unwrap_or_else(|_| panic!("PANIC parsing {:.120}", String::from_utf8_lossy(bytes)))
}

#[test]
fn unknown_fields_are_ignored_everywhere() {
    let with_extras = r#"{
        "extra_top": [1,2,3],
        "spec": {"expect":"page","requested":{"rxiv":false,"doi":"10.1/x","future_field":{"a":1}},"route":"oa","hint":"x"},
        "resp": {"status":200,"final_url":"https://example.org/","headers":{"content-type":"text/html"},"body_len":5,
                 "head_len":null,"truncated":false,"tls":{"version":"1.3"},"timing":12.5}
    }"#;
    let meta =
        parse(with_extras.as_bytes()).unwrap_or_else(|e| panic!("unknown fields rejected: {e}"));
    assert_eq!(meta.resp.status, 200);
    assert_eq!(meta.resp.body_len, 5);
    match meta.spec {
        InspectSpec::Page { requested, route } => assert_eq!(
            (requested.doi.as_deref(), route),
            (Some("10.1/x"), Route::Oa)
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn missing_optional_fields_take_their_defaults() {
    let meta = parse(VALID.as_bytes()).unwrap();
    assert_eq!(
        meta.resp,
        RespMeta {
            status: 200,
            final_url: "https://example.org/".into(),
            ..Default::default()
        }
    );
    assert!(
        meta.resp.headers.is_empty()
            && meta.resp.head_len.is_none()
            && !meta.resp.truncated
            && meta.resp.body_len == 0
    );
    // Every `Ids` field is optional.
    match &meta.spec {
        InspectSpec::Page { requested, .. } => assert_eq!(requested, &Ids::default()),
        other => panic!("{other:?}"),
    }
    let empty_ids = r#"{"spec":{"expect":"api","source":"openalex","requested":{}},"resp":{"status":404,"final_url":""}}"#;
    assert!(matches!(
        parse(empty_ids.as_bytes()).unwrap().spec,
        InspectSpec::Api {
            source: Source::Openalex,
            ..
        }
    ));
    // Explicit nulls are the same as absent for optional fields.
    let nulls = r#"{"spec":{"expect":"probe","route":"inst"},"resp":{"status":200,"final_url":"x","head_len":null}}"#;
    assert!(parse(nulls.as_bytes()).is_ok());
    // The `none` spec needs nothing.
    assert!(parse(br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":""}}"#).is_ok());
}

#[test]
fn wrong_types_and_malformed_documents_are_errors_not_panics() {
    let bad: Vec<(&str, Vec<u8>)> = vec![
        ("empty input", Vec::new()),
        ("whitespace", b"   ".to_vec()),
        ("not json", b"{ not json".to_vec()),
        ("array", b"[]".to_vec()),
        ("null", b"null".to_vec()),
        ("number", b"42".to_vec()),
        ("string", br#""spec""#.to_vec()),
        ("invalid UTF-8", vec![0xff, 0xfe, 0xfd]),
        ("lone surrogate escape", br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":"\ud800"}}"#.to_vec()),
        ("trailing garbage", format!("{VALID} x").into_bytes()),
        ("truncated document", VALID.as_bytes()[..VALID.len() - 3].to_vec()),
        ("missing spec", br#"{"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("missing resp", br#"{"spec":{"expect":"none"}}"#.to_vec()),
        ("missing status", br#"{"spec":{"expect":"none"},"resp":{"final_url":""}}"#.to_vec()),
        ("missing final_url", br#"{"spec":{"expect":"none"},"resp":{"status":200}}"#.to_vec()),
        ("spec is a string", br#"{"spec":"page","resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("unknown expect tag", br#"{"spec":{"expect":"teleport"},"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("missing expect tag", br#"{"spec":{"route":"oa"},"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("page without route", br#"{"spec":{"expect":"page","requested":{}},"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("unknown route", br#"{"spec":{"expect":"probe","route":"nowhere"},"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("unknown source", br#"{"spec":{"expect":"api","source":"nope","requested":{}},"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("upper-case enum", br#"{"spec":{"expect":"probe","route":"OA"},"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("status as string", br#"{"spec":{"expect":"none"},"resp":{"status":"200","final_url":""}}"#.to_vec()),
        ("status too large", br#"{"spec":{"expect":"none"},"resp":{"status":70000,"final_url":""}}"#.to_vec()),
        ("status negative", br#"{"spec":{"expect":"none"},"resp":{"status":-1,"final_url":""}}"#.to_vec()),
        ("status float", br#"{"spec":{"expect":"none"},"resp":{"status":200.5,"final_url":""}}"#.to_vec()),
        ("final_url null", br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":null}}"#.to_vec()),
        ("final_url number", br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":5}}"#.to_vec()),
        ("headers as array", br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":"","headers":[]}}"#.to_vec()),
        ("header value as number", br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":"","headers":{"a":1}}}"#.to_vec()),
        ("body_len as string", br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":"","body_len":"5"}}"#.to_vec()),
        ("body_len negative", br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":"","body_len":-5}}"#.to_vec()),
        ("body_len beyond u64", br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":"","body_len":18446744073709551616}}"#.to_vec()),
        ("head_len beyond u32", br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":"","head_len":4294967296}}"#.to_vec()),
        ("truncated as string", br#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":"","truncated":"yes"}}"#.to_vec()),
        ("requested as string", br#"{"spec":{"expect":"page","requested":"10.1/x","route":"oa"},"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("doi as number", br#"{"spec":{"expect":"page","requested":{"doi":5},"route":"oa"},"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("rxiv as string", br#"{"spec":{"expect":"page","requested":{"rxiv":"no"},"route":"oa"},"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("sources as array", br#"{"spec":{"expect":"page","requested":{"sources":[]},"route":"oa"},"resp":{"status":200,"final_url":""}}"#.to_vec()),
        ("deeply nested arrays", "[".repeat(10_000).into_bytes()),
        ("deeply nested objects", "{\"a\":".repeat(10_000).into_bytes()),
        ("deep nesting inside a valid frame", format!(r#"{{"spec":{{"expect":"none"}},"resp":{{"status":200,"final_url":"","headers":{}}}}}"#, "[".repeat(5_000)).into_bytes()),
        ("1 MB of braces", vec![b'{'; MIB]),
        ("unterminated 1 MB string", [b"{\"spec\":\"".as_slice(), &vec![b'a'; MIB]].concat()),
    ];
    for (name, bytes) in bad {
        let started = Instant::now();
        let result = parse(&bytes);
        assert!(
            result.is_err(),
            "{name}: expected an error, got {:?}",
            result.map(|m| m.resp.status)
        );
        assert!(
            started.elapsed() < LIMIT,
            "{name}: parsing took {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn well_formed_oddities_are_accepted_and_inspected() {
    // Valid JSON, strange values: the parse succeeds and `inspect` still answers.
    let docs = [
        r#"{"spec":{"expect":"none"},"resp":{"status":0,"final_url":""}}"#,
        r#"{"spec":{"expect":"none"},"resp":{"status":65535,"final_url":"\u0000","body_len":18446744073709551615}}"#,
        r#"{"spec":{"expect":"probe","route":"inst"},"resp":{"status":200,"final_url":"https://é.example/","headers":{"":"","content-type":"\u0000"}}}"#,
        r#"{"spec":{"expect":"pdf","requested":{"doi":"","pmid":"x","sources":{"a":"b"}},"route":"oa"},"resp":{"status":200,"final_url":"x","head_len":4294967295}}"#,
        r#"{"spec":{"expect":"page","requested":{},"route":"oa"},"resp":{"status":200,"final_url":"https://example.org/","headers":{"content-type":"text/html","content-type":"application/pdf"}}}"#,
    ];
    let mut stats = Stats::default();
    for doc in docs {
        let meta = parse(doc.as_bytes()).unwrap_or_else(|e| panic!("{doc}: {e}"));
        for body in [
            &b""[..],
            b"<html></html>",
            b"%PDF-1.7\n%%EOF",
            b"\xff\xfe\x00",
        ] {
            check(&mut stats, doc, &meta.spec, &meta.resp, body);
        }
    }
    stats.report("odd but valid meta");
}
