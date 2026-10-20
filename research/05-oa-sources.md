# 05 — Open-access full-text discovery sources (state as of 2026-10-20)

Scope: every open-access (OA) lookup source the `zotero-fulltext` plugin could call before it falls
back to KCL institutional access. For each source this brief gives the endpoint, lookup keys,
authentication, rate limits, the exact JSON fields that give a **PDF URL** versus a **landing-page
URL**, the OA status / version / licence fields, error shapes and gotchas. It ends with a
recommended source order, the parallelism model and the settings the plugin needs.

Live fixtures for every source are in `fixtures/oa/` (index: `fixtures/oa/README.md`,
machine-readable: `fixtures/oa/manifest.json`, headers: `fixtures/oa/_capture-log.txt`).
All captures: 2026-10-20, unauthenticated, no cookies, no personal email.

Confidence tags: **[high]** = verified in local Zotero code or empirically today;
**[medium]** = official documentation; **[low]** = inferred or second-hand.

Test set used throughout (details in the fixtures README):

| Key | DOI | Represents |
|---|---|---|
| gold | `10.1371/journal.pmed.1000097` | PLOS Medicine, gold OA, CC BY, also in PMC (PMC2707599) |
| green | `10.1038/nature12373` | Nature; "bronze" publisher copy plus arXiv, PMC author manuscript (PMC4221854), Harvard DASH |
| arxiv | `10.48550/arXiv.1706.03762` | arXiv paper with a DataCite DOI |
| medrxiv | `10.1101/2023.05.31.23290770` | medRxiv surgery preprint (CC BY), later published in PLOS ONE |
| paywalled | `10.1097/SLA.0000000000005603` | Annals of Surgery 2022, closed |
| nonexistent | `10.9999/zft.nonexistent.000001` | error shapes |

---

## 0. Key findings

