//! Identifier cleaners and `normalise`: table-driven vectors, including hostile input (spec: docs/wp/core-ids-oa.md §1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use fulltext_core::ids::{
    apply_learned, arxiv_from_doi, clean_arxiv, clean_doi, clean_pmcid, clean_pmid, extra_field,
    is_rxiv_doi, normalise, same_doi,
};
use fulltext_core::model::{Ids, ItemFields, LearnedIds};

type Cleaner = fn(&str) -> Option<String>;

fn run(name: &str, f: Cleaner, cases: &[(&str, Option<&str>)]) {
    for (raw, want) in cases {
        assert_eq!(f(raw).as_deref(), *want, "{name}({raw:?})");
    }
}

/// Inputs no cleaner may choke on.
fn hostile() -> Vec<String> {
    vec![
        "x".repeat(10 * 1024),
        "9".repeat(10 * 1024),
        "(".repeat(5_000) + &")".repeat(5_000),
        "10.1234/".to_owned() + &")".repeat(3_000),
        "\0".repeat(64),
        "\0\0 10.1234/ab\0cd \0".to_owned(),
        "10.".to_owned(),
        "10.1234".to_owned(),
        "https://".to_owned(),
        "://".to_owned(),
        "%".repeat(500),
        "%2F%2f%2".to_owned(),
        "１０.１２３４/ｘ".to_owned(),
        "PMID: ١٢٣٤٥".to_owned(),
        "arXiv:".to_owned() + &"1".repeat(10_000),
        "\u{202e}\u{feff}\u{200b}".to_owned(),
        "🦀🦀🦀 10.1234/🦀".to_owned(),
        "\n\r\t ".to_owned(),
        String::new(),
    ]
}

// ---------------------------------------------------------------------------------------------------- cleaners

#[test]
fn clean_doi_vectors() {
    let gold = Some("10.1371/journal.pmed.1000097");
    run(
        "clean_doi",
        clean_doi,
        &[
            ("10.1371/journal.pmed.1000097", gold),
            ("10.1371/JOURNAL.PMED.1000097", gold),
            ("  doi:10.1371/journal.pmed.1000097  ", gold),
            ("DOI: 10.1371/journal.pmed.1000097.", gold),
            ("https://doi.org/10.1371/journal.pmed.1000097", gold),
            (
                "http://dx.doi.org/10.1371/journal.pmed.1000097?utm_source=x#frag",
                gold,
            ),
            ("doi.org/10.1371/journal.pmed.1000097", gold),
            ("https://doi.org/10.1371%2Fjournal.pmed.1000097", gold),
            ("10.1371%2Fjournal.pmed.1000097", gold),
            ("(10.1371/journal.pmed.1000097)", gold),
            ("10.1371/journal.pmed.1000097;", gold),
            ("10.1002/bjs.11646", Some("10.1002/bjs.11646")),
            (
                "10.1016/S0140-6736(20)30183-5",
                Some("10.1016/s0140-6736(20)30183-5"),
            ),
            (
                "10.48550/arXiv.1706.03762",
                Some("10.48550/arxiv.1706.03762"),
            ),
            (
                "10.1101/2023.05.31.23290770",
                Some("10.1101/2023.05.31.23290770"),
            ),
            (
                "https://example.org/a b 10.1002/bjs.11646 trailing",
                Some("10.1002/bjs.11646"),
            ),
            ("", None),
            ("   ", None),
            ("doi", None),
            ("10.", None),
            ("10.12/short", None),
            ("10.1234", None),
            ("10.1234/", None),
            ("110.1234/x", None),
            ("abc10.1234/xyz", None),
            ("10.1234567890/x", None),
            ("11.1371/journal.pmed.1000097", None),
        ],
    );
}

#[test]
fn clean_doi_stops_at_control_characters() {
    assert_eq!(clean_doi("10.1234/ab\0cd").as_deref(), Some("10.1234/ab"));
    assert_eq!(clean_doi("10.1234/\0"), None);
}

