# core-page: HTML tokenizer wrapper, response classifier, PDF-link extractor, publisher rules

Package id `core-page`. Implementer: Sonnet. Depends on: nothing (skeleton compiles; `page::html::parse` already works).
Read first: `docs/design/core.md` sections 2 (decisions 4-6), 3; `research/06-publisher-pdf-patterns.md` sections 2-5, 8;
`research/04-kcl-institutional-access.md` sections 2-4 and 6.5; `research/SUMMARY.md` sections (c), (e);
`fixtures/html/README.md` and `fixtures/html/expected.json` (the oracle); `docs/design/contracts/model.rs`.

## Files you own
`crates/fulltext-core/src/page/mod.rs`, `page/html.rs`, `page/classify.rs`, `page/extract.rs`, `page/rules.rs`,
`crates/fulltext-core/tests/page_fixtures.rs`, `crates/fulltext-core/tests/classify.rs`, hand-made inputs in
`crates/fulltext-core/tests/data/page/`. Read-only: everything else; load fixtures with `mod common;`
(`common::html_fixture(file) -> (RespMeta, Vec<u8>)` gives the capture-time status and headers; `common::html_expected()`
gives the oracle entries; OA-side challenge pages come from `common::oa_fixture`).
Environment: `CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\core-page` for every cargo command.

## Rules for all code
No `unwrap`/`expect`/`panic!`/indexing/string slicing outside tests (use `get`, `?`, `let ... else`). Total on any bytes (NULs,
invalid UTF-8, 2 MiB of `<div>`, unterminated tags). Keep every published signature; add private helpers freely. Remove all
`SKELETON` markers. Decode bodies with `text::decode`; cap work at `text::MAX_TEXT_BYTES`.

## 1. Entry points (frozen signatures, bodies yours)
```rust
// page/mod.rs
pub fn inspect_page(resp: &RespMeta, requested: &Ids, route: Route, body: &[u8]) -> PageInspection;
pub fn classify_response(resp: &RespMeta, body: &[u8]) -> PageClass;
// page/classify.rs
pub struct Facts<'a> { pub status: u16, pub url: &'a Url, pub headers: &'a BTreeMap<String,String>,
                       pub body: &'a [u8], pub text: &'a str, pub doc: &'a Doc }
pub fn classify(f: &Facts<'_>) -> PageClass;
// page/extract.rs
pub struct PageCtx<'a> { pub url: &'a Url, pub requested: &'a Ids, pub route: Route, pub doc: &'a Doc }
pub struct Extraction { candidates, rejected, page_ids, doi_match, access, rule }
pub fn page_ids(doc: &Doc, url: &Url) -> PageIds;
pub fn extract(ctx: &PageCtx<'_>) -> Extraction;
// page/rules.rs
pub struct Rule { id, host_keys, build: fn(&RuleCtx<'_>) -> Vec<Candidate> }   pub static RULES: &[Rule];
pub fn rule_for(host_key: &str) -> Option<&'static Rule>;
```
`inspect_page`: build `Facts` from `resp` (unparseable `final_url` -> `https://unknown.invalid/`), `classify`, and run `extract`
only for `Landing`, `Html`, `ReaderShell` (for `ReaderShell { rewrite: Some(u) }` add `u` as a top candidate with signal
`reader_rewrite`). `Redirect`, challenge, login and block classes return no candidates. `PageInspection.rule` / `page_ids` /
`doi_match` / `access` / `rejected` come from `Extraction` (defaults for other classes). `classify_response` is the same
classification without extraction; the body may be empty (no-follow probes), and for 3xx the `location` header (lower-case
key in `resp.headers`) is resolved against `final_url`.

## 2. html.rs
Working first version exists (see its tests). Harden and add tests: unquoted / single-quoted / duplicate attributes (first wins),
uppercase tags, entities in attribute values (`&amp;` decoded), `<base href>`, `<a>` without `href` ignored, nested markup in
`<a>` text, `<title>` with entities, `meta contents=` fallback, `property=` / `itemprop=` names, unterminated tags at EOF, a
2 MiB document (no quadratic behaviour: assert it finishes, and keep anchor/script text capped at `MAX_SCRIPT_TEXT`). Remember the
tokenizer config makes `<noscript>`, `<iframe>`, `<noembed>`, `<xmp>` bodies raw text (module docs): add a test that an LWW-style
`<noscript><meta http-equiv=refresh ...>` is NOT collected. Add helpers you need (for example `Doc::input_by_id`).