1. **Unpaywall and OpenAlex are now one database.** Unpaywall moved onto the OpenAlex database in
   late 2025; the Unpaywall API is a legacy-format view of the same data
   ([help.openalex.org/access/unpaywall](https://help.openalex.org/access/unpaywall.md)) [medium].
   Empirically the Unpaywall, Zotero-OA and OpenAlex location lists are the same for all test
   DOIs [high]. **OpenAlex single-work lookups are free and keyless**, so OpenAlex can replace
   Unpaywall and needs no email.
2. **Zotero's own OA lookup is an Unpaywall mirror**: `POST https://services.zotero.org/oa/search`
   with `{"doi": …}`. It returns `[{url?, pageURL?, version?}]` = Unpaywall's
   `url_for_pdf` / `url_for_landing_page` / `version`. Zotero marks it as a private API
   (`.ref/omni-app/chrome/content/zotero/xpcom/utilities_internal.js:1427-1466`) [high].
3. **The PMC OA Web Service (`oa.fcgi`) was retired on 2026-08-25**, along with the FTP and old cloud
   article packages. The replacement is the public S3 bucket `pmc-oa-opendata`, which holds a
   per-article JSON with a direct `pdf_url`. This is the most reliable OA PDF route found:
   plain HTTPS, no bot wall [high].
4. **PMC website PDFs now sit behind a JavaScript proof-of-work interstitial** ("Preparing to
   download ...", cookie `cloudpmc-viewer-pow`). Zotero 10 handles it in a hidden browser
   (`Zotero.BrowserRequest`, commit `80658e928` "Handle download challenge on PubMed Central",
   2026-07-30) [high].
5. **europepmc.org (the `?pdf=render` and `ptpmcrender.fcgi` PDF URLs) returns a Cloudflare managed
   challenge (403) to non-browser clients**, so Europe PMC is no longer a plain-HTTP workaround
   for PMC PDFs. Europe PMC's REST API (`www.ebi.ac.uk/europepmc/webservices/rest`) is
   unaffected and remains very useful for metadata [high, from this network].
6. **medRxiv canonical PDF URLs intermittently return a Cloudflare WAF block (403)**. The
   `/content/medrxiv/early/YYYY/MM/DD/<suffix>.full.pdf` form, built from the API's `date` field,
   returned 200 `application/pdf` every time [high, empirical].
7. **Gold OA does not mean "has a PDF URL".** For the PLOS gold paper, Unpaywall, OpenAlex,
   Semantic Scholar and DOAJ all give only `https://doi.org/…` (a landing page). The PDF comes from
   the landing page's `<meta name="citation_pdf_url">`, so landing-page HTML extraction in the
   Rust core is essential [high].
8. **OpenAlex merges works.** The arXiv DOI resolves to a work whose canonical `doi` is
   `10.65215/2q58a426`, with a third-party mirror as `best_oa_location`. The medRxiv DOI resolves to
   the published PLOS ONE work. Never trust `best_oa_location` blindly. Rank all `locations[]` [high].
9. **Keys are now the norm.** OpenAlex replaced the `mailto` polite pool with free API keys in
   February 2026 (`mailto` is ignored). Without a key, list/search calls share a $0.10/day budget
   per IP address, which on the KCL network is shared with every other keyless user. Semantic
   Scholar throttles keyless calls almost at once (429). CORE allows 100 keyless tokens a day
   [high/medium].

---

## 1. How Zotero 10.0.6 finds OA files (source of truth: extracted omni.ja)

### 1.1 Resolver chain — `Zotero.Attachments.getFileResolvers(item, methods, automatic)`
`.ref/omni-app/chrome/content/zotero/xpcom/attachments.js:1275-1490` [high]

Default `methods = ['doi', 'url', 'oa', 'custom']`, which produces resolvers in this order:

1. `{ pageURL: 'https://doi.org/' + doi, accessMethod: 'doi' }`
2. `{ pageURL: item.url, accessMethod: 'url' }`
3. an async function that calls `Zotero.Utilities.Internal.getOpenAccessPDFURLs(doi)` and maps each
   result to `{ url, pageURL, articleVersion: o.version, accessMethod: 'oa' }`
4. if the item has a PMCID: `{ pageURL: 'https://pmc.ncbi.nlm.nih.gov/articles/PMC…/', accessMethod: 'oa' }`
5. custom resolvers from the pref `extensions.zotero.findPDFs.resolvers` (JSON: `name`, `method`
   GET/POST, `url` containing `{doi}`, `mode` html|json, `selector`, optional `attribute`/`index`,
   `mappings` {url,pageURL}).

### 1.2 The OA call — `Zotero.Utilities.Internal.getOpenAccessPDFURLs(doi, {timeout})`
`.ref/omni-app/chrome/content/zotero/xpcom/utilities_internal.js:1438-1466`; base URL from
`.ref/omni-app/resource/config.mjs:14` (`SERVICES_URL: 'https://services.zotero.org/'`) [high]

```js
// what Zotero does (abridged)
Zotero.HTTP.request('POST', 'https://services.zotero.org/oa/search', {
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ doi }),          // doi already passed through cleanDOI()
  responseType: 'json', timeout: 10000,
  errorDelayIntervals: [1000, 2000], errorDelayMax: 5000,
});
// -> [{ url?: string, pageURL?: string, version?: 'submittedVersion'|'acceptedVersion'|'publishedVersion' }]
```

Zotero adds `X-Zotero-Version` to every request to `*.zotero.org`
(`xpcom/zotero.js:2256-2260`) [high]. The comment at `utilities_internal.js:1430` says: "This uses a
private API. Please use Unpaywall directly for non-Zotero projects." Because the plugin runs inside
Zotero, calling `Zotero.Utilities.Internal.getOpenAccessPDFURLs()` (not a hand-rolled POST) is the
defensible way to use it. Note that the `Internal` namespace can change between releases.

### 1.3 The download loop — `Zotero.Attachments.downloadFirstAvailableFile(urlResolvers, path, options)`
`attachments.js:1978-2310` [high]
* Function resolvers run lazily, only if earlier entries failed. **At most 6 URLs are kept per
  function resolver** (`maxURLs = 6`, line 1979/2041).
* `http:` is forced to `https:` (line 2060-2064).
* "Tried" de-duplication normalises URLs by **dropping every query parameter except `download`**
  (lines 1987-1997). Two URLs that differ only in their query string count as the same URL.
* `url` is downloaded first. If it fails, `pageURL` is fetched with manual redirect following
  (max 10, max 2 per URL). HTML pages are parsed, meta-refreshes followed, and if the refresh
  target matches `Zotero.BrowserRequest.CHALLENGE_URLS` the page is loaded once in a hidden browser
  to bank cookies (lines 2216-2241). Then Zotero translators extract the file link
  (`getFileFromDocument`).
* Accepted file types: `FIND_AVAILABLE_FILE_TYPES = ['application/pdf', 'application/epub+zip']`
  (line 43). The file type is enforced after download.
* `addAvailableFiles()` throttles to 1 request per second per domain (`SAME_DOMAIN_REQUEST_DELAY = 1000`)
  and gives up on a domain after 5 consecutive failures (lines 1513-1514).

### 1.4 Bot-challenge registry — `Zotero.BrowserRequest.CHALLENGE_URLS`
`.ref/omni-app/chrome/content/zotero/xpcom/browserRequest.js:30-76` [high]
Hosts: `zotero-static.s3.amazonaws.com` test page, `www.sciencedirect.com` (`#captcha-box`),
`search.worldcat.org` (Turnstile, cookie `turnstile_passed`), **`pmc.ncbi.nlm.nih.gov`
("Proof-of-work challenge, so nothing for the user to solve")**, `www.mdpi.com` (Akamai).
`PLAIN_UA_HOSTS` remove the `Zotero/x` UA suffix because "Cloudflare Turnstile rejects the
Zotero/[version] suffix" (`browserRequest.js:125-127`, `zotero.js:2280-2292`).
`clearChallenge(url, {userContextId, entry, allowViewer})` loads the page in a `HiddenBrowser` that
shares the cookie jar with `Zotero.HTTP.request`.

**Implication for the plugin:** the JS shell can build its own resolver list (from the Rust
ranking) and hand it to `Zotero.Attachments.addFileFromURLs(item, resolvers, options)`
(`attachments.js:1872`). That reuses Zotero's challenge handling, landing-page translators, file-type
checks and attachment creation. It also inherits the 6-URL cap and the query-stripping
de-duplication, so the plugin should put its best URLs first and avoid relying on query-only
differences.

---

## 2. Source reference

### 2.1 Zotero OA service (Unpaywall mirror)
| | |
|---|---|
| Endpoint | `POST https://services.zotero.org/oa/search`, body `{"doi":"<doi>"}`, `Content-Type: application/json` [high] |
| Lookup keys | DOI only (Crossref DOIs; DataCite DOIs give `[]`) [high] |
| Auth / limits | none; no rate-limit headers seen; served via API Gateway + CloudFront [high]. Limits undocumented (private API) [low] |
| PDF field | `url` (= Unpaywall `url_for_pdf`) [high] |
| Landing field | `pageURL` (= Unpaywall `url_for_landing_page`) [high] |
| Version | `version`: `submittedVersion` / `acceptedVersion` / `publishedVersion`, sometimes absent [high] |
| OA status / licence | not returned [high] |
| Errors | always `200` + `[]` for unknown / invalid / closed DOIs; `GET` → `403` [high] |

Example (`zotero-oa__green.json`, abridged):
```json
[{"url":"https://www.nature.com/articles/nature12373.pdf","pageURL":"https://doi.org/10.1038/nature12373","version":"publishedVersion"},
 {"url":"https://arxiv.org/pdf/1304.1068","pageURL":"http://arxiv.org/abs/1304.1068","version":"submittedVersion"},
 {"pageURL":"https://www.ncbi.nlm.nih.gov/pmc/articles/4221854","version":"submittedVersion"}]
```
Gotchas: no licence or OA colour; cannot tell "no OA" apart from "unknown DOI"; PMC landing URLs use
the legacy `www.ncbi.nlm.nih.gov/pmc/articles/<number>` form, which 301-redirects to
`pmc.ncbi.nlm.nih.gov/articles/<number>` [high].

### 2.2 Unpaywall v2
| | |
|---|---|
| Endpoint | `GET https://api.unpaywall.org/v2/{doi}?email={email}` [high] |
| Lookup keys | DOI only; **Crossref DOIs only** (DataCite DOIs, such as arXiv's, give 404) [high]; docs: [help.openalex.org/access/unpaywall](https://help.openalex.org/access/unpaywall.md) [medium] |
| Auth | `email` query parameter **required**. Tested 2026-10-20 [high]: missing or empty → `422` "Email address required in API call"; `test@example.com` → `422` "Please use your own email address in API calls"; the non-email string `notanemail` → `422` (same message); other syntactically valid placeholders (`…@example.org`, `unpaywall_01@example.com`) → `200`. Ship no default email: make it a user setting and skip Unpaywall when it is empty. |
| Rate limit | "please limit use to 100,000 calls per day" (official docs page is a JS app; figure confirmed via search-engine snapshot of unpaywall.org/api/v2) [medium]. No rate-limit headers; served by Heroku [high] |
| PDF field | `oa_locations[].url_for_pdf` (nullable); `best_oa_location.url_for_pdf` [high] |
| Landing field | `oa_locations[].url_for_landing_page`; `url` = `url_for_pdf ?? url_for_landing_page` [high] |
| Status / version / licence | top level: `is_oa`, `oa_status` (`gold`/`hybrid`/`bronze`/`green`/`closed`), `has_repository_copy`, `journal_is_oa`, `journal_is_in_doaj`, `genre` (`journal-article`, `posted-content`, …), `data_standard`. Per location: `host_type` (`publisher`/`repository`), `version`, `license` (`cc-by`, `cc-by-nc`, `other-oa`, null), `is_best`, `oa_date`, `pmh_id`, `endpoint_id`, `repository_institution`. Also `first_oa_location`, `oa_locations_embargoed` [high] |
| Errors | unknown or DataCite or malformed DOI → **`404` with an HTML body** (`<title>404 Not Found</title>`), not JSON [high] |

Gotchas [high]: `evidence` and per-location `updated` are now the literal string `"deprecated"`, so
do not parse them as dates or enums. The top-level `updated` is still an ISO timestamp.
`url_for_pdf` is null for the PLOS gold publisher location. The `nature12373` "bronze" publisher PDF
redirects to an HTML page (see §4).

### 2.3 OpenAlex
| | |
|---|---|
| Single work | `GET https://api.openalex.org/works/doi:{doi}` (also `/works/https://doi.org/{doi}`, `/works/pmid:{pmid}`, `/works/W…`) [high]. **`/works/pmcid:{n}` returned 404 and `filter=ids.pmcid:` returned 0 hits** for PMC4221854, although `ids.pmcid` exists for other works. Do not rely on PMCID lookup [high] |
| Batch | `GET /works?filter=doi:{d1}|{d2}|…&select=id,doi,ids,open_access,best_oa_location,primary_location,locations&per_page=100` (≤100 OR values) [high/medium] |
| Title search | `GET /works?search={title}&per_page=3` (costs 10× a list call) [high] |
| Auth | optional key: `api_key=` query parameter or `Authorization: Bearer` header ([help.openalex.org/api/authentication](https://help.openalex.org/api/authentication.md)) [medium]. **`mailto` polite pool replaced by keys in Feb 2026; `mailto` is ignored** ([deprecations](https://help.openalex.org/api/deprecations.md)) [medium] |
| Pricing / limits | single-entity GET: **free** (unlimited); list+filter $0.0001/call; search $0.001/call; content PDF download $0.01/file. No key: $0.10/day **shared by every keyless caller on the same IP**; free key $1/day ([example-costs](https://help.openalex.org/access/example-costs.md)) [medium]. Rate: 10 req/s (no key), 30 (free key) [medium]. Headers seen: `X-RateLimit-Limit: 1000`, `X-RateLimit-Remaining`, `X-RateLimit-Credits-Used`, `X-RateLimit-Limit-USD: 0.1`, `X-RateLimit-Cost-USD`, `X-RateLimit-Reset` (seconds to midnight UTC); singleton `Cost-USD: 0`, filter call `0.0001` [high] |
| PDF field | `locations[].pdf_url`, `best_oa_location.pdf_url`, `primary_location.pdf_url` [high] |
| Landing field | `locations[].landing_page_url` (same three places); `open_access.oa_url` is "best URL", which may be a landing page [high] |
| Status / version / licence | `open_access.{is_oa, oa_status (gold/diamond/hybrid/bronze/green/closed), oa_url, any_repository_has_fulltext}`; per location `is_oa`, `version`, `license` (`cc-by`…), `license_id`, `is_accepted`, `is_published`, `source.{display_name,type (journal/repository),…}`, `id` (`doi:…`, `pmid:…`, `pmh:oai:…`, `mag:…`) [high] |
| Cached full text | `has_content: {pdf, grobid_xml}`, `content_urls.pdf = https://content.openalex.org/works/{W}.pdf`. Needs a key (`401 {"error":"API key required"}` without one), $0.01 each, so a free key covers 100 PDFs/day ([fulltext](https://help.openalex.org/access/fulltext.md)) [high/medium]. Cached for the green and arXiv test works, not for gold, medRxiv or paywalled [high] |
| Errors | unknown DOI → **`404` HTML body** (same Flask page as Unpaywall) [high] |

Example (`openalex__green.json` `locations[]`, abridged):
```json
{"id":"doi:10.1038/nature12373","is_oa":true,"landing_page_url":"https://doi.org/10.1038/nature12373",
 "pdf_url":"https://www.nature.com/articles/nature12373.pdf","source":{"display_name":"Nature","type":"journal"},
 "license":null,"version":"publishedVersion","is_accepted":true,"is_published":true}
{"id":"pmh:oai:arXiv.org:1304.1068","is_oa":true,"landing_page_url":"http://arxiv.org/abs/1304.1068",
 "pdf_url":"https://arxiv.org/pdf/1304.1068","version":"submittedVersion","is_accepted":false,"is_published":false}
```

Gotchas [high]:
* **Work merging**: `works/doi:10.48550/arXiv.1706.03762` returns W2626778328 with
  `doi: https://doi.org/10.65215/2q58a426`, and `best_oa_location.pdf_url` points at
  `langtaosha.org.cn/...` (a third-party copy). The genuine arXiv location is further down
  `locations[]` (`pmh:oai:arXiv.org:1706.03762`). `works/doi:10.1101/2023.05.31.23290770` returns the
  **PLOS ONE** work (`10.1371/journal.pone.0298402`). Its `locations[]` include the medRxiv PDF and a
  PMC PDF. Always compare `work.doi` and `location.id` with the requested identifier, and rank
  locations yourself.
* DOIs come back lowercased (`10.1097/sla…`), so compare case-insensitively.
* PMC locations' `pdf_url` (`https://pmc.ncbi.nlm.nih.gov/articles/PMC…/pdf/….pdf`) hits the PoW wall.
* The API's 404 is HTML, not JSON.
* Keyless list/search budget is per IP. KCL's NAT means it is shared campus-wide, so use singletons
  (free) or a user key.

### 2.4 Europe PMC REST
| | |
|---|---|
| Base | `https://www.ebi.ac.uk/europepmc/webservices/rest` [high] |
| By DOI | `GET /search?query=DOI:"{doi}"&resultType=core&format=json` (quotes URL-encoded) [high] |
| By PMID / PMCID / other | `query=EXT_ID:{pmid} AND SRC:MED`, `query=PMCID:PMC2707599`, preprints have `source: "PPR"`, `id: "PPR668781"` [high] |
| Batch | `query=DOI:"a" OR DOI:"b" OR …&resultType=lite&pageSize=100` worked (4 DOIs → 4 hits) [high] |
| Full text | `GET /{PMCID}/fullTextXML` → JATS XML for OA-subset articles (`200` for PMC2707599; `500` JSON for the AAM PMC4221854) [high] |
| Auth / limits | none; no rate-limit headers [high]; no published hard limit found (docs site is itself behind Cloudflare) [low] |
| PDF field | `resultList.result[].fullTextUrlList.fullTextUrl[]` where `documentStyle == "pdf"`: `{availability, availabilityCode, documentStyle, site, url}` [high] |
| Landing field | same list, `documentStyle` `html` or `doi` [high] |
| OA fields | `isOpenAccess` (`Y`/`N`), `inEPMC`, `inPMC`, `hasPDF`, `license` (`cc by`…), `pmcid`, `fullTextIdList.fullTextId[]`, `pubTypeList.pubType[]` (`Preprint`), `availabilityCode` `OA` (open access), `F` (free), `S` (subscription) [high] |
| Errors | unknown → `200`, `hitCount: 0`, `result: []` [high] |

Observed `fullTextUrl` values [high]:
```
gold : OA pdf  Europe_PMC https://europepmc.org/articles/PMC2707599?pdf=render
green: F  pdf  Europe_PMC https://europepmc.org/articles/PMC4221854?pdf=render
medrxiv: OA pdf Unpaywall https://www.medrxiv.org/content/medrxiv/early/2023/06/01/2023.05.31.23290770.full.pdf
paywalled: S doi DOI https://doi.org/10.1097/SLA.0000000000005603
```
Gotchas [high]: the `europepmc.org` URLs it hands out (`?pdf=render`, and the older
`https://europepmc.org/backend/ptpmcrender.fcgi?accid=PMC…&blobtype=pdf`) returned **403
Cloudflare "Just a moment..." (`cf-mitigated: challenge`)** to curl, with both a neutral and a
Zotero/Firefox UA (`europepmc-web__pdf-render-cloudflare-challenge.html`). The website article pages do
the same. Europe PMC re-exports Unpaywall links (`site: "Unpaywall"`). DOIs come back lowercased.
**Best use:** a cheap, keyless DOI/PMID → PMCID + licence + "is it in PMC?" oracle, plus preprint
records. Not a PDF host for non-browser clients any more.

### 2.5 NCBI / PMC
**ID converter** [high/medium]
* `GET https://pmc.ncbi.nlm.nih.gov/tools/idconv/api/v1/articles/?ids={id1,id2,…}&format=json&tool={name}&email={email}`
  (≤200 IDs of one type per call; `idtype=pmid|pmcid|doi|mid` optional; `versions=no`)
  ([docs](https://pmc.ncbi.nlm.nih.gov/tools/id-converter-api/)) [medium].
* `tool` and `email` are "requested"; without `email` the response carries
  `request.warnings: ["query param `email` is missing."]` and still works [high].
* Records: `{doi, pmcid, pmid, requested-id}`; not in PMC → `{status:"error", errmsg:"Identifier not found in PMC"}` per record [high].
* Only returns IDs for articles **in PMC** (so paywalled articles that are only in PubMed get no PMID) [medium].
* No published rate limit for idconv. Assume the NCBI norm of ≤3 req/s without a key [low].

**OA Web Service (`oa.fcgi`) — retired** [high]
* `https://www.ncbi.nlm.nih.gov/pmc/utils/oa/oa.fcgi?id=PMC…` → `404` HTML (`ncbi-pmc-oa__retired-404.html`).
* [pmc.ncbi.nlm.nih.gov/tools/oa-service](https://pmc.ncbi.nlm.nih.gov/tools/oa-service/): "August 25, 2026 As part of
  the PMC Article Dataset Changes announced on February 12, 2026, the PMC OA Web Service is no
  longer available." Legacy FTP and cloud article files were removed the week of 2026-08-24
  ([pmcaws](https://pmc.ncbi.nlm.nih.gov/tools/pmcaws/), [NCBI Insights 2026-02-12](https://ncbiinsights.ncbi.nlm.nih.gov/2026/02/12/pmc-article-dataset-distribution-services/)) [medium].

**PMC Cloud Service — S3 bucket `pmc-oa-opendata` (the replacement)** [high]
* Public HTTPS, no auth, `us-east-1` ([README.txt](https://pmc-oa-opendata.s3.amazonaws.com/README.txt)).
* Per article version: prefix `PMC{n}.{v}/` with `PMC{n}.{v}.json|.xml|.txt` and, when the licence
  allows, `PMC{n}.{v}.pdf` plus media. JSON copies live under `metadata/PMC{n}.{v}.json`.
* Metadata JSON fields: `pmcid, version, pmid, doi, mid, title, citation, is_pmc_openaccess,
  is_manuscript, is_historical_ocr, is_retracted, license_code ("CC BY"… or "TDM"), pdf_url?,
  xml_url, text_url, media_urls[]`.
* `pdf_url` is an **`s3://` URI with `?md5=`**:
  `s3://pmc-oa-opendata/PMC2707599.1/PMC2707599.1.pdf?md5=dc66…`. Rewrite it to
  `https://pmc-oa-opendata.s3.amazonaws.com/PMC2707599.1/PMC2707599.1.pdf`. The response is `200`,
  `Content-Type: binary/octet-stream` (not `application/pdf`), body starts `%PDF-1.5`. Sniff the bytes
  and supply the MIME type yourself.
* Version discovery: `GET https://pmc-oa-opendata.s3.amazonaws.com/?list-type=2&prefix=PMC2707599.`
  (trailing dot, so `PMC27075990…` is not matched). "Do not assume that a version 1 always exists"
  ([pmcaws FAQ](https://pmc.ncbi.nlm.nih.gov/tools/pmcaws/)) [medium]. Missing key → `404` S3 XML `NoSuchKey`.
* **Author manuscripts without a CC licence have no PDF here.** PMC4221854 (`nature12373`
  AAM) has `license_code: "TDM"`, `is_manuscript: true` and no `pdf_url`. Only XML and TXT exist
  ([cloud](https://pmc.ncbi.nlm.nih.gov/tools/cloud/): "PDFs … for author manuscripts without
  Creative Commons licenses are NOT made available") [high/medium].
* Usage policy ([textmining](https://pmc.ncbi.nlm.nih.gov/tools/textmining/)): "The PMC Cloud Service, PMC
  OAI-PMH Service, E-Utilities and BioC API are the only services that may be used for automated
  retrieval of PMC content. Systematic retrieval … through any other automated process is
  prohibited." [medium]. A per-item, user-initiated fetch is not bulk retrieval, but the S3 route is the
  sanctioned one, so prefer it.

**PMC website PDFs** [high]
* Pattern: `https://pmc.ncbi.nlm.nih.gov/articles/PMC{n}/pdf/{file}.pdf`. The file name comes from the
  article page's `<meta name="citation_pdf_url">`. `…/articles/PMC{n}/pdf/` 301-redirects to it.
  Legacy `https://www.ncbi.nlm.nih.gov/pmc/articles/…` URLs 301 to `pmc.ncbi.nlm.nih.gov`.
* A plain GET of the PDF URL returns `200 text/html`, `<title>Preparing to download ...</title>`, a
  module script `…/pow-*.js` and the inline constants `POW_CHALLENGE`, `POW_DIFFICULTY = "4"`,
  `POW_COOKIE_NAME = "cloudpmc-viewer-pow"`, `POW_COOKIE_EXPIRATION = "0.208333"` (days, about 5 h)
  (`pmc-web__pdf-pow-interstitial.html`).
* Zotero 10 handles this through `Zotero.BrowserRequest` (hidden browser runs the JS and banks the
  cookie). **The plugin must not re-implement the solver.** Detect the page (Rust
  `detect_challenge()`), then either delegate to Zotero's pipeline (`addFileFromURLs` with the PMC
  `pageURL`) or prefer the S3 copy.

### 2.6 arXiv (+ DataCite)
| | |
|---|---|
| DOI ↔ id | `10.48550/arXiv.{id}` (case-insensitive; Crossref's `/works/{doi}/agency` says `datacite`) ↔ arXiv id `{id}` (new `YYMM.NNNNN`, old `archive/NNNNNNN`) [high] |
| PDF URL | `https://arxiv.org/pdf/{id}` (latest) or `…/pdf/{id}v{N}`: `200 application/pdf`, no redirect [high] |
| Landing | `https://arxiv.org/abs/{id}` [high] |
| API | `GET https://export.arxiv.org/api/query?id_list={id}` → Atom; `<link title="pdf" rel="related" type="application/pdf" href="https://arxiv.org/pdf/1706.03762v7"/>`; unknown id → `200` with `<opensearch:totalResults>0` [high] |
| Limits | "no more than one request every three seconds", "a single connection at a time" ([API ToU](https://info.arxiv.org/help/api/tou.html)) [medium] |
| Licence | DataCite `rightsList` (`api.datacite.org/dois/10.48550/arXiv.…`): e.g. "arXiv.org perpetual, non-exclusive license" [high] |

Gotchas [high]: Unpaywall, Zotero OA and Crossref do not cover arXiv DOIs (404 / `[]`). Semantic
Scholar 404s on the arXiv DOI but finds `ARXIV:1706.03762`, though it reports `isOpenAccess: false`.
Build the PDF URL deterministically. No API call is needed unless you are resolving by title.

### 2.7 bioRxiv / medRxiv
| | |
|---|---|
| DOI prefix | `10.1101/` (both servers; Crossref member "openRxiv", `type: posted-content`, `subtype: preprint`) [high] |
| Details | `GET https://api.biorxiv.org/details/{biorxiv|medrxiv}/{doi}/na/json` → `collection[]` (one entry per version): `doi, title, version, date (YYYY-MM-DD of that version), type, license (cc_by, cc_no, cc_by_nc_nd…), category, jatsxml, published ("NA" or DOI), server` [high]. Unknown → `200 {"messages":[{"status":"no posts found"}],"collection":[]}` [high] |
| Published mapping | `GET https://api.biorxiv.org/pubs/{server}/{doi}/na/json` → `collection[].{preprint_doi, published_doi, published_journal, published_date}` [high]; [api.biorxiv.org](https://api.biorxiv.org/) [medium] |
| PDF URL (reliable) | `https://www.{server}.org/content/{server}/early/{YYYY}/{MM}/{DD}/{doi_suffix}.full.pdf`, with the date taken from the **latest version's `date`**. Verified for medRxiv v1 and v2 (a wrong date → 404) and for bioRxiv [high] |
| PDF URL (canonical) | `https://www.{server}.org/content/{doi}v{N}.full.pdf`. For medRxiv this returned a **Cloudflare WAF 403** ("Sorry, you have been blocked") on most tries; Cloudflare cache HITs returned the PDF. bioRxiv canonical returned 200 [high] |
| Limits | none documented [medium]; be gentle (≤1-2 req/s) [low] |

Gotchas [high]: the `published` field can be stale ("NA" for the test preprint, which OpenAlex
knows is published in PLOS ONE). `jatsxml` uses `/content/early/…` without the server segment.
`doi.org/10.1101/…` redirects to `http://medrxiv.org/lookup/doi/…`, which leads to the
Cloudflare-fronted abstract page (also intermittently 403). Unpaywall, Zotero OA and OpenAlex already
give the "early" PDF URL for medRxiv.

### 2.8 Semantic Scholar Graph API
| | |
|---|---|
| Single | `GET https://api.semanticscholar.org/graph/v1/paper/{id}?fields=title,externalIds,isOpenAccess,openAccessPdf` with `{id}` = `DOI:…`, `ARXIV:…`, `PMID:…`, `PMCID:{digits}`, `CorpusId:…`, `MAG:…`, `ACL:…`, `URL:…` (from `graph/v1/swagger.json`) [high] |
| Batch | `POST /graph/v1/paper/batch?fields=…` body `{"ids":[…]}`, ≤500 IDs, ≤10 MB response; unknown IDs → `null` in the array [high] |
| Auth | optional `x-api-key` header ([tutorial](https://www.semanticscholar.org/product/api/tutorial)) [medium] |
| Limits | keyless: "1000 requests per second, shared among all unauthenticated users"; key: introductory 1 req/s ([product/api](https://www.semanticscholar.org/product/api)) [medium]. **In practice, after six keyless single-paper calls spaced 2 s apart, every further single-paper call got `429`**, including retries spaced 15 s apart. Batch POSTs succeeded twice (one after a 90 s pause) and got 429 once [high] |
| PDF field | `openAccessPdf.url`. **Not always a PDF**: the PLOS gold case gives `https://doi.org/10.1371/journal.pmed.1000097`. When there is none: `{"url":"","status":null,"license":null}` (empty string, not null) [high] |
| Status / licence | `isOpenAccess`, `openAccessPdf.status` (`GOLD`, `GREEN`, `HYBRID`, `BRONZE`…), `openAccessPdf.license` (`CCBY`…), `externalIds.{DOI,ArXiv,PubMed,PubMedCentral,…}` [high] |
| Errors | `404 {"error":"Paper with id … not found"}`; `429 {"message":"Too Many Requests…","code":"429"}` [high] |

Value: low marginal value over OpenAlex for PDFs (it gave nothing OpenAlex lacked on the test set,
missed the medRxiv preprint and flagged the arXiv paper as not OA). Useful only as an identifier
cross-walk (`externalIds`) and, with a key, as a minor extra signal [high/low].

### 2.9 CORE API v3
| | |
|---|---|
| Discover | `POST https://api.core.ac.uk/v3/discover` body `{"doi":"…"}` → `{"fullTextLink": "http://core.ac.uk/labs/oadiscovery/redirect?url=https%3A%2F%2Fcore.ac.uk%2Freader%2F{id}&key=…", "source": "From CORE"}`; worked without a key [high] |
| Output | `GET /v3/outputs/{id}` → `downloadUrl` (`https://core.ac.uk/download/{id}.pdf`), `sourceFulltextUrls[]`, `links[]` (`download`, `reader`, `display`) [high]; `GET /v3/outputs/{id}/download` → `302` to `core.ac.uk/download/{id}.pdf` [high] |
| Search | `GET /v3/search/works/?q=doi:"…"` (note the trailing slash: without it you get a 301). Keyless returned `500 {"message":"The HTTP status code \"0\" is not valid."}` [high] |
| Auth | `Authorization: Bearer {key}`; "Access to the CORE API is free and requires no authentication" but registered users get more ([docs v3](https://api.core.ac.uk/docs/v3)) [medium] |
| Limits | keyless: **100 tokens/day, max 10/min**; registered personal: 1,000/day, 25/min; registered academic: 5,000/day, 10/min; simple query = 1 token, complex 3-5 [medium]. Headers `x-ratelimit-limit: 10`, `x-ratelimit-remaining`, `x-ratelimit-retry-after` [high] |
| Policy | PDFs must go through `/v3/outputs/{id}/download`. "Systematic harvesting of PDFs from CORE outside that circumvents the API, e.g. via `https://fileserver-az.core.ac.uk/download/{identifier}.pdf` … is unacceptable" [medium] |

Gotchas [high]: every `core.ac.uk` web URL (`/download/…pdf`, `/reader/…`, `/outputs/…`) returned a
**Cloudflare "Just a moment..." 403** to curl. **`/discover` produced a false positive** for the paywalled
Annals paper: it pointed at an output id that `GET /v3/outputs/{id}` reports as not found. Low value,
off by default.

### 2.10 Crossref REST
| | |
|---|---|
| Endpoint | `GET https://api.crossref.org/works/{doi}` → `message` [high] |
| Pools | public: `x-rate-limit-limit: 5`, `x-rate-limit-interval: 1s`, `x-concurrency-limit: 1`, `x-api-pool: public-single`; with `mailto={email}`: `10`/s, concurrency `3`, `polite-single` [high]; "Include your email address in the `mailto` parameter or `agent` header"; Plus pool 150/s with key ([access-and-authentication](https://www.crossref.org/documentation/retrieve-metadata/rest-api/access-and-authentication/)) [medium] |
| PDF field | `message.link[]` where `content-type == "application/pdf"`, with `content-version` (`vor`/`am`) and `intended-application` (`text-mining`, `similarity-checking`, `syndication`, `unspecified`) [high] |
| Landing | `message.resource.primary.URL`, `message.URL` [high] |
| Licence | `message.license[] {URL, content-version, delay-in-days, start.date-time}` (e.g. CC BY 4.0 for PLOS; `http://www.springer.com/tdm` for Nature) [high] |
| Other | `type`, `subtype` (`preprint`), `relation` (e.g. `is-preprint-of`; empty `{}` for the test preprint), `/works/{doi}/agency` (`crossref` vs `datacite`) [high] |
| Errors | unknown → `404`, `text/plain`, body `Resource not found.` [high] |

Value for OA: low. The test set had only one `application/pdf` link (Nature, `text-mining`/`similarity-checking`),
and that URL is not freely retrievable. Value for the **institutional phase: high**, because
`link[]` (`application/pdf`, `vor`) and `resource.primary.URL` give publisher URLs to rewrite through
the KCL EZproxy. Also a cheap DOI validity and registration-agency check.

### 2.11 DOAJ
| | |
|---|---|
| Endpoint | `GET https://doaj.org/api/search/articles/doi:{doi}` (alias of the current version; the response `last` link shows `/api/v4/…`; v4.0.1 since 2025-08-21) [high/medium] |
| Limits | 2 req/s, up to 5 queued ([docs](https://doaj.org/api/docs)) [medium]; Cloudflare-fronted, but the API answered curl [high] |
| Fields | `results[].bibjson.link[] {type:"fulltext", url}`, `bibjson.identifier[]`, `bibjson.journal.*`, `bibjson.license` (when present); none → `total: 0` [high] |

Value: negligible. The only link given is `https://doi.org/…` (landing page). Journal-in-DOAJ is
already in Unpaywall (`journal_is_in_doaj`) and OpenAlex. Skip it.

---

## 3. What each source returned for the test set

PDF = a direct PDF URL was offered; LP = landing page only; — = no OA info; 404/[] = not found.

| Source | gold (PLOS) | green (Nature) | arxiv DOI | medrxiv | paywalled | nonexistent |
|---|---|---|---|---|---|---|
| Zotero OA | LP×7 + 1 repo PDF (iris.unimore) | PDF nature.com, PDF arXiv, LP PMC/DASH | `[]` | PDF medRxiv early-path | `[]` | `[]` |
| Unpaywall | gold; same as Zotero OA (9 locs) | bronze; same | 404 HTML | green; PDF early-path | closed, 0 locs | 404 HTML |
| OpenAlex | gold; no `pdf_url` on publisher; 11 locs | bronze; PDF nature + arXiv; cached PDF | merged work, mirror PDF + arXiv loc | **returns PLOS ONE work**; medRxiv PDF + PMC PDF | closed | 404 HTML |
| Europe PMC | PMC2707599, OA, `?pdf=render` (CF-walled) | PMC4221854, Free, `?pdf=render` (CF) | (not queried) | PPR668781 + Unpaywall PDF link | MED only, S | hitCount 0 |
| PMC S3 | **PDF** (CC BY) | AAM, TDM, **no PDF** | — | — | — | NoSuchKey |
| Semantic Scholar | LP (doi.org) as `openAccessPdf.url` | PDF nature.com | 404 (ARXIV: id found, not OA) | 404 | `url: ""` | 404 |
| CORE discover | reader link | reader link | — | — | **false positive** | — |
| Crossref | no PDF link; CC BY licence | PDF link (TDM) | DataCite (404) | preprint, no PDF | no PDF | 404 text |
| DOAJ | LP (doi.org) | — | — | — | 0 | — |

## 4. PDF host behaviour with plain HTTP (curl, no cookies, 2026-10-20)

| URL pattern | Result | Confidence |
|---|---|---|
| `https://pmc-oa-opendata.s3.amazonaws.com/PMC{n}.{v}/PMC{n}.{v}.pdf` | 200, `binary/octet-stream`, `%PDF` | high |
| `https://arxiv.org/pdf/{id}[vN]` | 200 `application/pdf` | high |
| `https://www.medrxiv.org/content/medrxiv/early/Y/M/D/{suffix}.full.pdf` | 200 `application/pdf` (always, 6/6) | high |
| `https://www.medrxiv.org/content/10.1101/{suffix}v{N}.full.pdf` | **403 Cloudflare WAF**, intermittent 200 on cache HIT | high |
| `https://www.biorxiv.org/content/10.1101/{suffix}v{N}.full.pdf` and early path | 200 `application/pdf` | high (1 sample) |
| `https://journals.plos.org/plosmedicine/article/file?id={doi}&type=printable` (from `citation_pdf_url`) | 200 `application/pdf` | high |
| `https://www.nature.com/articles/nature12373.pdf` ("bronze") | 303 → `idp.nature.com/authorize?response_type=cookie…`; with a cookie jar it ends on the **HTML** article page | high |
| `https://pmc.ncbi.nlm.nih.gov/articles/PMC{n}/pdf/{file}.pdf` | 200 HTML **PoW interstitial** | high |
| `https://europepmc.org/articles/PMC{n}?pdf=render`, `…/backend/ptpmcrender.fcgi?accid=…&blobtype=pdf` | **403 Cloudflare managed challenge** | high (this network) |
| `https://core.ac.uk/download/{id}.pdf`, `/reader/{id}` | **403 Cloudflare managed challenge** | high |
| `https://content.openalex.org/works/{W}.pdf` | 401 without key; with key: cached PDF, $0.01 | high / medium |

The general rule: **always sniff the first bytes** (`%PDF-`) and the `Content-Type`. Treat
`text/html` responses as one of: a landing page (extract `citation_pdf_url`), a challenge page
(PMC PoW, Cloudflare `Just a moment...` / `cf-mitigated: challenge`, Cloudflare `Attention
Required! | Cloudflare` block), or a login page (institutional phase).

## 5. Normalised candidate model for the Rust core

Suggested types (the WASM boundary passes JSON strings in and JSON out):

```rust
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum OaSource { ZoteroOa, Unpaywall, OpenAlex, EuropePmc, PmcS3, Arxiv, BioRxiv,
                    SemanticScholar, Core, Crossref, Doaj }

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArticleVersion { Published, Accepted, Submitted, Unknown }   // map from *Version strings

#[derive(Serialize, Deserialize, Clone)]
pub struct Candidate {
    pub source: OaSource,
    pub pdf_url: Option<String>,        // direct file URL, if any
    pub landing_url: Option<String>,    // page to scrape / hand to Zotero translators
    pub version: ArticleVersion,
    pub license: Option<String>,        // normalised lowercase, e.g. "cc-by"
    pub host_kind: HostKind,            // Publisher | Repository | Preprint | Pmc | Cache
    pub record_doi: Option<String>,     // DOI of the record/location (detect OpenAlex merges)
    pub trust: u8,                      // computed by rank()
}

pub fn parse_zotero_oa(body: &str) -> Result<Vec<Candidate>, ParseError>;           // [{url,pageURL,version}]
pub fn parse_unpaywall(body: &str) -> Result<Vec<Candidate>, ParseError>;            // oa_locations[]
pub fn parse_openalex_work(body: &str, requested_doi: &str) -> Result<Vec<Candidate>, ParseError>; // locations[] + content_urls
pub fn parse_europepmc_search(body: &str) -> Result<EpmcHit, ParseError>;           // pmcid, license, isOpenAccess, fullTextUrl[]
pub fn parse_pmc_s3_meta(body: &str) -> Result<Option<Candidate>, ParseError>;       // pdf_url s3:// -> https://
pub fn parse_pmc_s3_list(xml: &str) -> Vec<String>;                                  // keys -> versions
pub fn parse_ncbi_idconv(body: &str) -> Result<Vec<IdRecord>, ParseError>;
pub fn parse_biorxiv_details(body: &str) -> Result<Option<Candidate>, ParseError>;   // builds the early-path URL
pub fn parse_semanticscholar(body: &str) -> Result<Vec<Candidate>, ParseError>;      // "" -> None
pub fn parse_crossref_work(body: &str) -> Result<CrossrefInfo, ParseError>;          // link[], license[], resource
pub fn arxiv_id_from_doi(doi: &str) -> Option<String>;                               // 10.48550/arXiv.X -> X
pub fn s3_to_https(s3_uri: &str) -> Option<String>;  // s3://pmc-oa-opendata/K?md5=… -> https://pmc-oa-opendata.s3.amazonaws.com/K
pub fn rank(cands: Vec<Candidate>, prefs: &RankPrefs) -> Vec<Candidate>;
pub fn sniff_pdf(first_bytes: &[u8]) -> bool;                                        // b"%PDF-" within first 1 KiB
pub fn detect_challenge(html: &str, status: u16, headers: &str) -> Option<ChallengeKind>; // PmcPow | CloudflareChallenge | CloudflareBlock
```

Ranking rules supported by the evidence above [high unless noted]:
1. Drop candidates whose host is known to be unreachable by plain HTTP and that have a better
   alternative: `europepmc.org/*?pdf=render`, `core.ac.uk/*`, and PMC website PDFs when a PMC S3 PDF exists.
2. Prefer `Published` > `Accepted` > `Submitted` (configurable). Within a version, prefer a direct
   `pdf_url` over `landing_url`, and hosts in this order: PMC S3 > publisher (gold/hybrid licence) >
   arXiv / bio-medRxiv early path > institutional repositories > others.
3. Down-weight OpenAlex locations whose `record_doi` differs from the requested DOI **and** whose
   source is null or unknown (the `langtaosha.org.cn` case). Keep merged-work locations that come from
   recognised sources (PMC, medRxiv, the publisher of the published version).
4. De-duplicate by normalised URL **including** the query string (unlike Zotero's tried-URL set).
   Map `http://` to `https://`, `www.ncbi.nlm.nih.gov/pmc/articles/N` to
   `pmc.ncbi.nlm.nih.gov/articles/PMCN/`, and `arxiv.org/abs/X` to `arxiv.org/pdf/X`.
5. Cap at about 6 URLs per item if handing them to `Zotero.Attachments.addFileFromURLs` (Zotero's cap).

## 6. Recommended source order and parallelism

**Phase 0 — local (no network).** Read DOI (field or `Extra`), PMID/PMCID (fields/`Extra`), URL;
derive the arXiv id from `10.48550/arXiv.*` or arxiv.org URLs; detect bio/medRxiv from the
`10.1101/` prefix plus the URL host. Rust: `normalise_identifiers()`.

**Phase 1 — metadata lookups, all in parallel per item** (`Promise.allSettled`, about 8-10 s
timeout each, no retries beyond one backoff on 429/5xx):

| # | Call | Condition | Why |
|---|---|---|---|
| a | OpenAlex `GET /works/doi:{doi}` (singleton, free) | DOI | Primary OA index (Unpaywall's data plus DataCite, merges, cached PDFs) |
| b | Zotero OA via `Zotero.Utilities.Internal.getOpenAccessPDFURLs(doi)` | DOI, and setting on | Zero-config fallback if OpenAlex is down or throttled. The same data, so drop it if (a) succeeded |
| c | Europe PMC `search?query=DOI:"…"` (or `EXT_ID:{pmid} AND SRC:MED`) | DOI or PMID | PMCID + licence + preprint record, keyless |
| d | bioRxiv `details/{server}/{doi}/na/json` | `10.1101/` DOI | Early-path PDF URL + version date |
| e | Unpaywall `v2/{doi}?email=` | user email set | Optional redundancy (same DB as OpenAlex) |
| f | Semantic Scholar batch | user key set | Optional extra signal |
| g | CORE `/v3/discover` | user key set | Optional; low precision |

Then, sequentially once the PMCID is known (from c, a's `ids.pmcid`, the item field, or NCBI idconv as a
fallback): **PMC S3** `metadata/PMC{n}.1.json`, or ListObjectsV2 when `.1` is missing.

**Phase 2 — download attempts, in rank order, stopping at the first verified PDF:**
1. PMC S3 PDF (CC-licensed PMC copies).
2. arXiv PDF (deterministic URL).
3. bio/medRxiv early-path PDF.
4. Direct `pdf_url`s from OpenAlex/Unpaywall (publisher gold/hybrid first, then repositories).
5. Landing pages: fetch the HTML, then Rust `extract_pdf_link()` (`citation_pdf_url`, then
   publisher-specific patterns), or hand the `pageURL` to Zotero's translators via `addFileFromURLs`.
6. Challenge-walled copies (PMC website, europepmc.org): only through Zotero's `BrowserRequest`
   pipeline (`addFileFromURLs` with `pageURL: https://pmc.ncbi.nlm.nih.gov/articles/PMC…/`), never by
   solving the challenge in plugin code.
7. OpenAlex content API `content.openalex.org/works/{W}.pdf?api_key=…`. Only with a user key and when
   `has_content.pdf` is true. A cheap, Cloudflare-free last OA resort ($0.01 each).
8. Otherwise go to the institutional (KCL EZproxy) phase, reusing Crossref `link[]` and
   `resource.primary.URL` (fetch Crossref in Phase 1 only if the institutional phase is enabled).

**Concurrency and politeness** (per host token buckets in the JS shell):

| Host | Limit to enforce | Basis |
|---|---|---|
| api.openalex.org | ≤10 req/s keyless (30 with key); prefer singletons (free) | docs [medium] + headers [high] |
| services.zotero.org | ≤2 req/s, 1 per item | undocumented [low] |
| www.ebi.ac.uk (Europe PMC) | ≤5 req/s | not documented [low] |
| pmc.ncbi.nlm.nih.gov (idconv) | ≤3 req/s | NCBI norm [low] |
| pmc-oa-opendata.s3.amazonaws.com | ≤5 req/s | S3, no limit stated [low] |
| export.arxiv.org | 1 req / 3 s, 1 connection | ToU [medium] |
| arxiv.org (PDF) | 1 concurrent | courtesy [low] |
| api.biorxiv.org / www.{bio,med}rxiv.org | ≤1-2 req/s | none stated [low] |
| api.unpaywall.org | ≤10 req/s, 100k/day | docs [medium] |
| api.crossref.org | 5 req/s, concurrency 1 (public); 10/3 with `mailto` | headers [high] |
| api.semanticscholar.org | 1 req/s with key; skip keyless | docs [medium] + observed 429 [high] |
| api.core.ac.uk | 10/min keyless (100/day); 25/min with personal key | docs [medium] |
| any publisher host | 1 req/s per domain, 5 consecutive failures → skip domain | mirror Zotero (`attachments.js:1513-1514`) [high] |

Across items: process up to about 4 items concurrently. Within an item, Phase 1 runs in parallel
and Phase 2 runs sequentially.

**Batching for multi-item selections** [high]: OpenAlex `filter=doi:a|b|…` (≤100, one list call,
$0.0001), Europe PMC `DOI:"a" OR DOI:"b"` (verified with 4 DOIs and `pageSize=100`; the maximum page
size is undocumented here [low]), NCBI idconv (≤200 IDs), Semantic Scholar batch (≤500). Singletons are free on OpenAlex, so batch there only for speed.

## 7. User-configurable settings

| Setting | Default | Used by | Notes |
|---|---|---|---|
| Unpaywall email | empty → Unpaywall skipped | Unpaywall | Validate the format. Unpaywall rejects `test@example.com` and non-emails. **Never ship a default.** OpenAlex covers the same data without it |
| OpenAlex API key | empty | OpenAlex (×10 budget, 30 req/s), content API PDFs | Free key from openalex.org/settings/api. Send as `Authorization: Bearer`. Store in prefs as a secret |
| Use Zotero OA mirror | on | services.zotero.org | Lets users opt out of the private API |
| Crossref mailto | empty | Crossref polite pool | Optional. Only matters if the institutional phase uses Crossref heavily |
| Semantic Scholar API key | empty → source off | S2 | `x-api-key` header |
| CORE API key | empty → source off | CORE | `Authorization: Bearer` |
| NCBI tool / email | tool fixed `zotero-fulltext`; email empty | idconv | Email is optional (warning only) |
| Accept versions | published + accepted + submitted | ranking | Some users want the published version only |
| Prefer published over preprint when OpenAlex merges | on | ranking | Handles the medRxiv → PLOS ONE case |
| Allow paid OpenAlex content downloads | on if key set | content API | Each costs $0.01 of the user's daily $1 |

## 8. Gotchas checklist (for tests)

1. Error bodies are not JSON: Unpaywall and OpenAlex 404 are HTML; Crossref 404 is plain text; the PMC
   S3 404 is XML; CORE output 404 is JSON served as `text/html`. Parse by status first.
2. "Not found" is often a 200: Zotero OA `[]`, Europe PMC `hitCount: 0`, bioRxiv
   `"no posts found"`, arXiv `totalResults 0`, idconv per-record `status: "error"`, S2 batch `null`.
3. Empty values come as `null`, `""` (S2 `openAccessPdf.url`), `"NA"` (bioRxiv `published`) or
   `"deprecated"` (Unpaywall `evidence`/location `updated`).
4. DOI case: Europe PMC and OpenAlex lowercase DOIs, so compare case-insensitively.
5. OpenAlex merges (arXiv → `10.65215/…`, medRxiv → PLOS ONE). Check `work.doi` against the request.
6. A "gold" or "bronze" label does not mean a fetchable PDF (PLOS: landing only; Nature: HTML after cookies).
7. The PMC S3 `pdf_url` is `s3://…?md5=` and is served as `binary/octet-stream`; versions are not always `.1`.
8. AAMs (`license_code: "TDM"`) have no S3 PDF.
9. Challenge detection: PMC PoW (`<title>Preparing to download ...`, `POW_COOKIE_NAME`),
   Cloudflare managed challenge (`<title>Just a moment...`, header `cf-mitigated: challenge`), Cloudflare
   block (`<title>Attention Required! | Cloudflare`, "Sorry, you have been blocked").
10. Zotero's `downloadFirstAvailableFile` strips the query string when de-duplicating and keeps only 6 URLs
    per function resolver.

## 9. Source index

Local Zotero 10.0.6 code (`C:\Users\abora\zotero-fulltext\.ref\omni-app`):
* `chrome/content/zotero/xpcom/utilities_internal.js:1427-1466`: `getOpenAccessPDFURLs`
* `resource/config.mjs:14`: `SERVICES_URL`
* `chrome/content/zotero/xpcom/attachments.js:43, 1275-1490, 1512-1600, 1872-1923, 1978-2300`: resolvers, queue, download loop
* `chrome/content/zotero/xpcom/browserRequest.js:30-186`: challenge registry and `clearChallenge`
* `chrome/content/zotero/xpcom/zotero.js:2238-2320`: `Zotero.VersionHeader` (UA, `X-Zotero-Version`)
* GitHub `zotero/zotero` commit `80658e928` (2026-07-30) "Handle download challenge on PubMed Central"

Official documentation:
* OpenAlex: https://help.openalex.org/api/authentication.md, https://help.openalex.org/access/example-costs.md,
  https://help.openalex.org/access/fulltext.md, https://help.openalex.org/access/unpaywall.md,
  https://help.openalex.org/api/deprecations.md, https://help.openalex.org/llms.txt
* Unpaywall: https://unpaywall.org/api/v2 (JS app; 100k/day via search snapshot), https://unpaywall.org/data-format
* PMC: https://pmc.ncbi.nlm.nih.gov/tools/oa-service/, https://pmc.ncbi.nlm.nih.gov/tools/pmcaws/,
  https://pmc.ncbi.nlm.nih.gov/tools/cloud/, https://pmc.ncbi.nlm.nih.gov/tools/textmining/,
  https://pmc.ncbi.nlm.nih.gov/tools/id-converter-api/, https://pmc-oa-opendata.s3.amazonaws.com/README.txt,
  https://ncbiinsights.ncbi.nlm.nih.gov/2026/02/12/pmc-article-dataset-distribution-services/
* arXiv: https://info.arxiv.org/help/api/tou.html
* bioRxiv/medRxiv: https://api.biorxiv.org/
* Semantic Scholar: https://www.semanticscholar.org/product/api, https://www.semanticscholar.org/product/api/tutorial,
  https://api.semanticscholar.org/graph/v1/swagger.json
* CORE: https://api.core.ac.uk/docs/v3 (rate-limit table, PDF download policy)
* Crossref: https://www.crossref.org/documentation/retrieve-metadata/rest-api/access-and-authentication/
* DOAJ: https://doaj.org/api/docs
* Europe PMC: REST base https://www.ebi.ac.uk/europepmc/webservices/rest. The docs site
  (europepmc.org/RestfulWebService) returned 403 Cloudflare to non-browser fetches.

Empirical: everything under `fixtures/oa/` (see `manifest.json` for URL, status and Content-Type of each body).
