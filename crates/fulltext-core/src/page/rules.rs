//! Per-publisher PDF-link rules, keyed on the UNPROXIED host. Owned by core-page (spec: `docs/wp/core-page.md`;
//! table: research/SUMMARY.md section (e), research/06 section 3).
//!
//! A rule may replace the generic `citation_pdf_url` answer (Wiley `/doi/pdf/` -> `/doi/pdfdirect/`), derive a URL
//! from the DOI (`/doi/pdf/<doi>` on Atypon sites that omit `citation_pdf_url`), or read an unusual signal
//! (LWW `wkhealth_pdf_url`, ScienceDirect `window.__PRELOADED_STATE__` JSON, JAMA `a#pdf-link[data-article-url]`).
//! Hosts are matched by [`crate::urls::host_key`] after [`super::util::unproxied_host_key`] removed an EZproxy suffix,
//! so a rule also applies to the proxified form of the host (`www-nejm-org.kcl.idm.oclc.org` -> `www-nejm-org`);
//! [`rule_for`] additionally accepts a key followed by `-` / `.` (an unstripped proxy suffix) and a key preceded by
//! `-` (a subdomain: `bmjopen-bmj-com` matches `bmj-com`, `www-karger-com` matches `karger-com`).
//!
//! Candidates are built on the page's OWN origin (a proxified page stays proxified) and carry the signal
//! `rule:<id>:<what>` and a score of 100 for the primary answer, 99 and below for alternates. A rule never hides a
//! better generic signal for its own host: [`super::extract`] still runs the generic pipeline and keeps the best
//! score per URL. Rules never emit signed `pdf.sciencedirectassets.com` URLs.
use super::classify::reader_rewrite;
use super::html::Doc;
use super::util::{
    abs_url, base_url, candidate, find_page_doi_raw, on_origin, pii_punctuated, unproxied_host_key,
};
use crate::model::{Candidate, Ids, PageIds};
use url::Url;

/// What a rule can see.
#[derive(Clone, Copy, Debug)]
pub struct RuleCtx<'a> {
    /// The page URL (the real publisher URL, un-proxified by the caller when possible).
    pub url: &'a Url,
    pub doc: &'a Doc,
    pub page_ids: &'a PageIds,
    pub requested: &'a Ids,
}

/// One publisher family.
#[derive(Debug)]
pub struct Rule {
    /// Stable id reported in `PageInspection.rule` (`"wiley"`, `"sciencedirect"`, `"atypon"`, ...).
    pub id: &'static str,
    /// Host keys (`www-sciencedirect-com`) this rule applies to; a proxified host key `starts_with` matches too.
    pub host_keys: &'static [&'static str],
    /// True when plain HTTP cannot fetch this family's PDFs (JS challenge, bot wall): its same-site candidates are
    /// marked `needs_browser`.
    pub needs_browser: bool,
    /// Produces candidates with `signal` set to the rule id plus the signal used (`"rule:wiley:citation_pdf_url+pdfdirect"`).
    pub build: fn(&RuleCtx<'_>) -> Vec<Candidate>,
}

/// All rules, most specific first.
pub static RULES: &[Rule] = &[
    Rule {
        id: "sciencedirect",
        host_keys: &["www-sciencedirect-com"],
        needs_browser: true,
        build: sciencedirect,
    },
    Rule {
        id: "elsevier-health",
        host_keys: &["www-thelancet-com", "www-cell-com"],
        needs_browser: false,
        build: elsevier_health,
    },
    Rule {
        id: "wiley",
        host_keys: &["onlinelibrary-wiley-com"],
        needs_browser: false,
        build: wiley,
    },
    Rule {
        id: "atypon",
        host_keys: &[
            "www-tandfonline-com",
            "journals-sagepub-com",
            "www-nejm-org",
            "www-atsjournals-org",
            "www-liebertpub-com",
        ],
        needs_browser: false,
        build: atypon,
    },
    Rule {
        id: "springer-nature",
        host_keys: &[
            "link-springer-com",
            "nature-com",
            "biomedcentral-com",
            "springeropen-com",
        ],
        needs_browser: false,
        build: springer_nature,
    },
    Rule {
        id: "silverchair",
        host_keys: &["academic-oup-com", "jamanetwork-com", "karger-com"],
        needs_browser: false,
        build: silverchair,
    },
    Rule {
        id: "lww",
        host_keys: &["journals-lww-com"],
        needs_browser: true,
        build: lww,
    },
    Rule {
        id: "highwire",
        host_keys: &["www-bmj-com", "bmj-com"],
        needs_browser: false,
        build: highwire,
    },
    Rule {
        id: "walled-misc",
        host_keys: &[
            "www-thieme-connect-com",
            "www-thieme-connect-de",
            "www-mdpi-com",
            "pmc-ncbi-nlm-nih-gov",
        ],
        needs_browser: true,
        build: walled_misc,
    },
    Rule {
        id: "open-misc",
        host_keys: &[
            "journals-plos-org",
            "www-frontiersin-org",
            "www-cambridge-org",
        ],
        needs_browser: false,
        build: open_misc,
    },
];

