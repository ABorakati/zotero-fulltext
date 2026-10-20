//! Landing page -> PDF candidates. Owned by core-page (spec: `docs/wp/core-page.md`; heuristics: research/06
//! section 2; oracle: `fixtures/html/expected.json`).
//!
//! Signal order (highest first, with the `Candidate.score` each gets): publisher rule ([`super::rules`], 100 and
//! below) > `citation_pdf_url` (meta name SUFFIX match, `content` or `contents`, first wins, 90) >
//! `<link rel=alternate type=application/pdf>` (80) > other head metas (`wkhealth_pdf_url`, `fulltext_pdf`,
//! `eprints.document_url`, `bepress_citation_pdf_url`, 75) > embedded JSON state (70) > scored anchors (40 to 70) >
//! `iframe` / `embed` / `object` (30). Each candidate carries `signal` (for example `"meta:citation_pdf_url"`,
//! `"anchor:#downloadPdf"`, `"rule:wiley:citation_pdf_url+pdfdirect"`), `kind: Pdf`, `source: Extractor`, and
//! `needs_browser` for hosts known to be bot-walled. The result is deduplicated by [`crate::urls::canonical_key`]
//! (best score wins) and sorted best-first.
//!
//! # Rejections
//!
//! Anchors and frames are checked before they can become candidates; refused URLs that are worth knowing about land in
//! `Extraction.rejected` with a stable `why`:
//!
//! | `why` | meaning |
//! |---|---|
//! | `other_doi` | the URL carries a DOI or PII different from the page's own |
//! | `supplement` | `suppl*`, `MOESM`, `ESM`, `mmc<n>`, `/attachment/`, `_protocol`, `_appendix`, `_disclosure` |
//! | `citation_export` | `ris`, `bib`, `enw`, `nbib`, `refman`, `endnote`, `citation`, `downloadCitation` ... |
//! | `reader_shell` | `/doi/epdf/`, `/doi/reader/` (rewritten by a rule instead) |
//! | `non_http` | `mailto:`, `tel:` and other non-web schemes |
//! | `image` | image or PowerPoint file |
//! | `captcha_variant` | `pdf-with-cover`, `accessCaptcha` |
//! | `javascript_href` | `javascript:` / `#` link without a `data-article-url` |
//! | `other_format` | `.xml`, `.epub`, `.full.txt`, `type=manuscript`, office documents |
//! | `reference_link` | `/servlet/linkout` (a cited work) |
//! | `reference_list` | the anchor sat inside a reference-list region |
//! | `off_host` | a PDF link to another site that does not mention the page's own DOI or PII |
//!
//! `<noscript>` content never reaches [`Doc`], so its false-positive refreshes cannot appear here.
use super::html::{Doc, Element};
use super::rules::{RuleCtx, rule_for};
use super::util::{
    abs_url, base_url, candidate, decoded_lower, find_dois, find_page_doi_raw, find_page_pii,
    find_piis, is_own_doi, json_state_pdf, same_site, unproxied_host_key, upgrade_https,
};
use crate::ids::{clean_doi, same_doi};
use crate::model::{AccessHint, Candidate, Ids, PageIds, Rejected, Route};
use crate::urls::{canonical_key, resolve};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

/// At most this many entries are kept in `Extraction.rejected`.
const MAX_REJECTED: usize = 100;
/// ... of which at most this many are links to cited works / other sites.
const MAX_REJECTED_NOISE: usize = 25;
/// Longest URL reported in `Extraction.rejected`.
const MAX_REJECTED_URL: usize = 512;

/// Input to [`extract`].
#[derive(Clone, Copy, Debug)]
pub struct PageCtx<'a> {
    /// Final URL the HTML was served from (relative hrefs resolve against `<base href>` or this).
    pub url: &'a Url,
    pub requested: &'a Ids,
    pub route: Route,
    pub doc: &'a Doc,
}

/// Output of [`extract`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Extraction {
    pub candidates: Vec<Candidate>,
    pub rejected: Vec<Rejected>,
    pub page_ids: PageIds,
    /// `Some(true/false)` when both the page DOI and the requested DOI are known.
    pub doi_match: Option<bool>,
    pub access: AccessHint,
    /// The publisher rule id that fired, if any.
    pub rule: Option<String>,
}

/// DOI and PII the page claims for itself (`citation_doi`, `dc.identifier` with `scheme=doi`, `prism.doi`, JSON-LD,
/// canonical URL, `/pii/S...` paths). The DOI is lower-case; the PII is the normalised `S` + 16 digits form.
pub fn page_ids(doc: &Doc, url: &Url) -> PageIds {
    PageIds {
        doi: find_page_doi_raw(doc, url).and_then(|d| clean_doi(&d)),
        pii: find_page_pii(doc, url),
    }
}

