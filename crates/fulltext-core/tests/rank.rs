//! Candidate merging, tiering, scoring and ordering (spec: docs/wp/core-ids-oa.md §3).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod common;

use fulltext_core::model::{ApiInspection, CandKind, Candidate, HostKind, Ids, Source, Version};
use fulltext_core::oa;
use fulltext_core::rank::{self, RankPrefs, merge, new_candidate, rank, score, tier};
use fulltext_core::urls::canonical_key;
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------------------------------- helpers

fn pdf(url: &str) -> Candidate {
    new_candidate(url, CandKind::Pdf, Source::Openalex)
}

fn landing(url: &str) -> Candidate {
    new_candidate(url, CandKind::Landing, Source::Openalex)
}

fn with(mut c: Candidate, f: impl FnOnce(&mut Candidate)) -> Candidate {
    f(&mut c);
    c
}

fn prefs() -> RankPrefs {
    RankPrefs::default()
}

fn requesting(doi: &str) -> RankPrefs {
    RankPrefs {
        requested: Ids {
            doi: Some(doi.to_owned()),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn urls(list: &[Candidate]) -> Vec<&str> {
    list.iter().map(|c| c.url.as_str()).collect()
}

fn position(list: &[Candidate], url: &str) -> usize {
    list.iter()
        .position(|c| c.url == url)
        .unwrap_or_else(|| panic!("{url} not in {:?}", urls(list)))
}

fn get<'a>(list: &'a [Candidate], url: &str) -> &'a Candidate {
    &list[position(list, url)]
}

fn parsed(source: Source, file: &str, requested: &Ids) -> ApiInspection {
    let (resp, body) = common::oa_fixture(file);
    oa::parse(source, &resp, &body, requested)
}

fn doi_request(doi: &str) -> Ids {
    Ids {
        doi: Some(doi.to_owned()),
        ..Default::default()
    }
}

/// Everything the OA parsers produce for the fixtures, as one pool (each fixture parsed for its own request).
fn big_pool() -> Vec<Candidate> {
    let gold = doi_request("10.1371/journal.pmed.1000097");
    let green = doi_request("10.1038/nature12373");
    let arxiv = Ids {
        doi: Some("10.48550/arxiv.1706.03762".into()),
        arxiv: Some("1706.03762".into()),
        ..Default::default()
    };
    let med = doi_request("10.1101/2023.05.31.23290770");
    let runs: &[(Source, &str, &Ids)] = &[
        (Source::Openalex, "openalex__gold.json", &gold),
        (Source::Openalex, "openalex__green.json", &green),
        (Source::Openalex, "openalex__arxiv.json", &arxiv),
        (Source::Openalex, "openalex__medrxiv.json", &med),
        (Source::Europepmc, "europepmc__gold.json", &gold),
        (Source::Europepmc, "europepmc__green.json", &green),
        (Source::Europepmc, "europepmc__medrxiv.json", &med),
        (Source::Biorxiv, "biorxiv-api__medrxiv-details.json", &med),
        (Source::PmcS3, "pmc-s3__meta-gold.json", &gold),
        (Source::PmcS3, "pmc-s3__list-PMC2707599.xml", &gold),
        (Source::Arxiv, "arxiv__export-api.xml", &arxiv),
        (Source::Unpaywall, "unpaywall__gold.json", &gold),
        (Source::Unpaywall, "unpaywall__green.json", &green),
        (Source::Unpaywall, "unpaywall__medrxiv.json", &med),
        (Source::SemanticScholar, "semanticscholar__gold.json", &gold),
        (
            Source::SemanticScholar,
            "semanticscholar__green.json",
            &green,
        ),
        (Source::Core, "core__output-green-no-key.json", &green),
        (Source::Core, "core__discover-gold-no-key.json", &gold),
        (Source::Doi, "crossref__green.json", &green),
        (Source::Doi, "crossref__gold.json", &gold),
        (Source::ZoteroOa, "zotero-oa__gold.json", &gold),
        (Source::ZoteroOa, "zotero-oa__green.json", &green),
    ];
    runs.iter()
        .flat_map(|(source, file, req)| parsed(*source, file, req).candidates)
        .collect()
}

/// A small deterministic PRNG (xorshift64*), so the shuffle test needs no dependency and never flakes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            items.swap(i, j);
        }
    }
}

// ---------------------------------------------------------------------------------------------------- tiers