/// The rule for a host key (exact, proxified-prefix or subdomain-suffix match).
pub fn rule_for(host_key: &str) -> Option<&'static Rule> {
    fn matches(host_key: &str, k: &str) -> bool {
        host_key == k
            || host_key
                .strip_prefix(k)
                .is_some_and(|rest| rest.starts_with(['-', '.']))
            || host_key
                .strip_suffix(k)
                .is_some_and(|rest| rest.ends_with('-'))
    }
    RULES
        .iter()
        .find(|r| r.host_keys.iter().any(|k| matches(host_key, k)))
}

// ------------------------------------------------------------------ helpers

impl RuleCtx<'_> {
    fn base(&self) -> Url {
        base_url(self.url, self.doc)
    }

    /// `citation_pdf_url` (name suffix, then "contains"), resolved against the base and upgraded to https.
    fn citation_pdf(&self) -> Option<Url> {
        let v = self
            .doc
            .meta_suffix("citation_pdf_url")
            .or_else(|| self.doc.meta_contains("citation_pdf_url"))?;
        abs_url(&self.base(), v)
    }

    /// The page DOI with its original letter case.
    fn raw_doi(&self) -> Option<String> {
        find_page_doi_raw(self.doc, self.url)
    }

    fn cand(&self, url: &Url, signal: &str, score: i32) -> Candidate {
        candidate(
            url,
            self.url,
            self.page_ids.doi.as_deref(),
            signal.to_owned(),
            score,
        )
    }

    /// True when the unproxied host key of the page ends with `suffix` (`karger-com`).
    fn host_is(&self, suffix: &str) -> bool {
        unproxied_host_key(self.url.host_str().unwrap_or("")).ends_with(suffix)
    }

    /// The same URL with `?download=true`.
    fn with_download(u: &Url) -> Url {
        let mut d = u.clone();
        d.set_query(Some("download=true"));
        d
    }
}

// ------------------------------------------------------------------ rules

/// ScienceDirect: embedded JSON state first, then the canonical `/pdfft` forms built from the PII. Signed
/// `pdf.sciencedirectassets.com` URLs are never produced (they are fetched on the spot by the browser path).
fn sciencedirect(c: &RuleCtx<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(u) = super::util::json_state_pdf(c.doc, c.url) {
        out.push(c.cand(&u, "rule:sciencedirect:json_state", 100));
    }
    if let Some(pii) = c.page_ids.pii.as_deref() {
        for (score, query) in [(99, "download=true"), (98, "isDTMRedir=true&download=true")] {
            if let Some(u) = on_origin(c.url, &format!("/science/article/pii/{pii}/pdfft?{query}"))
            {
                out.push(c.cand(&u, "rule:sciencedirect:pdfft", score));
            }
        }
    }
    out
}

/// The Lancet and Cell: `citation_pdf_url`, then the `showPdf?pii=` button target. Cited-work `servlet/linkout` links
/// and `mmc<n>` supplements are refused by the generic screen.
fn elsevier_health(c: &RuleCtx<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(u) = c.citation_pdf() {
        out.push(c.cand(&u, "rule:elsevier-health:citation_pdf_url", 100));
    }
    if let Some(pii) = c.page_ids.pii.as_deref().and_then(pii_punctuated) {
        let encoded = pii.replace('(', "%28").replace(')', "%29");
        if let Some(u) = on_origin(c.url, &format!("/action/showPdf?pii={encoded}")) {
            out.push(c.cand(&u, "rule:elsevier-health:showPdf", 99));
        }
    }
    out
}