/// Extract ranked PDF candidates from a parsed page.
pub fn extract(ctx: &PageCtx<'_>) -> Extraction {
    let _ = ctx.route; // reserved: the institutional route currently extracts exactly like the OA route
    let (doc, page) = (ctx.doc, ctx.url);
    let ids = page_ids(doc, page);
    let doi_match = match (ids.doi.as_deref(), ctx.requested.doi.as_deref()) {
        (Some(a), Some(b)) => Some(same_doi(a, b)),
        _ => None,
    };
    let page_host = page.host_str().unwrap_or("").to_ascii_lowercase();
    let rule = rule_for(&unproxied_host_key(&page_host));
    // Without a DOI of its own the page is assumed to be the requested item when screening foreign DOIs.
    let own_doi = ids
        .doi
        .clone()
        .or_else(|| ctx.requested.doi.as_deref().and_then(clean_doi));
    let own = Own {
        doi: own_doi.as_deref(),
        pii: ids.pii.as_deref(),
    };
    let base = base_url(page, doc);

    let mut st = State::default();

    // 1. Publisher rule.
    if let Some(r) = rule {
        let rc = RuleCtx {
            url: page,
            doc,
            page_ids: &ids,
            requested: ctx.requested,
        };
        for c in (r.build)(&rc) {
            st.push_screened(c);
        }
    }
    // 2. citation_pdf_url (name suffix, then "contains" for padded names), first wins.
    if let Some(v) = doc
        .meta_suffix("citation_pdf_url")
        .or_else(|| doc.meta_contains("citation_pdf_url"))
        && let Some(u) = abs_url(&base, v)
    {
        st.push_screened(candidate(
            &u,
            page,
            ids.doi.as_deref(),
            "meta:citation_pdf_url".into(),
            90,
        ));
    }
    // 3. <link rel=alternate type=application/pdf>
    for l in doc
        .links_with("alternate", "application/pdf")
        .filter(|l| !l.in_refs)
    {
        if let Some(u) = l.attr("href").and_then(|h| abs_url(&base, h)) {
            st.push_screened(candidate(
                &u,
                page,
                ids.doi.as_deref(),
                "link:alternate".into(),
                80,
            ));
        }
    }
    // 4. Other head metas.
    for name in [
        "wkhealth_pdf_url",
        "fulltext_pdf",
        "eprints.document_url",
        "bepress_citation_pdf_url",
    ] {
        for v in doc.meta_all(name) {
            if let Some(u) = abs_url(&base, v) {
                st.push_screened(candidate(
                    &u,
                    page,
                    ids.doi.as_deref(),
                    format!("meta:{name}"),
                    75,
                ));
            }
        }
    }
    // 5. Embedded JSON state.
    if let Some(u) = json_state_pdf(doc, page) {
        st.push_screened(candidate(
            &u,
            page,
            ids.doi.as_deref(),
            "json:pdfDownload".into(),
            70,
        ));
    }
    // 6. Scored anchors.
    let ac = AnchorCtx {
        page,
        base: &base,
        own: &own,
        doi: ids.doi.as_deref(),
    };
    for a in &doc.anchors {
        anchor(&mut st, &ac, a);
    }
    // 7. iframe / embed / object.
    for f in &doc.frames {
        frame(&mut st, &ac, f);
    }

    let mut candidates = finalize(st.candidates);
    for c in &mut candidates {
        let host = crate::urls::host_of(&c.url).unwrap_or_default();
        let walled_rule = rule.is_some_and(|r| r.needs_browser) && same_site(&host, &page_host);
        c.needs_browser |= walled_rule || host == "pmc.ncbi.nlm.nih.gov";
    }
    let fired = candidates
        .first()
        .and_then(|c| c.signal.as_deref())
        .and_then(|s| s.strip_prefix("rule:"))
        .and_then(|s| s.split(':').next())
        .map(str::to_owned);
    Extraction {
        access: access_hint(rule.map(|r| r.id), doc),
        candidates,
        rejected: st.rejected,
        page_ids: ids,
        doi_match,
        rule: fired,
    }
}

// ------------------------------------------------------------------ state

/// The identifiers the page claims for itself, used to screen foreign links.
#[derive(Clone, Copy, Debug)]
struct Own<'a> {
    doi: Option<&'a str>,
    pii: Option<&'a str>,
}

#[derive(Default)]
struct State {
    candidates: Vec<Candidate>,
    rejected: Vec<Rejected>,
    seen: BTreeSet<(String, &'static str)>,
    /// How many entries of the "reference noise" kinds (see [`State::reject`]) are in `rejected`.
    noise: usize,
}

impl State {
    /// Record a refused URL (deduplicated, bounded). Links to cited works (`off_host`, `reference_list`, `other_doi`,
    /// `reference_link`) can number in the hundreds on a reference-heavy page, so they share a smaller budget and
    /// cannot crowd out the informative refusals (supplements, citation exports, reader shells).
    fn reject(&mut self, url: &str, why: &'static str) {
        let noisy = matches!(
            why,
            "off_host" | "reference_list" | "other_doi" | "reference_link"
        );
        if self.rejected.len() >= MAX_REJECTED || (noisy && self.noise >= MAX_REJECTED_NOISE) {
            return;
        }
        // Keep the reported URL short (a `mailto:` body can be kilobytes).
        let mut end = url.len().min(MAX_REJECTED_URL);
        while end > 0 && !url.is_char_boundary(end) {
            end -= 1;
        }
        let url = url.get(..end).unwrap_or("");
        if self.seen.insert((url.to_owned(), why)) {
            self.noise += usize::from(noisy);
            self.rejected.push(Rejected {
                url: url.to_owned(),
                why: why.to_owned(),
            });
        }
    }