## 3. classify.rs: rule order is normative (first match wins)
Inputs are status, final URL, lower-case headers (`content-type`, `server`, `location`, `retry-after`, `cf-mitigated`, `akamai-grn`),
body bytes, lossy text, parsed `Doc`. KCL constants live in a private `mod kcl` here: EZproxy login host `login.kcl.idm.oclc.org`,
suffix `kcl.idm.oclc.org`; also treat any host ending `.idm.oclc.org` and any response with header `server: EZproxy` generically.

1. `Pdf { junk_prefix }`: `content-type` contains `application/pdf` (or `x-pdf`) and (body empty or `%PDF-` found), or `%PDF-` found in the first 1024 bytes; `junk_prefix` = its offset.
2. EZproxy (check before generic SAML): `EzproxyNeedLogin` when (a) 3xx from a `*.kcl.idm.oclc.org` host other than the login host with `location` matching `^https?://login.kcl.idm.oclc.org/login\?(q)?url=`; (b) 200 from the login host, path `/login`, and the text contains `name='EZproxyForm'` (either quote) or title `Shibboleth Authentication Request` or `name='RelayState' value='ezp.`; (c) 3xx from `login.../<x>` to `login?url=...<x>`. `EzproxyHostNotConfigured` when a 3xx from `X.kcl.idm.oclc.org` has `location == https://login.kcl.idm.oclc.org<same path+query>` (no `/login`), or 404 from the login host with `<H1>404 Not found</H1>`. `EzproxyError { suspend }` for a 200 `text/html` from the login host `/login...` WITHOUT `EZproxyForm` and without redirect (`suspend` true if the text contains `suspend`, `usage limit` or `excessive`).
3. SSO: `IdpLogin { idp, ca_error }` for hosts `login.microsoftonline.com`, `login.live.com`, `login.microsoft.com`, `device.login.microsoftonline.com` (idp `Entra`), `login.openathens.net` with path `/auth/...` (`Openathens`), `kclidp.kcl.ac.uk` (`Kcl`); `ca_error = Some("AADSTSnnnnn")` when the text contains `AADSTS` followed by digits (conditional-access / device-policy refusals such as AADSTS53000, 530003, 50105). `SamlAutoPost`: a `form` with method post containing an input named `SAMLRequest` or `SAMLResponse` (case-insensitive). `OpenathensWait`: host `go.openathens.net`, status 200, title `OpenAthens / Please wait...`; `OpenathensUnknownResource`: same host, 404, title `OpenAthens: 404`; `OpenathensBadOrg`: same host, 403. `PublisherSsoHop`: URL (or 3xx `location`) containing `/Shibboleth.sso/Login`, `/Shibboleth.sso/discovery`, `/action/ssostart`, `auth.elsevier.com/ShibAuth`, `fsso.springer.com`, `sp.nature.com/saml`, `secure.nature.com/saml2`, `oup-sp.sams-sigma.com`, `shibboleth.highwire.org/session/init`, `profile.thieme.de/HTML/sso`, `karger.com/ExternalAuthentication`, `shibboleth.cambridge.org`.
4. Bot walls: `Challenge { vendor, interactive }`: Cloudflare = header `cf-mitigated: challenge`, or (`server: cloudflare`, status 403/503, and `_cf_chl_opt` or `/cdn-cgi/challenge-platform/` or title `Just a moment...`); `interactive` = text mentions `cf-turnstile`, `turnstile`, `g-recaptcha` or `h-captcha`. PMC proof of work = title starts `Preparing to download` plus `POW_CHALLENGE` or a script `src` containing `pow-` (`Pmc`). Thieme = title `Please wait` plus a script `src` containing `/products/assets/challenge-`. ScienceDirect = text contains `Preparing your download`, `/craft/challenge/pdf` or `ref=cra_js_challenge`. Akamai Bot Manager = a refresh target containing `bm-verify=`. `Blocked { vendor }`: Cloudflare = `server: cloudflare`, status 403, and `cf-error-details` or `Sorry, you have been blocked` or title `Attention Required! | Cloudflare`; Akamai = 403 and (`akamai-grn` header or `errors.edgesuite.net` or title `Access Denied`).
5. `RateLimited { retry_after_s }` for 429, or 403/503 whose text contains `unusual traffic` / `too many requests` (`retry-after` seconds if present). `LicenceStop` when the text contains one of `excessive download`, `systematic download`, `your access has been suspended`, `usage policy violation` (case-insensitive; extend from research/04 section 8 if you find more). `NotFound` for 404/410 not matched above. `AccessDenied` for 401/403 not matched above.
6. `Redirect { next }`: `input#redirectURL` value (percent-decoded once, `http` upgraded to `https`, resolved against the page URL) or the first `Doc::refreshes` entry with delay <= 15 s AND a non-empty URL (a refresh with no URL, e.g. Cloudflare `content="360"`, is ignored); for 3xx the resolved `location`. Refreshes inside `<noscript>` never reach `Doc` (tokenizer behaviour), which is the required outcome for LWW, ScienceDirect and Europe PMC.
7. `ReaderShell { rewrite }`: path contains `/doi/epdf/`, `/doi/reader/`, `/epdf/` or ends `.pdf+html`. `rewrite`: Wiley `/doi/epdf/X` -> `/doi/pdfdirect/X`; other Atypon `/doi/epdf/X` and `/doi/reader/X` -> `/doi/pdf/X`; `.pdf+html` -> `.pdf`; drop `needAccess` queries.
8. `Landing` when the doc has any of `citation_title`, `citation_doi`, `citation_pdf_url`, `dc.identifier`, `prism.doi`, `wkhealth_pdf_url`, `og:type=article`; else `Html`.