/// Wiley: `/doi/pdf/<doi>` (or the ePDF reader) from `citation_pdf_url` becomes `/doi/pdfdirect/<doi>`, plus the
/// `?download=true` form. Never `/doi/epdf/`.
fn wiley(c: &RuleCtx<'_>) -> Vec<Candidate> {
    let direct = match c.citation_pdf() {
        Some(mut u) => {
            let path = u.path().to_owned();
            if let Some(marker) = ["/doi/pdf/", "/doi/epdf/"]
                .into_iter()
                .find(|m| path.contains(m))
            {
                u.set_path(&path.replacen(marker, "/doi/pdfdirect/", 1));
            }
            Some((u, "rule:wiley:citation_pdf_url+pdfdirect"))
        }
        None => c
            .raw_doi()
            .and_then(|d| on_origin(c.url, &format!("/doi/pdfdirect/{d}")))
            .map(|u| (u, "rule:wiley:doi+pdfdirect")),
    };
    let mut out = Vec::new();
    if let Some((u, signal)) = direct {
        out.push(c.cand(
            &RuleCtx::with_download(&u),
            "rule:wiley:pdfdirect+download",
            99,
        ));
        out.insert(0, c.cand(&u, signal, 100));
    }
    out
}

/// Atypon sites without `citation_pdf_url` (T&F, SAGE, NEJM, ATS, Liebert): `/doi/pdf/<doi>` and its `?download=true`
/// form; `/doi/reader/` and `/doi/epdf/` links of THIS article are rewritten to `/doi/pdf/`.
fn atypon(c: &RuleCtx<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    if c.doc.meta_suffix("citation_pdf_url").is_some() {
        return out; // the generic pipeline handles pages that do carry the meta
    }
    if let Some(u) = c
        .raw_doi()
        .and_then(|d| on_origin(c.url, &format!("/doi/pdf/{d}")))
    {
        out.push(c.cand(
            &RuleCtx::with_download(&u),
            "rule:atypon:doi+pdf+download",
            99,
        ));
        out.insert(0, c.cand(&u, "rule:atypon:doi+pdf", 100));
    }
    let base = c.base();
    let own = c.page_ids.doi.as_deref();
    for a in c.doc.anchors.iter().filter(|a| !a.in_refs) {
        let rewritten = a
            .attr("href")
            .and_then(|h| abs_url(&base, h))
            .and_then(|u| reader_rewrite(&u))
            .and_then(|s| Url::parse(&s).ok())
            .filter(|r| r.path().contains("/doi/pdf"))
            .filter(|r| own.is_none_or(|d| r.path().to_ascii_lowercase().contains(d)));
        if let Some(r) = rewritten {
            out.push(c.cand(&r, "rule:atypon:reader_rewrite", 98));
        }
    }
    out
}

/// Springer Link, Nature, BMC: `citation_pdf_url`, then the `a[data-article-pdf]` download button (supplements on
/// `media.springernature.com` are never taken).
fn springer_nature(c: &RuleCtx<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(u) = c.citation_pdf() {
        out.push(c.cand(&u, "rule:springer-nature:citation_pdf_url", 100));
    }
    let base = c.base();
    for a in c
        .doc
        .anchors
        .iter()
        .filter(|a| !a.in_refs && a.has_attr("data-article-pdf"))
    {
        if let Some(u) = a
            .attr("href")
            .and_then(|h| abs_url(&base, h))
            .filter(|u| u.host_str() != Some("media.springernature.com"))
        {
            out.push(c.cand(&u, "rule:springer-nature:data-article-pdf", 99));
        }
    }
    out
}