    /// Add a candidate from a trusted signal (rule, meta, link, JSON) after the lenient screen (image, captcha,
    /// reader shell): the publisher itself vouches for these URLs.
    fn push_screened(&mut self, c: Candidate) {
        match Url::parse(&c.url).ok().and_then(|u| {
            reject_reason(
                &u,
                &Own {
                    doi: None,
                    pii: None,
                },
                false,
            )
        }) {
            Some(why) => self.reject(&c.url, why),
            None => self.candidates.push(c),
        }
    }
}

/// Deduplicate by canonical URL (best score wins, `needs_browser` is ORed) and sort best-first (stable).
fn finalize(cands: Vec<Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::with_capacity(cands.len());
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    for c in cands {
        let key = canonical_key(&c.url);
        match index.get(&key).copied() {
            Some(i) => {
                if let Some(prev) = out.get_mut(i) {
                    let needs = prev.needs_browser || c.needs_browser;
                    if c.score > prev.score {
                        *prev = c;
                    }
                    prev.needs_browser = needs;
                }
            }
            None => {
                index.insert(key, out.len());
                out.push(c);
            }
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.score));
    out
}

// ------------------------------------------------------------------ screening

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "svg", "webp", "tif", "tiff", "bmp", "ico", "ppt", "pptx", "pps",
    "ppsx",
];
const OTHER_FORMAT_EXTS: &[&str] = &[
    "xml", "epub", "txt", "doc", "docx", "xls", "xlsx", "zip", "csv", "mobi", "rtf",
];
const CITATION_TOKENS: &[&str] = &[
    "ris",
    "bib",
    "bibtex",
    "bibtext",
    "enw",
    "nbib",
    "refman",
    "endnote",
    "medlars",
    "refworks",
    "citation",
    "citations",
    "downloadcitation",
];

/// Why a URL must not become a candidate, if it must not. `strict` adds the checks that only make sense for links
/// found in the page body (supplements, citation exports, foreign identifiers, other formats); trusted signals are
/// screened leniently (images, CAPTCHA variants, reader shells).
fn reject_reason(url: &Url, own: &Own<'_>, strict: bool) -> Option<&'static str> {
    let path = url.path().to_ascii_lowercase();
    let pq = decoded_lower(&match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    });
    let segment = path.rsplit('/').next().unwrap_or("");
    let ext = segment.rsplit_once('.').map_or("", |(_, e)| e);

    if IMAGE_EXTS.contains(&ext) || pq.contains("/powerpoint/") || pq.contains("downloadfigure") {
        return Some("image");
    }
    if pq.contains("pdf-with-cover") || pq.contains("accesscaptcha") {
        return Some("captcha_variant");
    }
    if pq.contains("/doi/epdf/") || pq.contains("/doi/reader/") || path.contains("/epdf/") {
        return Some("reader_shell");
    }
    if !strict {
        return None;
    }
    let tokens = || {
        pq.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| !t.is_empty())
    };
    let supplement = tokens().any(|t| {
        (t.starts_with("suppl") && !t.starts_with("supply"))
            || t.starts_with("moesm")
            || matches!(t, "esm" | "attachment" | "attachments")
            || t.strip_prefix("mmc")
                .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
    }) || [
        "_protocol",
        "_appendix",
        "_disclosure",
        "/protocol",
        "/appendix",
        "/disclosure",
    ]
    .iter()
    .any(|m| pq.contains(m));
    if supplement {
        return Some("supplement");
    }
    if tokens().any(|t| CITATION_TOKENS.contains(&t)) || pq.contains("/citation/download") {
        return Some("citation_export");
    }
    if OTHER_FORMAT_EXTS.contains(&ext)
        || pq.contains(".full.txt")
        || pq.contains("type=manuscript")
        || matches!(segment, "xml" | "epub")
    {
        return Some("other_format");
    }
    if foreign_id(&decoded_lower(url.as_str()), own) {
        return Some("other_doi");
    }
    if pq.contains("/servlet/linkout") {
        return Some("reference_link");
    }
    None
}

/// True when the (decoded, lower-case) URL carries a DOI or PII that is not the page's own.
fn foreign_id(decoded: &str, own: &Own<'_>) -> bool {
    let foreign_doi = own
        .doi
        .is_some_and(|doi| find_dois(decoded).iter().any(|d| !is_own_doi(d, doi)));
    let foreign_pii = own
        .pii
        .is_some_and(|pii| find_piis(decoded).iter().any(|p| p != pii));
    foreign_doi || foreign_pii
}

/// True when the URL mentions the page's own DOI or PII.
fn mentions_own_id(decoded: &str, own: &Own<'_>) -> bool {
    own.doi.is_some_and(|d| decoded.contains(d))
        || own
            .pii
            .is_some_and(|p| find_piis(decoded).iter().any(|x| x == p))
}