Tests (`tests/classify.rs`): one case per rule and per sub-signal. Where no recorded fixture exists (EZproxy, Entra, OpenAthens, SSO, licence stop, rate limit) write small inputs in `tests/data/page/` using the shapes in research/04 sections 2.2, 2.3, 3.5, 4.2-4.3, 6.5 (keep them under 2 KB, note the research section in a comment). Include negative cases (an article page that mentions "Just a moment" in prose stays `Landing`; `Access Denied` in a normal article is not `Blocked`).

## 4. extract.rs
Signal order (best first): publisher rule > `citation_pdf_url` > `link[rel~=alternate][type=application/pdf]` > other head metas
(`wkhealth_pdf_url`, `fulltext_pdf`, `eprints.document_url`, `bepress_citation_pdf_url`) > embedded JSON state (`"pdfDownload"` with
`urlMetadata` in a plain `<script>`; find with a brace-balanced slice, build `.../science/article/pii/<pii>/pdfft?md5=..&pid=..`) > scored anchors
> `iframe`/`embed`/`object` pointing at a PDF. `citation_pdf_url` matches by NAME SUFFIX via `Doc::meta_suffix` (also `contents`), first wins. Resolve every URL
against `<base href>` or the page URL (`urls::resolve`), upgrade `http` to `https`, drop fragments. Every candidate: `kind Pdf`, `source Source::Extractor`,
`signal` like `meta:citation_pdf_url` / `link:alternate` / `meta:wkhealth_pdf_url` / `json:pdfDownload` / `anchor:#downloadPdf` / `rule:<id>`,
`score` (rule 100, citation_pdf_url 90, alternate 80, other metas 75, json 70, anchors 40-70 by score, frames 30), `needs_browser` true for
`pmc.ncbi.nlm.nih.gov` and for hosts whose rule says so, `host_kind` Publisher/Pmc/Preprint/Repository from the host, `record_doi` = the page DOI.
Return candidates sorted best-first and deduplicated by `urls::canonical_key`.