#[test]
fn clean_pmid_vectors() {
    run(
        "clean_pmid",
        clean_pmid,
        &[
            ("19621072", Some("19621072")),
            ("  19621072\n", Some("19621072")),
            ("PMID: 19621072", Some("19621072")),
            ("pmid:19621072", Some("19621072")),
            ("PMID:19621072", Some("19621072")),
            ("PubMed: 19621072", Some("19621072")),
            (
                "https://pubmed.ncbi.nlm.nih.gov/19621072/",
                Some("19621072"),
            ),
            ("https://pubmed.ncbi.nlm.nih.gov/19621072", Some("19621072")),
            (
                "http://www.ncbi.nlm.nih.gov/pubmed/19621072",
                Some("19621072"),
            ),
            ("007", Some("7")),
            ("000123", Some("123")),
            ("123456789", Some("123456789")),
            ("1", Some("1")),
            ("", None),
            ("PMID:", None),
            ("PMID", None),
            ("abc", None),
            ("0", None),
            ("000", None),
            ("-123", None),
            ("12 34", None),
            ("19621072x", None),
            ("1234567890", None),
            ("PMC2707599", None),
            ("10.1371/journal.pmed.1000097", None),
            ("https://example.com/19621072", None),
            ("https://pubmed.ncbi.nlm.nih.gov/", None),
            ("ftp://pubmed.ncbi.nlm.nih.gov/19621072", None),
            ("١٢٣٤٥", None),
            ("１２３４５", None),
        ],
    );
}

#[test]
fn clean_pmcid_vectors() {
    let want = Some("PMC2707599");
    run(
        "clean_pmcid",
        clean_pmcid,
        &[
            ("PMC2707599", want),
            ("pmc2707599", want),
            ("  PMC2707599 ", want),
            ("PMC2707599.1", want),
            ("PMCID: PMC2707599", want),
            ("pmcid:pmc2707599", want),
            ("PMCID: 2707599", want),
            ("2707599", want),
            ("/pmc/articles/PMC2707599/", want),
            ("/articles/PMC2707599/pdf/pmed.1000097.pdf", want),
            ("https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/", want),
            (
                "https://pmc.ncbi.nlm.nih.gov/articles/PMC2707599/pdf/pmed.1000097.pdf",
                want,
            ),
            (
                "https://www.ncbi.nlm.nih.gov/pmc/articles/PMC2707599/",
                want,
            ),
            ("https://www.ncbi.nlm.nih.gov/pmc/articles/2707599", want),
            ("https://europepmc.org/articles/PMC2707599?pdf=render", want),
            ("PMC0007", Some("PMC7")),
            ("", None),
            ("PMC", None),
            ("PMCID:", None),
            ("PMCABC", None),
            ("PMC2707599x", None),
            ("0", None),
            ("12345678901", None),
            ("10.1371/journal.pmed.1000097", None),
            ("10.1234", None),
            ("pubmed", None),
            ("https://example.com/articles/PMC2707599", None),
            ("https://pmc.ncbi.nlm.nih.gov/", None),
            ("/articles/2707599", None),
        ],
    );
}

#[test]
fn clean_arxiv_vectors() {
    let attn = Some("1706.03762");
    run(
        "clean_arxiv",
        clean_arxiv,
        &[
            ("1706.03762", attn),
            ("1706.03762v1", attn),
            ("1706.03762v12", attn),
            ("arXiv:1706.03762", attn),
            ("ARXIV:1706.03762v5", attn),
            ("arxiv: 1706.03762", attn),
            ("arXiv:1706.03762v5 [cs.CL]", attn),
            ("1706.03762 [cs.CL]", attn),
            ("  arXiv:1706.03762  ", attn),
            ("https://arxiv.org/abs/1706.03762", attn),
            ("https://arxiv.org/abs/1706.03762v5", attn),
            ("http://arxiv.org/abs/1706.03762v5", attn),
            ("https://arxiv.org/pdf/1706.03762.pdf", attn),
            ("https://arxiv.org/pdf/1706.03762v5.pdf", attn),
            ("https://arxiv.org/pdf/1706.03762", attn),
            ("https://export.arxiv.org/abs/1706.03762", attn),
            ("https://www.arxiv.org/abs/1706.03762/", attn),
            ("arxiv.org/abs/1706.03762", attn),
            ("0704.0001", Some("0704.0001")),
            ("2501.12345", Some("2501.12345")),
            ("cs/0112017", Some("cs/0112017")),
            ("hep-th/9901001v2", Some("hep-th/9901001")),
            ("arXiv:hep-th/9901001", Some("hep-th/9901001")),
            ("HEP-TH/9901001", Some("hep-th/9901001")),
            ("math.GT/0309136", Some("math.GT/0309136")),
            ("math.gt/0309136v3", Some("math.GT/0309136")),
            (
                "https://arxiv.org/abs/hep-th/9901001v2",
                Some("hep-th/9901001"),
            ),
            ("", None),
            ("arXiv:", None),
            ("170.03762", None),
            ("1706.037", None),
            ("1706.0376222", None),
            ("1706-03762", None),
            ("17a6.03762", None),
            ("cs/011201", None),
            ("cs/01120177", None),
            ("cs.AIX/0112017", None),
            ("/0112017", None),
            ("v1", None),
            ("https://example.org/abs/1706.03762", None),
            ("https://arxiv.org/list/cs/new", None),
            ("https://arxiv.org/abs/", None),
            ("ftp://arxiv.org/abs/1706.03762", None),
            ("10.48550/arxiv.1706.03762", None),
            ("１７０６.03762", None),
        ],
    );
}

