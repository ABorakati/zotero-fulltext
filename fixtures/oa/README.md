# OA-source API fixtures

Live responses captured **2026-10-20** from the open-access discovery APIs, for unit-testing the
Rust/WASM parsers (`parse_*`) and the JS fetch layer. All requests were unauthenticated, with no
cookies, and used the User-Agent `zotero-fulltext-research/0.1 (fixture capture; no contact)`.
No personal email was sent anywhere. Where an API required an email (Unpaywall) or accepts an
optional one (Crossref polite-pool probe), the obvious placeholder
`zotero-fulltext-fixtures@example.org` was used.

* `manifest.json` — machine-readable index: file, source, case, DOI, method, URL, request body,
  HTTP status, Content-Type, size. **Use this in tests** to know the status code that went
  with each body (several bodies are HTML/plain-text error pages served for JSON endpoints).
* `_capture-log.txt` — raw capture log with selected response headers (rate-limit headers etc.).
* Bodies are stored byte-for-byte as received, except `medrxiv-web__pdf-403-block.html`,
  where the client IP address shown by Cloudflare was replaced with `REDACTED-IPV6`.

Naming: `<source>__<case>.<ext>`. The extension reflects the **actual body** (`.json`, `.xml`,
`.html`, `.txt`); a `-404`/`-500` suffix marks an error body served with that status.

The research brief that interprets these fixtures is `research/05-oa-sources.md`.

## Test set

| Case key | DOI | What it represents | Observed OA situation (2026-10-20) |
|---|---|---|---|
| `gold` | `10.1371/journal.pmed.1000097` | PRISMA statement, PLOS Medicine 2009 (gold OA, CC BY) | Gold, CC BY. **No direct PDF URL from any index** (publisher location is the doi.org landing page). PDF only via landing-page `citation_pdf_url` or PMC S3 copy (PMC2707599). |
| `green` | `10.1038/nature12373` | Classic Unpaywall example, Nature 2013 | Unpaywall/OpenAlex call it **bronze** with publisher PDF `nature.com/articles/nature12373.pdf`, but that URL now cookie-redirects to the HTML article page. Green copies: arXiv 1304.1068 (PDF works), PMC4221854 (author manuscript, licence TDM, **no PDF in PMC S3**), Harvard DASH. |
| `arxiv` | `10.48550/arXiv.1706.03762` | "Attention Is All You Need" (DataCite DOI) | Not in Unpaywall / Zotero OA / Crossref (DataCite DOI). OpenAlex resolves it to a *merged* work whose canonical DOI is `10.65215/2q58a426` and whose `best_oa_location` is a third-party mirror. PDF via `https://arxiv.org/pdf/1706.03762`. |
| `medrxiv` | `10.1101/2023.05.31.23290770` | medRxiv preprint (surgery, CC BY), bariatric surgery outcomes | Green (preprint). PDF via `medrxiv.org/content/medrxiv/early/2023/06/01/2023.05.31.23290770.full.pdf`. OpenAlex resolves the DOI to the **published** PLOS ONE version (`10.1371/journal.pone.0298402`), which bioRxiv's own API does not know (`published: "NA"`). Not in Semantic Scholar. |
| `paywalled` | `10.1097/SLA.0000000000005603` | Annals of Surgery 2022 (Wolters Kluwer), cold-stored whole blood | Closed in every source. CORE `/discover` nevertheless returns a link (false positive; the CORE output id 404s). |
| `nonexistent` | `10.9999/zft.nonexistent.000001` | Made-up DOI | Error shapes. |

Other identifiers used: PMID `19621072` (gold), PMCID `PMC2707599` (gold), PMCID `PMC4221854`
(green AAM), arXiv `1706.03762`, medRxiv `10.1101/2024.01.30.24301991` (v2 date-path check) and
`10.1101/2024.02.26.24303409` (has a `published_doi`, used for the `/pubs` fixture).

## Files by source

### Zotero OA service (`zotero-oa__*`)
`POST https://services.zotero.org/oa/search` with body `{"doi": "<doi>"}`. Always `200` + JSON
array (empty array for unknown, invalid, DataCite and closed DOIs). `GET` returns `403`.
Cases: `gold`, `green`, `arxiv` (`[]`), `medrxiv`, `paywalled` (`[]`), `nonexistent` (`[]`),
`invalid-doi` (`[]`).

### Unpaywall v2 (`unpaywall__*`)
`GET https://api.unpaywall.org/v2/<doi>?email=<email>`.
`gold`, `green`, `medrxiv`, `paywalled` → `200` JSON. `arxiv-404.html`, `nonexistent-404.html`,
`error-invalid-doi-404.html` → `404` with an **HTML** body. `error-no-email.json` and
`error-test-example-email.json` → `422` JSON (`test@example.com` is explicitly rejected).

### OpenAlex (`openalex__*`, `openalex-content__*`)
`GET https://api.openalex.org/works/doi:<doi>` → `gold`, `green`, `arxiv`, `medrxiv`, `paywalled`
(`200`); `nonexistent-404.html` (`404`, HTML body). `by-pmid` (`/works/pmid:19621072`, `200`),
`by-pmcid-404.html` (`/works/pmcid:4221854`, `404`), `by-pmcid-filter` (`filter=ids.pmcid:…`,
`200`, 0 results), `filter-doi-select` (batch of 3 DOIs via `filter=doi:a|b|c&select=…`),
`green-select` (single work with `select=`), `title-search` (`search=`).
`openalex-content__no-key.json`: `content.openalex.org` PDF download without key → `401`.