Anchor scoring (research/06 section 2.2): positive = href matches `\.pdf($|[?#])`, `/pdf(/|$|\?)`, `/pdfft`, `/pdfdirect/`, `/doi/pdf/`, `/content/pdf/`, `/article-pdf/`, `/articlepdf/`, `/pdfs/journals/`,
`type=printable`, `showPdf`, `downloadpdf.aspx`, `getPdf`, `pdf=render`; attribute `data-article-pdf` / `data-article-url` / `pdfurl` / class or id containing `pdf`; text matching `^(download )?pdf$`, `Download PDF`, `View PDF`, `Full Text PDF`;
the href containing the page's own DOI or PII. `href="javascript:;"` / `#` -> use `data-article-url` if present else skip. Negative = reference-list region (`ol.references`, `#references`, `.ref-list`, `[id^=ref]` ancestors: you cannot see ancestors in `Doc`, so extend `html.rs` to record on each anchor whether it sat under such a region), off-host links without the page DOI.

Reject into `Extraction.rejected { url, why }` (stable `why` strings: `other_doi`, `supplement`, `citation_export`, `reader_shell`, `non_http`, `image`, `captcha_variant`, `javascript_href`): URLs containing a DOI or PII different from the page's own; `suppl`, `supplement`, `MOESM`, `ESM`, `mmc\d`, `appendix`, `protocol`, `disclosure`, `/attachment/`; `ris`, `bib`, `enw`, `nbib`, `refman`, `endnote`, `citation`, `downloadCitation`, `/Citation/Download`; `/doi/epdf/` and `/doi/reader/` (rewrite instead); `mailto:`, `javascript:`; image / PowerPoint extensions; `pdf-with-cover`, `accessCaptcha`, `.xml`, `.epub`, `.full.txt`, `type=manuscript`.

`page_ids`: `PageIds.doi` from `citation_doi`, `dc.identifier` (any `scheme`) or `dc.Identifier` with `scheme=doi`, `prism.doi`, `og:url`/canonical/`rft_id` DOIs, JSON-LD `"doi"`; `PageIds.pii` from `/pii/<S...>` in the URL or canonical, `citation_pii`, `PIIS...` in a Lancet/Cell path. `doi_match`: `Some(true/false)` when both the page and `requested.doi` DOIs are known (`ids::same_doi`), else `None`.
`access` hints (no extra request): Springer/Nature `a[data-article-pdf]` present -> `Granted`, absent while `citation_pdf_url` exists -> `NoAccess`; ScienceDirect `.accessContent` / `.access-options-link-text` / `#check-access-popover` -> `NoAccess`, JSON `"openAccess":true` -> `Granted`; Silverchair `a.article-pdfLink` -> `Granted`; Wiley `div.doi-access` "Free Access|Open Access" -> `Granted`; T&F `.access-icon.oa` -> `Granted`; BMJ `citation_access=all` or `dc.accessrights=open-access` -> `Granted`; JAMA class `free-access` -> `Granted`; otherwise `Unknown`.