#[test]
fn arxiv_from_doi_vectors() {
    for (doi, want) in [
        ("10.48550/arxiv.1706.03762", Some("1706.03762")),
        ("10.48550/arXiv.1706.03762", Some("1706.03762")),
        ("10.48550/ARXIV.1706.03762v7", Some("1706.03762")),
        ("10.48550/arxiv.hep-th/9901001", Some("hep-th/9901001")),
        ("10.48550/arxiv.", None),
        ("10.48550/arxiv.nonsense", None),
        ("10.1101/2023.05.31.23290770", None),
        ("10.65215/2q58a426", None),
        ("", None),
    ] {
        assert_eq!(arxiv_from_doi(doi).as_deref(), want, "{doi}");
    }
}

#[test]
fn rxiv_and_same_doi() {
    assert!(is_rxiv_doi("10.1101/2023.05.31.23290770"));
    assert!(!is_rxiv_doi("10.1371/journal.pmed.1000097"));
    assert!(!is_rxiv_doi("10.11010/x"));
    assert!(same_doi(
        "DOI:10.1097/SLA.0000000000005603",
        "https://doi.org/10.1097/sla.0000000000005603"
    ));
    assert!(!same_doi(
        "10.1097/sla.0000000000005603",
        "10.1097/sla.0000000000005604"
    ));
    assert!(!same_doi("", ""), "two non-DOIs are not the same DOI");
    assert!(!same_doi("10.1097/sla.0000000000005603", "junk"));
}

#[test]
fn extra_field_vectors() {
    let cases: &[(&str, &str, Option<&str>)] = &[
        ("DOI: 10.1/x\nPMID: 5", "DOI", Some("10.1/x")),
        ("DOI: 10.1/x\nPMID: 5", "PMID", Some("5")),
        ("doi:10.1/x", "DOI", Some("10.1/x")),
        ("DoI:   10.1/x   ", "doi", Some("10.1/x")),
        ("  PMID:   7  ", "PMID", Some("7")),
        (
            "Some prose with DOI: 10.1/x\nDOI: 10.2/y",
            "DOI",
            Some("10.2/y"),
        ),
        ("PMID:\nPMID: 7", "PMID", Some("7")),
        ("DOI: first\nDOI: second", "DOI", Some("first")),
        ("A: 1\r\nB: 2\r\n", "B", Some("2")),
        ("A: 1\rB: 2", "B", Some("2")),
        ("arXiv ID: 1706.03762", "arXiv ID", Some("1706.03762")),
        ("DOIs: 10.1/x", "DOI", None),
        ("Not a DOI: 10.1/x", "DOI", None),
        ("DOI 10.1/x", "DOI", None),
        ("DOI", "DOI", None),
        ("", "DOI", None),
        ("DOI: 10.1/x", "", None),
        ("DOI: 10.1/x", "   ", None),
        ("ＤＯＩ: x", "DOI", None),
    ];
    for (extra, key, want) in cases {
        assert_eq!(
            extra_field(extra, key).as_deref(),
            *want,
            "extra_field({extra:?}, {key:?})"
        );
    }
}

#[test]
fn cleaners_are_total_on_hostile_input() {
    let fields = |s: &str| ItemFields {
        doi: s.into(),
        pmid: s.into(),
        pmcid: s.into(),
        url: s.into(),
        archive_id: s.into(),
        extra: s.into(),
    };
    for s in hostile() {
        let _ = (
            clean_doi(&s),
            clean_pmid(&s),
            clean_pmcid(&s),
            clean_arxiv(&s),
            arxiv_from_doi(&s),
        );
        let _ = (
            extra_field(&s, "DOI"),
            extra_field(&s, &s),
            same_doi(&s, &s),
            is_rxiv_doi(&s),
        );
        let ids = normalise(&fields(&s));
        // Whatever came out is well formed.
        if let Some(d) = ids.doi {
            assert!(
                d.starts_with("10.") && d.contains('/') && !d.chars().any(char::is_control),
                "{d:?}"
            );
        }
        if let Some(p) = ids.pmid {
            assert!(
                p.len() <= 9 && p.bytes().all(|b| b.is_ascii_digit()),
                "{p:?}"
            );
        }
    }
    // 10 KB of nothing useful is nothing.
    let junk = "x".repeat(10 * 1024);
    assert_eq!(normalise(&fields(&junk)), Ids::default());
}

