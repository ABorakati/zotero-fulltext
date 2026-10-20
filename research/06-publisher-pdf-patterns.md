# 06: From publisher landing page to PDF bytes (medical and surgical publishers)

Date: 2026-10-20. Scope: getting from an article landing page, possibly a proxied one, to the PDF bytes. Covers the publishers a medical or surgical researcher at KCL uses most. Companion files:

- Fixtures: `fixtures/html/*.html` (31 trimmed pages), `fixtures/html/expected.json` (test oracle) and `fixtures/html/README.md` (sources).
- Tools: `.ref/tools/trim-fixture.js` (the fixture trimmer) and `.ref/tools/survey.js` (dumps the PDF candidates found on a raw page).
- Raw captures: `.ref/scratch/raw/` (untrusted data; not part of the deliverable).
- Related briefs: `01`/`02` (Zotero plugin API and download internals), `04` (KCL access: SAML/OpenAthens, EZproxy, Alma), `05` (OA sources).

Confidence labels: **high** means verified in source code or empirically (live request, or archived HTML read). **medium** means official documentation, or code read without a run. **low** means inferred.

---

## 0. Summary of decisions

1. **Do not reimplement Zotero's translators. Run them first.** Zotero's own "Find Available PDF" already fetches the landing page. It handles redirects and meta refresh, calls `Zotero.Utilities.Internal.getFileFromDocument(doc)` (which runs all ~750 bundled web translators), and downloads with a `%PDF-` check. The translator engine matches on the *unproxied* URL and re-proxifies attachment URLs automatically. The plugin can call the same functions from JS. The Rust/WASM extractor then works as:
   - (a) a **fallback ranker** for pages where translators find nothing,
   - (b) the **page classifier** (PDF, landing, interstitial, challenge, block or login page),
   - (c) **publisher rules for the gaps** Zotero leaves open:
     - LWW `wkhealth_pdf_url`, which Zotero deliberately ignores.
     - ScienceDirect `window.__PRELOADED_STATE__`, which the translator's `script[type=application/json]` selector misses.
     - NEJM and T&F pages, which have no `citation_pdf_url`.
     - Elsevier linkinghub interstitials.
2. **The ranking signal is `citation_pdf_url`, then the publisher rule, then the other heads, then anchors.** 15 of the 21 publishers and platforms in the fixtures emit `citation_pdf_url` (about 70%; §2). The ones that don't are T&F, SAGE, NEJM, LWW (it uses `wkhealth_pdf_url`) and ScienceDirect (JSON state). A PDF URL is only a *candidate*. Springer and LWW emit it without entitlement and then 302 back to HTML, so **always sniff `%PDF-`** (Zotero only accepts it at offset 0: `mime.js:34`).
3. **Bot walls are the main practical blocker, not URL patterns.** On 2026-10-20, a plain HTTP client from this machine got blocked:
   - Cloudflare *managed challenge* (403, `cf-mitigated: challenge`): ScienceDirect, Lancet, Cell, Wiley, OUP, LWW, Karger, T&F, SAGE, JAMA, NEJM and Europe PMC.
   - Cloudflare WAF block: BMJ journals.
   - Akamai: MDPI.
   - JS proof-of-work page with HTTP 200: Thieme, and the PMC PDF endpoint.
   - Not blocked: PLOS, Springer/BMC, Nature, Frontiers, Cambridge, PMC article pages and the Europe PMC PDF endpoint.

   The plugin therefore has to reuse Zotero's Gecko stack and `Zotero.BrowserRequest`, which runs a hidden browser for automatic JS challenges and opens a visible viewer when a person must act. It must never solve CAPTCHAs itself (§5).
4. **Match rules on a hyphen-normalised host key, not on a "deproxified" host.** EZproxy HttpsHyphens is lossy: real hyphens are *not* doubled (brief 04 §3.2). Zotero's `toProper` turns every `-` into `.` (`proxy.js:273`), which mangles hosts such as `www.thieme-connect.com`. Compare `host.replace('.', '-')` against the rule's hyphenated host instead (§3).
5. **At KCL most of these publisher hosts are *not* behind EZproxy.** Only `doi.org` and `www.bmj.com` in this set are proxied (brief 04 §2.4). Access is by SAML/OpenAthens session cookies on the publisher's own host. The proxy-aware matching still matters for portability and for BMJ.
6. **Annals of Surgery (LWW) is the hardest case.** The PDF URL exists only in `meta[name=wkhealth_pdf_url]` (`downloadpdf.aspx?an=<Ovid accession number>`). It sits behind Cloudflare, has no OpenAthens deep link, Zotero's translator ignores it, and without entitlement it 302s back to the HTML. What can be done is described in §3.6.

---

## 1. How Zotero 10.0.6 does landing page to PDF (reuse targets)

Paths are relative to `.ref/omni-app/chrome/content/zotero/xpcom/`.

| Step | Code | Behaviour (high, code read) |
|---|---|---|
| Resolver list | `attachments.js:1275` `getFileResolvers(item, methods=['doi','url','oa','custom'])` | DOI resolves to `{pageURL:'https://doi.org/'+doi}`. Item URL gives `{pageURL:url}`. OA gives `getOpenAccessPDFURLs(doi)`. PMCID gives `pmc.ncbi.nlm.nih.gov/articles/PMCxxx/`. Custom resolvers come from pref `findPDFs.resolvers`. |
| Queue and politeness | `attachments.js:1513-1514` | `MAX_CONSECUTIVE_DOMAIN_FAILURES = 5`, `SAME_DOMAIN_REQUEST_DELAY = 1000` ms. On 429/503 it honours `Retry-After` up to 3600 s; otherwise it waits 10 s. |
| Main loop | `attachments.js:1978` `downloadFirstAvailableFile(urlResolvers, path, options)` | At most 6 URLs per resolver function (`maxURLs`). Forces https (`:2060`). `normalizeURL` drops **all query params except `download`** ("Keep 'download' param for Atypon", `:1987-1995`), so URLs that differ only in query count as already tried. |
| Page fetch | `:2150-2205` | `Zotero.HTTP.request('GET', url, {responseType:'blob', followRedirects:false})`. Follows 301/302/303/307 by hand, at most 10 hops and 2 tries per redirect URL. |
| Meta refresh | `:2217` → `http.js:1045` `getHTMLMetaRefreshURL(doc,url)` | `querySelector('meta[http-equiv="refresh" i]')`. Splits on `;\s*url=`. Follows if the delay is ≤ 15 s. Strips single quotes. If the refresh target matches `BrowserRequest.CHALLENGE_URLS`, it runs `clearChallenge` once in a hidden browser and then retries. |
| Direct file | `:2281` | If `Content-Type` starts with application/pdf or epub, it saves and checks the type. |
| Translate page | `:2288` → `utilities_internal.js:1475` `getFileFromDocument(doc)` | `new Zotero.Translate.Web()`, `setDocument(doc)`, `getTranslators()`, then the first translator. It returns the first attachment whose `mimeType` is in `FIND_AVAILABLE_FILE_TYPES` as `{title, mimeType, url}`. |
| Download | `:2307`, `downloadFile` `:1170-1216` | Uses `Referer = responseURL`. `_enforceFileType` sniffs the first 1000 bytes. **On an invalid-PDF error or HTTP 403**, if the URL matches `BrowserRequest.CHALLENGE_URLS`, it calls `Zotero.BrowserRequest.downloadPDF(url, path, options)`, which uses a hidden browser and intercepts `application/pdf`. |
| Proxies in translation | `translate/src/translators.js:92`, `translate/src/proxy.js:44`, `translation/translate.js:1407-1446` | Translators are matched against **every unproxied candidate** of the page URL (`getPotentialProxies`: known proxies plus a TLD heuristic plus the HttpsHyphens heuristic). Attachment URLs pass through `resolveURL`, which resolves relative URLs and then calls `this._proxy.toProxy(resolved)`, so translator output is re-proxified. |
| Challenge registry | `browserRequest.js:30-77` | `CHALLENGE_URLS`: `://www.sciencedirect.com` (captchaLocator `#captcha-box`), `://pmc.ncbi.nlm.nih.gov` (proof of work, no captcha), `://www.mdpi.com` (Akamai `bm-verify` proof of work), `search.worldcat.org` (Turnstile). `PLAIN_UA_HOSTS`: `challenges.cloudflare.com`, `www.sciencedirect.com`, **`pdf.sciencedirectassets.com`**, `search.worldcat.org`, `www.mdpi.com`. |
| User-Agent | `zotero.js:2238-2330` `Zotero.VersionHeader` | Adds `Firefox/<ver>` and `Zotero/<ver>` to the UA on every non-zotero.org request. For `PLAIN_UA_HOSTS` it **strips `Zotero/`** because "Turnstile won't pass with Zotero/ in the UA". The match is on the exact `channel.URI.host`, so it does **not** apply to proxied hostnames. |

