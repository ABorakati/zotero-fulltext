//! Pure plugin logic exposed over the JSON ABI. Representative, not complete:
//! the point of the spike is to size and test a realistic mix (serde + url + HTML).

use crate::abi::{CoreResult, err};
use crate::export_json;
use crate::html::{self, Extracted};
use serde::{Deserialize, Serialize};
use url::Url;

// ---------------------------------------------------------------- PDF sniff

/// `%PDF-` within the first 1024 bytes (what Acrobat/pdf.js tolerate).
pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    let head = bytes.get(..bytes.len().min(1024)).unwrap_or_default();
    head.windows(5).any(|w| w == b"%PDF-")
}

// ---------------------------------------------------------------- DOI

#[derive(Deserialize)]
pub struct DoiIn {
    pub text: String,
}

/// Extract and normalise the first DOI in free text or a doi.org URL.
pub fn normalize_doi(input: DoiIn) -> CoreResult<Option<String>> {
    let decoded_input = if input.text.contains('%') { percent_decode(input.text.trim()) } else { input.text };
    let t = decoded_input.trim();
    let lower = t.to_ascii_lowercase();
    let Some(start) = lower.find("10.") else { return Ok(None) };
    let cand = t.get(start..).unwrap_or_default();
    // prefix: 10.<4-9 digits>/
    let slash = match cand.find('/') {
        Some(s) if (4..=10).contains(&s) && cand.get(3..s).is_some_and(|d| d.bytes().all(|b| b.is_ascii_digit() || b == b'.')) => s,
        _ => return Ok(None),
    };
    let suffix: String = cand
        .get(slash + 1..)
        .unwrap_or_default()
        .chars()
        .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '<' | '>'))
        .collect();
    let suffix = suffix.trim_end_matches(['.', ',', ';', ')', ']']);
    if suffix.is_empty() {
        return Ok(None);
    }
    let prefix = cand.get(..slash).unwrap_or_default();
    Ok(Some(format!("{prefix}/{suffix}").to_ascii_lowercase()))
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| s.to_owned())
}

// ---------------------------------------------------------------- EZproxy

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProxyIn {
    /// https://www.nature.com/x -> https://www-nature-com.<suffix>/x
    Hostname { url: String, suffix: String },
    /// <prefix><url-encoded target>, e.g. https://login.<suffix>/login?url=
    Prefix { url: String, prefix: String },
}

pub fn proxy_url(input: ProxyIn) -> CoreResult<String> {
    match input {
        ProxyIn::Hostname { url, suffix } => {
            let mut u = Url::parse(&url).map_err(|e| err("bad_url", e.to_string()))?;
            let host = u.host_str().ok_or_else(|| err("bad_url", "no host"))?.to_owned();
            if host.ends_with(suffix.trim_start_matches('.')) {
                return Ok(u.into()); // already proxied
            }
            // EZproxy "Option HttpsHyphens": '-' -> '--', then '.' -> '-'.
            let mut new_host = host.replace('-', "--").replace('.', "-");
            new_host.push('.');
            new_host.push_str(suffix.trim_start_matches('.'));
            u.set_host(Some(&new_host)).map_err(|e| err("bad_url", e.to_string()))?;
            u.set_scheme("https").map_err(|()| err("bad_url", "scheme"))?;
            Ok(u.into())
        }
        ProxyIn::Prefix { url, prefix } => {
            let _ = Url::parse(&url).map_err(|e| err("bad_url", e.to_string()))?;
            Ok(format!("{prefix}{url}"))
        }
    }
}

// ---------------------------------------------------------------- Unpaywall