/// OUP, JAMA, Karger (Silverchair): `citation_pdf_url`; JAMA's `a#pdf-link[data-article-url]`; Karger's DOI-based
/// `article-pdf/doi/<doi>` alternate.
fn silverchair(c: &RuleCtx<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(u) = c.citation_pdf() {
        out.push(c.cand(&u, "rule:silverchair:citation_pdf_url", 100));
    }
    let base = c.base();
    if let Some(a) = c.doc.anchor_by_id("pdf-link") {
        let href = a
            .attr("data-article-url")
            .filter(|s| !s.trim().is_empty())
            .or_else(|| a.attr("href"));
        if let Some(u) = href.and_then(|h| abs_url(&base, h)) {
            out.push(c.cand(&u, "rule:silverchair:pdf-link", 99));
        }
    }
    if c.host_is("karger-com")
        && let Some(u) = c
            .raw_doi()
            .and_then(|d| on_origin(c.url, &format!("/article-pdf/doi/{d}")))
    {
        out.push(c.cand(&u, "rule:silverchair:karger_doi", 97));
    }
    out
}

/// LWW: only `meta[name=wkhealth_pdf_url]` is usable (a Cloudflare-walled `downloadpdf.aspx` URL).
fn lww(c: &RuleCtx<'_>) -> Vec<Candidate> {
    c.doc
        .meta("wkhealth_pdf_url")
        .and_then(|v| abs_url(&c.base(), v))
        .map(|u| c.cand(&u, "rule:lww:wkhealth_pdf_url", 100))
        .into_iter()
        .collect()
}

/// BMJ (HighWire): `citation_pdf_url`, then `<link rel=alternate type=application/pdf>`.
fn highwire(c: &RuleCtx<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(u) = c.citation_pdf() {
        out.push(c.cand(&u, "rule:highwire:citation_pdf_url", 100));
    }
    let base = c.base();
    for l in c.doc.links_with("alternate", "application/pdf") {
        if let Some(u) = l.attr("href").and_then(|h| abs_url(&base, h)) {
            out.push(c.cand(&u, "rule:highwire:link_alternate", 99));
        }
    }
    out
}

/// Thieme, MDPI, PMC: `citation_pdf_url` (Thieme's `http://` is upgraded, MDPI keeps `?version=`, the CAPTCHA-gated
/// `pdf-with-cover` is screened out by [`super::extract`]). The host needs a real browser.
fn walled_misc(c: &RuleCtx<'_>) -> Vec<Candidate> {
    c.citation_pdf()
        .map(|u| c.cand(&u, "rule:walled-misc:citation_pdf_url", 100))
        .into_iter()
        .collect()
}

/// PLOS, Frontiers, Cambridge: `citation_pdf_url`; for Cambridge also the same file without the cosmetic trailing
/// slug (`.../content/view/<hash>/<pii>a.pdf/<slug>.pdf` -> `.../<pii>a.pdf`).
fn open_misc(c: &RuleCtx<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(u) = c.citation_pdf() {
        if c.host_is("cambridge-org")
            && let Some(bare) = without_slug(&u)
        {
            out.push(c.cand(&bare, "rule:open-misc:cambridge_without_slug", 95));
        }
        out.insert(0, c.cand(&u, "rule:open-misc:citation_pdf_url", 100));
    }
    out
}