### Europe PMC REST (`europepmc__*`, `europepmc-web__*`)
`GET https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=DOI:"<doi>"&resultType=core&format=json`
→ `gold`, `green`, `medrxiv` (a `PPR` preprint record), `paywalled`, `nonexistent` (`hitCount: 0`).
Also `by-pmcid` (core), `by-extid-pmid` (lite), `batch-or-lite` (4 DOIs OR-ed in one query),
`fulltext-xml-gold.xml` (`/PMC2707599/fullTextXML`, JATS).
`europepmc-web__pdf-render-cloudflare-challenge.html`: `europepmc.org/articles/PMC2707599?pdf=render`
→ `403`, Cloudflare managed challenge.

### NCBI PMC (`ncbi-idconv__*`, `ncbi-pmc-oa__*`, `pmc-s3__*`, `pmc-web__*`)
* ID converter `https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?ids=…&format=json&tool=…`:
  `gold-green-paywalled` (3 DOIs in one call; paywalled → per-record `status: "error"`), `pmid`,
  `nonexistent`.
* `ncbi-pmc-oa__retired-404.html`: the old OA Web Service `oa.fcgi` (retired 2026-08-25) → `404`.
* PMC Cloud Service S3 bucket `pmc-oa-opendata` (public, no auth):
  `list-PMC2707599.xml` / `list-PMC4221854.xml` (ListObjectsV2 by prefix),
  `meta-gold.json` (`metadata/PMC2707599.1.json`, has `pdf_url`),
  `meta-green-aam.json` (`metadata/PMC4221854.1.json`, author manuscript, licence `TDM`, **no** `pdf_url`),
  `meta-nonexistent.xml` (`404` S3 `NoSuchKey` XML).
* `pmc-web__pdf-pow-interstitial.html`: `pmc.ncbi.nlm.nih.gov/articles/PMC2707599/pdf/pmed.1000097.pdf`
  → `200 text/html` "Preparing to download ..." proof-of-work page.

### arXiv / DataCite (`arxiv__*`, `datacite__*`)
`arxiv__export-api.xml` (`export.arxiv.org/api/query?id_list=1706.03762`, Atom),
`arxiv__export-api-nonexistent.xml` (Atom feed with `totalResults` 0, still `200`),
`datacite__arxiv.json` (`api.datacite.org/dois/10.48550/arXiv.1706.03762`).

### bioRxiv / medRxiv (`biorxiv-api__*`, `medrxiv-web__*`)
`biorxiv-api__medrxiv-details.json` (`api.biorxiv.org/details/medrxiv/<doi>/na/json`),
`biorxiv-api__medrxiv-pubs.json` (`/pubs/medrxiv/<doi>/na/json`, preprint→published mapping),
`biorxiv-api__nonexistent.json` (`200`, `messages[0].status: "no posts found"`).
`medrxiv-web__pdf-403-block.html`: canonical `…v1.full.pdf` URL → `403` Cloudflare WAF block (intermittent).

### Semantic Scholar (`semanticscholar__*`)
`GET https://api.semanticscholar.org/graph/v1/paper/DOI:<doi>?fields=title,externalIds,isOpenAccess,openAccessPdf,publicationTypes`
→ `gold`, `green`, `paywalled` (`200`); `arxiv`, `medrxiv`, `nonexistent` (`404` JSON `{"error": …}`).
`batch.json` and `batch-other-ids.json`: `POST /graph/v1/paper/batch` (unknown ids → `null` entries;
covers `ARXIV:`, `PMID:`, `PMCID:` prefixes). `error-429-unauthenticated.json`: shared-pool throttle.

### CORE v3 (`core__*`)
All without an API key. `discover-{gold,green,paywalled}-no-key.json` (`POST /v3/discover`),
`output-green-no-key.json` (`GET /v3/outputs/28948651`), `output-paywalled-hit-no-key.json` (`404`),
`search-no-key-500.json` (`GET /v3/search/works/?q=doi:"…"` → `500`).

### Crossref (`crossref__*`)
`GET https://api.crossref.org/works/<doi>` → `gold`, `green` (has `link[]` with
`application/pdf` + `intended-application: text-mining`), `paywalled`, `medrxiv`
(`type: posted-content`, `subtype: preprint`); `nonexistent-404.txt` (`404`, `text/plain`
"Resource not found.").

### DOAJ (`doaj__*`)
`GET https://doaj.org/api/search/articles/doi:<doi>` → `gold` (1 hit, `bibjson.link[].type: fulltext`
pointing at doi.org), `paywalled` (`total: 0`).

## Refreshing

Re-capture with the same URLs from `manifest.json`. Respect each API's limits (see the brief);
in particular Semantic Scholar throttles unauthenticated single-paper calls almost immediately,
arXiv asks for at most one request every 3 seconds, and CORE allows 10 keyless requests a minute.
Expect drift: OpenAlex/Unpaywall locations, Cloudflare behaviour and the PMC proof-of-work page
change over time, so tests should assert on structure, not on exact URL lists.