#[test]
fn cleaners_are_idempotent() {
    for raw in [
        "https://doi.org/10.1371/JOURNAL.pmed.1000097",
        "doi:10.1002/bjs.11646",
        "10.48550/arXiv.1706.03762",
    ] {
        let once = clean_doi(raw).unwrap();
        assert_eq!(clean_doi(&once), Some(once.clone()), "{raw}");
    }
    for raw in ["PMID: 007", "pmid:19621072"] {
        let once = clean_pmid(raw).unwrap();
        assert_eq!(clean_pmid(&once), Some(once.clone()), "{raw}");
    }
    for raw in ["pmc2707599.1", "2707599"] {
        let once = clean_pmcid(raw).unwrap();
        assert_eq!(clean_pmcid(&once), Some(once.clone()), "{raw}");
    }
    for raw in ["arXiv:1706.03762v5 [cs.CL]", "math.gt/0309136v3"] {
        let once = clean_arxiv(raw).unwrap();
        assert_eq!(clean_arxiv(&once), Some(once.clone()), "{raw}");
    }
}

// ---------------------------------------------------------------------------------------------------- normalise

fn item(f: impl FnOnce(&mut ItemFields)) -> Ids {
    let mut fields = ItemFields::default();
    f(&mut fields);
    normalise(&fields)
}

fn source<'a>(ids: &'a Ids, name: &str) -> Option<&'a str> {
    ids.sources.get(name).map(String::as_str)
}

#[test]
fn normalise_every_field_empty() {
    let ids = normalise(&ItemFields::default());
    assert_eq!(ids, Ids::default());
    assert_eq!(
        (ids.doi, ids.pmid, ids.pmcid, ids.arxiv, ids.url, ids.rxiv),
        (None, None, None, None, None, false)
    );
    assert!(ids.sources.is_empty());
}

#[test]
fn normalise_whitespace_only_fields_are_empty() {
    let ids = item(|f| {
        f.doi = "  ".into();
        f.pmid = "\t".into();
        f.pmcid = "\n".into();
        f.url = "   ".into();
        f.archive_id = " \r\n ".into();
        f.extra = "\n\n   \n".into();
    });
    assert_eq!(ids, Ids::default());
}

#[test]
fn normalise_journal_article_with_doi_in_extra() {
    let ids = item(|f| {
        f.extra = "tex.ids: moher2009\nPMID: 19621072\nDOI: 10.1371/journal.pmed.1000097\nPMCID: PMC2707599".into();
        f.url =
            "https://journals.plos.org/plosmedicine/article?id=10.1371/journal.pmed.1000097".into();
    });
    assert_eq!(ids.doi.as_deref(), Some("10.1371/journal.pmed.1000097"));
    assert_eq!(
        (ids.pmid.as_deref(), ids.pmcid.as_deref()),
        (Some("19621072"), Some("PMC2707599"))
    );
    assert_eq!(
        (
            source(&ids, "doi"),
            source(&ids, "pmid"),
            source(&ids, "pmcid")
        ),
        (Some("extra"), Some("extra"), Some("extra"))
    );
    assert_eq!(
        ids.url.as_deref(),
        Some("https://journals.plos.org/plosmedicine/article?id=10.1371/journal.pmed.1000097")
    );
    assert!(!ids.rxiv);
    assert_eq!(ids.arxiv, None);
}

#[test]
fn normalise_doi_field_wins_over_extra_and_url() {
    let ids = item(|f| {
        f.doi = "https://doi.org/10.1002/BJS.11646".into();
        f.extra = "DOI: 10.9999/other.1".into();
        f.url = "https://doi.org/10.8888/other.2".into();
    });
    assert_eq!(ids.doi.as_deref(), Some("10.1002/bjs.11646"));
    assert_eq!(source(&ids, "doi"), Some("field"));
}