fn without_slug(u: &Url) -> Option<Url> {
    let (head, last) = u.path().rsplit_once('/')?;
    let (_, previous) = head.rsplit_once('/')?;
    if !last.to_ascii_lowercase().ends_with(".pdf")
        || !previous.to_ascii_lowercase().ends_with(".pdf")
    {
        return None;
    }
    let mut bare = u.clone();
    bare.set_path(head);
    Some(bare)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::extract::{PageCtx, extract};
    use crate::page::html::parse;

    fn run(url: &str, html: &str) -> (Vec<String>, Option<String>) {
        let url = Url::parse(url).unwrap();
        let doc = parse(html);
        let e = extract(&PageCtx {
            url: &url,
            requested: &Ids::default(),
            route: crate::model::Route::Oa,
            doc: &doc,
        });
        (e.candidates.iter().map(|c| c.url.clone()).collect(), e.rule)
    }

    #[test]
    fn host_key_matching() {
        assert_eq!(rule_for("www-nejm-org").map(|r| r.id), Some("atypon"));
        assert_eq!(
            rule_for("www-nejm-org-kcl-idm-oclc-org").map(|r| r.id),
            Some("atypon"),
            "unstripped proxy suffix matches by prefix"
        );
        assert_eq!(
            rule_for("bmjopen-bmj-com").map(|r| r.id),
            Some("highwire"),
            "subdomain matches by suffix"
        );
        assert_eq!(
            rule_for("www-karger-com").map(|r| r.id),
            Some("silverchair")
        );
        assert_eq!(
            rule_for("www-nature-com").map(|r| r.id),
            Some("springer-nature")
        );
        assert_eq!(
            rule_for("bmcsurg-biomedcentral-com").map(|r| r.id),
            Some("springer-nature")
        );
        assert!(rule_for("www-bionature-com").is_none());
        assert!(rule_for("example-org").is_none());
        assert!(rule_for("").is_none());
    }

    #[test]
    fn table_is_consistent() {
        let mut ids: Vec<&str> = RULES.iter().map(|r| r.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), RULES.len(), "rule ids are unique");
        assert_eq!(RULES.len(), 10);
        assert!(RULES.iter().all(|r| {
            !r.host_keys.is_empty()
                && r.host_keys
                    .iter()
                    .all(|k| !k.contains('.') && *k == k.to_ascii_lowercase())
        }));
        assert!(RULES.iter().filter(|r| r.needs_browser).map(|r| r.id).eq([
            "sciencedirect",
            "lww",
            "walled-misc"
        ]));
    }

    #[test]
    fn wiley_rewrites_to_pdfdirect_and_never_epdf() {
        let (urls, rule) = run(
            "https://onlinelibrary.wiley.com/doi/full/10.1002/wjs.12026",
            r#"<meta name="citation_doi" content="10.1002/wjs.12026"><meta name="citation_pdf_url" content="https://onlinelibrary.wiley.com/doi/pdf/10.1002/wjs.12026">
               <a href="/doi/epdf/10.1002/wjs.12026" class="pdf-download">PDF</a>"#,
        );
        assert_eq!(
            urls[0],
            "https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/wjs.12026"
        );
        assert!(
            urls.contains(
                &"https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/wjs.12026?download=true"
                    .to_owned()
            )
        );
        assert!(
            urls.contains(&"https://onlinelibrary.wiley.com/doi/pdf/10.1002/wjs.12026".to_owned()),
            "the original stays as a lower-ranked alternate"
        );
        assert!(urls.iter().all(|u| !u.contains("/epdf/")));
        assert_eq!(rule.as_deref(), Some("wiley"));
        // Without citation_pdf_url the DOI builds it.
        let (urls, _) = run(
            "https://onlinelibrary.wiley.com/doi/full/10.1002/wjs.12026",
            r#"<meta name="citation_doi" content="10.1002/wjs.12026">"#,
        );
        assert_eq!(
            urls[0],
            "https://onlinelibrary.wiley.com/doi/pdfdirect/10.1002/wjs.12026"
        );
    }

    #[test]
    fn atypon_derives_doi_pdf_and_keeps_doi_case() {
        let (urls, rule) = run(
            "https://www.nejm.org/doi/full/10.1056/NEJMoa2002032",
            r#"<meta name="dc.Identifier" scheme="doi" content="10.1056/NEJMoa2002032"><a href="/doi/reader/10.1056/NEJMoa2002032">Reader</a><a href="/doi/reader/10.1056/OTHER1">Other</a>"#,
        );
        assert_eq!(
            urls[0],
            "https://www.nejm.org/doi/pdf/10.1056/NEJMoa2002032"
        );
        assert_eq!(
            urls[1],
            "https://www.nejm.org/doi/pdf/10.1056/NEJMoa2002032?download=true"
        );
        assert_eq!(
            urls.len(),
            2,
            "the reader link of another article is not rewritten: {urls:?}"
        );
        assert_eq!(rule.as_deref(), Some("atypon"));
        // With citation_pdf_url the rule stays out of the way.
        let (urls, rule) = run(
            "https://www.atsjournals.org/doi/full/10.1164/x.1",
            r#"<meta name="citation_doi" content="10.1164/x.1"><meta name="citation_pdf_url" content="/doi/pdf/10.1164/x.1">"#,
        );
        assert_eq!(urls[0], "https://www.atsjournals.org/doi/pdf/10.1164/x.1");
        assert_eq!(rule, None);
    }

    #[test]
    fn sciencedirect_prefers_json_then_pdfft_forms() {
        let html = r#"<meta name="citation_pii" content="S1743919121000406"><script>window.__PRELOADED_STATE__ = {"article":{"pdfDownload":{"urlMetadata":{"queryParams":{"md5":"abc","pid":"1-s2.0-S1743919121000406-main.pdf"},"pii":"S1743919121000406","pdfExtension":"/pdfft","path":"science/article/pii"}}}};</script>"#;
        let (urls, rule) = run(
            "https://www.sciencedirect.com/science/article/pii/S1743919121000406",
            html,
        );
        assert_eq!(
            urls[0],
            "https://www.sciencedirect.com/science/article/pii/S1743919121000406/pdfft?md5=abc&pid=1-s2.0-S1743919121000406-main.pdf"
        );
        assert_eq!(
            urls[1],
            "https://www.sciencedirect.com/science/article/pii/S1743919121000406/pdfft?download=true"
        );
        assert_eq!(
            urls[2],
            "https://www.sciencedirect.com/science/article/pii/S1743919121000406/pdfft?isDTMRedir=true&download=true"
        );
        assert_eq!(rule.as_deref(), Some("sciencedirect"));
        let (urls, _) = run(
            "https://www.sciencedirect.com/science/article/pii/S1743919121000406",
            "",
        );
        assert_eq!(
            urls[0],
            "https://www.sciencedirect.com/science/article/pii/S1743919121000406/pdfft?download=true",
            "PII from the URL when the JSON is missing"
        );
    }

    #[test]
    fn elsevier_health_adds_show_pdf() {
        let (urls, rule) = run(
            "https://www.thelancet.com/journals/lancet/article/PIIS0140-6736(20)30183-5/fulltext",
            r#"<meta name="citation_pdf_url" content="https://www.thelancet.com/pdfs/journals/lancet/PIIS0140-6736(20)30183-5.pdf">"#,
        );
        assert_eq!(
            urls[0],
            "https://www.thelancet.com/pdfs/journals/lancet/PIIS0140-6736(20)30183-5.pdf"
        );
        assert_eq!(
            urls[1],
            "https://www.thelancet.com/action/showPdf?pii=S0140-6736%2820%2930183-5"
        );
        assert_eq!(rule.as_deref(), Some("elsevier-health"));
    }

    #[test]
    fn silverchair_jama_and_karger() {
        let (urls, _) = run(
            "https://jamanetwork.com/journals/jama/fullarticle/1760318",
            r#"<a href="javascript:;" id="pdf-link" data-article-url="/journals/jama/articlepdf/1760318/jsc130006.pdf">PDF</a>"#,
        );
        assert_eq!(
            urls,
            vec!["https://jamanetwork.com/journals/jama/articlepdf/1760318/jsc130006.pdf"]
        );
        let (urls, rule) = run(
            "https://karger.com/crg/article/1/1/21/87289/Peptic",
            r#"<meta name="citation_doi" content="10.1159/000104224"><meta name="citation_pdf_url" content="https://karger.com/crg/article-pdf/1/1/21/2505605/000104224.pdf">"#,
        );
        assert_eq!(
            urls,
            vec![
                "https://karger.com/crg/article-pdf/1/1/21/2505605/000104224.pdf",
                "https://karger.com/article-pdf/doi/10.1159/000104224"
            ]
        );
        assert_eq!(rule.as_deref(), Some("silverchair"));
    }

    #[test]
    fn lww_reads_wkhealth_pdf_url_and_needs_a_browser() {
        let url =
            Url::parse("https://journals.lww.com/annalsofsurgery/fulltext/2023/01000/x.51.aspx")
                .unwrap();
        let doc = parse(
            r#"<meta name="wkhealth_pdf_url" content="https://journals.lww.com/annalsofsurgery/_layouts/15/oaks.journals/downloadpdf.aspx?trckng_src_pg=Other&amp;an=00000658-202301000-00051">"#,
        );
        let e = extract(&PageCtx {
            url: &url,
            requested: &Ids::default(),
            route: crate::model::Route::Oa,
            doc: &doc,
        });
        assert_eq!(e.candidates.len(), 1);
        assert!(e.candidates[0].needs_browser);
        assert_eq!(
            e.candidates[0].url,
            "https://journals.lww.com/annalsofsurgery/_layouts/15/oaks.journals/downloadpdf.aspx?trckng_src_pg=Other&an=00000658-202301000-00051"
        );
        assert_eq!(e.rule.as_deref(), Some("lww"));
    }

    #[test]
    fn highwire_takes_the_alternate_link_too() {
        let (urls, rule) = run(
            "https://www.bmj.com/content/372/bmj.n71",
            r#"<meta name="citation_pdf_url" content="https://www.bmj.com/content/bmj/372/bmj.n71.full.pdf"><link rel="alternate" type="application/pdf" href="/content/372/bmj.n71.full.pdf">"#,
        );
        assert_eq!(
            urls,
            vec![
                "https://www.bmj.com/content/bmj/372/bmj.n71.full.pdf",
                "https://www.bmj.com/content/372/bmj.n71.full.pdf"
            ]
        );
        assert_eq!(rule.as_deref(), Some("highwire"));
    }

    #[test]
    fn walled_misc_upgrades_thieme_and_keeps_mdpi_version() {
        let (urls, _) = run(
            "https://www.thieme-connect.com/products/ejournals/html/10.1055/a-0495-1582",
            r#"<meta name="citation_pdf_url" content="http://www.thieme-connect.com/products/ejournals/pdf/10.1055/a-0495-1582.pdf">"#,
        );
        assert_eq!(
            urls,
            vec!["https://www.thieme-connect.com/products/ejournals/pdf/10.1055/a-0495-1582.pdf"]
        );
        let (urls, _) = run(
            "https://www.mdpi.com/2077-0383/9/4/1225",
            r#"<meta name="citation_pdf_url" content="https://www.mdpi.com/2077-0383/9/4/1225/pdf?version=1587732386"><meta name="citation_pdf_url" content="https://www.mdpi.com/2077-0383/9/4/1225/pdf-with-cover">"#,
        );
        assert_eq!(
            urls,
            vec!["https://www.mdpi.com/2077-0383/9/4/1225/pdf?version=1587732386"]
        );
    }

    #[test]
    fn cambridge_matches_padded_meta_names_and_adds_the_bare_file() {
        let (urls, rule) = run(
            "https://www.cambridge.org/core/journals/bjo/article/slug/2524DD3D",
            r#"<meta name=" citation_pdf_url x" content="/core/services/aop-cambridge-core/content/view/2524DD3D/S2056a.pdf/div-slug-div.pdf">"#,
        );
        assert_eq!(
            urls,
            vec![
                "https://www.cambridge.org/core/services/aop-cambridge-core/content/view/2524DD3D/S2056a.pdf/div-slug-div.pdf",
                "https://www.cambridge.org/core/services/aop-cambridge-core/content/view/2524DD3D/S2056a.pdf",
            ]
        );
        assert_eq!(rule.as_deref(), Some("open-misc"));
    }

    #[test]
    fn springer_nature_button_and_no_supplements() {
        let (urls, rule) = run(
            "https://www.nature.com/articles/s41598-024-73565-8",
            r#"<meta name="citation_pdf_url" content="https://www.nature.com/articles/s41598-024-73565-8.pdf">
               <a href="/articles/s41598-024-73565-8.pdf" data-article-pdf="true">Download PDF</a>
               <a href="https://media.springernature.com/original/x/MediaObjects/41598_2024_73565_MOESM1_ESM.pdf" data-article-pdf="true">Supp</a>"#,
        );
        assert_eq!(
            urls,
            vec!["https://www.nature.com/articles/s41598-024-73565-8.pdf"]
        );
        assert_eq!(rule.as_deref(), Some("springer-nature"));
    }
}
