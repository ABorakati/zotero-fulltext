//! bioRxiv / medRxiv content API: `GET https://api.biorxiv.org/details/<server>/<doi>/na/json` and `/pubs/<server>/<doi>/na/json`.
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! Auto-detects `details` vs `pubs` bodies. `messages[0].status == "no posts found"` (a 200) -> `Empty`. For `details`
//! take the LATEST `collection[]` entry (highest `version`) and build the early-path PDF
//! `https://www.<server>.org/content/<server>/early/YYYY/MM/DD/<doi suffix>.full.pdf` from its `date`
//! (signal `biorxiv:early_path`), plus the canonical `https://www.<server>.org/content/<doi>v<version>.full.pdf`
//! (signal `biorxiv:canonical`, which medRxiv's WAF often blocks); `license` lower-cased. Both are `Pdf` candidates on
//! a Preprint host with version Submitted. A `published` DOI (anything but `"NA"` or empty) goes to `note` as
//! `published_doi:<doi>`. A `pubs` answer is `Found` with that note alone: it names the journal version but offers no
//! file. Fixtures: `fixtures/oa/biorxiv-api__*.json`.
use super::util::{self, CandidateSet};
use crate::ids;
use crate::model::{ApiInspection, CandKind, HostKind, Ids, LearnedIds, RespMeta, Source, Version};
use serde_json::Value;

const SOURCE: Source = Source::Biorxiv;

/// Parse a bioRxiv / medRxiv `details` or `pubs` response.
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    if let Some(done) = super::gate(SOURCE, resp) {
        return done;
    }
    let Some(root) = util::json(body) else {
        return super::unparsed(SOURCE, "bad_json");
    };
    if !root.is_object() || (root.get("collection").is_none() && root.get("messages").is_none()) {
        return super::unparsed(SOURCE, "unexpected_shape");
    }
    let collection = util::array(&root, "collection");
    if collection.is_empty() {
        let first_status = util::array(&root, "messages")
            .first()
            .and_then(|m| util::str_of(m, "status"))
            .unwrap_or("");
        let note = if first_status.to_ascii_lowercase().starts_with("no posts") {
            "no_posts"
        } else {
            "empty_collection"
        };
        return util::nothing(SOURCE, note);
    }
    let is_pubs = collection
        .iter()
        .any(|e| e.get("published_doi").is_some() || e.get("preprint_doi").is_some());
    if is_pubs {
        pubs(collection, requested)
    } else {
        details(collection, requested)
    }
}

/// `pubs`: the journal version of a preprint.
fn pubs(collection: &[Value], requested: &Ids) -> ApiInspection {
    let about_request = |e: &&Value| {
        requested
            .doi
            .as_deref()
            .is_some_and(|d| util::str_of(e, "preprint_doi").is_some_and(|p| ids::same_doi(p, d)))
    };
    let entry = collection.iter().find(about_request).or(collection.first());
    let published = entry.and_then(|e| util::doi_of(e, "published_doi"));
    match published {
        Some(doi) => {
            let mut out = util::finish(
                SOURCE,
                LearnedIds::default(),
                Vec::new(),
                None,
                Some(format!("published_doi:{doi}")),
            );
            out.status = crate::model::ApiStatus::Found;
            out
        }
        None => util::nothing(SOURCE, "not_published"),
    }
}