// ------------------------------------------------------------------ anchors and frames

struct AnchorCtx<'a> {
    page: &'a Url,
    base: &'a Url,
    own: &'a Own<'a>,
    doi: Option<&'a str>,
}

/// Does the URL path or query look like a PDF download (research/06 section 2.2)?
fn href_signal(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    let query = url.query().unwrap_or("").to_ascii_lowercase();
    let pq = if query.is_empty() {
        path.clone()
    } else {
        format!("{path}?{query}")
    };
    path.ends_with(".pdf")
        || pq.contains(".pdf?")
        || path.ends_with("/pdf")
        || path.contains("/pdf/")
        || pq.contains("/pdf?")
        || query.contains("type=printable")
        || query.contains("pdf=render")
        || [
            "/pdfft",
            "/pdfdirect/",
            "/doi/pdf/",
            "/content/pdf/",
            "/article-pdf/",
            "/articlepdf/",
            "/pdfs/journals/",
            "showpdf",
            "downloadpdf.aspx",
            "getpdf",
        ]
        .iter()
        .any(|p| pq.contains(p))
}

/// Attribute evidence: `data-article-pdf`, `data-article-url`, `pdfurl`, a class or id containing `pdf`, or a
/// tracking attribute that names a PDF download.
fn attr_signal(a: &Element) -> bool {
    let has = |n: &str| {
        a.attr(n)
            .is_some_and(|v| v.to_ascii_lowercase().contains("pdf"))
    };
    a.has_attr("data-article-pdf")
        || a.has_attr("data-article-url")
        || a.has_attr("pdfurl")
        || has("class")
        || has("id")
        || has("data-track-action")
        || a.attr("data-ga-label")
            .is_some_and(|v| v.to_ascii_lowercase().starts_with("pdf"))
}

/// Text evidence from the link text, `title` or `aria-label`: `PDF`, `Download PDF`, `View PDF`, `Full Text PDF`.
fn text_signal(a: &Element) -> bool {
    [Some(a.text.as_str()), a.attr("title"), a.attr("aria-label")]
        .into_iter()
        .flatten()
        .any(|t| {
            let t = t.trim().to_ascii_lowercase();
            t == "pdf"
                || t.starts_with("pdf (")
                || [
                    "download pdf",
                    "view pdf",
                    "full text pdf",
                    "full-text pdf",
                    "save pdf",
                    "open pdf",
                ]
                .iter()
                .any(|p| t.contains(p))
        })
}

/// A short stable label for the signal of an anchor: `#id`, else the first class containing `pdf`, else the first class.
fn anchor_label(a: &Element) -> String {
    if let Some(id) = a.attr("id").filter(|i| !i.is_empty()) {
        return format!("anchor:#{id}");
    }
    let classes: Vec<&str> = a
        .attr("class")
        .map(|c| c.split_whitespace().collect())
        .unwrap_or_default();
    let pick = classes
        .iter()
        .find(|c| c.to_ascii_lowercase().contains("pdf"))
        .or_else(|| classes.first());
    match pick {
        Some(c) => format!("anchor:.{c}"),
        None => "anchor:a".to_owned(),
    }
}

fn anchor(st: &mut State, cx: &AnchorCtx<'_>, a: &Element) {
    let raw = a.attr("href").unwrap_or("").trim();
    let lower = raw.to_ascii_lowercase();
    let scripted = raw.is_empty() || raw.starts_with('#') || lower.starts_with("javascript:");
    let data_url = a
        .attr("data-article-url")
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let pdfish_attrs = attr_signal(a) || text_signal(a);

    let effective = if scripted {
        match data_url {
            Some(d) => d,
            None => {
                // MDPI hides its CAPTCHA-gated variants in data-target.
                if let Some(t) = a.attr("data-target").and_then(|t| resolve(cx.base, t))
                    && let Some(why) = reject_reason(&t, cx.own, false)
                {
                    st.reject(t.as_str(), why);
                } else if pdfish_attrs && !raw.is_empty() {
                    st.reject(raw, "javascript_href");
                }
                return;
            }
        }
    } else {
        raw
    };

    let Some(resolved) = resolve(cx.base, effective) else {
        if pdfish_attrs || lower.contains("pdf") {
            st.reject(raw, "non_http");
        }
        return;
    };
    // Candidates are upgraded to https; refusals are reported as the page wrote them.
    let url = upgrade_https(resolved.clone());

    let decoded = decoded_lower(url.as_str());
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let page_host = cx.page.host_str().unwrap_or("").to_ascii_lowercase();
    let signals = Signals {
        href: href_signal(&url),
        own_id: mentions_own_id(&decoded, cx.own),
        attr: attr_signal(a),
        text: text_signal(a),
        download: a.has_attr("download"),
        same_origin: host == page_host,
    };
    // "Worth reporting if refused": anything that mentions PDF in its path, attributes or text.
    let pdfish = signals.href
        || pdfish_attrs
        || url.path().to_ascii_lowercase().contains("pdf")
        || a.text.to_ascii_lowercase().contains("pdf");

    // URL-level screens first (stable `why`s), then the placement screens.
    let why = reject_reason(&url, cx.own, true).or_else(|| {
        if a.in_refs {
            Some("reference_list")
        } else if !same_site(&host, &page_host) && !signals.own_id {
            Some("off_host")
        } else {
            None
        }
    });
    if let Some(why) = why {
        let always = matches!(
            why,
            "supplement" | "citation_export" | "reader_shell" | "captcha_variant" | "other_format"
        );
        if always || pdfish {
            st.reject(resolved.as_str(), why);
        }
        return;
    }

    if !signals.qualifies(same_site(&host, &page_host)) {
        return;
    }
    st.candidates.push(candidate(
        &url,
        cx.page,
        cx.doi,
        anchor_label(a),
        signals.score(),
    ));
}