#[test]
fn normalise_bad_doi_field_falls_through_to_extra() {
    let ids = item(|f| {
        f.doi = "not a doi".into();
        f.extra = "DOI: 10.1002/bjs.11646".into();
    });
    assert_eq!(
        (ids.doi.as_deref(), source(&ids, "doi")),
        (Some("10.1002/bjs.11646"), Some("extra"))
    );
}

#[test]
fn normalise_url_only_item() {
    let ids = item(|f| f.url = "https://doi.org/10.1002/bjs.11646".into());
    assert_eq!(ids.doi.as_deref(), Some("10.1002/bjs.11646"));
    assert_eq!(source(&ids, "doi"), Some("url"));
    assert_eq!(
        ids.url.as_deref(),
        Some("https://doi.org/10.1002/bjs.11646")
    );
    let dx = item(|f| f.url = "http://dx.doi.org/10.1002/bjs.11646?x=1".into());
    assert_eq!(dx.doi.as_deref(), Some("10.1002/bjs.11646"));
}

#[test]
fn normalise_publisher_url_is_not_trusted_for_a_doi() {
    let ids = item(|f| f.url = "https://www.nature.com/articles/10.1038/nature12373".into());
    assert_eq!(ids.doi, None, "only doi.org URLs are certain");
    assert_eq!(
        ids.url.as_deref(),
        Some("https://www.nature.com/articles/10.1038/nature12373")
    );
}

#[test]
fn normalise_preprint_with_archive_id_and_arxiv_doi() {
    let ids = item(|f| {
        f.doi = "10.48550/arXiv.1706.03762".into();
        f.archive_id = "arXiv:1706.03762v5".into();
        f.url = "https://arxiv.org/abs/1706.03762v5".into();
    });
    assert_eq!(ids.doi.as_deref(), Some("10.48550/arxiv.1706.03762"));
    assert_eq!(ids.arxiv.as_deref(), Some("1706.03762"));
    assert_eq!(
        source(&ids, "arxiv"),
        Some("doi"),
        "the DOI is consulted first"
    );
    assert!(!ids.rxiv);
}

#[test]
fn normalise_arxiv_from_each_source() {
    let archive = item(|f| f.archive_id = "arXiv:1706.03762v5 [cs.CL]".into());
    assert_eq!(
        (archive.arxiv.as_deref(), source(&archive, "arxiv")),
        (Some("1706.03762"), Some("archive_id"))
    );
    assert_eq!(archive.doi, None, "an arXiv id does not invent a DOI");

    let url = item(|f| f.url = "https://arxiv.org/pdf/1706.03762v5.pdf".into());
    assert_eq!(
        (url.arxiv.as_deref(), source(&url, "arxiv")),
        (Some("1706.03762"), Some("url"))
    );

    let extra = item(|f| f.extra = "arXiv: hep-th/9901001v2".into());
    assert_eq!(
        (extra.arxiv.as_deref(), source(&extra, "arxiv")),
        (Some("hep-th/9901001"), Some("extra"))
    );

    let from_doi = item(|f| f.extra = "DOI: 10.48550/arXiv.2501.12345".into());
    assert_eq!(
        (from_doi.arxiv.as_deref(), source(&from_doi, "arxiv")),
        (Some("2501.12345"), Some("doi"))
    );

    let journal = item(|f| f.doi = "10.1038/nature12373".into());
    assert_eq!(journal.arxiv, None);
}

#[test]
fn normalise_medrxiv_and_biorxiv_dois_set_rxiv() {
    let med = item(|f| f.doi = "10.1101/2023.05.31.23290770".into());
    assert!(med.rxiv);
    assert_eq!(med.doi.as_deref(), Some("10.1101/2023.05.31.23290770"));
    let from_archive = item(|f| f.archive_id = "10.1101/2023.05.31.23290770".into());
    assert!(from_archive.rxiv);
    assert_eq!(source(&from_archive, "doi"), Some("archive_id"));
    let not = item(|f| f.doi = "10.1371/journal.pmed.1000097".into());
    assert!(!not.rxiv);
}