#[derive(Deserialize, Default)]
pub struct UnpaywallLocation {
    pub url: Option<String>,
    pub url_for_pdf: Option<String>,
    pub url_for_landing_page: Option<String>,
    pub host_type: Option<String>,
    pub version: Option<String>,
    pub license: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct UnpaywallIn {
    pub is_oa: Option<bool>,
    pub best_oa_location: Option<UnpaywallLocation>,
    #[serde(default)]
    pub oa_locations: Vec<UnpaywallLocation>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Candidate {
    pub url: String,
    pub kind: &'static str,
    pub source: String,
    pub score: i32,
}

pub fn rank_unpaywall(input: UnpaywallIn) -> CoreResult<Vec<Candidate>> {
    let mut out: Vec<Candidate> = Vec::new();
    let locs = input.best_oa_location.into_iter().chain(input.oa_locations);
    for (i, loc) in locs.enumerate() {
        let version_score = match loc.version.as_deref() {
            Some("publishedVersion") => 30,
            Some("acceptedVersion") => 20,
            Some("submittedVersion") => 10,
            _ => 0,
        };
        let host_score = if loc.host_type.as_deref() == Some("publisher") { 5 } else { 0 };
        let base = version_score + host_score - i32::try_from(i).unwrap_or(i32::MAX).min(9);
        if let Some(pdf) = loc.url_for_pdf {
            push_unique(&mut out, Candidate { url: pdf, kind: "pdf", source: "unpaywall".into(), score: base + 50 });
        }
        if let Some(lp) = loc.url_for_landing_page.or(loc.url) {
            push_unique(&mut out, Candidate { url: lp, kind: "landing", source: "unpaywall".into(), score: base });
        }
    }
    out.sort_by(|a, b| b.score.cmp(&a.score));
    Ok(out)
}

fn push_unique(v: &mut Vec<Candidate>, c: Candidate) {
    if !v.iter().any(|x| x.url == c.url) {
        v.push(c);
    }
}

// ---------------------------------------------------------------- landing page -> PDF candidates

#[derive(Deserialize)]
pub struct LandingIn {
    pub html: String,
    pub url: String,
    /// Also return the raw extraction (debugging / tests).
    #[serde(default)]
    pub raw: bool,
}

#[derive(Serialize)]
pub struct LandingOut {
    pub candidates: Vec<Candidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<Extracted>,
}

pub fn landing_pdf_candidates(input: LandingIn) -> CoreResult<LandingOut> {
    let page = Url::parse(&input.url).map_err(|e| err("bad_url", e.to_string()))?;
    let ex = html::extract_default(&input.html);
    let candidates = candidates_from(&ex, &page);
    Ok(LandingOut { candidates, raw: input.raw.then_some(ex) })
}

pub fn candidates_from(ex: &Extracted, page: &Url) -> Vec<Candidate> {
    let base = ex.base_href.as_deref().and_then(|b| page.join(b).ok()).unwrap_or_else(|| page.clone());
    let resolve = |h: &str| -> Option<String> {
        let u = base.join(h).ok()?;
        matches!(u.scheme(), "http" | "https").then(|| u.into())
    };
    let mut out: Vec<Candidate> = Vec::new();
    for m in &ex.metas {
        let score = match m.name.as_str() {
            "citation_pdf_url" => 100,
            "eprints.document_url" | "bepress_citation_pdf_url" => 90,
            "og:url" | "citation_abstract_html_url" | "citation_fulltext_html_url" => continue,
            _ => continue,
        };
        if let Some(u) = resolve(&m.content) {
            push_unique(&mut out, Candidate { url: u, kind: "pdf", source: format!("meta:{}", m.name), score });
        }
    }
    for l in &ex.links {
        if l.r#type.eq_ignore_ascii_case("application/pdf") && let Some(u) = resolve(&l.href) {
            push_unique(&mut out, Candidate { url: u, kind: "pdf", source: "link:alternate".into(), score: 80 });
        }
    }
    for a in &ex.anchors {
        let text = a.text.to_ascii_lowercase();
        let href = a.href.to_ascii_lowercase();
        let mut score = 0;
        if href.ends_with(".pdf") || href.contains(".pdf?") || href.contains("/pdf/") || href.contains("/pdf?") {
            score += 40;
        }
        if text.contains("pdf") {
            score += 30;
        }
        if text.contains("download") || text.contains("full text") {
            score += 10;
        }
        if text.contains("supplement") || href.contains("supplement") || href.contains("/suppl") {
            score -= 50;
        }
        if score >= 40 && let Some(u) = resolve(&a.href) {
            push_unique(&mut out, Candidate { url: u, kind: "pdf?", source: "anchor".into(), score });
        }
    }
    for src in &ex.iframes {
        if let Some(u) = resolve(src) {
            let s = if u.to_ascii_lowercase().contains("pdf") { 60 } else { 5 };
            push_unique(&mut out, Candidate { url: u, kind: "pdf?", source: "iframe".into(), score: s });
        }
    }
    out.sort_by(|a, b| b.score.cmp(&a.score));
    out
}

// ---------------------------------------------------------------- exports

export_json!(fx_normalize_doi, DoiIn, normalize_doi);
export_json!(fx_proxy_url, ProxyIn, proxy_url);
export_json!(fx_rank_unpaywall, UnpaywallIn, rank_unpaywall);
export_json!(fx_landing_pdf_candidates, LandingIn, landing_pdf_candidates);

/// Two-buffer variant: big payload (HTML) travels as raw UTF-8, not inside JSON.
///
/// # Safety
/// Both (ptr, len) pairs must describe buffers obtained from `fx_alloc`.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fx_landing_raw(url_ptr: *const u8, url_len: usize, html_ptr: *const u8, html_len: usize) -> u64 {
    // SAFETY: caller contract.
    let url = unsafe { core::slice::from_raw_parts(url_ptr, url_len) };
    let html = unsafe { core::slice::from_raw_parts(html_ptr, html_len) };
    let r = match (core::str::from_utf8(url), core::str::from_utf8(html)) {
        (Ok(u), Ok(h)) => Url::parse(u)
            .map_err(|e| err("bad_url", e.to_string()))
            .map(|page| LandingOut { candidates: candidates_from(&html::extract_default(h), &page), raw: None }),
        _ => Err(err("bad_utf8", "input is not UTF-8")),
    };
    crate::abi::respond(r)
}