#[test]
fn every_tier_constant_is_assigned_by_its_rule() {
    let publisher = |url: &str| with(pdf(url), |c| c.host_kind = HostKind::Publisher);
    let cases: Vec<(Candidate, u8)> = vec![
        (
            pdf("https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf"),
            tier::PMC_S3,
        ),
        (
            pdf("https://europepmc.org/api/getPdf?pmcid=PMC2707599"),
            tier::EUROPEPMC_GETPDF,
        ),
        (pdf("https://arxiv.org/pdf/1706.03762"), tier::ARXIV),
        (
            pdf(
                "https://www.medrxiv.org/content/medrxiv/early/2023/06/01/2023.05.31.23290770.full.pdf",
            ),
            tier::RXIV_EARLY,
        ),
        (
            pdf("https://www.biorxiv.org/content/10.1101/2020.01.01.123456v1.full.pdf"),
            tier::RXIV_EARLY,
        ),
        (
            with(pdf("https://mirror.example.org/x.pdf"), |c| {
                c.signal = Some("biorxiv:early_path".into())
            }),
            tier::RXIV_EARLY,
        ),
        (
            publisher("https://www.nature.com/articles/nature12373.pdf"),
            tier::DIRECT_PUBLISHER,
        ),
        (
            pdf("https://iris.unimore.it/bitstream/11380/1368022/2/document.pdf"),
            tier::DIRECT_REPOSITORY,
        ),
        (
            with(pdf("https://repository.example.edu/a.pdf"), |c| {
                c.host_kind = HostKind::Repository
            }),
            tier::DIRECT_REPOSITORY,
        ),
        (
            with(pdf("https://mirror.example.org/a.pdf"), |c| {
                c.host_kind = HostKind::Aggregator
            }),
            tier::DIRECT_REPOSITORY,
        ),
        (
            landing("https://doi.org/10.1371/journal.pmed.1000097"),
            tier::LANDING,
        ),
        (
            landing("https://www.medrxiv.org/content/10.1101/2023.05.31.23290770v1"),
            tier::LANDING,
        ),
        (
            with(pdf("https://walled.example.org/a.pdf"), |c| {
                c.needs_browser = true
            }),
            tier::BROWSER_ONLY,
        ),
        (
            pdf("https://pmc.ncbi.nlm.nih.gov/articles/PMC11156280/pdf/pone.0298402.pdf"),
            tier::BROWSER_ONLY,
        ),
        (
            pdf("https://europepmc.org/articles/PMC4221854?pdf=render"),
            tier::BROWSER_ONLY,
        ),
        (
            pdf("https://content.openalex.org/works/W2159974629.pdf"),
            tier::PAID_API,
        ),
        (
            with(pdf("https://example.org/paid.pdf"), |c| {
                c.signal = Some("openalex:content_api".into())
            }),
            tier::PAID_API,
        ),
    ];
    for (c, want) in cases {
        let url = c.url.clone();
        let out = rank(vec![c], &prefs());
        assert_eq!(out.len(), 1, "{url}");
        assert_eq!(out[0].tier, want, "{url}");
    }
    let all = [
        tier::PMC_S3,
        tier::EUROPEPMC_GETPDF,
        tier::ARXIV,
        tier::RXIV_EARLY,
        tier::DIRECT_PUBLISHER,
        tier::DIRECT_REPOSITORY,
        tier::LANDING,
        tier::BROWSER_ONLY,
        tier::PAID_API,
    ];
    assert_eq!(all, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
}

#[test]
fn tiers_order_the_attempts() {
    let pool = vec![
        pdf("https://content.openalex.org/works/W1.pdf"),
        with(pdf("https://x.example.org/walled.pdf"), |c| {
            c.needs_browser = true
        }),
        landing("https://doi.org/10.1/x"),
        pdf("https://iris.example.it/a.pdf"),
        with(pdf("https://www.nature.com/a.pdf"), |c| {
            c.host_kind = HostKind::Publisher
        }),
        pdf("https://www.medrxiv.org/content/medrxiv/early/2023/06/01/x.full.pdf"),
        pdf("https://arxiv.org/pdf/1706.03762"),
        pdf("https://europepmc.org/api/getPdf?pmcid=PMC1"),
        pdf("https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf"),
    ];
    let tiers: Vec<u8> = rank(pool, &prefs()).iter().map(|c| c.tier).collect();
    assert_eq!(tiers, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
}

#[test]
fn a_landing_page_never_gets_a_pdf_tier() {
    let out = rank(
        vec![
            landing("https://arxiv.org/list/cs/new"),
            landing("https://www.medrxiv.org/content/10.1101/x"),
            landing("https://europepmc.org/api/getPdf?pmcid=PMC1"),
        ],
        &prefs(),
    );
    let by_url: Vec<(&str, u8)> = out.iter().map(|c| (c.url.as_str(), c.tier)).collect();
    assert!(
        by_url
            .iter()
            .filter(|(u, _)| !u.contains("getPdf"))
            .all(|(_, t)| *t == tier::LANDING),
        "{by_url:?}"
    );
}

// ---------------------------------------------------------------------------------------------------- dedupe and aliases

#[test]
fn query_strings_distinguish_plos_pdf_variants() {
    let printable = "https://journals.plos.org/plosmedicine/article/file?id=10.1371/journal.pmed.1000097&type=printable";
    let manuscript = "https://journals.plos.org/plosmedicine/article/file?id=10.1371/journal.pmed.1000097&type=manuscript";
    let out = rank(
        vec![
            pdf(printable),
            pdf(manuscript),
            pdf(printable),
            pdf(&printable.replace("https", "http")),
        ],
        &prefs(),
    );
    assert_eq!(out.len(), 2, "{:?}", urls(&out));
    assert!(out.iter().any(|c| c.url == printable) && out.iter().any(|c| c.url == manuscript));
}

#[test]
fn http_and_https_copies_merge_and_the_better_fields_survive() {
    let mut a = pdf("http://repo.example.org/paper.pdf");
    a.version = Version::Submitted;
    let mut b = pdf("https://repo.example.org/paper.pdf#page=2");
    b.version = Version::Published;
    b.license = Some("cc-by".into());
    b.source = Source::Unpaywall;
    let out = rank(vec![a, b], &prefs());
    assert_eq!(out.len(), 1);
    let c = &out[0];
    assert_eq!(c.url, "https://repo.example.org/paper.pdf");
    assert_eq!(
        (c.version, c.license.as_deref()),
        (Version::Published, Some("cc-by"))
    );
}

#[test]
fn pmc_aliases_collapse_to_the_modern_article_url() {
    let article = "https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/";
    let out = rank(
        vec![
            landing("https://www.ncbi.nlm.nih.gov/pmc/articles/2707599"),
            landing("http://www.ncbi.nlm.nih.gov/pmc/articles/PMC2707599/"),
            landing("https://www.ncbi.nlm.nih.gov/pmc/articles/PMC2707599/?report=reader"),
            landing("https://pmc.ncbi.nlm.nih.gov/articles/2707599"),
            landing(article),
        ],
        &prefs(),
    );
    assert_eq!(urls(&out), [article]);
    assert_eq!(out[0].host_kind, HostKind::Pmc);
}

#[test]
fn pmc_pdf_paths_survive_the_alias() {
    let out = rank(
        vec![pdf(
            "https://www.ncbi.nlm.nih.gov/pmc/articles/PMC2707599/pdf/pmed.1000097.pdf",
        )],
        &prefs(),
    );
    assert_eq!(
        out[0].url,
        "https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/pdf/pmed.1000097.pdf"
    );
    assert_eq!(
        (out[0].tier, out[0].needs_browser),
        (tier::BROWSER_ONLY, true)
    );
}

#[test]
fn arxiv_abstract_pages_become_pdfs_and_versions_merge() {
    let out = rank(
        vec![
            landing("http://arxiv.org/abs/1706.03762"),
            pdf("https://arxiv.org/pdf/1706.03762"),
            landing("https://arxiv.org/pdf/1706.03762v5"),
            pdf("https://arxiv.org/pdf/1706.03762v7.pdf"),
            landing("https://arxiv.org/abs/hep-th/9901001v2"),
        ],
        &prefs(),
    );
    assert_eq!(
        urls(&out),
        [
            "https://arxiv.org/pdf/1706.03762",
            "https://arxiv.org/pdf/hep-th/9901001v2"
        ]
    );
    assert!(out.iter().all(|c| c.kind == CandKind::Pdf
        && c.tier == tier::ARXIV
        && c.host_kind == HostKind::Preprint));
    assert_eq!(
        out[0].landing_url.as_deref(),
        Some("https://arxiv.org/abs/1706.03762")
    );
}

#[test]
fn unusable_urls_are_dropped() {
    let junk = [
        "",
        "   ",
        "ftp://example.org/a.pdf",
        "javascript:void(0)",
        "mailto:a@b.c",
        "/relative/a.pdf",
        "a.pdf",
        "s3://pmc-oa-opendata/x.pdf",
    ];
    let mut pool: Vec<Candidate> = junk.iter().map(|u| pdf(u)).collect();
    pool.push(pdf("https://ok.example.org/a.pdf"));
    assert_eq!(
        urls(&rank(pool, &prefs())),
        ["https://ok.example.org/a.pdf"]
    );
    assert!(rank(Vec::new(), &prefs()).is_empty());
}

#[test]
fn merge_pools_by_canonical_key() {
    let mut first = pdf("https://a.example.org/x.pdf?id=1");
    first.version = Version::Submitted;
    first.tier = 6;
    first.score = 3;
    first.source = Source::Openalex;
    let mut dup = pdf("http://A.example.org:80/x.pdf?id=1#top");
    dup.version = Version::Published;
    dup.license = Some("cc-by".into());
    dup.tier = 5;
    dup.score = 9;
    dup.source = Source::Unpaywall;
    let other_query = pdf("https://a.example.org/x.pdf?id=2");
    let mut pool = vec![first];
    merge(&mut pool, vec![dup, other_query]);
    assert_eq!(pool.len(), 2, "the query distinguishes");
    let merged = &pool[0];
    assert_eq!(
        (
            merged.version,
            merged.license.as_deref(),
            merged.tier,
            merged.score
        ),
        (Version::Published, Some("cc-by"), 5, 9)
    );
    assert_eq!(
        merged.source,
        Source::Openalex,
        "the candidate already pooled keeps its source"
    );
    assert_eq!(merged.url, "https://a.example.org/x.pdf?id=1");
    // A zero tier means unranked: the other side's tier wins.
    let mut unranked = vec![pdf("https://b.example.org/y.pdf")];
    merge(
        &mut unranked,
        vec![with(pdf("https://b.example.org/y.pdf"), |c| c.tier = 7)],
    );
    assert_eq!(unranked[0].tier, 7);
    // Landing + Pdf at one URL is a Pdf.
    let mut kinds = vec![landing("https://c.example.org/z.pdf")];
    merge(&mut kinds, vec![pdf("https://c.example.org/z.pdf")]);
    assert_eq!(kinds[0].kind, CandKind::Pdf);
}

// ---------------------------------------------------------------------------------------------------- supersession

#[test]
fn walled_pmc_urls_are_dropped_when_an_open_pmc_copy_exists() {
    let walled = [
        "https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/pdf/pmed.1000097.pdf",
        "https://europepmc.org/articles/PMC2707599?pdf=render",
    ];
    let keep = pdf("https://iris.example.it/a.pdf");
    for open in [
        "https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf",
        "https://europepmc.org/api/getPdf?pmcid=PMC2707599",
    ] {
        let mut pool: Vec<Candidate> = walled.iter().map(|u| pdf(u)).collect();
        pool.push(keep.clone());
        pool.push(pdf(open));
        let out = rank(pool, &prefs());
        assert_eq!(
            urls(&out),
            [open, "https://iris.example.it/a.pdf"],
            "{open}"
        );
    }
    // Without an open copy they are kept, as browser-only attempts.
    let mut pool: Vec<Candidate> = walled.iter().map(|u| pdf(u)).collect();
    pool.push(keep);
    let out = rank(pool, &prefs());
    assert_eq!(out.len(), 3);
    assert_eq!(
        out.iter().filter(|c| c.tier == tier::BROWSER_ONLY).count(),
        2
    );
}

#[test]
fn core_ac_uk_is_dropped_when_a_real_pdf_exists() {
    let core_pdf = with(pdf("https://core.ac.uk/download/28948651.pdf"), |c| {
        c.source = Source::Core
    });
    let core_page = with(landing("https://core.ac.uk/reader/28948651"), |c| {
        c.source = Source::Core
    });
    let real = with(pdf("https://iris.example.it/a.pdf"), |c| {
        c.host_kind = HostKind::Repository
    });
    let out = rank(
        vec![core_pdf.clone(), core_page.clone(), real.clone()],
        &prefs(),
    );
    assert_eq!(urls(&out), ["https://iris.example.it/a.pdf"]);
    // An aggregator PDF elsewhere is not a "real" PDF.
    let mirror = with(
        pdf("https://langtaosha.org.cn/index.php/lts/preprint/download/10/108"),
        |c| c.host_kind = HostKind::Aggregator,
    );
    let out = rank(vec![core_pdf.clone(), mirror], &prefs());
    assert_eq!(out.len(), 2);
    // A landing page alone does not make CORE redundant.
    let out = rank(
        vec![core_pdf, core_page, landing("https://doi.org/10.1/x")],
        &prefs(),
    );
    assert_eq!(out.len(), 3);
}

// ---------------------------------------------------------------------------------------------------- fixtures

#[test]
fn openalex_arxiv_work_prefers_arxiv_over_the_merged_work_mirror() {
    let requested = Ids {
        doi: Some("10.48550/arxiv.1706.03762".into()),
        arxiv: Some("1706.03762".into()),
        ..Default::default()
    };
    let prefs = RankPrefs {
        requested: requested.clone(),
        ..Default::default()
    };
    let api = parsed(Source::Openalex, "openalex__arxiv.json", &requested);
    let out = rank(api.candidates.clone(), &prefs);

    let arxiv = position(&out, "https://arxiv.org/pdf/1706.03762");
    assert_eq!(out[arxiv].tier, tier::ARXIV);
    assert_eq!(arxiv, 0, "{:?}", urls(&out));
    let mirror = position(
        &out,
        "https://langtaosha.org.cn/index.php/lts/preprint/download/10/108",
    );
    assert!(arxiv < mirror);
    assert_eq!(
        out[mirror].tier,
        tier::DIRECT_REPOSITORY,
        "kept, not dropped"
    );
    assert!(
        out[mirror]
            .record_doi
            .as_deref()
            .is_some_and(|d| d.starts_with("10.65215/"))
    );
    // The mirror carries the foreign-record and aggregator penalties.
    let neutral = with(pdf("https://iris.example.it/x.pdf"), |c| {
        c.version = out[mirror].version
    });
    assert!(
        out[mirror].score <= rank::score(&neutral, &prefs) - 70,
        "{} vs {}",
        out[mirror].score,
        rank::score(&neutral, &prefs)
    );
    // The v5 duplicate and the abstract page merged into the one arXiv PDF.
    assert_eq!(
        out.iter().filter(|c| c.url.contains("arxiv.org/")).count(),
        1,
        "{:?}",
        urls(&out)
    );
    // The paid cache comes last.
    assert_eq!(out.last().map(|c| c.tier), Some(tier::PAID_API));
}

#[test]
fn medrxiv_pool_keeps_the_published_copy_and_the_early_path_pdf() {
    let requested = doi_request("10.1101/2023.05.31.23290770");
    let prefs = RankPrefs {
        requested: requested.clone(),
        ..Default::default()
    };
    let mut pool = parsed(Source::Openalex, "openalex__medrxiv.json", &requested).candidates;
    pool.extend(
        parsed(
            Source::Biorxiv,
            "biorxiv-api__medrxiv-details.json",
            &requested,
        )
        .candidates,
    );
    let out = rank(pool, &prefs);

    let early_url =
        "https://www.medrxiv.org/content/medrxiv/early/2023/06/01/2023.05.31.23290770.full.pdf";
    let early = get(&out, early_url);
    assert_eq!(early.tier, tier::RXIV_EARLY);
    assert_eq!(
        out.iter().filter(|c| c.url == early_url).count(),
        1,
        "OpenAlex and bioRxiv agree on it: merged"
    );
    assert_eq!(early.license.as_deref(), Some("cc-by"));
    assert_eq!(position(&out, early_url), 0, "{:?}", urls(&out));

    let plos = get(&out, "https://doi.org/10.1371/journal.pone.0298402");
    assert_eq!(
        plos.record_doi.as_deref(),
        Some("10.1371/journal.pone.0298402")
    );
    assert!(matches!(plos.tier, tier::DIRECT_PUBLISHER | tier::LANDING));
    assert_eq!((plos.kind, plos.tier), (CandKind::Landing, tier::LANDING));
    assert_eq!(
        plos.host_kind,
        HostKind::Publisher,
        "a recognised source: no foreign-record penalty"
    );

    // The canonical URL is tried after the early path (it is often WAF-blocked).
    let canonical = get(
        &out,
        "https://www.medrxiv.org/content/10.1101/2023.05.31.23290770v1.full.pdf",
    );
    assert_eq!(canonical.tier, tier::RXIV_EARLY);
    assert!(canonical.score < early.score);
    // Nothing open from PMC is in the pool, so the PMC website PDF stays as a browser-only attempt.
    assert_eq!(
        get(
            &out,
            "https://pmc.ncbi.nlm.nih.gov/articles/PMC11156280/pdf/pone.0298402.pdf"
        )
        .tier,
        tier::BROWSER_ONLY
    );
}

#[test]
fn an_open_pmc_copy_supersedes_the_walled_pmc_pdf_in_a_real_pool() {
    let requested = doi_request("10.1371/journal.pmed.1000097");
    let prefs = RankPrefs {
        requested: requested.clone(),
        ..Default::default()
    };
    let mut pool = parsed(
        Source::Openalex,
        "openalex__medrxiv.json",
        &doi_request("10.1101/2023.05.31.23290770"),
    )
    .candidates;
    pool.extend(parsed(Source::PmcS3, "pmc-s3__meta-gold.json", &requested).candidates);
    pool.extend(parsed(Source::Europepmc, "europepmc__gold.json", &requested).candidates);
    let out = rank(pool, &prefs);
    assert_eq!(out[0].tier, tier::PMC_S3);
    assert_eq!(out[1].tier, tier::EUROPEPMC_GETPDF);
    assert!(
        out.iter().all(|c| !c.needs_browser),
        "walled copies are dropped: {:?}",
        urls(&out)
    );
    assert!(out.iter().all(|c| c.tier != tier::BROWSER_ONLY));
}

#[test]
fn ranking_the_whole_fixture_pool_gives_a_sane_order() {
    let pool = big_pool();
    assert!(pool.len() > 60, "{}", pool.len());
    let out = rank(pool, &prefs());
    assert!(!out.is_empty());
    for pair in out.windows(2) {
        assert!(
            (pair[0].tier, -pair[0].score) <= (pair[1].tier, -pair[1].score),
            "{} before {}",
            pair[0].url,
            pair[1].url
        );
    }
    let mut keys = BTreeSet::new();
    for c in &out {
        assert!((1..=9).contains(&c.tier), "{}", c.url);
        assert!(
            c.url.starts_with("https://") && !c.url.contains('#'),
            "{}",
            c.url
        );
        assert!(keys.insert(canonical_key(&c.url)), "duplicate {}", c.url);
        assert_eq!(c.score, score(c, &prefs()), "{}", c.url);
        assert_eq!(
            c.needs_browser,
            c.needs_browser || oa::is_browser_walled(&c.url)
        );
    }
    // Spot checks across the pool.
    assert_eq!(out[0].tier, tier::PMC_S3);
    assert!(out.iter().any(|c| c.tier == tier::ARXIV));
    assert!(out.iter().any(|c| c.tier == tier::RXIV_EARLY));
    assert!(
        out.iter().all(|c| !c.url.contains("pdf=render")
            || out.iter().all(|o| o.tier != tier::EUROPEPMC_GETPDF))
    );
    assert!(
        out.iter().all(|c| !c.url.contains("core.ac.uk")),
        "a real PDF exists, so CORE is dropped"
    );
}

// ---------------------------------------------------------------------------------------------------- version order, limits, scores

#[test]
fn version_order_decides_between_otherwise_equal_candidates() {
    let published = with(pdf("https://one.example.org/a.pdf"), |c| {
        c.version = Version::Published
    });
    let accepted = with(pdf("https://two.example.org/a.pdf"), |c| {
        c.version = Version::Accepted
    });
    let submitted = with(pdf("https://three.example.org/a.pdf"), |c| {
        c.version = Version::Submitted
    });
    let pool = vec![submitted, accepted, published];

    let default = rank(pool.clone(), &prefs());
    assert_eq!(
        urls(&default),
        [
            "https://one.example.org/a.pdf",
            "https://two.example.org/a.pdf",
            "https://three.example.org/a.pdf"
        ]
    );
    let explicit = RankPrefs {
        version_order: vec![
            Version::Published,
            Version::Accepted,
            Version::Submitted,
            Version::Unknown,
        ],
        ..Default::default()
    };
    assert_eq!(urls(&rank(pool.clone(), &explicit)), urls(&default));

    let flipped = RankPrefs {
        version_order: vec![Version::Accepted, Version::Published],
        ..Default::default()
    };
    let out = rank(pool.clone(), &flipped);
    assert_eq!(
        urls(&out),
        [
            "https://two.example.org/a.pdf",
            "https://one.example.org/a.pdf",
            "https://three.example.org/a.pdf"
        ],
        "unlisted versions rank last"
    );

    let preprints_first = RankPrefs {
        version_order: vec![Version::Submitted, Version::Accepted, Version::Published],
        ..Default::default()
    };
    assert_eq!(
        rank(pool, &preprints_first)[0].url,
        "https://three.example.org/a.pdf"
    );
}

#[test]
fn version_order_never_overrides_the_tier() {
    let arxiv = with(pdf("https://arxiv.org/pdf/1706.03762"), |c| {
        c.version = Version::Submitted
    });
    let publisher = with(pdf("https://www.nature.com/a.pdf"), |c| {
        c.version = Version::Published;
        c.host_kind = HostKind::Publisher;
    });
    let out = rank(vec![publisher, arxiv], &prefs());
    assert_eq!(
        urls(&out),
        [
            "https://arxiv.org/pdf/1706.03762",
            "https://www.nature.com/a.pdf"
        ]
    );
}

#[test]
fn max_candidates_keeps_the_best() {
    let pool: Vec<Candidate> = (0..10)
        .map(|i| pdf(&format!("https://r{i}.example.org/a.pdf")))
        .chain([pdf("https://arxiv.org/pdf/1706.03762")])
        .collect();
    let all = rank(pool.clone(), &prefs());
    assert_eq!(all.len(), 11);
    for n in [1, 3, 6, 11, 50] {
        let limited = rank(
            pool.clone(),
            &RankPrefs {
                max_candidates: n,
                ..Default::default()
            },
        );
        assert_eq!(
            urls(&limited),
            urls(&all)[..n.min(11)],
            "max_candidates = {n}"
        );
    }
    assert_eq!(
        rank(
            pool,
            &RankPrefs {
                max_candidates: 0,
                ..Default::default()
            }
        )
        .len(),
        11
    );
}

#[test]
fn scores_follow_the_documented_weights() {
    let p = prefs();
    let base = score(&pdf("https://x.example.org/a.pdf"), &p);
    let delta = |f: &dyn Fn(&mut Candidate)| {
        score(&with(pdf("https://x.example.org/a.pdf"), |c| f(c)), &p) - base
    };
    assert_eq!(
        base, 100,
        "Unknown is the fourth version in the default order: no bonus, no penalty"
    );
    assert_eq!(delta(&|c| c.version = Version::Published), 20);
    assert_eq!(delta(&|c| c.version = Version::Submitted), 4);
    assert_eq!(delta(&|c| c.version = Version::Accepted), 10);
    assert_eq!(delta(&|c| c.license = Some("cc-by".into())), 5);
    assert_eq!(delta(&|c| c.license = Some("other-oa".into())), 0);
    assert_eq!(delta(&|c| c.host_kind = HostKind::Publisher), 10);
    assert_eq!(delta(&|c| c.host_kind = HostKind::Aggregator), -30);
    assert_eq!(delta(&|c| c.needs_browser = true), -10);
    assert_eq!(delta(&|c| c.signal = Some("crossref:link_tdm".into())), -15);
    assert_eq!(delta(&|c| c.signal = Some("biorxiv:canonical".into())), -5);
    // `score` reads fields, never the stored score.
    assert_eq!(delta(&|c| c.score = 12345), 0);
}

#[test]
fn only_unrecognised_hosts_pay_for_a_foreign_record_doi() {
    let p = requesting("10.48550/arxiv.1706.03762");
    let foreign = |host_kind: HostKind, doi: &str| {
        score(
            &with(pdf("https://x.example.org/a.pdf"), |c| {
                c.host_kind = host_kind;
                c.record_doi = Some(doi.into());
            }),
            &p,
        )
    };
    let same = foreign(HostKind::Other, "10.48550/ARXIV.1706.03762");
    assert_eq!(foreign(HostKind::Other, "10.65215/2q58a426"), same - 40);
    assert_eq!(
        foreign(HostKind::Aggregator, "10.65215/2q58a426"),
        foreign(HostKind::Aggregator, "10.48550/arxiv.1706.03762") - 40
    );
    for kind in [
        HostKind::Publisher,
        HostKind::Repository,
        HostKind::Pmc,
        HostKind::Preprint,
    ] {
        assert_eq!(
            foreign(kind, "10.65215/2q58a426"),
            foreign(kind, "10.48550/arxiv.1706.03762"),
            "{kind:?}"
        );
    }
    // No requested DOI, or no record DOI: nothing to compare.
    let none = score(
        &with(pdf("https://x.example.org/a.pdf"), |c| {
            c.record_doi = Some("10.65215/2q58a426".into())
        }),
        &prefs(),
    );
    assert_eq!(none, score(&pdf("https://x.example.org/a.pdf"), &prefs()));
}

#[test]
fn a_record_doi_that_confirms_the_request_wins_a_merge() {
    let requested = doi_request("10.1101/2023.05.31.23290770");
    let prefs = RankPrefs {
        requested,
        ..Default::default()
    };
    let url = "https://mirror.example.org/preprint.pdf";
    let merged_work = with(pdf(url), |c| {
        c.record_doi = Some("10.1371/journal.pone.0298402".into())
    });
    let confirming = with(pdf(url), |c| {
        c.record_doi = Some("10.1101/2023.05.31.23290770".into());
        c.source = Source::Unpaywall;
    });
    let a = rank(vec![merged_work.clone(), confirming.clone()], &prefs);
    let b = rank(vec![confirming, merged_work], &prefs);
    assert_eq!(a, b);
    assert_eq!(
        a[0].record_doi.as_deref(),
        Some("10.1101/2023.05.31.23290770")
    );
}

// ---------------------------------------------------------------------------------------------------- idempotence and determinism

#[test]
fn ranking_is_idempotent() {
    let pools: Vec<(Vec<Candidate>, RankPrefs)> = vec![
        (big_pool(), prefs()),
        (
            big_pool(),
            RankPrefs {
                max_candidates: 12,
                ..Default::default()
            },
        ),
        (
            big_pool(),
            RankPrefs {
                version_order: vec![Version::Accepted, Version::Submitted],
                requested: doi_request("10.1038/nature12373"),
                max_candidates: 0,
            },
        ),
        (
            vec![
                landing("http://arxiv.org/abs/1706.03762"),
                pdf("https://www.ncbi.nlm.nih.gov/pmc/articles/PMC1/pdf/a.pdf"),
                pdf("https://pmc-oa-opendata.s3.amazonaws.com/PMC1.1/PMC1.1.pdf"),
                pdf("https://core.ac.uk/download/1.pdf"),
            ],
            prefs(),
        ),
        (Vec::new(), prefs()),
    ];
    for (pool, p) in pools {
        let once = rank(pool, &p);
        let twice = rank(once.clone(), &p);
        assert_eq!(once, twice);
    }
}

#[test]
fn ranking_ignores_input_order() {
    let pool = big_pool();
    let variants = [
        prefs(),
        requesting("10.1101/2023.05.31.23290770"),
        RankPrefs {
            max_candidates: 9,
            ..Default::default()
        },
        RankPrefs {
            version_order: vec![Version::Submitted, Version::Published],
            ..Default::default()
        },
    ];
    for p in &variants {
        let baseline = rank(pool.clone(), p);
        let mut reversed = pool.clone();
        reversed.reverse();
        assert_eq!(rank(reversed, p), baseline, "reversed input");
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..60 {
            let mut shuffled = pool.clone();
            rng.shuffle(&mut shuffled);
            assert_eq!(rank(shuffled, p), baseline, "shuffle {round}");
        }
    }
}

#[test]
fn equal_scores_are_ordered_by_url_not_by_arrival() {
    let a = pdf("https://a.example.org/x.pdf");
    let b = pdf("https://b.example.org/x.pdf");
    let forward = rank(vec![a.clone(), b.clone()], &prefs());
    let backward = rank(vec![b, a], &prefs());
    assert_eq!(forward, backward);
    assert_eq!(
        urls(&forward),
        ["https://a.example.org/x.pdf", "https://b.example.org/x.pdf"]
    );
}

#[test]
fn rank_is_total_on_hostile_candidates() {
    let hostile = vec![
        pdf(&"x".repeat(10_000)),
        pdf(&format!("https://{}.example.org/a.pdf", "a".repeat(5_000))),
        pdf("https://arxiv.org/abs/"),
        pdf("https://arxiv.org/pdf/"),
        pdf("https://www.ncbi.nlm.nih.gov/pmc/articles/"),
        pdf("https://www.ncbi.nlm.nih.gov/pmc/articles/PMC"),
        pdf("https://pmc.ncbi.nlm.nih.gov/articles/%00/pdf/%ff"),
        pdf("https://europepmc.org/"),
        with(pdf("https://ok.example.org/a.pdf"), |c| {
            c.license = Some("\0".into());
            c.record_doi = Some("not a doi".into());
            c.landing_url = Some(String::new());
            c.signal = Some("\u{202e}".into());
            c.tier = 255;
            c.score = i32::MAX;
        }),
        with(pdf("https://ok.example.org/b.pdf"), |c| c.score = i32::MIN),
    ];
    let out = rank(hostile, &requesting("10.1234/x"));
    assert!(out.iter().all(|c| (1..=9).contains(&c.tier)));
    assert_eq!(rank(out.clone(), &requesting("10.1234/x")), out);
}