**Plugin implications**

- Calling `Zotero.Attachments.downloadFirstAvailableFile(resolvers, tmpPath, {enforceFileType:true, shouldDisplayCaptcha:true})` with your own `{pageURL}` and `{url}` resolvers gives you redirects, meta refresh, translators, proxies, the `%PDF-` check and the challenge fallback for free. Brief 02 covers its signature and limits.
- Use the Rust extractor where this returns `false`.
- `Zotero.BrowserRequest.clearChallenge(url, {entry})` takes an arbitrary `entry` object, so the plugin can pass `{captchaLocator: null}` for hosts outside Zotero's registry. It is an internal API, so feature-detect it before use. (medium: read from the signature at `browserRequest.js:114`, not run.)

---

## 2. Generic heuristics, ranked

### 2.1 Signal ranking (for `extract_pdf_candidates`)

| Rank | Signal | Details and quirks | Evidence |
|---|---|---|---|
| 1 | **Publisher rule** for a recognised host (§3) | Encodes the known corrections: Wiley `/pdf/` → `/pdfdirect/`, the ScienceDirect JSON, LWW `wkhealth_pdf_url`, NEJM/T&F/SAGE `/doi/pdf/<doi>`. The rule may *replace* `citation_pdf_url`, for example on Wiley. | translators, §3 |
| 2 | `<meta name="citation_pdf_url">` | Zotero's Embedded Metadata matches **name suffix** (`substring(@name, ...)` = name), so `bepress_citation_pdf_url` and `DC.citation_pdf_url` also match. It reads `@content` **or `@contents`**, in `<head>` or `<body>`, and takes the **first** one (`Embedded Metadata.js:181-189, 529-540`). Cambridge sometimes has whitespace in the name attribute, which is why its translator uses `contains()` (`Cambridge Core.js:107-110`). Values can be `http://` (Thieme) or carry a version query (`?version=` on MDPI). | high (code + fixtures) |
| 3 | `<link rel="alternate" type="application/pdf" href>` | BMJ/HighWire (`/content/372/bmj.n71.full.pdf`) and MDPI. Often relative. | high (bmj, mdpi fixtures) |
| 4 | Other head metas: `wkhealth_pdf_url` (LWW), `fulltext_pdf` (MDPI), `eprints.document_url`, `bepress_citation_pdf_url`, `DC.identifier`/`dc.relation` ending in `.pdf` (rare) | LWW is the important one. | high (lww fixtures) |
| 5 | Embedded JSON state | ScienceDirect: `"pdfDownload":{"urlMetadata":{"queryParams":{"md5":…,"pid":…},"pii":…,"pdfExtension":"/pdfft","path":"science/article/pii"}}` inside a **plain** `<script>window.__PRELOADED_STATE__ = {...}`, not `type=application/json`. Find it with a regex or a brace-balanced slice on `"pdfDownload":`. IEEE has a similar `"pdfPath":"…"` (Unpaywall extension). | high (archived SD page) |
| 6 | Anchors | Score each anchor (rules below). Common winners: `.c-pdf-download__link[data-article-pdf]` (Springer Nature), `a#downloadPdf` (PLOS), `a.article-pdfLink` (Silverchair: OUP, Karger), `a#pdf-link[data-article-url]` (JAMA; `href="javascript:;"`), `a.article-tools__item__pdf` → `/action/showPdf?pii=` (Lancet/Cell), `.btn--pdf` and `#downloadPdfUrl` (NEJM), `a.show-pdf` with text "Download PDF" (T&F), `a#pdfLink.gotoLink` (Thieme), `a[data-ga-label^=pdf_download_]` (PMC), `.DownloadArticleButton__action` (Frontiers), `a#js-button-download` (MDPI), `a[pdfurl]` (old ScienceDirect, Unpaywall extension), `a[data-download-content=Article]` (old NEJM, Unpaywall extension). | high (fixtures) |
| 7 | `iframe[src]`, `embed[src]`, `object[data]` pointing at a PDF | ScienceDirect `.PdfEmbed > object[data]` (`ScienceDirect.js:84-93`). Ovid `#embedded-frame` (`Ovid.js:180-181`). HighWire `.full.pdf+html` frameset (`HighWire 2.0.js:388-437`). Ignore GTM `ns.html` iframes. | medium (code) |
| 8 | Interstitial redirects | ScienceDirect intermediate page: `meta[http-equiv=Refresh] content="\d+;URL=…"` or `#redirect-message a` (`ScienceDirect.js:180-200`). Elsevier linkinghub: `input#redirectURL` (URL-encoded) plus a 2 s refresh with a **single-quoted** URL. Cell Press legacy: `onload="javascript:redirectToScienceURL();"` with `value="…"` (`Cell Press.js:105-110`). ProQuest/EZproxy intermediate page (`attachments.js:2254`). | high (linkinghub live; code) |

### 2.2 Anchor scoring

Positive signals:

- href matches `\.pdf($|[?#])`, `/pdf(/|$|\?)`, `/pdfft`, `/pdfdirect/`, `/doi/pdf/`, `/content/pdf/`, `/article-pdf/`, `/articlepdf/`, `/pdfs/journals/`, `type=printable`, `showPdf`, `downloadpdf.aspx`, `getPdf`, `pdf=render`
- the anchor has `data-article-pdf`, `data-article-url`, `pdfurl`, `data-track-action="download pdf"`, or a class or id containing `pdf`
- the link text is `^(download )?pdf$`, `Download PDF`, `View PDF` or `Full Text PDF`
- the anchor is in the toolbar or header region, not the references
- the href contains the **page's own DOI or PII**

Negative signals (each from a fixture):

| False positive | Example (fixture) | Rule |
|---|---|---|
| Reference-list PDFs (guidelines, reports) | EMA PDF (plos), WHO PDF (nejm, cell, mdpi), KDIGO (lancet), apa.org (sage) | Penalise an off-host href with no page DOI. Drop links inside `ol.references`, `#references`, `.ref-list` and `[id^=ref]`. |
| **Links to other articles' PDFs** | PLOS "related" `file?type=printable&id=…pmed.1004449`; Lancet/Cell `/servlet/linkout?…&key=<other DOI>&cf=pdf` "Full Text (PDF)" | Reject if the URL contains a DOI or PII **other than** the page's own. |
| Supplementary files | Nature/Springer `media.springernature.com/…/MediaObjects/…_MOESM1_ESM.pdf`; Lancet `/cms/<doi>/attachment/<uuid>/mmc1.pdf`; NEJM `/doi/suppl/<doi>/suppl_file/*_protocol.pdf`, `_appendix.pdf`, `_disclosures.pdf` (these carry a `download` attribute) | Reject `suppl`, `supplement`, `MOESM`, `ESM`, `mmc\d`, `appendix`, `protocol`, `disclosure` and `/attachment/`. |
| Citation exports | `citation-needed.springer.com/…format=refman`, `/action/downloadCitation`, `/Citation/Download?…` (Karger), `/highwire/citation/…/ris` (BMJ), `/ris/…/BIB` (Thieme), `.nbib`, Frontiers `public-pages-files-2025…/bibTex` | Reject `ris`, `bib`, `enw`, `nbib`, `refman`, `endnote`, `citation`. |
| ePDF and eReader viewers (HTML, not PDF) | Wiley `/doi/epdf/`, T&F `/doi/epdf/…?needAccess=true`, SAGE `/doi/reader/`, Atypon `/doi/epdf/` | Never download them. Rewrite `/epdf/` → `/pdf/` (or `/pdfdirect/` on Wiley), and `/reader/` → `/pdf/` on SAGE. |
| XML and other formats | PLOS `file?…type=manuscript`, MDPI `/xml`, `/epub`, BMJ `.full.txt` | Check the format token. |
| CAPTCHA-gated variants | MDPI `a.accessCaptcha[data-target=…/pdf-with-cover]` | Skip `accessCaptcha` and `pdf-with-cover`. |
| Figure and slide downloads | `downloadFigures`, `/highwire/powerpoint/`, `?download=true` on `.jpg` | Reject image and PowerPoint extensions. |
| `mailto:` "recommend" links that contain "Full text PDF" | Thieme | Reject non-http schemes. |
| `href="javascript:;"` or `href="#"` | JAMA `#pdf-link`, ScienceDirect "View PDF" | Use `data-article-url` if present, otherwise skip. |
| `<noscript>` meta refresh | LWW `3;url=/_layouts/1033/OAKS.Journals/Error/JavaScript.html`; ScienceDirect challenge `0; url=…?ref=cra_js_challenge`; Europe PMC `?javascript_support=no` | Never treat a refresh inside `<noscript>` as a PDF redirect. **Caveat:** a scripting-disabled DOMParser or html5ever parse puts these metas in the head DOM, and Zotero's `getHTMLMetaRefreshURL` probably follows them. (low/medium: inferred from the HTML spec; not run in Zotero) |
| Refresh without a URL | Cloudflare challenge `content="360"` | Ignore it. |
| A `citation_pdf_url` that returns HTML | Springer paywalled article (302 back to `/article/…`, live); LWW `downloadpdf.aspx` without entitlement (302 to fulltext, Wayback); PMC PDF (proof-of-work HTML, live) | The download step must sniff the bytes; the extractor cannot tell. |