/// `details`: one entry per version of the preprint.
fn details(collection: &[Value], requested: &Ids) -> ApiInspection {
    let about_request = |e: &&Value| match requested.doi.as_deref() {
        Some(d) => util::str_of(e, "doi").is_some_and(|x| ids::same_doi(x, d)),
        None => true,
    };
    // Highest version wins; the earliest entry wins a tie.
    let latest = collection
        .iter()
        .filter(about_request)
        .fold(None::<(&Value, u32)>, |best, e| {
            let n = util::id_of(e, "version")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            match best {
                Some((_, m)) if m >= n => best,
                _ => Some((e, n)),
            }
        });
    let Some((entry, version_no)) = latest else {
        return util::nothing(SOURCE, "no_match");
    };

    let published = util::str_of(entry, "published")
        .filter(|p| !p.eq_ignore_ascii_case("na"))
        .and_then(ids::clean_doi)
        .map(|doi| format!("published_doi:{doi}"));

    let (Some(doi), Some(server), Some(date)) = (
        util::doi_of(entry, "doi"),
        server_of(entry),
        util::str_of(entry, "date").and_then(date_path),
    ) else {
        return ApiInspection {
            note: published.or(Some("incomplete_record".to_owned())),
            ..super::empty(SOURCE, crate::model::ApiStatus::Empty)
        };
    };
    let Some(suffix) = doi.strip_prefix("10.1101/") else {
        return ApiInspection {
            note: published.or(Some("not_a_rxiv_doi".to_owned())),
            ..super::empty(SOURCE, crate::model::ApiStatus::Empty)
        };
    };

    let license = util::str_of(entry, "license").and_then(super::license_slug);
    let host = format!("https://www.{server}.org");
    let landing = format!("{host}/content/{doi}v{version_no}");
    let make = |url: String, signal: &str| {
        util::candidate(SOURCE, CandKind::Pdf, &url).map(|mut c| {
            c.version = Version::Submitted;
            c.license = license.clone();
            c.host_kind = HostKind::Preprint;
            c.record_doi = Some(doi.clone());
            c.landing_url = Some(landing.clone());
            c.signal = Some(format!("biorxiv:{signal}"));
            c
        })
    };
    let mut set = CandidateSet::default();
    set.push(make(
        format!("{host}/content/{server}/early/{date}/{suffix}.full.pdf"),
        "early_path",
    ));
    set.push(make(
        format!("{host}/content/{doi}v{version_no}.full.pdf"),
        "canonical",
    ));
    util::finish(
        SOURCE,
        LearnedIds::default(),
        set.into_vec(),
        None,
        published,
    )
}

/// `medrxiv` or `biorxiv`, from the `server` field, else from the JATS URL's host.
fn server_of(entry: &Value) -> Option<&'static str> {
    let named = util::str_of(entry, "server").map(str::to_ascii_lowercase);
    let from_jats = util::str_of(entry, "jatsxml").map(str::to_ascii_lowercase);
    [named, from_jats].into_iter().flatten().find_map(|s| {
        if s.contains("medrxiv") {
            Some("medrxiv")
        } else if s.contains("biorxiv") {
            Some("biorxiv")
        } else {
            None
        }
    })
}

/// `2023-06-01` -> `2023/06/01`; anything that is not a plausible calendar date is `None`.
fn date_path(date: &str) -> Option<String> {
    let mut parts = date.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    let in_range = |s: &str, max: u32| s.parse::<u32>().is_ok_and(|n| (1..=max).contains(&n));
    let valid = parts.next().is_none()
        && digits(y, 4)
        && digits(m, 2)
        && digits(d, 2)
        && in_range(m, 12)
        && in_range(d, 31);
    valid.then(|| format!("{y}/{m}/{d}"))
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
    fn dates_become_paths() {
        assert_eq!(date_path("2023-06-01").as_deref(), Some("2023/06/01"));
        for bad in [
            "",
            "2023-6-1",
            "2023-13-01",
            "2023-06-00",
            "2023-06-01-02",
            "x",
            "2023/06/01",
        ] {
            assert_eq!(date_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn latest_version_wins_and_published_goes_to_note() {
        let body = br#"{"messages":[{"status":"ok"}],"collection":[
            {"doi":"10.1101/2023.05.31.23290770","date":"2023-06-01","version":"1","server":"medRxiv","license":"cc_by","published":"NA"},
            {"doi":"10.1101/2023.05.31.23290770","date":"2023-09-15","version":"2","server":"medRxiv","license":"cc_by","published":"10.1371/journal.pone.0298402"}]}"#;
        let out = parse(&resp(), body, &Ids::default());
        assert_eq!(out.status, ApiStatus::Found);
        assert_eq!(
            out.candidates[0].url,
            "https://www.medrxiv.org/content/medrxiv/early/2023/09/15/2023.05.31.23290770.full.pdf"
        );
        assert_eq!(
            out.candidates[1].url,
            "https://www.medrxiv.org/content/10.1101/2023.05.31.23290770v2.full.pdf"
        );
        assert_eq!(
            out.note.as_deref(),
            Some("published_doi:10.1371/journal.pone.0298402")
        );
    }

    #[test]
    fn garbage_is_unparseable() {
        for body in [&b""[..], b"[]", b"{}", b"<html>", b"\0"] {
            assert_eq!(
                parse(&resp(), body, &Ids::default()).status,
                ApiStatus::Unparseable,
                "{body:?}"
            );
        }
    }
}
