//! PMC Cloud Service bucket `pmc-oa-opendata` plus the NCBI ID converter.
//! Owned by core-ids-oa (spec: `docs/wp/core-ids-oa.md`).
//!
//! The parser auto-detects three body shapes by content, because `InspectSpec::Api` carries only the source:
//! 1. `metadata/PMC<n>.<v>.json`: `pdf_url` (`s3://...?md5=`, rewritten with [`super::s3_to_https`]) becomes a `Pdf`
//!    candidate (the object is served as `binary/octet-stream`; the host sniffs the bytes). The version in the key
//!    (`.1`, `.2`) is whatever the metadata says; it is not always 1. `license_code "TDM"` or a missing `pdf_url`
//!    (author manuscripts) -> `Empty` with note `no_pdf`.
//! 2. ListObjectsV2 XML: the highest-version `.pdf` key; no PDF key -> `Empty`; `<Error><Code>NoSuchKey</Code>` ->
//!    `NotFound`.
//! 3. ID-converter JSON (`records[]`; a per-record `status: "error"` means not in PMC): `learned` pmcid / pmid / doi
//!    only, `Found` when at least one is learned.
//!
//! Fixtures: `fixtures/oa/pmc-s3__*`, `fixtures/oa/ncbi-idconv__*`.
use super::util::{self, CandidateSet};
use super::xml;
use crate::ids;
use crate::model::{
    ApiInspection, ApiStatus, CandKind, HostKind, Ids, LearnedIds, RespMeta, Source, Version,
};
use serde_json::Value;

const SOURCE: Source = Source::PmcS3;
const BUCKET_URL: &str = "https://pmc-oa-opendata.s3.amazonaws.com";

/// Parse a PMC S3 metadata, listing or ID-converter response.
pub fn parse(resp: &RespMeta, body: &[u8], requested: &Ids) -> ApiInspection {
    if let Some(done) = super::gate(SOURCE, resp) {
        return done;
    }
    match body.iter().copied().find(|b| !b.is_ascii_whitespace()) {
        Some(b'{') => json_shape(body, requested),
        Some(b'<') => xml_shape(body),
        _ => super::unparsed(SOURCE, "unknown_shape"),
    }
}

fn json_shape(body: &[u8], requested: &Ids) -> ApiInspection {
    let Some(root) = util::json(body) else {
        return super::unparsed(SOURCE, "bad_json");
    };
    if root.get("records").is_some() {
        idconv(&root, requested)
    } else if ["pmcid", "pdf_url", "xml_url"]
        .iter()
        .any(|k| root.get(k).is_some())
    {
        metadata(&root, requested)
    } else {
        super::unparsed(SOURCE, "unexpected_shape")
    }
}

/// `metadata/PMC<n>.<v>.json`.
fn metadata(root: &Value, requested: &Ids) -> ApiInspection {
    let pmcid = util::str_of(root, "pmcid").and_then(ids::clean_pmcid);
    let record_doi = util::doi_of(root, "doi");
    let learned = LearnedIds {
        pmcid: pmcid.clone(),
        pmid: util::id_of(root, "pmid")
            .as_deref()
            .and_then(ids::clean_pmid),
        doi: if requested.doi.is_none() {
            record_doi.clone()
        } else {
            None
        },
        arxiv: None,
    };
    let license_code = util::str_of(root, "license_code");
    let tdm_only = license_code.is_some_and(|l| l.eq_ignore_ascii_case("tdm"));
    let pdf = util::str_of(root, "pdf_url")
        .and_then(|u| super::s3_to_https(u).or_else(|| util::https(u)));
    let Some(pdf) = pdf.filter(|_| !tdm_only) else {
        return ApiInspection {
            learned,
            note: Some("no_pdf".to_owned()),
            ..super::empty(SOURCE, ApiStatus::Empty)
        };
    };
    let manuscript = root.get("is_manuscript").and_then(Value::as_bool) == Some(true);
    let mut set = CandidateSet::default();
    set.push(util::candidate(SOURCE, CandKind::Pdf, &pdf).map(|mut c| {
        c.version = if manuscript {
            Version::Accepted
        } else {
            Version::Published
        };
        c.license = license_code.and_then(super::license_slug);
        c.host_kind = HostKind::Pmc;
        c.record_doi = record_doi;
        c.landing_url = pmcid
            .as_ref()
            .map(|id| format!("https://pmc.ncbi.nlm.nih.gov/articles/{id}/"));
        c.signal = Some("pmc_s3:metadata".to_owned());
        c
    }));
    util::finish(SOURCE, learned, set.into_vec(), None, None)
}