**Access hints** (these cost no request):

- Springer/Nature: the `a[data-article-pdf]` button is present for OA and absent for paywalled articles (live).
- Silverchair: `a.article-pdfLink` only when entitled (`Silverchair.js:161`).
- ScienceDirect: no access if `.accessContent`, `.access-options-link-text` or `#check-access-popover` is present (`ScienceDirect.js:71`). OA shows as JSON `"openAccess":true` / `"openArchive":true`, plus `meta citation_fulltext_world_readable`.
- Wiley: `div.doi-access` "Free Access" or "Open Access" (Unpaywall extension `pageSaysPdfIsFree`).
- T&F: `.access-icon.oa`.
- BMJ: `DC.AccessRights=open-access` and `citation_access=all`.
- JAMA: `free-access` class.

---

## 3. Per-publisher rules

Notation: `<doi>` is the raw DOI, without encoding the slash unless the rule says so. "Translator" names the Zotero translator that handles the host (bundled == GitHub master as of 2026-10-20, checked by `lastUpdated`). The "Blocker" column is the 2026-10-20 probe result for a plain curl client (§5).

### 3.1 Rules table

| Publisher / host pattern (unproxied) | Landing URL shapes | PDF URL rule (in priority order) | Interstitials, params, headers | Blocker (curl, 2026-10-20) | Translator / confidence |
|---|---|---|---|---|---|
| **Elsevier ScienceDirect** `www.sciencedirect.com` | `/science/article/pii/<PII>`, `/science/article/abs/pii/<PII>`. DOI → `linkinghub.elsevier.com/retrieve/pii/<PII>` (200 interstitial, then a 2 s refresh to `sciencedirect.com/…?via%3Dihub`) | 1. JSON `__PRELOADED_STATE__.article.pdfDownload.urlMetadata` → `https://www.sciencedirect.com/{path}/{pii}{pdfExtension}?md5={md5}&pid={pid}`. 2. `#pdfLink[href]` or `citation_pdf_url` → intermediate page. 3. `.PdfEmbed > object[data]` (direct if `isDTMRedir=Y`). 4. canonical + `/pdfft?download=true` (strip `:5037`). 5. metapub variant `…/pdfft?isDTMRedir=true&download=true` | The pdfft response is either (a) an intermediate page with `meta Refresh "N;URL=<pdf>"` or `#redirect-message a`, which leads to a **signed, short-lived** `https://pdf.sciencedirectassets.com/<n>/1-s2.0-<PII>/1-s2.0-<PII>-main.pdf?X-Amz-…` (host from `browserRequest.js:74`; URL shape medium), or (b) the **"Preparing your download" JS challenge** (`/craft/challenge/pdf/…`, `<noscript>` refresh back to the article with `?ref=cra_js_challenge`; archived 2026-04-18) | Cloudflare managed challenge on the article page | `ScienceDirect.js:69-200`. Zotero `CHALLENGE_URLS` and `PLAIN_UA_HOSTS`. high |
| **The Lancet** `www.thelancet.com` (Elsevier "Health" platform) | `/journals/<j>/article/PII<pii-with-hyphens>/fulltext`. DOI → linkinghub → `www.thelancet.com/retrieve/pii/<PII>` (live) | 1. `citation_pdf_url` = `https://www.thelancet.com/pdfs/journals/<j>/PII<pii>.pdf`. 2. `a.article-tools__item__pdf` = `/action/showPdf?pii=<PII url-encoded>`. 3. Fallback: the same PII on ScienceDirect (`/science/article/pii/<PII-no-punct>`) (low/medium: Lancet content is mirrored on ScienceDirect) | Avoid `/servlet/linkout?…cf=pdf` and `/cms/…/mmc*.pdf` | Cloudflare | `Elsevier Health Journals.js` → Embedded Metadata. high (fixture) |
| **Cell Press** `www.cell.com` | `/<journal>/fulltext/S….` | 1. `citation_pdf_url` = `https://www.cell.com/<journal>/pdf/<PII>.pdf`. 2. `/action/showPdf?pii=` | Legacy translator: GET the PDF URL; if the body has `redirectToScienceURL`, take `value="…"`, otherwise append `intermediate=true` (`Cell Press.js:95-115`, 2017, may be stale) | Cloudflare | `Cell Press.js`. high (fixture) |
| **Wiley Online Library** `onlinelibrary.wiley.com`, `*.onlinelibrary.wiley.com` (society subdomains) | `/doi/(abs\|full\|epdf\|pdf\|pdfdirect)/<doi>`, `/doi/<doi>` | 1. `citation_pdf_url` (`/doi/pdf/<doi>`) **rewritten to `/doi/pdfdirect/<doi>`** (`Wiley Online Library.js:183-191`); optionally add `?download=true`. 2. Crossref link `https://onlinelibrary.wiley.com/doi/pdf/<doi>` | `/doi/epdf/<doi>` is the ePDF HTML reader: never the target. Translator line 482 maps `/pdf/`, `/epdf/` and `/pdfdirect/` back to the abstract. | Cloudflare | high (code, fixture); pdfdirect behaviour today: medium |
| **Springer Link** `link.springer.com` | `/article/<doi>`, `/chapter/<doi>` | 1. `citation_pdf_url` = `https://link.springer.com/content/pdf/<doi>.pdf` (`encodeURIComponent(doi)` also works: `Springer Link.js:239`, live). 2. `a.c-pdf-download__link[data-article-pdf]` | **Cookie dance** to `idp.springer.com/authorize?response_type=cookie…` and `/transit`, then back. Without a cookie jar the client lands on `?error=cookies_not_supported` (live). Without entitlement the PDF URL 302s to `/article/<doi>` (HTML, live). | none | high (live %PDF for OA) |
| **BioMed Central** `*.biomedcentral.com`, `*.springeropen.com` | **Now 301 to `link.springer.com/article/10.1186/…`** (live, bmcsurg) | Same as Springer Link: `https://link.springer.com/content/pdf/10.1186/<suffix>.pdf` | The BMC translator target (`biomedcentral\|springeropen\.com/articles`) no longer matches after the redirect; Springer Link's translator does | none | high (live) |
| **Nature** `www.nature.com` | `/articles/<id>` | 1. `citation_pdf_url` = `https://www.nature.com/articles/<id>.pdf`. 2. `a[data-track-action="download pdf"]` (`Nature Publishing Group.js:269-274`). Legacy `/full/x.html` → `/pdf/x.pdf` | Cookie dance via `idp.nature.com` (live) | none | high (live %PDF) |
| **Oxford Academic incl. BJS** `academic.oup.com` (Silverchair) | `/<j>/article/<vol>/<iss>/<page>/<id>`, `/<j>/article/doi/<doi>/<id>`, `/article-abstract/…`, `/advance-article/doi/…`. DOIs `10.1093/bjs/*` **and legacy `10.1002/bjs.*`** both resolve to `academic.oup.com/bjs/…` (live) | 1. `citation_pdf_url` = `/<j>/article-pdf/<vol>/<iss>/<page>/<fileId>/<name>.pdf` or `/<j>/advance-article-pdf/doi/<doi>/<fileId>/<name>.pdf`. `fileId` is internal, so it **cannot be built from the DOI**. 2. `a.article-pdfLink` (only when entitled). 3. Crossref `link[content-type=application/pdf]` gives the same URL (live) | Silverchair rate limit text: "We are sorry, but we are experiencing unusual traffic at this time." (`Silverchair.js:166`) | Cloudflare | `Silverchair.js:144-200`. high (fixture, Crossref) |
| **Wolters Kluwer LWW** `journals.lww.com` (Annals of Surgery: `annalsofsurgery`; IJS: `international-journal-of-surgery`) | `/<j>/fulltext/<yyyy>/<mm000>/<slug>.<n>.aspx`, `/<j>/abstract/…`. DOI `10.1097/SLA.*` → `https://journals.lww.com/10.1097/SLA.…` (live) | 1. **`meta[name=wkhealth_pdf_url]`** = `https://journals.lww.com/<j>/_layouts/15/oaks.journals/downloadpdf.aspx?trckng_src_pg=Other&an=<AN>`, where AN = `meta wkhealth_ovid_accession_number` (for example `00000658-202301000-00051`; `00000658` = Annals). Not constructible from the DOI without the page. 2. Nothing else on the page | Entitled: 302 to `pdfs.journals.lww.com/<j>/<yyyy>/<iss>/<slug>.<n>.pdf` (the host exists per Wayback CDX; a token query is likely: low). Not entitled: 302 back to the fulltext `.aspx` (Wayback). `<noscript>` refresh to `JavaScript.html` (false positive) | Cloudflare | **Zotero's LWW translator does not attach PDFs** ("currently this does not work", `LWW.js:125-135`) and deliberately skips `*_pdf_url` (`:86`). high (fixture, code) |
| **BMJ** `www.bmj.com`, `<j>.bmj.com` (e.g. `bmjopen`, `gut`) (HighWire/Drupal) | `/content/<vol>/bmj.<id>`, `/content/<vol>/<iss>/<page>`. DOI → `/lookup/doi/<doi>` (live) | 1. `citation_pdf_url` = `https://www.bmj.com/content/bmj/<vol>/bmj.<id>.full.pdf`. 2. `link[rel=alternate][type=application/pdf]` = `/content/<vol>/bmj.<id>.full.pdf`. 3. `a.pdf-link`. 4. Specialty journals: `https://<j>.bmj.com/content/<vol>/<iss>/<page>.full.pdf` (metapub "VIP" pattern: medium) | `.full.pdf+html` frame pages exist on older HighWire sites | Cloudflare **WAF block** ("Sorry, you have been blocked") on bmjopen.bmj.com | `HighWire 2.0.js` → Embedded Metadata. high (fixture) |
| **NEJM** `www.nejm.org` (Atypon) | `/doi/full/<doi>`, `/doi/<doi>` | 1. `/doi/pdf/<doi>` (`T&F+NEJM.js:203`; anchor `.btn--pdf`). 2. `/doi/pdf/<doi>?download=true` (`#downloadPdfUrl`). **No `citation_pdf_url`** | Supplements are under `/doi/suppl/<doi>/suppl_file/*.pdf` | Cloudflare | high (fixture, code) |
| **JAMA Network** `jamanetwork.com` (Silverchair) | `/journals/<j>/fullarticle/<articleId>` | 1. `citation_pdf_url` = `/journals/<j>/articlepdf/<articleId>/<file>.pdf` (the file name has hashes, so it can't be constructed). 2. `a#pdf-link[data-article-url]` (href is `javascript:;`). 3. Crossref link gives the same URL (live) | `data-ajax-url="/Content/CheckPdfAccess"`: the site checks access by XHR before showing the PDF | Cloudflare | `Silverchair.js:163`. high (fixture, Crossref) |
| **Taylor & Francis** `www.tandfonline.com` (Atypon) | `/doi/(abs\|full\|epdf\|epub)/<doi>` | 1. `/doi/pdf/<doi>` (+ `?download=true`) (`T&F+NEJM.js:203`; anchor "Download PDF"). EPUB: `/doi/epub/<doi>?download=true`. **No `citation_pdf_url`**; the DOI comes from `meta dc.Identifier[scheme=doi]` | `/doi/epdf/<doi>?needAccess=true` is a reader | Cloudflare | high (fixture, code) |
| **SAGE** `journals.sagepub.com` (Atypon) | `/doi/(abs\|full\|pdf\|reader)/<doi>` | 1. `/doi/pdf/<doi>` (`SAGE Journals.js:88`) (+ `?download=true`). 2. Crossref text-mining link `/doi/pdf/<doi>`. **No `citation_pdf_url`, no PDF anchor**: only `/doi/reader/<doi>` | — | Cloudflare | high (code, fixture); download behaviour untested |
| **Karger** `karger.com` (Silverchair since about 2023) | `/<j>/article/<vol>/<iss>/<page>/<id>/<slug>`. DOI → `karger.com/article/doi/<doi>` (live) | 1. `citation_pdf_url` = `/<j>/article-pdf/<vol>/<iss>/<page>/<fileId>/<id>.pdf`. 2. `a.article-pdfLink`. 3. Crossref: `https://karger.com/article-pdf/doi/<doi>` (DOI-based form; untested) | — | Cloudflare | `Silverchair.js`. high (fixture); DOI form low |
| **Thieme** `www.thieme-connect.com`, `www.thieme-connect.de` | `/products/ejournals/(abstract\|html)/<doi>`, `/DOI/DOI?<doi>` | 1. `citation_pdf_url` = `http://www.thieme-connect.com/products/ejournals/pdf/<doi>.pdf` (**http**: upgrade to https). 2. `a#pdfLink`, `a.pdf-download-link` (the href has whitespace/newlines: trim it) | — | **HTTP 200 "Please wait" JS challenge** (`/products/assets/challenge-*.js`) on `thieme-connect.de` (live) | `Thieme.js` → Embedded Metadata. high |
| **Cambridge Core** `www.cambridge.org/core` | `/core/journals/<j>/article/<slug>/<HASH>` | 1. `citation_pdf_url` = `/core/services/aop-cambridge-core/content/view/<HASH>/<PII>a.pdf/<slug>.pdf` (the slug is cosmetic: both slugs gave %PDF). 2. `.actions a[target=_blank][href*=.pdf]` | — | none | `Cambridge Core.js:107-112`. high (live %PDF) |
| **Atypon (generic)** (Wiley, T&F, SAGE, NEJM, ASCO, Liebert, AHA, ATS, Science) | `/doi/(abs\|abstract\|full\|figure\|ref\|citedby\|book\|epdf\|pdf)/<doi>`. Detect by `/action/showCitFormats`, `/action/downloadCitation`, `dc.Identifier` | `/doi/pdf/<doi>`, then `/doi/pdfdirect/<doi>?download=true` (to skip embedded readers), then `/doi/pdfplus/<doi>`. Avoid `/doi/epdf/`. Keep the `download` param. | SAML: `/action/ssostart?idp=<entityID>&redirectUri=<T>` (brief 04) | mostly Cloudflare | `Atypon Journals.js:189-209`, `attachments.js:1990`. high |
| **PLOS** `journals.plos.org` | `/<j>/article?id=<doi>` | `citation_pdf_url` = `/<j>/article/file?id=<doi>&type=printable` | Avoid `type=manuscript` (XML) and related-article links | none | `PLoS Journals.js` → Embedded Metadata. high (live %PDF) |
| **Frontiers** `www.frontiersin.org` | `/journals/<j>/articles/<doi>/full`, legacy `/articles/<doi>/full` | `citation_pdf_url` = `/journals/<j>/articles/<doi>/pdf`. Legacy `/articles/<doi>/pdf` 301s there (live). The translator builds the legacy form (`Frontiers.js:44,217`) | Citation exports are on `public-pages-files-<year>.frontiersin.org` | none | high (live %PDF) |
| **MDPI** `www.mdpi.com` | `/<issn>/<vol>/<iss>/<art>`, `/htm` | `citation_pdf_url` = `fulltext_pdf` = `link[rel=alternate][type=application/pdf]` = `/<issn>/<vol>/<iss>/<art>/pdf?version=<ts>` | `pdf-with-cover` and `xml` are CAPTCHA-gated (`a.accessCaptcha`) | **Akamai 403 "Access Denied"** (live). Zotero: Akamai Bot Manager `bm-verify` meta refresh plus proof of work, cleared in a hidden browser (`browserRequest.js:59-67`) | `MDPI Journals.js` → Embedded Metadata. high |
| **PubMed Central** `pmc.ncbi.nlm.nih.gov` | `/articles/PMC<id>/` | `citation_pdf_url` = `/articles/PMC<id>/pdf/<file>.pdf`; `/articles/PMC<id>/pdf` 301s to it (live). Translator: `a[data-ga-label^=pdf_download_]`, otherwise `/articles/${pmcid}/pdf` (`PubMed Central.js:70-74,251`) | **The PDF URL returns HTTP 200 HTML "Preparing to download ..." with a proof-of-work script (`pow-*.js`, `POW_CHALLENGE`)** to plain clients (live). Zotero clears it in a hidden browser (`browserRequest.js:53-56`) | PDF only | high |
| **Europe PMC** `europepmc.org` | `/article/MED/<pmid>`, `/article/PMC/<pmcid>` (client-rendered app: no meta) | API: `https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=DOI:<doi>&resultType=core&format=json` → `resultList.result[0].fullTextUrlList.fullTextUrl[]` where `documentStyle=='pdf'` and `availabilityCode=='OA'` → `https://europepmc.org/articles/PMC<id>?pdf=render`, which 302s to `https://europepmc.org/api/getPdf?pmcid=PMC<id>` and returned **%PDF** (live). Translator API: `europepmc.org/api/get/articleApi?query=…` (`Europe PMC.js:99,175`) | `<noscript>` refresh to `?javascript_support=no` | Cloudflare on `/article/…` pages only; the PDF endpoint was open | high (live) |

### 3.2 DOI-only shortcut: Crossref `link` metadata (no landing page needed)

`GET https://api.crossref.org/works/<doi>` (or `/journals/<issn>/works`) returns `message.link[]` with `URL`, `content-type` and `intended-application` (`text-mining`, `similarity-checking`, `syndication`). Observed on 2026-10-20 (live, high):

| Publisher | Crossref link URL pattern |
|---|---|
| OUP | `https://academic.oup.com/bjs/article-pdf/112/8/znaf176/64113458/znaf176.pdf` (application/pdf). Advance articles also have `/advance-article-pdf/doi/…` |
| JAMA | `https://jamanetwork.com/journals/<j>/articlepdf/<id>/<file>.pdf` |
| Karger | `https://karger.com/<j>/article-pdf/<vol>/<iss>/<page>/<fileId>/<id>.pdf` or `https://karger.com/article-pdf/doi/<doi>` |
| Springer / BMC | `https://link.springer.com/content/pdf/<doi>.pdf` |
| Nature | `https://www.nature.com/articles/<id>.pdf` |
| Wiley | `https://onlinelibrary.wiley.com/doi/pdf/<doi>` (+ `full-xml`) |
| SAGE | `https://journals.sagepub.com/doi/pdf/<doi>` (+ `full-xml`) |
| T&F | `https://www.tandfonline.com/doi/pdf/<doi>` |
| NEJM | `http://www.nejm.org/doi/pdf/<doi>` |
| Thieme | `http://www.thieme-connect.de/products/ejournals/pdf/<doi>.pdf` |
| MDPI | `https://www.mdpi.com/<issn>/<v>/<i>/<a>/pdf` |
| LWW | landing only: `https://journals.lww.com/10.1097/…` |
| BMJ | `syndication.highwire.org/content/doi/<doi>` (not a PDF) |
| Cambridge | `…/services/aop-cambridge-core/content/view/<PII>` |
| Elsevier | `api.elsevier.com/content/article/PII:…?httpAccept=text/xml` (needs an API key; useless here) |

Use these as **candidates**: they skip the Cloudflare-protected landing page. metapub uses exactly this tactic for OUP, JAMA and Karger "because the site blocks automated access" (metapub `findit/dances/oxford_academic.py`, `jama.py`, `karger.py`). The candidates still need entitlement and still hit the bot wall on the PDF host, so sniff the result.

### 3.3 ScienceDirect detail

1. Detect the page: host key `www-sciencedirect-com`, or `meta[name=citation_pii]`.
2. Read the PII from `meta citation_pii`, the URL `/pii/(S[0-9X]+)`, or the JSON `"pii"`.
3. Candidates, in order:
   - JSON `pdfDownload` → `/science/article/pii/<PII>/pdfft?md5=…&pid=1-s2.0-<PII>-main.pdf`
   - `/pdfft?download=true`
   - `/pdfft?isDTMRedir=true&download=true`
4. Follow the answer:
   - Intermediate HTML with a refresh or `#redirect-message a` → `pdf.sciencedirectassets.com` (signed). Fetch it straight away, *without* re-proxifying; its signature expires.
   - If you get the "Preparing your download" page (`h1` text; `craft/challenge/pdf`), hand off to the hidden browser: `Zotero.BrowserRequest.downloadPDF(url, path, {shouldDisplayCaptcha:true})`.
5. Not entitled: `.accessContent`, `#check-access-popover` or `.access-options-link-text` is present. Skip to OA sources.

### 3.4 Lancet and Cell detail

- `citation_pdf_url` is enough. `/action/showPdf?pii=` is the button's target and probably redirects to `/pdfs/…` (medium).
- DOI resolution goes `doi.org` → linkinghub → `thelancet.com/retrieve/pii/<PII>`. Brief 04 found the OpenAthens redirector returns 404 for `thelancet.com` URLs, so for SAML **resolve via DOI** (the Elsevier `auth.elsevier.com` pattern).

### 3.5 Wiley detail

Prefer `/doi/pdfdirect/<doi>?download=true`, then `/doi/pdf/<doi>`. Treat `/doi/epdf/` as the reader only. On Wiley `/doi/pdf/` often renders the reader in browsers (medium); Zotero's rewrite to `pdfdirect` is the tested fix.

### 3.6 LWW / Annals of Surgery: what is feasible

The facts (high unless marked):

- `wkhealth_pdf_url` gives the only usable URL.
- Cloudflare challenges plain clients on `journals.lww.com`.
- The OpenAthens redirector has no LWW deep link (it passes the URL through unchanged: brief 04 §4.4).
- KCL's EZproxy does not proxy `journals.lww.com` (brief 04 §2.4).
- Zotero's translator ignores PDFs.
- Without entitlement, `downloadpdf.aspx` 302s back to the article.

What the plugin can do, in order:

1. **OA first.** Many Annals articles carry CC BY-NC-ND (for example `10.1097/SLA.0000000000006356` in Crossref's `license`) or have PMC author manuscripts. Check Unpaywall and Europe PMC (brief 05) before touching LWW.
2. **Browser-context fetch.** Load the landing page in a Zotero `HiddenBrowser` or visible viewer. The page JS clears Cloudflare, and the user's campus IP or LWW session supplies entitlement; if the session is missing, the viewer lets the user log in. Then read `wkhealth_pdf_url` from the live DOM and download with the same cookie jar: `Zotero.HTTP.request` with the default context or `userContextId`, or `BrowserRequest.downloadPDF`, which captures the `application/pdf` the browser receives.
3. **Entitlement check before any work:** query the Alma uresolver (brief 04 §4.5) for the DOI. If KCL's portfolio for Annals is **Ovid** rather than `journals.lww.com`, the LWW route won't work. Then link the user to Ovid, where Zotero's Ovid translator can pull the PDF from `ovidweb.cgi?…PDFLink=` or `#embedded-frame` (`Ovid.js:170-195,270-290`).
4. Otherwise, fail cleanly with "Open in browser" (the LibKey Nomad route also exists; §7).

### 3.7 Atypon recognition without host lists

Any host whose path matches `^/doi/(abs|full|epdf|pdf|reader|book)?/?10\.\d{4,9}/` and which carries `meta[name=dc.Identifier][scheme=doi]` can be treated as Atypon. Then:

```
/doi/pdf/<doi>
/doi/pdfdirect/<doi>?download=true
/doi/epub/<doi>?download=true   (EPUB is also accepted by Zotero's FIND_AVAILABLE_FILE_TYPES)
```

---

## 4. Rules and EZproxy hostnames

### 4.1 Matching

- **Unproxied host.** A rule matches on the unproxied host. A proxied page host has the form `<label>.<proxy-suffix>`, with suffix `kcl.idm.oclc.org` at KCL.
  - Under HttpsHyphens, `<label>` is the original host with `.` replaced by `-`. Original hyphens are **not** escaped (brief 04 §3.2: the Penn and Oxford probes showed that `--` forms are not configured). Example: `www-thieme-connect-com`.
  - **So do not decode. Encode the rule's host instead.** Each rule stores `host_key = host.replace('.', '-')`, and the matcher checks `label == host_key` (or `label.ends_with("-" + host_key)` for subdomain-wildcard rules such as `*.onlinelibrary.wiley.com` and `*.bmj.com`). This is exact, and it handles real hyphens.
  - For http origins EZproxy keeps the dots (`doi.org.kcl.idm.oclc.org`: brief 04). Strip the suffix and match normally.
- **Zotero's heuristic is lossy for hyphenated hosts.** `getPotentialProxies` replaces every hyphen in the first label with a dot and then guesses the TLD (`proxy.js:72-77`). `toProper` replaces *all* `-` with `.` "Potentially troublesome with domains that contain dashes" (`proxy.js:273-277`). For `www-thieme-connect-com` this yields `www.thieme.connect.com`, so the Thieme translator target (`thieme-connect\.com`) would not match through a proxy. This affects `thieme-connect`, `els-cdn` and `public-pages-files-2025.frontiersin.org`. (medium: code read, not run)
- **Proxy suffix discovery:** use the plugin's configured suffix (KCL: `.kcl.idm.oclc.org`) or Zotero's configured proxies (`Zotero.Proxies.proxies`). Don't guess from TLDs.

### 4.2 Re-proxifying

1. **Server redirects (`Location`) are followed verbatim.** EZproxy has already rewritten the ones it controls. Never proxify a `Location` yourself.
2. **Absolute URLs found in proxied HTML.** EZproxy rewrites links for hosts covered by `HJ`/`DJ` in the stanza. If a URL still points at a covered host unproxied, proxify it. If it points at a host the stanza does not cover (CDNs, signed asset hosts), **leave it unproxied**. A proxified URL to an unconfigured host yields EZproxy "host not configured" (KCL: `login.kcl.idm.oclc.org/<path>` → 404, brief 04).
3. **URLs the Rust rules construct** (relative paths, `/doi/pdf/<doi>`, `pdfdirect`) are built on the **page's own origin**. If the page was proxied they are proxied too, with no extra step. Build them by `Url::join(page_url, path)`, not from the unproxied host.
4. Zotero's `toProxy` skips hosts matching `edu$`, `google.com$`, `wikipedia.org$`, single-label hosts, `doubleclick.net$` and `eutils.ncbi.nlm.nih.gov` (`proxy.js:114-135`). Mirror that blacklist.

### 4.3 PDFs on a different host than the landing page

| Publisher | PDF host | Proxied? |
|---|---|---|
| ScienceDirect | `pdf.sciencedirectassets.com` (signed `X-Amz-*` query; Zotero sends a plain UA there) | **Not in OCLC's ScienceDirect stanza.** The stanza covers `DJ *.els-cdn.com`, `DJ sciencedirect.com`, `HJ linkinghub.elsevier.com` and rewrites `pdfurl=` attributes (OCLC help page "Elsevier ScienceDirect and Scopus", 2025-11-04: medium). The signed URL is fetched unproxied. KCL's EZproxy proxies neither host (brief 04: high). |
| LWW | `pdfs.journals.lww.com` (low: token) | unknown. `DJ lww.com` would cover it. Not relevant at KCL (LWW is not proxied). |
| Springer / Nature supplements | `media.springernature.com` (not the article PDF) | — |
| Frontiers exports | `public-pages-files-<yyyy>.frontiersin.org` (not the PDF) | — |
| PMC | same host; the proof-of-work script comes from `cdn.ncbi.nlm.nih.gov` | — |
| Europe PMC | `europepmc.org/api/getPdf` (same host) | — |
| All other publishers in §3.1 | the PDF is on the **same host** as the landing page | — |

### 4.4 KCL reality check

Brief 04 §2.4 probed KCL's EZproxy (high). Of the hosts in this brief, only `doi.org`/`dx.doi.org` and `www.bmj.com` are configured. The rest are not (ScienceDirect, Wiley, OUP, LWW, Lancet, Cell, NEJM, JAMA, Springer, Nature, T&F, SAGE, Karger, Thieme, Cambridge). In practice at KCL:

- Almost all landing and PDF URLs are **unproxied**.
- Entitlement comes from SAML/OpenAthens session cookies on the publisher domain, or from on-campus IP.
- `www-bmj-com.kcl.idm.oclc.org` is the main proxied case.

---

## 5. Bot walls and challenge pages

### 5.1 Empirical probe

Probe method, 2026-10-20: `curl` on Windows (schannel TLS), with a Firefox 140 UA, browser-like `Accept` headers and no cookies.

| Host | Response | Vendor / type |
|---|---|---|
| academic.oup.com, journals.lww.com, karger.com, onlinelibrary.wiley.com, www.tandfonline.com, journals.sagepub.com, www.sciencedirect.com, www.thelancet.com, www.cell.com, jamanetwork.com, www.nejm.org, europepmc.org/article | 403, `server: cloudflare`, **`cf-mitigated: challenge`**, `<title>Just a moment...</title>`, `window._cf_chl_opt`, `/cdn-cgi/challenge-platform/…`, `<meta http-equiv=refresh content=360>` | Cloudflare managed challenge |
| bmjopen.bmj.com | 403, `server: cloudflare`, no `cf-mitigated`, "Attention Required! \| Cloudflare", "Sorry, you have been blocked" | Cloudflare WAF block (cannot be solved) |
| www.mdpi.com | 403 "Access Denied", `Akamai-GRN`, `errors.edgesuite.net` reference | Akamai |
| www.thieme-connect.de | **200** "Please wait", `/products/assets/challenge-*.js` | custom JS challenge |
| pmc.ncbi.nlm.nih.gov `…/pdf/…` | **200 text/html** "Preparing to download ...", `pow-*.js`, `POW_CHALLENGE` | proof of work |
| www.sciencedirect.com `…/pdfft` (Wayback 2026-04-18) | 200 "Preparing your download", `/craft/challenge/pdf/…` | Elsevier JS challenge |
| journals.plos.org, link.springer.com, www.nature.com, www.frontiersin.org, www.cambridge.org, pmc article pages, europepmc.org `?pdf=render` | 200 | none |

**Caveat** (open question 1): curl's TLS and HTTP fingerprint differs from Gecko's. Zotero's own requests (Firefox network stack, Firefox UA plus `Zotero/x`) may score better or worse. Zotero added `BrowserRequest` in 2026 precisely because ScienceDirect, PMC and MDPI challenge it (code comments at `browserRequest.js:40-67`, `zotero.js:2281-2289`).

### 5.2 Classifier rules (`classify_page`, Rust)

**Inputs:** status, selected headers, final URL, the first ~64 KB of the body.

- **Cloudflare challenge:** header `cf-mitigated: challenge`, or (`server: cloudflare` and status 403/503 and body contains `_cf_chl_opt` or `/cdn-cgi/challenge-platform/` or title `Just a moment...`).
- **Cloudflare block:** `server: cloudflare`, 403, body `cf-error-details` or "Sorry, you have been blocked".
- **Akamai block:** 403 and (`Akamai-GRN` header or body `errors.edgesuite.net` or title `Access Denied`).
- **Akamai Bot Manager interstitial:** meta refresh to `?bm-verify=` (Zotero comment at `browserRequest.js:59-64`).
- **Proof-of-work or JS interstitial (HTTP 200):** title `Preparing to download ...` with `POW_CHALLENGE` (PMC); title `Please wait` with `/products/assets/challenge-` (Thieme); h1 `Preparing your download` or `/craft/challenge/pdf` or `ref=cra_js_challenge` (ScienceDirect).
- **Redirect interstitials:** linkinghub (title `Redirecting`, `input#redirectURL`); a meta refresh ≤ 15 s with a URL that is **not** inside `<noscript>`.
- **Login pages:** EZproxy `Server: EZproxy` with `login?qurl=`; OpenAthens `OpenAthens / Please wait...` (brief 04). Publisher SSO pages are a matter for brief 04.
- **PDF viewer shells:** Wiley/Atypon `/doi/epdf/`, SAGE `/doi/reader/`, HighWire `.pdf+html`.

**Actions:**

| Classification | Action |
|---|---|
| challenge (automatic) | Hidden browser via `Zotero.BrowserRequest.clearChallenge(url, {entry:{captchaLocator:null}})`, then retry with plain fetch. |
| challenge (interactive) or repeated failure | Visible viewer (`Zotero.openInViewer`). **The user** solves it. The plugin never automates CAPTCHA or Turnstile clicks. |
| block | Stop for that host and mark it "blocked by publisher". Do not retry in a loop: Zotero stops after 5 consecutive failures per domain. |

**Politeness:**

- At least 1 s between requests to the same host (Zotero's default).
- Honour `Retry-After`.
- Do one item per host at a time.
- Never parallelise a batch against one publisher. Silverchair shows an "unusual traffic" page, and Cloudflare scores bursts.

---

## 6. Fixtures (deliverable 4)

`fixtures/html/` contains 31 files covering 20 publishers and platforms plus 3 bot-wall vendors. Each file is a trimmed head (citation, DC, PRISM and wkhealth metas, refresh, canonical and alternate links) plus PDF-like anchors and iframes, with no article text or scripts. The files are 0.5–5 KB each, 125 KB in total.

- **Live, verified %PDF** (6): PLOS, BMC→Springer Link, Nature, Frontiers, Cambridge, plus the Europe PMC alternate.
- **Live, returns HTML without access** (2): Springer paywalled, PMC.
- **Live challenge pages** (2): PMC proof of work, Thieme.
- **Live redirect interstitials** (2): Elsevier linkinghub to Lancet and to ScienceDirect.
- **Live block pages** (3): Cloudflare challenge, Cloudflare WAF, Akamai.
- **Wayback raw captures** (`web.archive.org/web/<ts>id_/…`), used because a plain client is bot-blocked (16): OUP BJS, Wiley, T&F, SAGE, Karger, Thieme article, JAMA, BMJ, NEJM, ScienceDirect article (JSON state), ScienceDirect pdfft challenge, Lancet, Cell, MDPI, LWW Annals of Surgery, LWW IJS (OA), Europe PMC shell.

`expected.json` gives `base_url`, `expected_pdf_url`, `alternates`, `must_not_select`, `via`, `verified` and `kind` (landing, challenge, blocked or redirect) for each file. Suggested Rust test:

```rust
#[test] fn fixtures() {
    let cases: Expected = serde_json::from_str(include_str!("../../fixtures/html/expected.json")).unwrap();
    for c in cases.fixtures {
        let html = std::fs::read_to_string(format!("../../fixtures/html/{}", c.file)).unwrap();
        let kind = classify_page(200, &[], &html, &c.base_url);
        assert_eq!(kind.tag(), c.kind, "{}", c.file);           // landing / challenge / blocked / redirect
        if let Some(exp) = c.expected_pdf_url {
            let cands = extract_pdf_candidates(&html, &c.base_url);
            let top = &cands[0].url;
            assert!(top == &exp || c.alternates.contains(top), "{}: got {top}", c.file);
            for bad in &c.must_not_select { assert!(!cands.iter().any(|x| &x.url == bad), "{}: picked {bad}", c.file); }
        }
    }
}
```

Fixture caveats:

- The `kind` of the Cloudflare and Akamai fixtures needs the status code (403) and headers. Feed `classify_page` those from the README rather than `200`, or let body markers alone trigger it.
- The SAGE expected URL is rule-derived (`via: rule`), not found in the HTML.

---

## 7. Lessons from comparable tools

| Tool | What it does | Lesson for us | Source / confidence |
|---|---|---|---|
| **Zotero "Find Available PDF"** | DOI page, then translators, then OA, then custom resolvers. Hidden browser for registered challenge hosts since 2026. | It is the baseline to beat and reuse. Its gaps: LWW (no PDF by design), ScienceDirect when the JSON isn't `type=application/json`, Thieme challenges, hosts outside the challenge registry, lossy hyphen deproxifying. | code, high |
| **Unpaywall extension** (`ourresearch/unpaywall-extension/extension/unpaywall.js`) | Gets the DOI from metas (`citation_doi`, `doi`, `dc.doi`, `dc.identifier`, `dc.identifier.doi`, `bepress_citation_doi`; it ignores `dc.identifier` whose `scheme` isn't `doi`, because SAGE has publisher-id variants) or a unique `[data-doi]`. Gets the PDF from `citation_pdf_url`, then site-specific anchors (`pdfurl` attribute on ScienceDirect, `data-download-content="Article"` on NEJM, T&F `/doi/pdf/…needAccess=true`, IEEE `"pdfPath"` JSON). | It **verifies by streaming the PDF and checking Content-Type on progress**, then aborts, rather than trusting the link. It skips verification when the page itself says "Free/Open Access" (Wiley `doi-access`, IEEE `isOpenAccess`). The same `scheme=doi` trap appears in our SAGE and T&F fixtures. | GitHub source, high |
| **metapub FindIt** (Python, `metapub/findit/dances/*.py`) | One "dance" per publisher: PMID/DOI to PDF URL. | For Cloudflare publishers (OUP, JAMA, Karger, Wolters Kluwer) it **gave up on scraping and uses Crossref `link`** metadata or URL construction. Builds the ScienceDirect URL from the PII (`/pdfft?isDTMRedir=true&download=true`) and BMJ "VIP" URLs from volume, issue and page. It separates *paywall* (`AccessDenied`) from *no link* (`NoPDFLink`) in its errors; copy that distinction for user messages. | GitHub source, high |
| **LibKey Nomad / LibKey API** (Third Iron) | Curated, per-library knowledge base mapping DOI → `fullTextFile` (direct PDF link) and `contentLocation` (landing page), with a "best available" waterfall, Unpaywall and retraction data. KCL licenses Nomad (library ID 286, brief 04). | The hard per-publisher PDF links are a **maintained database**, not heuristics; that is why commercial tools work. The API (`public-api.thirdiron.com/public/v1/libraries/{id}/articles/doi/{doi}?access_token=…`) needs a per-library token (API++ subscription), so it is not usable without KCL's cooperation. Pointing the user at LibKey/Nomad as a manual fallback is cheap. | Third Iron docs (thirdiron.atlassian.net BrowZineAPIDocs pages 2961408001, 2930671617): medium. KCL FAQ 280440: medium |
| **Lean Library** | Access-broker extension (Sage-owned until a reported December 2025 sale). Several UK libraries (Kent 2024, Sydney 2025) dropped it for Nomad. | Product churn: don't depend on third-party brokers. | web search, low/medium |
| **EndNote Click** (formerly Kopernio, Clarivate) | Freemium extension using institutional auth plus OA and its own scraping. | Closed source; the reported pain points are the same (bot walls, SSO hops). | Aaron Tay 2019–2023 comparison: low |
| **GetFTR** | Publisher-run entitlement API plus browser extension (beta 2023). | The publishers' own answer to "is this user entitled, and where is the PDF". Integrators need an agreement. | Aaron Tay update 26/3/2023: low |
| **OA-only tools** (Open Access Button / OA.Works, CORE Discovery) | DOI → OA copy via APIs. | Covered by brief 05. Lesson: a combination of OA sources outperforms any single one by about a third (Knoth analysis cited by Tay). | low/medium |

**Recurring failure modes.** These come from the tools above plus our probes:

1. Bot walls (Cloudflare, Akamai, proof of work) block non-browser clients. Every serious tool now runs inside a real browser context.
2. `citation_pdf_url` is present when the user has no access. Sniff the bytes.
3. Reader shells (ePDF, eReader, `.pdf+html`) return HTML.
4. Supplementary and reference PDFs get mistaken for the article.
5. Signed, short-lived PDF URLs (ScienceDirect) must be fetched immediately and must not be proxified or cached.
6. Cookie dances (Springer and Nature `idp` with `cookies_not_supported`) break clients without a cookie jar.
7. Platform migrations silently break host rules: BMC moved to link.springer.com, BJS moved from Wiley to OUP (legacy `10.1002/bjs` DOIs now resolve to OUP), Karger moved to Silverchair, Frontiers moved to `/journals/<j>/articles` URLs. Keep rules data-driven, and fall back to generic heuristics.
8. Per-domain rate limiting and lockouts. Batch jobs must be serial per host.

---

## 8. Proposed Rust core API (pure, wasm32-unknown-unknown)

```rust
pub enum Signal { PublisherRule(&'static str), CitationPdfUrl, LinkAlternatePdf, HeadMeta(&'static str) /* wkhealth_pdf_url, fulltext_pdf, … */,
                  JsonState(&'static str), Anchor, Frame, Interstitial, Crossref }
pub struct Candidate { pub url: String, pub signal: Signal, pub score: i32, pub needs_browser: bool }

/// Ranked PDF candidates. `page_url` is the URL the HTML was served from (may be proxied). Relative URLs are joined to it.
pub fn extract_pdf_candidates(html: &str, page_url: &str) -> Vec<Candidate>;

pub enum PageKind { Pdf, Landing, ReaderShell, Redirect { next: String }, Challenge { vendor: Vendor, interactive: bool },
                    Blocked { vendor: Vendor }, Login { kind: LoginKind }, Unknown }
pub fn classify_page(status: u16, headers: &[(String, String)], body_head: &[u8], url: &str) -> PageKind;

/// Hyphen-normalised host key for rule matching through EZproxy HttpsHyphens. Pass the configured proxy suffixes.
pub fn host_key(host: &str, proxy_suffixes: &[&str]) -> (String /*key*/, Option<String> /*suffix*/);
pub fn rule_for(host_key: &str) -> Option<&'static PublisherRule>;

/// Article identity from the page: citation_doi | dc.identifier[scheme=doi] | prism.doi | URL; citation_pii; PMCID.
pub fn page_ids(html: &str, page_url: &str) -> PageIds;

/// `%PDF-` at offset 0 is what Zotero accepts (mime.js:34). Optionally allow up to 1024 bytes of leading junk.
pub fn sniff_pdf(prefix: &[u8]) -> bool;
```

HTML parsing in WASM:

- Use **`tl`** (small, fast, tolerant) or **`lol_html`**, which is built for streaming and selectors and is proven on wasm in Cloudflare Workers.
- `scraper`/`html5ever` gives exact spec behaviour (including `<noscript>` parsing) but costs more binary size. Without `wasm-opt`, size matters.
- Whichever parser you pick, track "inside `<noscript>`" for meta refresh.
- The ScienceDirect JSON needs a brace-balanced slice; do not parse all of `__PRELOADED_STATE__`, which can run to megabytes.

---

## 9. Open questions

1. **Gecko vs curl.** Do `Zotero.HTTP.request` calls from the user's machine (Firefox TLS fingerprint, `Zotero/` UA, user cookies) get the same Cloudflare challenges? This needs a runtime probe in Zotero (brief 03 harness) against ScienceDirect, Wiley, OUP, LWW and JAMA. It decides how often the hidden browser is needed.
2. **Annals of Surgery at KCL:** is the portfolio on `journals.lww.com` or Ovid (Alma uresolver, brief 04 §4.5)? With entitlement, does `downloadpdf.aspx` 302 to a tokenised `pdfs.journals.lww.com` URL, and does it need a `Referer` or a JS-set cookie?
3. Does Zotero's `getHTMLMetaRefreshURL` follow `<noscript>` refreshes in DOMParser documents (LWW → `JavaScript.html`; ScienceDirect challenge → article; Europe PMC → `?javascript_support=no`)? If it does, the plugin should strip `<noscript>` before handing a doc to `downloadFirstAvailableFile`/`getFileFromDocument`, or do its own page loop.
4. How often does an entitled ScienceDirect session get the "Preparing your download" challenge? Is `pdf.sciencedirectassets.com` reachable directly from a SAML session?
5. Does Wiley `/doi/pdfdirect/<doi>` still return raw PDF in 2026 for entitled SAML sessions, or does it now redirect to the reader?
6. Karger's DOI-based `karger.com/article-pdf/doi/<doi>` and SAGE's `/doi/pdf/<doi>?download=true`: untested because of Cloudflare.
7. Is reusing `Zotero.BrowserRequest`/`HiddenBrowser` (internal, added 2026) from a plugin stable across 10.x? Brief 01/03 should confirm; feature-detect regardless.

---

## 10. Sources

**Local Zotero 10.0.6 code** (`.ref/omni-app/…`):

- `xpcom/attachments.js:43,1170-1243,1250,1275-1490,1513-1514,1978-2330`
- `xpcom/browserRequest.js:28-90,114-190,567-617`
- `xpcom/http.js:1045-1073`
- `xpcom/utilities_internal.js:1475-1502`
- `xpcom/zotero.js:2238-2335`
- `xpcom/mime.js:34,245`
- `xpcom/translate/src/proxy.js:44-135,254-320`
- `xpcom/translate/src/translators.js:80-130`
- `xpcom/translate/src/translation/translate.js:1407-1446`

**Translators** (`.ref/omni-app/translators/`, identical in `lastUpdated` to github.com/zotero/translators master on 2026-10-20): Embedded Metadata, ScienceDirect, Cell Press, Elsevier Health Journals, Wiley Online Library, Springer Link, BioMed Central, Nature Publishing Group, Silverchair, Lippincott Williams and Wilkins, HighWire 2.0, Taylor and Francis+NEJM, Atypon Journals, SAGE Journals, Thieme, Cambridge Core, PLoS Journals, Frontiers, MDPI Journals, PubMed Central, Europe PMC, Ovid, Clinical Key.

**Live probes, 2026-10-20** (raw files under `.ref/scratch/raw/` and `.ref/scratch/pdfprobe/`): the landing pages and PDF URLs listed in `fixtures/html/README.md`; doi.org resolution for 10 DOIs; linkinghub interstitials; unauthenticated KCL EZproxy redirects (`www-sciencedirect-com`, `pdf-sciencedirectassets-com`, `journals-lww-com`, `academic-oup-com`: all `302 → login.kcl.idm.oclc.org/<path>`, which means not configured per brief 04).

**Crossref REST API:** `https://api.crossref.org/journals/<issn>/works?filter=…&select=DOI,title,URL,license,link`.

**Europe PMC REST:** `https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=DOI:10.1371/journal.pmed.1000097&resultType=core&format=json`.

**Internet Archive CDX and raw captures:** `http://web.archive.org/cdx/search/cdx?…` and `https://web.archive.org/web/<ts>id_/<url>` (timestamps in the fixtures README).

**OCLC EZproxy stanza:** https://help.oclc.org/Library_Management/EZproxy/EZproxy_database_stanzas/Database_stanzas_E/Elsevier_ScienceDirect_and_Scopus (2025-11-04). Option HttpsHyphens: https://help.oclc.org/Library_Management/EZproxy/Configure_resources/Option_HttpsHyphens

**Unpaywall extension:** https://github.com/ourresearch/unpaywall-extension, `extension/unpaywall.js` (findDoiFromMetaTags, findPdfUrl, pageSaysPdfIsFree, downloadPdf).

**metapub FindIt:** https://github.com/metapub/metapub, `metapub/findit/dances/{sciencedirect,wolterskluwer,jama,bmj,oxford_academic,karger}.py`.

**Third Iron LibKey API docs:** https://thirdiron.atlassian.net/wiki/spaces/BrowZineAPIDocs/pages/2961408001 and https://thirdiron.atlassian.net/wiki/spaces/BrowZineAPIDocs/pages/2930671617

**KCL LibKey Nomad FAQ:** https://libanswers.kcl.ac.uk/faq/280440

**Aaron Tay, comparison of access-broker extensions (2019, updated to 2023):** https://aarontay.substack.com/p/a-comparison-of-6-access-broker-browser

**Lean Library status (low):** https://casrai.org/compare/libkey-nomad-vs-browzine-vs-lean-library, https://kent.ac.uk/guides/lean-library
