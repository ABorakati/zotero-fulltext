//! Europe PMC REST search: `GET https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=DOI:"<doi>"&resultType=core&format=json`.
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! `hitCount: 0` (a 200) -> `Empty`. The result whose DOI (or PMID / PMCID) matches `requested` is used; with several
//! results and no match the answer is `Empty`. `learned.pmcid` / `learned.pmid` (and the DOI when the request had
//! none) come from that record. `fullTextUrlList.fullTextUrl[]` entries with `documentStyle` `pdf` -> `Pdf`
//! candidates, `html` / `doi` -> `Landing` ones, keeping only `availabilityCode` `OA` or `F` (`S` is a subscription).
//! When a PMCID is known and the record is open access (`isOpenAccess == "Y"`), also emit
//! `https://europepmc.org/api/getPdf?pmcid=<PMCID>` (signal `europepmc:getpdf`). The `europepmc.org/...?pdf=render`
//! form is Cloudflare-walled: it is emitted (flagged `needs_browser`) only when the record offers no other PDF. A
//! `source == "PPR"` record is a Preprint host. `lite` results carry no `fullTextUrlList` but still yield the getPdf
//! candidate. Fixtures: `fixtures/oa/europepmc__*.json`.
use super::util::{self, CandidateSet};
use crate::ids;
use crate::model::{
    ApiInspection, CandKind, Candidate, HostKind, Ids, LearnedIds, RespMeta, Source, Version,
};
use serde_json::Value;

const SOURCE: Source = Source::Europepmc;

/// Parse a Europe PMC search response (`core` or `lite` result type).
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    if let Some(done) = super::gate(SOURCE, resp) {
        return done;
    }
    let Some(root) = util::json(body) else {
        return super::unparsed(SOURCE, "bad_json");
    };
    let Some(results) = root
        .get("resultList")
        .and_then(|r| r.get("result"))
        .and_then(Value::as_array)
    else {
        let zero_hits = root.get("hitCount").and_then(Value::as_u64) == Some(0);
        return if zero_hits {
            util::nothing(SOURCE, "no_hits")
        } else {
            super::unparsed(SOURCE, "unexpected_shape")
        };
    };
    if results.is_empty() {
        return util::nothing(SOURCE, "no_hits");
    }
    let Some(record) = select(results, requested) else {
        return util::nothing(SOURCE, "no_match");
    };

    let preprint = util::str_of(record, "source") == Some("PPR");
    let pmcid = util::str_of(record, "pmcid").and_then(ids::clean_pmcid);
    let record_doi = util::doi_of(record, "doi");
    let open_access = util::str_of(record, "isOpenAccess") == Some("Y");
    let license = util::str_of(record, "license").and_then(super::license_slug);

    let learned = LearnedIds {
        pmcid: pmcid.clone(),
        pmid: util::id_of(record, "pmid")
            .as_deref()
            .and_then(ids::clean_pmid),
        doi: if requested.doi.is_none() {
            record_doi.clone()
        } else {
            None
        },
        arxiv: None,
    };

    let finish = |mut c: Candidate, signal: &str| -> Candidate {
        c.version = if preprint {
            Version::Submitted
        } else {
            Version::Unknown
        };
        c.license = license.clone();
        c.record_doi = record_doi.clone();
        if preprint {
            c.host_kind = HostKind::Preprint;
        }
        c.signal = Some(format!("europepmc:{signal}"));
        c
    };

    let mut set = CandidateSet::default();
    if let Some(pmcid) = pmcid.as_deref().filter(|_| open_access) {
        let url = format!("https://europepmc.org/api/getPdf?pmcid={pmcid}");
        let c = util::candidate(SOURCE, CandKind::Pdf, &url).map(|mut c| {
            c.landing_url = Some(format!("https://europepmc.org/articles/{pmcid}"));
            finish(c, "getpdf")
        });
        set.push(c);
    }
    let mut walled = Vec::new();
    let list = record
        .get("fullTextUrlList")
        .map(|l| util::array(l, "fullTextUrl"))
        .unwrap_or_default();
    for entry in list {
        if !matches!(util::str_of(entry, "availabilityCode"), Some("OA" | "F")) {
            continue;
        }
        let Some(url) = util::str_of(entry, "url") else {
            continue;
        };
        match util::str_of(entry, "documentStyle") {
            Some("pdf") => {
                let c =
                    util::candidate(SOURCE, CandKind::Pdf, url).map(|c| finish(c, "fulltext_pdf"));
                match c {
                    Some(c) if c.needs_browser => walled.push(c),
                    other => set.push(other),
                }
            }
            Some("html" | "doi") => set.push(
                util::candidate(SOURCE, CandKind::Landing, url).map(|c| finish(c, "fulltext_url")),
            ),
            _ => {}
        }
    }
    if !set.has_pdf() {
        for c in walled {
            set.push(Some(c));
        }
    }
    util::finish(SOURCE, learned, set.into_vec(), None, None)
}

/// The record that is about the requested work: DOI, PMID or PMCID match; any record when nothing is requested.
fn select<'a>(results: &'a [Value], requested: &Ids) -> Option<&'a Value> {
    let names_something =
        requested.doi.is_some() || requested.pmid.is_some() || requested.pmcid.is_some();
    if !names_something {
        return results.first();
    }
    results.iter().find(|r| {
        let doi = requested
            .doi
            .as_deref()
            .is_some_and(|d| util::str_of(r, "doi").is_some_and(|x| ids::same_doi(x, d)));
        let pmid = requested.pmid.is_some()
            && util::id_of(r, "pmid").as_deref().and_then(ids::clean_pmid) == requested.pmid;
        let pmcid = requested.pmcid.is_some()
            && util::str_of(r, "pmcid").and_then(ids::clean_pmcid) == requested.pmcid;
        doi || pmid || pmcid
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ApiStatus;

    fn resp() -> RespMeta {
        RespMeta {
            status: 200,
            ..Default::default()
        }
    }

    #[test]
    fn open_access_pmc_record_offers_get_pdf() {
        let body = br#"{"hitCount":1,"resultList":{"result":[{"id":"1","source":"MED","pmid":"19621072","pmcid":"PMC2707599",
            "doi":"10.1371/journal.pmed.1000097","isOpenAccess":"Y","license":"cc by"}]}}"#;
        let out = parse(&resp(), body, &Ids::default());
        assert_eq!(out.status, ApiStatus::Found);
        assert_eq!(out.candidates.len(), 1);
        let c = &out.candidates[0];
        assert_eq!(
            (c.url.as_str(), c.license.as_deref(), c.kind),
            (
                "https://europepmc.org/api/getPdf?pmcid=PMC2707599",
                Some("cc-by"),
                CandKind::Pdf
            )
        );
        assert_eq!(
            (out.learned.pmcid.as_deref(), out.learned.pmid.as_deref()),
            (Some("PMC2707599"), Some("19621072"))
        );
    }

    #[test]
    fn garbage_and_misses() {
        assert_eq!(
            parse(&resp(), b"<html>", &Ids::default()).status,
            ApiStatus::Unparseable
        );
        assert_eq!(
            parse(&resp(), br#"{"hitCount":0}"#, &Ids::default()).status,
            ApiStatus::Empty
        );
        let other = br#"{"hitCount":1,"resultList":{"result":[{"doi":"10.1234/other","pmcid":"PMC1","isOpenAccess":"Y"}]}}"#;
        let want = Ids {
            doi: Some("10.1234/wanted".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            parse(&resp(), other, &want).note.as_deref(),
            Some("no_match")
        );
    }
}