/// NCBI ID converter: `records[]` with `doi`, `pmcid`, `pmid` (a number) or `status: "error"`.
fn idconv(root: &Value, requested: &Ids) -> ApiInspection {
    let records = util::array(root, "records");
    if records.is_empty() {
        return if util::str_of(root, "status") == Some("error") {
            super::unparsed(SOURCE, "idconv_error")
        } else {
            util::nothing(SOURCE, "no_records")
        };
    }
    let names_something =
        requested.doi.is_some() || requested.pmid.is_some() || requested.pmcid.is_some();
    let about_request = |r: &&Value| {
        let doi = requested.doi.as_deref().is_some_and(|d| {
            [util::str_of(r, "doi"), util::str_of(r, "requested-id")]
                .into_iter()
                .flatten()
                .any(|x| ids::same_doi(x, d))
        });
        let pmid = requested.pmid.is_some()
            && util::id_of(r, "pmid").as_deref().and_then(ids::clean_pmid) == requested.pmid;
        let pmcid = requested.pmcid.is_some()
            && util::str_of(r, "pmcid").and_then(ids::clean_pmcid) == requested.pmcid;
        doi || pmid || pmcid
    };
    let record = if names_something {
        records.iter().find(about_request)
    } else {
        records.first()
    };
    let Some(record) = record else {
        return util::nothing(SOURCE, "no_match");
    };
    // A per-record error ("Identifier not found in PMC") is the API's way of saying the work is not in PMC.
    if util::str_of(record, "status") == Some("error") {
        return util::nothing(SOURCE, "not_in_pmc");
    }

    let learned = LearnedIds {
        pmcid: util::str_of(record, "pmcid").and_then(ids::clean_pmcid),
        pmid: util::id_of(record, "pmid")
            .as_deref()
            .and_then(ids::clean_pmid),
        doi: if requested.doi.is_none() {
            util::doi_of(record, "doi")
        } else {
            None
        },
        arxiv: None,
    };
    let found = learned.pmcid.is_some() || learned.pmid.is_some() || learned.doi.is_some();
    let status = if found {
        ApiStatus::Found
    } else {
        ApiStatus::Empty
    };
    ApiInspection {
        learned,
        ..super::empty(SOURCE, status)
    }
}

/// ListObjectsV2 listing or S3 error document.
fn xml_shape(body: &[u8]) -> ApiInspection {
    let text = crate::text::decode(body);
    if text.contains("<ListBucketResult") {
        return listing(&text);
    }
    if text.contains("<Error>") {
        return match xml::first_text(&text, "Code").as_deref() {
            Some("NoSuchKey" | "NoSuchBucket") => ApiInspection {
                note: Some("no_such_key".to_owned()),
                ..super::empty(SOURCE, ApiStatus::NotFound)
            },
            _ => super::unparsed(SOURCE, "s3_error"),
        };
    }
    super::unparsed(SOURCE, "not_s3_xml")
}

/// The highest-version `PMC<n>.<v>/PMC<n>.<v>.pdf` key of a listing.
fn listing(text: &str) -> ApiInspection {
    let best = xml::elements(text, "Key")
        .iter()
        .filter_map(|e| {
            let key = xml::text(e.inner);
            pdf_key_version(&key).map(|v| (v, key))
        })
        .fold(None::<(u32, String)>, |best, (v, key)| match best {
            Some((m, _)) if m >= v => best,
            _ => Some((v, key)),
        });
    let Some((_, key)) = best else {
        return util::nothing(SOURCE, "no_pdf");
    };
    let url = format!("{BUCKET_URL}/{key}");
    let mut set = CandidateSet::default();
    set.push(util::candidate(SOURCE, CandKind::Pdf, &url).map(|mut c| {
        c.host_kind = HostKind::Pmc;
        c.landing_url = key
            .split_once('.')
            .map(|(id, _)| format!("https://pmc.ncbi.nlm.nih.gov/articles/{id}/"));
        c.signal = Some("pmc_s3:listing".to_owned());
        c
    }));
    util::finish(SOURCE, LearnedIds::default(), set.into_vec(), None, None)
}

/// The article version of a key shaped `PMC<n>.<v>/PMC<n>.<v>.pdf`.
fn pdf_key_version(key: &str) -> Option<u32> {
    let (dir, file) = key.split_once('/')?;
    let stem = file.strip_suffix(".pdf")?;
    let (digits, version) = dir.strip_prefix("PMC")?.split_once('.')?;
    let well_formed =
        dir == stem && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    well_formed.then(|| version.parse().ok())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp() -> RespMeta {
        RespMeta {
            status: 200,
            ..Default::default()
        }
    }

    #[test]
    fn pdf_keys() {
        assert_eq!(pdf_key_version("PMC2707599.1/PMC2707599.1.pdf"), Some(1));
        assert_eq!(pdf_key_version("PMC2707599.3/PMC2707599.3.pdf"), Some(3));
        for bad in [
            "PMC2707599.1/PMC2707599.1.xml",
            "PMC1.1/PMC2.1.pdf",
            "x/PMC2707599.1.pdf",
            "PMC.1/PMC.1.pdf",
            "metadata/PMC1.1.json",
        ] {
            assert_eq!(pdf_key_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn highest_version_wins() {
        let xml = "<ListBucketResult><Contents><Key>PMC7.1/PMC7.1.pdf</Key></Contents><Contents><Key>PMC7.2/PMC7.2.pdf</Key></Contents><Contents><Key>PMC7.2/PMC7.2.xml</Key></Contents></ListBucketResult>";
        let out = parse(&resp(), xml.as_bytes(), &Ids::default());
        assert_eq!(out.status, ApiStatus::Found);
        assert_eq!(
            out.candidates[0].url,
            "https://pmc-oa-opendata.s3.amazonaws.com/PMC7.2/PMC7.2.pdf"
        );
    }

    #[test]
    fn garbage() {
        for body in [&b""[..], b"\0\0", b"{", b"{}", b"<html>", b"[1]"] {
            assert_eq!(
                parse(&resp(), body, &Ids::default()).status,
                ApiStatus::Unparseable,
                "{body:?}"
            );
        }
    }
}