## 5. rules.rs (data-driven, keyed on `urls::host_key(unproxied host)`; a proxified key matches by prefix)
| rule id | host keys | behaviour |
|---|---|---|
| `sciencedirect` | `www-sciencedirect-com` | JSON `__PRELOADED_STATE__` `pdfDownload.urlMetadata` -> `/science/article/pii/<pii>/pdfft?md5=<md5>&pid=<pid>`; else canonical + `/pdfft?download=true`; `needs_browser` true (JS challenge); never proxify or cache signed `pdf.sciencedirectassets.com` URLs |
| `elsevier-health` | `www-thelancet-com`, `www-cell-com` | `citation_pdf_url`; alternate `/action/showPdf?pii=<pii>`; reject `/servlet/linkout...cf=pdf` and `mmc\d` |
| `wiley` | `onlinelibrary-wiley-com` | `citation_pdf_url` with `/doi/pdf/` rewritten to `/doi/pdfdirect/<doi>`; alternate `?download=true`; never `/doi/epdf/` |
| `atypon` | `www-tandfonline-com`, `journals-sagepub-com`, `www-nejm-org`, `www-atsjournals-org`, `www-liebertpub-com` | when `citation_pdf_url` is absent: `/doi/pdf/<doi>` (DOI from `dc.identifier`/URL), alternate `?download=true`; rewrite `/doi/reader/` and `/doi/epdf/` to `/doi/pdf/` |
| `springer-nature` | `link-springer-com`, `www-nature-com` | `citation_pdf_url`; `a[data-article-pdf]`; reject `media.springernature.com` supplements; `access` from the button |
| `silverchair` | `academic-oup-com`, `jamanetwork-com`, `karger-com` | `citation_pdf_url`; JAMA `a#pdf-link[data-article-url]`; Karger alternate `https://karger.com/article-pdf/doi/<doi>` |
| `lww` | `journals-lww-com` | `meta[name=wkhealth_pdf_url]` (the `downloadpdf.aspx?...&an=<AN>` URL); `needs_browser` true |
| `highwire` | `www-bmj-com` | `citation_pdf_url`, `link[rel=alternate][type=application/pdf]`; reject `/highwire/citation/` and `.full.txt` |
| `walled-misc` | `www-thieme-connect-com`, `www-thieme-connect-de`, `www-mdpi-com`, `pmc-ncbi-nlm-nih-gov` | `citation_pdf_url` (Thieme `http` upgraded; MDPI keeps `?version=`; skip `pdf-with-cover`); `needs_browser` true |
| `open-misc` | `journals-plos-org`, `www-frontiersin-org`, `www-cambridge-org` | `citation_pdf_url` (Cambridge: `contains` match on the meta name, alternate without the cosmetic trailing slug) |
`PageInspection.rule` = the id of the rule that produced the first candidate. A rule may add candidates but the generic pipeline still runs, so a rule never hides a better generic signal for its own host unless the table says "replace".

## 6. Tests and acceptance
`tests/page_fixtures.rs`, driven by `common::html_expected()` + `common::html_fixture`: for every entry (a) `kind == "landing"` with `expected_pdf_url`: call `inspect_page` with `requested = Ids::default()` and again with the page's DOI; class is `Landing` or `Html`; `candidates[0].url` equals `expected_pdf_url` or one of `alternates`; no candidate URL equals any `must_not_select` (compare exact and by `canonical_key`); `rejected` mentions the must-not-select URLs that appear in the HTML where a stable `why` applies; (b) `expected_pdf_url == null` landing (`europepmc__spa-shell.html`): no candidates, class not `Redirect`/`Challenge`; (c) `kind == "challenge"` -> `Challenge` with the vendor implied by the file name (`cloudflare__managed-challenge` Cloudflare, `pmc__pdf-proof-of-work` Pmc, `thieme__bot-challenge` Thieme, `sciencedirect__pdfft-js-challenge` Sciencedirect); (d) `kind == "blocked"` -> `Blocked` (Cloudflare / Akamai); (e) `kind == "redirect"` -> `Redirect { next == next_url }`. Plus the OA-side pages: `pmc-web__pdf-pow-interstitial.html` -> `Challenge { Pmc }`, `europepmc-web__pdf-render-cloudflare-challenge.html` -> `Challenge { Cloudflare }`, `medrxiv-web__pdf-403-block.html` -> `Blocked { Cloudflare }`. Specific assertions: Springer paywalled has the candidate AND `access == NoAccess`; PMC article candidate has `needs_browser`; Thieme candidate is `https`; Wiley uses `pdfdirect`; JAMA/ScienceDirect/LWW come from `data-article-url` / JSON / `wkhealth_pdf_url`; MDPI never selects `pdf-with-cover`; T&F, SAGE and NEJM (no `citation_pdf_url`) get the rule-derived `/doi/pdf/<doi>`; `rule` is set for the rule-driven ones.
Totality: every fixture truncated to 0, 1, 100, len/2 and len-1 bytes, and with every byte after offset 100 replaced by NUL, must return without panicking.
```
set CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\core-page
cargo test -p fulltext-core --test page_fixtures --test classify
cargo test -p fulltext-core page::
cargo clippy -p fulltext-core --all-targets -- -D warnings
git grep --no-index -n -E "SKELETON|not implemented" -- crates/fulltext-core/src/page
```
The last command must print nothing.