/// The evidence collected for one anchor.
struct Signals {
    href: bool,
    own_id: bool,
    attr: bool,
    text: bool,
    download: bool,
    same_origin: bool,
}

impl Signals {
    /// A bare `.pdf` link is not enough (cited-work PDFs, recruitment brochures): the URL must look like a PDF AND
    /// something else must agree (own id in the URL, PDF-ish attribute or text, `download` attribute); or the
    /// attributes and the text must both say PDF on the same site.
    fn qualifies(&self, same_site: bool) -> bool {
        (self.href && (self.own_id || self.attr || self.text || self.download))
            || (self.attr && self.text && same_site)
    }

    /// 40 to 70 by evidence.
    fn score(&self) -> i32 {
        let raw = 25 * i32::from(self.href)
            + 15 * i32::from(self.own_id)
            + 10 * i32::from(self.attr)
            + 10 * i32::from(self.text)
            + 5 * i32::from(self.download)
            + 5 * i32::from(self.same_origin);
        raw.clamp(40, 70)
    }
}

fn frame(st: &mut State, cx: &AnchorCtx<'_>, f: &Element) {
    let src = f
        .attr("src")
        .or_else(|| f.attr("data"))
        .unwrap_or("")
        .trim();
    let Some(resolved) = resolve(cx.base, src) else {
        return;
    };
    let url = upgrade_https(resolved.clone());
    let typed_pdf = f
        .attr("type")
        .is_some_and(|t| t.to_ascii_lowercase().contains("pdf"));
    if !(href_signal(&url) || typed_pdf) {
        return;
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let page_host = cx.page.host_str().unwrap_or("").to_ascii_lowercase();
    let own_id = mentions_own_id(&decoded_lower(url.as_str()), cx.own);
    let why = reject_reason(&url, cx.own, true).or_else(|| {
        if f.in_refs {
            Some("reference_list")
        } else if !same_site(&host, &page_host) && !own_id {
            Some("off_host")
        } else {
            None
        }
    });
    match why {
        Some(why) => st.reject(resolved.as_str(), why),
        None => st.candidates.push(candidate(
            &url,
            cx.page,
            cx.doi,
            format!("frame:{}", f.tag),
            30,
        )),
    }
}

// ------------------------------------------------------------------ access hints

/// Free-of-charge access hints from the page itself (no extra request), keyed on the publisher rule of the host.
fn access_hint(rule: Option<&str>, doc: &Doc) -> AccessHint {
    let granted = |yes: bool| {
        if yes {
            AccessHint::Granted
        } else {
            AccessHint::Unknown
        }
    };
    match rule {
        Some("springer-nature") => {
            if doc.anchors.iter().any(|a| a.has_attr("data-article-pdf")) {
                AccessHint::Granted
            } else if doc.meta_suffix("citation_pdf_url").is_some() {
                AccessHint::NoAccess
            } else {
                AccessHint::Unknown
            }
        }
        Some("sciencedirect") => {
            let no_access = [
                "accesscontent",
                "access-options-link-text",
                "check-access-popover",
            ]
            .iter()
            .any(|t| doc.marks_with(t).next().is_some());
            let open = doc.scripts.iter().any(|s| {
                s.src.is_none()
                    && (s.text.contains("\"openAccess\":true")
                        || s.text.contains("\"openAccess\": true"))
            });
            if no_access {
                AccessHint::NoAccess
            } else {
                granted(open)
            }
        }
        Some("silverchair") => granted(
            doc.anchors.iter().any(|a| a.has_class("article-pdfLink"))
                || doc.marks_with("free-access").next().is_some(),
        ),
        Some("wiley") => granted(doc.marks_with("doi-access").any(|m| {
            let t = m.text.to_ascii_lowercase();
            t.contains("free access") || t.contains("open access")
        })),
        Some("atypon") => granted(
            doc.marks
                .iter()
                .any(|m| m.has("access-icon") && m.has("oa")),
        ),
        Some("highwire") => granted(
            doc.meta("citation_access")
                .is_some_and(|v| v.eq_ignore_ascii_case("all"))
                || doc
                    .meta("dc.accessrights")
                    .is_some_and(|v| v.eq_ignore_ascii_case("open-access")),
        ),
        _ => AccessHint::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::html::parse;

    fn run(url: &str, html: &str) -> Extraction {
        let url = Url::parse(url).unwrap();
        let doc = parse(html);
        extract(&PageCtx {
            url: &url,
            requested: &Ids::default(),
            route: Route::Oa,
            doc: &doc,
        })
    }

    fn urls(e: &Extraction) -> Vec<&str> {
        e.candidates.iter().map(|c| c.url.as_str()).collect()
    }

    #[test]
    fn citation_pdf_url_beats_everything_and_is_resolved_and_upgraded() {
        let e = run(
            "https://pub.example.org/a/b?x=1",
            r#"<meta name="citation_doi" content="10.1234/abc"><meta name="DC.citation_pdf_url" contents="http://pub.example.org/x.pdf#frag">
               <meta name="citation_pdf_url" content="/second.pdf"><link rel="alternate" type="application/pdf" href="alt.pdf">
               <a href="/a/anchor.pdf" class="pdf">Download PDF</a>"#,
        );
        // The first meta whose NAME ENDS WITH citation_pdf_url wins, read from `contents`, upgraded, fragment dropped.
        assert_eq!(
            urls(&e),
            vec![
                "https://pub.example.org/x.pdf",
                "https://pub.example.org/a/alt.pdf",
                "https://pub.example.org/a/anchor.pdf"
            ]
        );
        assert_eq!(
            e.candidates[0].signal.as_deref(),
            Some("meta:citation_pdf_url")
        );
        assert_eq!(e.candidates[0].score, 90);
        assert_eq!(e.candidates[1].signal.as_deref(), Some("link:alternate"));
        assert!(
            e.candidates
                .iter()
                .all(|c| c.source == crate::model::Source::Extractor
                    && c.kind == crate::model::CandKind::Pdf)
        );
        assert_eq!(e.candidates[0].record_doi.as_deref(), Some("10.1234/abc"));
        assert_eq!(
            e.candidates[0].landing_url.as_deref(),
            Some("https://pub.example.org/a/b?x=1")
        );
    }

    #[test]
    fn base_href_changes_resolution() {
        let e = run(
            "https://pub.example.org/a/",
            r#"<base href="https://cdn.example.org/files/"><meta name="citation_pdf_url" content="p.pdf">"#,
        );
        assert_eq!(urls(&e), vec!["https://cdn.example.org/files/p.pdf"]);
    }

    #[test]
    fn dedupe_keeps_best_score_and_query_variants() {
        let e = run(
            "https://pub.example.org/article/1",
            r#"<meta name="citation_pdf_url" content="https://pub.example.org/f?id=1&type=printable">
               <a href="/f?id=1&amp;type=printable" class="pdf">PDF</a><a href="/f?id=1&amp;type=other.pdf" class="pdf">PDF</a>"#,
        );
        assert_eq!(e.candidates.len(), 2, "queries distinguish variants");
        assert_eq!(e.candidates[0].score, 90);
    }

    #[test]
    fn anchor_needs_more_than_a_bare_pdf_extension() {
        let e = run(
            "https://pub.example.org/article/1",
            r#"<a href="/brochure.pdf">For Recruiters</a><a href="/doi/pdf/10.1/x" class="show-pdf">Download PDF</a>"#,
        );
        assert_eq!(urls(&e), vec!["https://pub.example.org/doi/pdf/10.1/x"]);
    }

    #[test]
    fn rejects_other_doi_supplement_citation_reader_and_friends_with_stable_reasons() {
        let e = run(
            "https://pub.example.org/article/1",
            r##"<meta name="citation_doi" content="10.1371/journal.pmed.1004377">
               <a href="/file?type=printable&amp;id=10.1371/journal.pmed.1004449" class="pdf">PDF</a>
               <a href="/doi/suppl/10.1371/journal.pmed.1004377/suppl_file/a_protocol.pdf" class="pdf">Download</a>
               <a href="https://media.springernature.com/x/MediaObjects/41598_MOESM1_ESM.pdf">Supplementary</a>
               <a href="/highwire/citation/12/ris" class="citation-link">Download</a>
               <a href="/doi/epdf/10.1371/journal.pmed.1004377" class="pdf">PDF</a>
               <a href="mailto:a@b.org?body=http://x.org/full.pdf">Full text PDF</a>
               <a href="/fig.png" class="pdf">PDF figure</a>
               <a href="/2077/1/pdf-with-cover" class="pdf">PDF</a>
               <a href="#" data-target="/2077/1/pdf-with-cover" class="accessCaptcha pdf">Download PDF with Cover</a>
               <a href="javascript:void(0)" class="pdf-button">x</a>
               <a href="/x.epub" class="pdf">e</a>
               <a href="/servlet/linkout?doi=10.1371/journal.pmed.1004377&amp;key=10.1234/other&amp;cf=pdf">Full Text (PDF)</a>
               <a href="https://ema.europa.eu/report.pdf" class="pdf">https://ema.europa.eu/report.pdf</a>"##,
        );
        assert!(e.candidates.is_empty(), "{:?}", e.candidates);
        let why = |needle: &str| {
            e.rejected
                .iter()
                .find(|r| r.url.contains(needle))
                .map(|r| r.why.as_str())
        };
        assert_eq!(why("1004449"), Some("other_doi"));
        assert_eq!(why("suppl_file"), Some("supplement"));
        assert_eq!(why("MOESM1"), Some("supplement"));
        assert_eq!(why("highwire/citation"), Some("citation_export"));
        assert_eq!(why("doi/epdf"), Some("reader_shell"));
        assert_eq!(why("mailto:"), Some("non_http"));
        assert_eq!(why("fig.png"), Some("image"));
        assert_eq!(why("pdf-with-cover"), Some("captcha_variant"));
        assert_eq!(why("javascript:void"), Some("javascript_href"));
        assert_eq!(why("x.epub"), Some("other_format"));
        assert_eq!(why("servlet/linkout"), Some("other_doi"));
        assert_eq!(why("ema.europa.eu"), Some("off_host"));
    }

    #[test]
    fn rejected_list_is_bounded_and_noise_cannot_crowd_out_informative_refusals() {
        let mut html = String::from(r#"<meta name="citation_doi" content="10.1234/own">"#);
        for i in 0..500 {
            html.push_str(&format!(
                r#"<a href="https://www.other{i}.org/r{i}.pdf" class="pdf">PDF</a>"#
            ));
        }
        html.push_str(r#"<a href="/doi/suppl/10.1234/own/suppl_file/a.pdf" class="pdf">S</a>"#);
        html.push_str(&format!(
            r#"<a href="mailto:a@b.org?body={}">Full text PDF</a>"#,
            "x".repeat(5000)
        ));
        let e = run("https://pub.example.org/article/1", &html);
        assert!(e.candidates.is_empty());
        assert!(e.rejected.len() <= MAX_REJECTED);
        assert_eq!(
            e.rejected.iter().filter(|r| r.why == "off_host").count(),
            MAX_REJECTED_NOISE
        );
        assert!(
            e.rejected.iter().any(|r| r.why == "supplement"),
            "the informative refusal survives the noise"
        );
        assert!(e.rejected.iter().all(|r| r.url.len() <= MAX_REJECTED_URL));
        assert!(e.rejected.iter().any(|r| r.why == "non_http"));
    }

    #[test]
    fn reference_list_anchors_are_dropped() {
        let e = run(
            "https://pub.example.org/article/1",
            r#"<ol class="references"><li><a href="/doi/pdf/10.1/cited" class="pdf">Download PDF</a></li></ol>
               <a href="/doi/pdf/10.1/mine" class="pdf">Download PDF</a>"#,
        );
        assert_eq!(urls(&e), vec!["https://pub.example.org/doi/pdf/10.1/mine"]);
        assert_eq!(e.rejected.len(), 1);
        assert_eq!(e.rejected[0].why, "reference_list");
    }

    #[test]
    fn javascript_href_uses_data_article_url() {
        let e = run(
            "https://jamanetwork.com/journals/jama/fullarticle/1760318",
            r##"<a href="javascript:;" id="pdf-link" data-article-url="/journals/jama/articlepdf/1760318/jsc130006.pdf">PDF</a><a href="#" class="x">top</a>"##,
        );
        assert_eq!(
            urls(&e),
            vec!["https://jamanetwork.com/journals/jama/articlepdf/1760318/jsc130006.pdf"]
        );
        assert!(e.rejected.is_empty());
    }

    #[test]
    fn frames_count_only_when_they_look_like_pdfs() {
        let e = run(
            "https://pub.example.org/article/1",
            r#"<iframe src="https://www.googletagmanager.com/ns.html?id=GTM-1"></iframe><embed src="/files/full.pdf" type="application/pdf"><object data="/viewer?doc=1" type="application/pdf"></object>"#,
        );
        assert_eq!(
            urls(&e),
            vec![
                "https://pub.example.org/files/full.pdf",
                "https://pub.example.org/viewer?doc=1"
            ]
        );
        assert_eq!(e.candidates[0].score, 30);
        assert_eq!(e.candidates[0].signal.as_deref(), Some("frame:embed"));
    }

    #[test]
    fn page_ids_sources() {
        let url =
            Url::parse("https://pub.example.org/science/article/pii/S1743919121000406").unwrap();
        let from = |html: &str| page_ids(&parse(html), &url);
        assert_eq!(
            from(r#"<meta name="citation_doi" content="DOI:10.1016/J.IJSU.2021.105906">"#)
                .doi
                .as_deref(),
            Some("10.1016/j.ijsu.2021.105906")
        );
        assert_eq!(from(r#"<meta name="dc.Identifier" scheme="publisher-id" content="1186828"><meta name="dc.Identifier" scheme="doi" content="10.1080/07853890.2016.1186828">"#).doi.as_deref(), Some("10.1080/07853890.2016.1186828"));
        assert_eq!(
            from(r#"<meta name="prism.doi" content="doi:10.1007/s00464-025-11552-4">"#)
                .doi
                .as_deref(),
            Some("10.1007/s00464-025-11552-4")
        );
        assert_eq!(
            from(r#"<meta name="rft_id" content="info:doi/10.1234/ABC">"#)
                .doi
                .as_deref(),
            Some("10.1234/abc")
        );
        assert_eq!(from(r#"<script type="application/ld+json">{"@type":"Article","mainEntity":{"doi":"10.5555/ld.1"}}</script>"#).doi.as_deref(), Some("10.5555/ld.1"));
        assert_eq!(from(r#"<link rel="canonical" href="https://pub.example.org/article?id=10.1371/journal.pmed.1004377&x=1">"#).doi.as_deref(), Some("10.1371/journal.pmed.1004377"));
        assert_eq!(
            from("").pii.as_deref(),
            Some("S1743919121000406"),
            "PII from the URL"
        );
        assert_eq!(
            from(r#"<meta name="citation_pii" content="S0140-6736(20)30183-5">"#)
                .pii
                .as_deref(),
            Some("S0140673620301835"),
            "citation_pii wins"
        );
        assert_eq!(from("").doi, None);
    }

    #[test]
    fn doi_match_compares_page_and_requested() {
        let url = Url::parse("https://pub.example.org/a").unwrap();
        let doc = parse(r#"<meta name="citation_doi" content="10.1000/Same">"#);
        let go = |requested: Option<&str>| {
            let ids = Ids {
                doi: requested.map(str::to_owned),
                ..Default::default()
            };
            extract(&PageCtx {
                url: &url,
                requested: &ids,
                route: Route::Oa,
                doc: &doc,
            })
            .doi_match
        };
        assert_eq!(go(Some("10.1000/same")), Some(true));
        assert_eq!(go(Some("10.1000/other")), Some(false));
        assert_eq!(go(None), None);
    }

    #[test]
    fn proxied_pages_match_rules_and_keep_their_origin() {
        let e = run(
            "https://www-nejm-org.kcl.idm.oclc.org/doi/full/10.1056/NEJMoa2002032",
            r#"<meta name="dc.Identifier" scheme="doi" content="10.1056/NEJMoa2002032">"#,
        );
        assert_eq!(
            urls(&e)[0],
            "https://www-nejm-org.kcl.idm.oclc.org/doi/pdf/10.1056/NEJMoa2002032"
        );
        assert_eq!(e.rule.as_deref(), Some("atypon"));
    }

    #[test]
    fn access_hints_by_publisher() {
        let go = |url: &str, html: &str| run(url, html).access;
        assert_eq!(
            go(
                "https://link.springer.com/article/1",
                r#"<meta name=citation_pdf_url content="/a.pdf">"#
            ),
            AccessHint::NoAccess
        );
        assert_eq!(
            go(
                "https://link.springer.com/article/1",
                r#"<meta name=citation_pdf_url content="/a.pdf"><a data-article-pdf="true" href="/a.pdf">PDF</a>"#
            ),
            AccessHint::Granted
        );
        assert_eq!(
            go(
                "https://www.sciencedirect.com/science/article/pii/S1743919121000406",
                r#"<div class="accessContent">buy</div>"#
            ),
            AccessHint::NoAccess
        );
        assert_eq!(
            go(
                "https://www.sciencedirect.com/science/article/pii/S1743919121000406",
                r#"<script>x={"openAccess":true}</script>"#
            ),
            AccessHint::Granted
        );
        assert_eq!(
            go(
                "https://karger.com/crg/article/1",
                r#"<a class="al-link article-pdfLink" href="/x.pdf">p</a>"#
            ),
            AccessHint::Granted
        );
        assert_eq!(
            go(
                "https://jamanetwork.com/journals/jama/fullarticle/1",
                r#"<div class="free-access">free</div>"#
            ),
            AccessHint::Granted
        );
        assert_eq!(
            go(
                "https://onlinelibrary.wiley.com/doi/full/10.1002/x",
                r#"<div class="doi-access"><span>Open Access</span></div>"#
            ),
            AccessHint::Granted
        );
        assert_eq!(
            go(
                "https://www.tandfonline.com/doi/full/10.1080/x.1",
                r#"<span class="access-icon oa"></span>"#
            ),
            AccessHint::Granted
        );
        assert_eq!(
            go(
                "https://www.bmj.com/content/372/bmj.n71",
                r#"<meta name="citation_access" content="all">"#
            ),
            AccessHint::Granted
        );
        assert_eq!(
            go(
                "https://www.bmj.com/content/372/bmj.n71",
                r#"<meta name="DC.AccessRights" content="open-access">"#
            ),
            AccessHint::Granted
        );
        assert_eq!(
            go(
                "https://pub.example.org/x",
                r#"<meta name="citation_access" content="all">"#
            ),
            AccessHint::Unknown
        );
    }

    #[test]
    fn empty_and_garbage_pages_extract_nothing() {
        let e = run("https://pub.example.org/", "");
        assert!(e.candidates.is_empty() && e.rejected.is_empty() && e.rule.is_none());
        let _ = run(
            "https://pub.example.org/",
            "<a href=\"\u{0}\"><link href=><meta name=citation_pdf_url content=\"::::\">",
        );
    }
}