#[test]
fn normalise_pmid_and_pmcid_fields_then_extra() {
    let fields = item(|f| {
        f.pmid = "PMID: 19621072".into();
        f.pmcid = "2707599".into();
        f.extra = "PMID: 11111111\nPMCID: PMC999".into();
    });
    assert_eq!(
        (fields.pmid.as_deref(), fields.pmcid.as_deref()),
        (Some("19621072"), Some("PMC2707599"))
    );
    assert_eq!(
        (source(&fields, "pmid"), source(&fields, "pmcid")),
        (Some("field"), Some("field"))
    );

    let extra = item(|f| f.extra = "PMCID: PMC2707599.1\nPMID:19621072".into());
    assert_eq!(
        (extra.pmid.as_deref(), extra.pmcid.as_deref()),
        (Some("19621072"), Some("PMC2707599"))
    );
    assert_eq!(
        (source(&extra, "pmid"), source(&extra, "pmcid")),
        (Some("extra"), Some("extra"))
    );

    let junk = item(|f| {
        f.pmid = "not a pmid".into();
        f.pmcid = "not a pmcid".into();
    });
    assert_eq!((junk.pmid, junk.pmcid), (None, None));
}

#[test]
fn normalise_url_is_kept_only_when_http_s() {
    for (raw, want) in [
        (
            "https://example.org/a?b=c",
            Some("https://example.org/a?b=c"),
        ),
        ("  http://example.org/a  ", Some("http://example.org/a")),
        ("ftp://example.org/a", None),
        ("javascript:alert(1)", None),
        ("file:///etc/passwd", None),
        ("example.org/a", None),
        ("not a url", None),
        ("", None),
    ] {
        let ids = item(|f| f.url = raw.into());
        assert_eq!(ids.url.as_deref(), want, "{raw}");
    }
}

#[test]
fn normalise_never_invents_an_identifier() {
    let ids = item(|f| {
        f.extra = "Prose that mentions a DOI: 10.1234/abc and PMID 123 but not as lines.".into();
        f.archive_id = "SomeArchive:42".into();
    });
    assert_eq!(ids, Ids::default());
}

// ---------------------------------------------------------------------------------------------------- apply_learned

#[test]
fn apply_learned_fills_only_missing_ids_and_records_the_source() {
    let mut ids = Ids {
        doi: Some("10.1371/journal.pmed.1000097".into()),
        ..Ids::default()
    };
    ids.sources.insert("doi".into(), "field".into());
    let learned = LearnedIds {
        doi: Some("10.9999/other.doi".into()),
        pmid: Some("PMID: 19621072".into()),
        pmcid: Some("pmc2707599".into()),
        arxiv: Some("arXiv:1706.03762v5".into()),
    };
    assert!(apply_learned(&mut ids, &learned, "openalex"));
    assert_eq!(
        ids.doi.as_deref(),
        Some("10.1371/journal.pmed.1000097"),
        "a known id is never overwritten"
    );
    assert_eq!(
        (
            ids.pmid.as_deref(),
            ids.pmcid.as_deref(),
            ids.arxiv.as_deref()
        ),
        (Some("19621072"), Some("PMC2707599"), Some("1706.03762"))
    );
    assert_eq!(source(&ids, "doi"), Some("field"));
    assert_eq!(
        (
            source(&ids, "pmid"),
            source(&ids, "pmcid"),
            source(&ids, "arxiv")
        ),
        (Some("openalex"), Some("openalex"), Some("openalex"))
    );
    // Nothing left to learn.
    assert!(!apply_learned(&mut ids, &learned, "europepmc"));
    assert_eq!(source(&ids, "pmid"), Some("openalex"));
}

#[test]
fn apply_learned_ignores_invalid_values_and_empty_input() {
    let mut ids = Ids::default();
    let learned = LearnedIds {
        doi: Some("nope".into()),
        pmid: Some("12345678901".into()),
        pmcid: Some("".into()),
        arxiv: Some("1706.037".into()),
    };
    assert!(!apply_learned(&mut ids, &learned, "x"));
    assert!(!apply_learned(&mut ids, &LearnedIds::default(), "x"));
    assert_eq!(ids, Ids::default());
}

#[test]
fn apply_learned_sets_rxiv_for_a_learned_biorxiv_doi() {
    let mut ids = Ids::default();
    let learned = LearnedIds {
        doi: Some("https://doi.org/10.1101/2023.05.31.23290770".into()),
        ..Default::default()
    };
    assert!(apply_learned(&mut ids, &learned, "europepmc"));
    assert!(ids.rxiv);
    assert_eq!(ids.doi.as_deref(), Some("10.1101/2023.05.31.23290770"));

    let mut known = Ids {
        doi: Some("10.1371/journal.pmed.1000097".into()),
        ..Ids::default()
    };
    assert!(!apply_learned(&mut known, &learned, "europepmc"));
    assert!(!known.rxiv, "a known non-rxiv DOI stays non-rxiv");
}
