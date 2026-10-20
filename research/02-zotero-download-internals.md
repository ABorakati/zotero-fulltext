# 02 — How Zotero 10 finds and downloads PDFs ("Find Full Text"), and what our plugin can reuse

Scope: Zotero **10.0.6** (BuildID 20261007153950, Gecko 140.17.0esr) as installed at `C:\Program Files\Zotero`
(`app/application.ini`: `Version=10.0.6`). Primary source is the extracted `omni.ja` under
`C:\Users\abora\zotero-fulltext\.ref\omni-app`. Cross-checked against `zotero/zotero` tag **`10.0.6`** on GitHub
(branches `10.0` and `main` also exist).

Path shorthands used below:

| Shorthand | Real path (relative to `C:\Users\abora\zotero-fulltext`) |
|---|---|
| `XPCOM/` | `.ref/omni-app/chrome/content/zotero/xpcom/` |
| `CONTENT/` | `.ref/omni-app/chrome/content/zotero/` |
| `PREFS` | `.ref/omni-app/defaults/preferences/zotero.js` (Firefox `firefox.js` defaults + Zotero app prefs concatenated) |
| `CONFIG` | `.ref/omni-app/resource/config.mjs` |

Confidence: **high** = read in local 10.0.6 source or observed empirically; **medium** = official docs / forum statements by
Zotero staff; **low** = inference.

---

## 0. TL;DR

1. **The whole "Find Full Text" pipeline is public-ish JS on `Zotero.Attachments`** and is already called by third-party
   plugins: `addAvailableFiles(items)` (queue + UI) → `addFileFromURLs(item, resolvers, opts)` (download + save) →
   `downloadFirstAvailableFile(resolvers, path, opts)` (download only) → `downloadFile(url, path, opts)`. We can feed it
   our own resolvers (`{url}`, `{pageURL}`, or async functions returning arrays of those). (high)
2. **Landing-page handling is static, not a browser**: the page is fetched with `Zotero.HTTP.request` (`responseType:
   'blob'`, `followRedirects:false`, redirects followed manually, ≤10 hops), parsed with `DOMParser`, meta-refresh
   followed, then the **highest-priority web translator** is run on the static DOM to get the PDF URL (e.g. Embedded
   Metadata → `citation_pdf_url`). No JS runs, except for a **hard-coded allow-list of bot-challenge hosts**
   (ScienceDirect, PMC, MDPI, WorldCat, a test URL) handled by the new `Zotero.BrowserRequest` (hidden browser, then a
   visible CAPTCHA window). (high)
3. **No EZproxy support in the app.** `Zotero.Proxies` still exists but only as a URL-unproxying helper for translator
   matching; client-side proxy redirection was removed in 2017 (commit `7a062a7493`), there is no UI to add proxies, and
   this user's `proxies` table is empty. Zotero staff confirm the app only gets gated PDFs via IP access (campus/VPN). (high)
4. **Cookies: Zotero 10 uses the default Firefox cookie jar for everything** (`Zotero.HTTP.request`, `HTTP.download`,
   `HiddenBrowser`, `Zotero.openInViewer`) unless a `userContextId` is passed. Zotero 8.x/9.x isolated Find Full Text
   requests in per-site `CookieSandbox`es; 10.0 removed that (commit `cc76207a23`). So an EZproxy session cookie set
   in a Zotero viewer window **is** sent by later `Zotero.HTTP.request`/`download` calls and by Find Full Text itself. (high
   for the code path; medium that `cookieBehavior=5` does not partition it — see §12)
5. **Zotero's OA lookup** is `POST https://services.zotero.org/oa/search` with `{"doi": "..."}` → JSON array of
   `{url?, pageURL?, version}` (Zotero's mirror of the Unpaywall data feed). It is explicitly a *private API*. Reuse it
   only indirectly through `getFileResolvers(item, ['oa'])`. (high)
6. **Recommended architecture**: our own pipeline (Rust/WASM decides; JS does `Zotero.HTTP.request` with the default
   jar) for OA + EZproxy, with **login-page detection** and precise per-item diagnostics, falling back to Zotero's
   `downloadFirstAvailableFile` for pages our extractor cannot handle (gets translators + BrowserRequest for free), and
   saving via `createURLAttachmentFromTemporaryStorageDirectory` (same storage shape as Find Full Text). Feature-detect
   every internal we call. (recommendation)

---

## 1. Call graph of "Find Full Text" in 10.0.6

```
Item context menu "Find Full Text"            (zotero.properties:325  pane.items.menu.findAvailableFile = Find Full Text)
  shown iff Zotero.Attachments.canFindFileForItem(item)          CONTENT/zoteroPane.js:4427
  ZoteroPane.findFilesForSelectedItems()                         CONTENT/zoteroPane.js:5400-5406
    Zotero.Attachments.addAvailableFiles(items, {})              XPCOM/attachments.js:1512
      per item: getFileResolvers(item, options.methods)          :1275
      per item: addFileFromURLs(item, resolvers, {onBeforeRequest, onAfterRequest, onRequestError})  :1872
        createTemporaryStorageDirectory()                        :2853  (<storage>/tmp-XXXXXX)
        downloadFirstAvailableFile(resolvers, tmp/file.tmp, {enforceFileType:true, shouldDisplayCaptcha:true, ...})  :1978
          direct URL  -> downloadFile() -> Zotero.HTTP.download() (fetch)          :1176, http.js:636
          page URL    -> Zotero.HTTP.request(GET, blob, followRedirects:false)     http.js:222
                       -> Zotero.Utilities.Internal.blobToHTMLDocument()           utilities_internal.js:966
                       -> Zotero.HTTP.getHTMLMetaRefreshURL()                      http.js:1045
                       -> Zotero.BrowserRequest.clearChallenge() (allow-listed)    browserRequest.js:114
                       -> Zotero.Utilities.Internal.getFileFromDocument(doc)       utilities_internal.js:1475
                       -> downloadFile(pdfURL, referrer=page)
          on InvalidPDF/403 for allow-listed host -> Zotero.BrowserRequest.downloadPDF()  browserRequest.js:567
              -> downloadPDFViaViewer() (visible CAPTCHA window)                    browserRequest.js:616
        Zotero.File.rename(tmp, getFileBaseNameFromItem(item,{attachmentTitle}) + ext)
        createURLAttachmentFromTemporaryStorageDirectory({...})  :682  (LINK_MODE_IMPORTED_URL, url+accessDate set, FT indexing queued)
```

Other callers of the same machinery (high):
- Connector save fallback: `XPCOM/translation/translate_item.js:157` and `:332-445` (`_getOpenAccessPDFURLs`,
  `saveOpenAccessAttachment`) — uses `['oa']` then `['custom']` with `automatic=true`, gated by pref
  `downloadAssociatedFiles`.
- Local connector endpoints `/connector/hasAttachmentResolvers` and `/connector/saveAttachmentFromResolver`
  (`XPCOM/server/server_connector.js:668-718`) — `getFileResolvers(item, ['oa','custom'], true)` + `addFileFromURLs`.

---

## 2. API reference (exact signatures, 10.0.6)

All in `XPCOM/attachments.js` unless noted. Constants: `FIND_AVAILABLE_FILE_TYPES = ['application/pdf',
'application/epub+zip']` (:43); link modes `IMPORTED_FILE=0, IMPORTED_URL=1, LINKED_FILE=2, LINKED_URL=3,
EMBEDDED_IMAGE=4` (:35-39). (high)

### 2.1 `canFindFileForItem(item) → boolean` (:1250)
True iff `item.isRegularItem()`, not a feed item, has **DOI field or `DOI:` in Extra or URL or PMCID**, and has **zero
file attachments** of type `application/pdf` **and** `application/epub+zip`
(`numFileAttachmentsWithContentType`). `canFindPDFForItem` is a deprecated alias (:1261). (high)

### 2.2 `getFileResolvers(item, methods = ['doi','url','oa','custom'], automatic = false) → Array<Object|Function>` (:1275)
Order of the returned list is fixed (high):

| # | when | resolver produced |
|---|---|---|
| 1 | `'doi'` and cleaned DOI | `{ pageURL: 'https://doi.org/' + doi, accessMethod: 'doi' }` |
| 2 | `'url'` and URL field | `{ pageURL: Zotero.Utilities.cleanURL(url), accessMethod: 'url' }` |
| 3 | `'oa'` and DOI | **async function** → `getOpenAccessPDFURLs(doi)` mapped to `{ url, pageURL, articleVersion: version, accessMethod: 'oa' }` |
| 4 | `'oa'` and PMCID (`/\bPMC\d+\b/i`) | `{ pageURL: 'https://pmc.ncbi.nlm.nih.gov/articles/PMCxxxx/', accessMethod: 'oa' }` (after OA since commit `7abcf23ef7`) |
| 5 | `'custom'` and DOI | one async function per entry of pref `extensions.zotero.findPDFs.resolvers` (see 2.3); with `automatic=true` only entries with `automatic: true` |

DOI comes from `item.getField('DOI') || item.getExtraField('DOI')`, then `Zotero.Utilities.cleanDOI`. Custom resolvers
are **only added when a DOI exists**. `getPDFResolvers(item, methods)` is a deprecated alias (:1496) and drops the
`automatic` argument. Zotero 10's schema has native `PMID`, `PMCID`, `citationKey` fields on journalArticle
(`.ref/omni-app/resource/schema/global/schema.json`, schema version 44). (high)

### 2.3 Custom resolver pref `extensions.zotero.findPDFs.resolvers` (default `'[]'`, PREFS:1610)
JSON string; an array, or a single object with `method` (auto-wrapped) (:1354). Validation (:1365-1396), per entry:

| key | required | rules |
|---|---|---|
| `name` | yes | shown as `accessMethod` |
| `method` | yes | `GET` or `POST` (case-insensitive) |
| `url` | yes | must contain `{doi}`; **only the first** `{doi}` is replaced (`url.replace(/\{doi}/, doi)`, not URL-encoded) |
| `mode` | yes | `html` or `json` |
| `selector` | yes | CSS selector (html) or **JSPath** (json, `resource://zotero/jspath.js`) |
| `attribute` | html only, optional | attribute to read; else `textContent` |
| `index` | html only, optional | `querySelectorAll(selector).item(index)`; note `index: 0` is falsy → behaves like `querySelector` |
| `mappings` | json only, optional | `{ url: '<prop>', pageURL: '<prop>' }` — only these two keys honoured |
| `automatic` | optional | if true, also used when saving from the Connector |

Request: `Zotero.HTTP.request(METHOD, url, { responseType: mode=='json'?'json':'document', timeout: 5000,
errorDelayMax: 0, noRetryOnThrottle: true })` (:1403-1415). HTML result → `[{accessMethod:name, url: absolutised
value, referrer: url}]`; JSON result → each match (string) → `{url}`; with mappings → `{url?, pageURL?}`; each gets
`accessMethod` and `referrer`. Invalid entries are skipped with a debug message (:1479-1483). Official doc:
https://www.zotero.org/support/kb/custom_pdf_resolvers (says plugins may distribute resolvers by writing this pref at
startup). (high / doc medium)

Example (from the doc, email placeholder only):
```json
[{"name":"Unpaywall","method":"GET","url":"https://api.unpaywall.org/v2/{doi}?email=unpaywall@example.org",
  "mode":"json","selector":".oa_locations","mappings":{"url":"url_for_pdf","pageURL":"url_for_landing_page"},
  "automatic":true}]
```
Limitation for us: custom resolvers are **DOI-only**, cannot proxy-rewrite, cannot authenticate, and the pref is
global/user-visible. Writing it from our plugin is possible but would also affect the user's Connector saves when
`automatic:true`. Not recommended as our main mechanism. (recommendation)

### 2.4 Resolver object shape (consumed by `downloadFirstAvailableFile`, docstring :1958-1977) (high)
```ts
type UrlResolver =
  | string                                   // treated as { url }
  | { url?: string;                          // direct file URL
      pageURL?: string;                      // landing page to translate
      accessMethod?: string;                 // passed to onAccessMethodStart (UI label)
      articleVersion?: 'submittedVersion'|'acceptedVersion'|'publishedVersion'; // -> attachment title
      referrer?: string;                     // sets Referer for subsequent downloads (sticky! see 4.4)
    }
  | (() => Promise<UrlResolver[]>);          // lazily run; result capped to 6 entries; spliced in place
```
Each normalised URL is tried at most once per call. `url` is tried before `pageURL` within one resolver. `http://` and
protocol-relative URLs are forced to `https://` (:2060-2064).

### 2.5 `addAvailableFiles(items, options = {}) → Promise<void>` (:1512)
- `options.methods` → `getFileResolvers`; `options.sameDomainRequestDelay` (default 1000 ms, tests only).
- Filters with `canFindFileForItem` (ineligible items silently omitted); a single module-level queue
  `_findFileQueue`; re-entrant calls join the running queue (:1600-1603).
- UI: `Zotero.ProgressQueues.create({id:'findFile', title:'pane.items.menu.findAvailableFile',
  columns:['general.item','attachment.fullText']})`; rows updated with `Zotero.ProgressQueue.ROW_SUCCEEDED/FAILED` and
  `findPDF.noFileFound` / `general.failed`; final status `find-pdf-files-added` (Fluent, `localization/en-US/zotero.ftl:1046`).
- Throttling (only here, via callbacks passed to `addFileFromURLs`):
  - `MAX_CONSECUTIVE_DOMAIN_FAILURES = 5` → `Error("Too many failed requests for <host>")` (:1513, :1688)
  - same-host delay 1000 ms, **except `doi.org` and `localhost`** (:1681)
  - delayed items are parked (`continuation`) and the queue moves on to other items (:1694-1709)
  - `onRequestError`: for `UnexpectedStatusException` increments host failures; 429/503 with `Retry-After`
    (integer seconds or HTTP-date, max 3600 s) → schedule and retry; other 429/5xx → 10 s and retry; everything
    else → give up on that URL (:1728-1776).
- Returns nothing; results only visible in the dialog. Deprecated alias `addAvailablePDFs` (:1828). (high)

### 2.6 `addAvailableFile(item, options = {}) → Promise<Zotero.Item|false>` (:1847)
`addFileFromURLs(item, getFileResolvers(item, options.methods))` — **no** throttling callbacks, **no**
`canFindFileForItem` check, no UI. Alias `addAvailablePDF` (:1856). (high)

### 2.7 `addFileFromURLs(item, urlResolvers, options = {}) → Promise<Zotero.Item|false>` (:1872)
Options forwarded: `onAccessMethodStart(accessMethod)`, `onBeforeRequest(url, noDelay)` (async; **throw to skip the
URL**), `onRequestError(e) → boolean` (true = retry). **`onAfterRequest` is NOT forwarded** (only `addAvailableFiles`
passes it, and it is dropped at :1883-1888). Always sets `enforceFileType:true, shouldDisplayCaptcha:true`.
On success: MIME from translator or sniffed, must be PDF/EPUB; title = translator title or
`_getTitleFromVersion(props.articleVersion)` → `attachment.fullText` "Full Text" / `attachment.acceptedVersion`
"Accepted Version" / `attachment.submittedVersion` "Submitted Version" (:1935-1955, `zotero.properties:653-655`);
filename = `getFileBaseNameFromItem(item, {attachmentTitle: title}) + '.' + ext` (**always renamed**, regardless of the
`autoRenameFiles` pref); creates an `IMPORTED_URL` child with `url` = final file URL and `accessDate`. Throws on
unexpected errors (temp dir removed). Alias `addPDFFromURLs` (:1929). (high)

### 2.8 `downloadFirstAvailableFile(urlResolvers, path, options) → Promise<{title?, mimeType?, url, props} | false>` (:1978)
Download-only (no item created) — usable by plugins to get Zotero's landing-page logic and then attach the file
ourselves. Options read: `onBeforeRequest`, `onAfterRequest`, `onRequestError`, `onAccessMethodStart`, plus anything
`downloadFile` reads (`enforceFileType`, `shouldDisplayCaptcha`, `referrer`, `errorDelayMax`, `noRetryOnThrottle`).
`props` is the winning resolver object (so `props.accessMethod` tells us which source won). Note `options` is
**mutated** (`options.referrer` set from resolvers). (high)

### 2.9 `downloadFile(url, path, options = {}) → Promise` (:1176)
`Zotero.HTTP.download(url, path, {headers:{Referer?}, errorDelayMax, noRetryOnThrottle})`; if `enforceFileType`,
sniffs first 1000 chars and throws `Zotero.Attachments.InvalidPDFException` unless PDF/EPUB (:1223-1240; when debug
output is on, **the whole non-PDF body is dumped to the debug log**, up to 20,000 chars). On `InvalidPDFException` or
HTTP 403 with `enforceFileType`, and only if `Zotero.BrowserRequest.getEntryForURL(url)` matches, retries via
`BrowserRequest.downloadPDF` (:1205-1215). (high)

### 2.10 Other `Zotero.Attachments` helpers relevant to saving
| function | line | notes |
|---|---|---|
| `importFromFile({file, libraryID?, parentItemID?, title?, collections?, fileBaseName?, contentType?, charset?, saveOptions?, moveFile?})` | :63 | `IMPORTED_FILE`; title auto via `setAutoAttachmentTitle()` ("PDF" for first PDF) if omitted; runs `Zotero.Fulltext.indexItems` |
| `importFromURL({libraryID, url, parentItemID?, collections?, title?, fileBaseName?, renameIfAllowedType?, contentType?, referrer?, saveOptions?})` | :528 | http(s) only; for PDF (non-native MIME) downloads with `downloadFile(..., {enforceFileType, shouldDisplayCaptcha:true})` then `createURLAttachmentFromTemporaryStorageDirectory`; HTML goes through a `HiddenBrowser` snapshot instead; does a HEAD (`getMIMETypeFromURL`) if `contentType` omitted |
| `createURLAttachmentFromTemporaryStorageDirectory({directory, libraryID?, parentItemID?, filename, url, contentType, title?, collections?, saveOptions?})` | :682 | moves the whole temp dir to `storage/<KEY>/`, `IMPORTED_URL`, sets `url` + `accessDate`, queues full-text indexing; title defaults to filename |
| `linkFromURL({url, parentItemID?, contentType?, title?, collections?, saveOptions?})` | :746 | `LINKED_URL` (no file) |
| `createTemporaryStorageDirectory() → nsIFile` | :2853 | `<storage>/tmp-<6 random chars>`; same volume as storage |
| `getFileBaseNameFromItem(item, {formatString?, attachmentTitle?}) → string` | :2380 | uses library's synced `attachmentRenameTemplate`, default `{{ firstCreator suffix=" - " }}{{ year suffix=" - " }}{{ title truncate="100" }}` (`CONTENT/renameFiles.mjs:30`); output passed through `getValidFileName` |
| `getStorageDirectory(item)` | :2817 | |

There is **no `importFromBuffer`**. `Zotero.File.putContentsAsync(path, data)` (`XPCOM/file.js:418`) accepts string,
Blob or nsIInputStream — a raw `ArrayBuffer` is documented but **not actually handled**; use
`IOUtils.write(path, new Uint8Array(buf))`. (high)

---

## 3. The landing-page decision flow (`downloadFirstAvailableFile`, :2024-2327)

For each resolver in order (functions expanded lazily, ≤6 URLs each):

```
skip url/pageURL already tried (normalizeURL: drop ALL query params except 'download', sort; :1987-1997)
onAccessMethodStart(accessMethod)        // fires for every resolver (typo at :2085 deletes 'onAccessMethod')
if url:                                  // direct file attempt, up to 3 tries
    onBeforeRequest(url) ; downloadFile(url, path, {...options, errorDelayMax: 7500, noRetryOnThrottle: true})
    success -> return {url, props}
    error   -> onRequestError(e) ? retry : fall through to pageURL
if pageURL:
    nextURL = pageURL; redirectLimit = 10; per-URL redirect tries <= 2
    loop:
        onBeforeRequest(nextURL, noDelay = host already seen in this chain)
        req = Zotero.HTTP.request('GET', nextURL, {responseType:'blob', followRedirects:false, errorDelayMax:0})
        if status in {301,302,303,307}: nextURL = resolve(Location); skip if already tried; continue
        responseURL = req.responseURL ; contentType = Content-Type header
        if text/html:
            doc = blobToHTMLDocument(blob, responseURL)        // DOMParser + wrapDocument (fake location)
            refresh = getHTMLMetaRefreshURL(doc, responseURL)  // <meta http-equiv=refresh>, delay <= 15 s, http(s) only
            if refresh and BrowserRequest.getEntryForURL(refresh) and not yet cleared:
                BrowserRequest.clearChallenge(responseURL, {entry}) ; retry responseURL     // MDPI/Akamai, PMC etc.
            elif refresh: follow it (unless already tried)
            elif responseURL == 'https://www.proquest.com/intermediateredirectforezproxy': go back to pageURL
        addTriedURL(responseURL)       // only final, non-redirecting URLs are marked (Nature IdP cookie bounce, forum 81182)
    if contentType starts with application/pdf|application/epub+zip:
        write blob, sniff, return {url: responseURL, props}   // "URL resolves directly to file"
    elif doc:
        {title, mimeType, url} = getFileFromDocument(doc)     // translators on the static DOM
    if no url: "No file found on <page>" -> next resolver
    download url with referrer = responseURL (up to 4 attempts, `tries-- >= 0`)
```

`getFileFromDocument(doc)` (`XPCOM/utilities_internal.js:1475-1502`): `new Zotero.Translate.Web()`,
`setDocument(doc)`, `getTranslators()`, **uses only `translators[0]`** (highest priority), `translate({libraryID:false,
saveAttachments:true})`, returns the first attachment whose `mimeType` is PDF/EPUB. If no translator matches it returns
`false` ("TEMP: Until there's a generic webpage translator"). Embedded Metadata (priority 320) turns
`citation_pdf_url` into `{title:"Full Text PDF", mimeType:"application/pdf"}`
(`.ref/omni-app/translators/Embedded Metadata.js:529-541`). Translator matching unproxies URLs via
`Zotero.Proxies.getPotentialProxies()` (`XPCOM/translation/translators.js:400-414`), so an HttpsHyphens EZproxy page
like `https://www-nature-com.<proxy>/...` still selects the Nature translator (TLD heuristic, `XPCOM/proxy.js:202-231`). (high)

Things it does **not** do (high, from code):
- run page JavaScript (except BrowserRequest allow-list); JS-rendered PDF buttons fail (open issue zotero#4255);
- detect login/SSO pages — an EZproxy/IdP login page is just "No file found" or `InvalidPDFException`;
- handle a missing `Content-Type` on the landing page (`contentType.startsWith` on null → caught → "Error getting file
  from …");
- surface reasons to the caller (only `false`, or exceptions; details are in the debug log).

Code-level stall risks still present in 10.0.6 (medium — read in code, not reproduced):
- landing-page request sets `errorDelayMax:0` but **not** `noRetryOnThrottle`, and `_retryOnServerError` honours
  `Retry-After` on 429/503 **before** checking `errorDelayMax` (`XPCOM/http.js:1595-1607`) — unbounded wait;
- the translator-derived PDF download reuses caller `options` without `errorDelayMax` → default 1 h of 5xx retries,
  unless the caller of `downloadFirstAvailableFile` passes `errorDelayMax`/`noRetryOnThrottle` itself
  (`addFileFromURLs` does not).

### 3.1 Debug-log strings to grep when diagnosing (all `Zotero.debug`, high)
`Looking for available files` · `Looking for open-access PDFs for <doi>` · `Found N open-access PDF URLs` ·
`Looking for files for <doi> via <name>` · `Keeping 6 URLs` · `File at <url> was already tried -- skipping` ·
`Page at <url> was already tried -- skipping` · `Looking for file on <page>` · `Redirected to <url>` ·
`Too many redirects -- stopping` · `Redirect URL has already been tried -- skipping` · `Meta refresh URL has already
been tried -- skipping` · `URL resolves directly to file` · `No file found on <url>` · `Error downloading <url>: …` ·
`Error getting file from <page>: …` · `Downloaded file was not a supported type` · `Delaying request to <host> for N ms`
· `Too many failed requests for <host>` · `Got Retry-After: …` · `BrowserRequest: …` (Clearing challenge / Escalating to
viewer / Downloading PDF via hidden browser / Found our PDF at …) · `HTTP GET <url> failed with status code N`.

---

## 4. Bot challenges: `Zotero.BrowserRequest` (new in 10.0; was `Zotero.BrowserDownload` since 7.0)

`XPCOM/browserRequest.js` (added by `f0474fa250`, 2026-04-22; predecessor `browserDownload.js` from `8b77c96e97`,
2023-12-27). (high)

- `CHALLENGE_URLS` (:30-68): entries `{match, captchaLocator, getChallengeURL?, successCookie?, detectBlock?}` for
  `zotero-static.s3…/test-pdf-redirect.html`, `://www.sciencedirect.com` (`#captcha-box`), `://search.worldcat.org`
  (Turnstile, `successCookie turnstile_passed`), `://pmc.ncbi.nlm.nih.gov` (proof-of-work, no captcha),
  `://www.mdpi.com` (Akamai bm-verify).
- `PLAIN_UA_HOSTS` (:71-76) registered with `Zotero.VersionHeader.registerPlainUAHost()` (:677-679): requests to these
  hosts drop the `Zotero/x.y` UA token because Cloudflare Turnstile rejects it.
- `getEntryForURL(url)` (:84) — matches after `Zotero.Proxies.getPotentialProxies()` unproxying, so proxied
  ScienceDirect URLs also match.
- `clearChallenge(url, {userContextId?, entry?, allowViewer=false})` (:114) — `HiddenBrowser` with plain Firefox UA,
  waits until location stable for `browserRequest.onLoadTimeout` (3000 ms) within `browserRequest.timeout` (60000 ms)
  (PREFS:1675-1676); cookies land in the shared jar. Comment (:100-102): *cookies acquired by the browser remain in the
  shared jar keyed on userContextId; a subsequent `Zotero.HTTP.request` using the same ID will see them* — this is
  Zotero's own design assumption that we also rely on for EZproxy.
- `downloadPDF(url, path, {shouldDisplayCaptcha})` (:567) — hidden browser + `Zotero.MIMETypeHandler.addHandlers(
  'application/pdf', handler, true)` to capture the PDF blob; on failure and visible captcha → `downloadPDFViaViewer`
  (:616): **opens a visible `Zotero.openInViewer` window**, user solves CAPTCHA, PDF captured, window closed.
- `clearChallengeInViewer`, `_loadAndWaitForCookieInViewer`, `_pollForCookie({successCookie:{host,name},
  userContextId, onFound, intervalMs=250})` (:299) — **reusable pattern** for "open a login window and wait until the
  EZproxy cookie appears" (copy the pattern; `_`-prefixed methods are private).

Prior art (Beaver) explicitly vetoes CAPTCHA windows for unattended runs by throwing from `onBeforeRequest` when
`Zotero.BrowserRequest.getEntryForURL(url)?.captchaLocator` is set (github.com/jlegewie/beaver-zotero
`src/utils/pdfChallengeUrls.ts`). (high, read on GitHub)

---

## 5. Zotero's OA lookup service

`XPCOM/utilities_internal.js:1427-1466` — docstring: *"Look for open-access PDFs for a given DOI using Zotero's
Unpaywall mirror. Note: This uses a private API. Please use Unpaywall directly for non-Zotero projects."* (high)

```http
POST https://services.zotero.org/oa/search          (ZOTERO_CONFIG.SERVICES_URL, CONFIG:14)
Content-Type: application/json
X-Zotero-Version: 10.0.6                            (added for *.zotero.org by Zotero.VersionHeader, XPCOM/zotero.js:2257-2260)

{"doi":"10.1234/abcd"}
```
Client options: `responseType:'json', timeout: 10000, errorDelayIntervals:[1000,2000], errorDelayMax:5000`.
Response: JSON **array** of `{ url?: string, pageURL?: string, version: 'submittedVersion'|'acceptedVersion'|
'publishedVersion' }` (docstring :1435-1436; mock in `test/tests/attachmentsTest.js:910-932` at tag 10.0.6). Zotero
keeps at most 6 entries (Unpaywall rows with huge URL lists, :2038-2044). Data source: Zotero subscribes to the
Unpaywall Data Feed and runs its own lookup "with no logging of the contents of requests"
(https://www.zotero.org/blog/improved-pdf-retrieval-with-unpaywall-integration/, medium). **Rate limits: none
documented** (not tested; I did not call the endpoint because it is private). Recommendation: do not call it from our
plugin directly; if we want Zotero's OA answer, get it through `getFileResolvers(item, ['oa'])` (the sanctioned code
path) or query Unpaywall/OpenAlex ourselves.

---

## 6. `Zotero.HTTP` (`XPCOM/http.js`)

### 6.1 `Zotero.HTTP.request(method, url, options = {}) → Promise<XMLHttpRequest>` (:179-228)
Options (doc :184-221 + code): `body`, `headers` (object or Headers; `Content-Type` defaults to
`application/x-www-form-urlencoded` when a body is present), `followRedirects` (default true), `debug`, `noCache`,
`foreground` (allow cert/auth dialogs; default background → `mozBackgroundRequest=true`), `logBodyLength` (1024),
`requestObserver(xhr)`, `cancellerReceiver(fn)`, `onAuthorizationHeader`, `responseType` (any XHR value: `''`,
`'text'`, `'json'`, `'document'`, `'blob'`, `'arraybuffer'`), `responseCharset`, `successCodes` (array, or `false` =
accept any status), `anon` (no cookies), `userContextId`, `timeout` (default 30000; 0 = none), `errorDelayIntervals`,
`errorDelayMax` (default 1 h; 0 = no 5xx retries), `noRetryOnThrottle`, `compressBody`, `notificationCallbacks`,
`cookieSandbox` (legacy: a number is treated as `userContextId`, :263-267). (high)

- **Final URL**: `xhr.responseURL` (used at `attachments.js:2207`, `http.js:517`, `:1417`). With
  `followRedirects:false` a 3xx returns a **fake object** `{status, getResponseHeader}` (Location available, no body, no
  `responseURL`) (:487-502).
- `responseType:'document'` auto-follows `<meta http-equiv=refresh>` up to 3 times (:511-540).
- Errors: `UnexpectedStatusException {status, url, xmlhttp, channel, is4xx(), is5xx()}`, `TimeoutException`,
  `BrowserOfflineException`, `CancelledException`, `SecurityException` (cert errors).
- Retry policy `_retryOnServerError` (:1581-1665): 429/5xx retried with `[2500 … 300000]` ms intervals up to
  `errorDelayMax`; `Retry-After` (integer seconds only; HTTP-date is logged as invalid) honoured for 429/503 unless
  `noRetryOnThrottle`.
- Status 0 + `_checkConnection` → `UnexpectedStatusException` with "check connection" message.

### 6.2 Cookie jar (high)
- `new XMLHttpRequest({ mozAnon })` from chrome, `mozAnon = !!(username || options.anon)` (:278-283) → by default the
  **default Firefox cookie jar** of the Zotero profile (`cookies.sqlite` in
  `%APPDATA%\Zotero\Zotero\Profiles\<profile>`).
- `channel.forceAllowThirdPartyCookie = true` on every request (:331).
- `options.userContextId` → `xhr.setOriginAttributes({userContextId})` = separate jar (:308-310).
- `Zotero.HTTP.newCookieContext() → {id, getCookies(host), dispose()}` (:159-176): ids start at 100000+random;
  `dispose()` → `Services.cookies.removeCookiesWithOriginAttributes({userContextId:id})`. Pass the same `id` to
  `HiddenBrowser({userContextId})` and `Zotero.openInViewer(url, {userContextId})` to share that jar
  (`CONTENT/HiddenBrowser.mjs:59-90`, `XPCOM/zotero.js:1316-1321`, `CONTENT/standalone/basicViewer.js:39-44`).
- History: 8.x/9.x attached a per-site (eTLD+1, 24 h) `Zotero.CookieSandbox` to Find Full Text requests
  (`91732419b6`, 2025-07-31). The old sandbox **cleared global-jar cookies** on every tracked request and swallowed
  `Set-Cookie` (`cookieSandbox.js` at tag 9.0.0, `observe()`), so login cookies from a viewer window were *not* used.
  10.0 removed `CookieSandbox` entirely (`cc76207a23`, 2026-04-17; "Cookie management" section of
  https://www.zotero.org/support/dev/zotero_10_for_developers). Our plugin should target ≥10.0 (or not rely on Find
  Full Text sharing cookies on 8/9).

### 6.3 `Zotero.HTTP.download(uri, path, options) → Promise<Response>` (:636-809)
Rewritten in 10.0 to stream with `fetch()` (dev page). Options: `headers`, `noCache`, `successCodes`, `onProgress(total,
contentLength)`, `cancellerReceiver`, `timeout` (30 s connect + inactivity), `errorDelayIntervals`, `errorDelayMax`,
`noRetryOnThrottle`, `displayURL`. `credentials:'include'` (default jar) unless URL embeds credentials (:719); a
`Referer` header is converted to `fetchOptions.referrer` with `referrerPolicy:'unsafe-url'` (:710-716). Final URL:
standard `Response.url`. Writes chunk-by-chunk with `IOUtils.write(..., {mode:'append'})`. (high)

### 6.4 User-Agent and proxies
- `Zotero.VersionHeader` (`XPCOM/zotero.js:2238-2336`) rewrites the UA of **every** non-zotero.org request to insert
  `Firefox/140.0` before `Zotero/10.0.6`; hosts in `_plainUAHosts` get the `Zotero/…` token stripped instead.
  `registerPlainUAHost(host)` is callable by plugins but there is no unregister (global until restart). (high)
- Network proxies (OS/PAC) are Gecko's; `Zotero.HTTP.triggerProxyAuth()` (:1212) makes a foreground HEAD at startup to
  trigger proxy auth (prefs `triggerProxyAuthentication`, `proxyAuthenticationURLs`, PREFS:1604-1606). This is
  **HTTP proxy auth**, unrelated to EZproxy. (high)
- Other helpers: `getHTMLMetaRefreshURL(doc, url)` (:1045), `wrapDocument(doc, url)` (:1720),
  `processDocuments(urls, processor, {headers})` (:1395), `CookieBlocker` / `RequestModifier` observers (:1080-1200).

---

## 7. Saving files ourselves (recommended snippets)

**A. Our own bytes → same storage shape as Find Full Text (IMPORTED_URL with url + accessDate):**
```js
async function attachPdfBytes(parent, bytes /* Uint8Array */, finalURL, title) {
  const tmpDir = (await Zotero.Attachments.createTemporaryStorageDirectory()).path;
  try {
    const base = Zotero.Attachments.getFileBaseNameFromItem(parent, { attachmentTitle: title });
    const filename = `${base}.pdf`;
    await IOUtils.write(PathUtils.join(tmpDir, filename), bytes);
    return await Zotero.Attachments.createURLAttachmentFromTemporaryStorageDirectory({
      directory: tmpDir, libraryID: parent.libraryID, parentItemID: parent.id,
      filename, title, url: finalURL, contentType: 'application/pdf',
    });
  } catch (e) {
    await IOUtils.remove(tmpDir, { recursive: true, ignoreAbsent: true });
    throw e;
  }
}
// title: Zotero.getString('attachment.fullText')  -> "Full Text" (what Find Full Text uses),
//        or 'attachment.acceptedVersion' / 'attachment.submittedVersion' for OA versions.
```
**B. Simpler, IMPORTED_FILE** (pattern used by github.com/Littview/zotero-openalex-pdf `src/finder.js`
`_attachHostedPDF`): write to a temp storage dir, `Zotero.Attachments.importFromFile({file, parentItemID, title,
contentType:'application/pdf', fileBaseName, moveFile:true})`, then `setField('url', …)`, `setField('accessDate',
'CURRENT_TIMESTAMP')`, `saveTx()`, and remove the temp dir.

**C. Let Zotero download a known PDF URL**: `Zotero.Attachments.importFromURL({libraryID, parentItemID, url,
contentType:'application/pdf', title, referrer, renameIfAllowedType:true})` — default jar, type enforcement and
BrowserRequest fallback included; no landing-page logic.

Privacy note: whichever path, the attachment's `url` field becomes the **final file URL**, i.e. an EZproxy-proxied host
and possibly signed query tokens (e.g. Silverchair `watermark…?token=`). Consider storing the unproxied/DOI URL instead
(our own path A lets us choose). (recommendation)

Undo: Zotero 10 only records saves that pass `undoAction`; attachment creation is not undoable anyway
(dev page "Undo/redo"). (medium)

---

## 8. "Already has a PDF?" / dedupe

| API | location | semantics |
|---|---|---|
| `item.numFileAttachmentsWithContentType(ct)` / `getFileAttachmentsWithContentType(ct)` | `XPCOM/data/item.js:2850-2858` | non-trashed child file attachments with exactly that MIME; linked-URL "PDF" links do not count |
| `item.numPDFAttachments()` | item.js:2862 | shorthand |
| `item.getBestAttachments()` | item.js:4317 | SQL: excludes `LINKED_URL` and trashed; order `contentType='application/pdf' DESC, url==parent.url DESC, dateAdded ASC` |
| `item.getBestAttachment()` | item.js:4297 | first of the above or `false`; regular items only (throws otherwise) |
| `item.getBestAttachmentState()` | item.js:4347 | `{type:'none'|'pdf'|'snapshot'|'epub'|'image'|'video'|'other', exists, key}`; cached; `exists:false` = record present but file not on disk (e.g. synced, not downloaded) — **do not re-fetch** in that case |
| `Zotero.Attachments.canFindFileForItem(item)` | attachments.js:1250 | Zotero's own eligibility rule (no PDF and no EPUB) |

Recommendation: eligible = regular item, editable library with `filesEditable`, and
`numFileAttachmentsWithContentType('application/pdf') == 0`; optionally ignore EPUB-only items (Zotero treats an EPUB
as "has full text"). (high for APIs)

---

## 9. `Zotero.Proxies` in Zotero 10

Status (high):
- Still shipped: `XPCOM/proxy.js` (553 lines), loaded for all modes (`CONTENT/zotero.mjs:57`), `await
  Zotero.Proxies.init()` at startup (`XPCOM/zotero.js:741`). Loads proxies from DB tables `proxies`/`proxyHosts`.
- **Client-side redirection/learning was removed on 2017-09-25** (`7a062a7493` "Remove connector proxy code: Observing
  requests, auto-redirecting and learning proxies no longer relevant on this codebase"), i.e. in the Zotero 5.0
  standalone era. The leftover `CONTENT/proxy.js`/`proxy.xul` dialog is dead code. No preferences UI adds proxies.
- This user's DB has **0 rows** in `proxies` and `proxyHosts` (read-only query on a copy of `~/Zotero/zotero.sqlite`,
  2026-10-20), so `Zotero.Proxies.proxies` is `[]`.
- Proxy configuration and redirection live in the **Zotero Connector** (browser), incl. HttpsHyphens and OpenAthens
  (`toProxyScheme: 'https://go.openathens.net/redirector/<org>?url=%u'`); see zotero/zotero-connectors
  `src/common/proxy.js` (EZproxy detector, `toProperScheme`/`toProxyScheme` model, host blacklist, redirect-loop
  prevention). Connector-side proxies are **not** visible to the client (the client only *serves*
  `/connector/proxies` from its own legacy DB, `server_connector.js:1061-1075`).

API that remains usable by a plugin (high):
```js
Zotero.Proxies.proxyToProper(url, onlyReturnIfProxied)   // known DB proxies only
Zotero.Proxies.properToProxy(url, onlyReturnIfProxied)   // known DB proxies only
Zotero.Proxies.getPotentialProxies(url) → { [properURL]: null | {scheme:'https://%h.<proxyHost>/%p'} }
                                         // TLD heuristic; handles "0-" prefix and https hyphens
new Zotero.Proxy({scheme, multiHost?, autoAssociate?})   // in-memory; .toProper(url), .toProxy(url), .regexp
```
Caveats (high, code-verified; impact on real EZproxy hosts not tested):
- `Zotero.Proxy.prototype.toProxy` (client) for https does `uri.hostPort.replace(/-/g, '.')` (`proxy.js:513`) — it
  does **not** convert dots to hyphens, so it cannot build HttpsHyphens URLs. The translate-module and Connector
  versions do the right thing (`hostname.replace(/\./g, '-')`; `xpcom/translate/src/proxy.js`, connector
  `proxy.js:779`). Translators matched on a proxied page receive a client `Zotero.Proxy`
  (`XPCOM/translate/src/translator.js:91-92`) and call `toProxy` on absolute unproxied URLs
  (`translate.js:1441-1447`), so **absolute** PDF URLs (e.g. an unrewritten `citation_pdf_url`) could be proxified
  into the wrong `www.nature.com.<proxy>` form. Relative URLs are unaffected.
- `toProper` replaces *every* `-` with `.` for https (comment "Potentially troublesome with domains that contain
  dashes", `proxy.js:476-480`).
- Do not write to the `proxies` tables from our plugin; implement EZproxy rewriting in Rust (the Connector's
  `Zotero.Proxy` class is the best reference design).

---

## 10. Relevant prefs (all with `extensions.zotero.` prefix unless absolute)

| pref | default | source | relevance |
|---|---|---|---|
| `findPDFs.resolvers` | `'[]'` | PREFS:1610 | custom resolvers (§2.3); user has none set |
| `downloadAssociatedFiles` | `true` | PREFS:1609 | Connector-save OA fallback |
| `browserRequest.onLoadTimeout` / `browserRequest.timeout` | `3000` / `60000` ms | PREFS:1675-1676 | challenge/viewer waits |
| `autoRenameFiles`, `.linked`, `.fileTypes` | `true`, `false`, `application/pdf,application/epub+zip` | PREFS:1618-1620 | `addFileFromURLs` renames anyway |
| `proxies.transparent`, `.autoRecognize`, `.disableByDomain`, `.disableByDomainString`, `.showRedirectNotification` | `true`, `true`, `false`, `.edu`, `true` | PREFS:1775-1779 | legacy; only `transparent` still gates `getPotentialProxies` |
| `triggerProxyAuthentication`, `proxyAuthenticationURLs` | `true`, list | PREFS:1604-1606 | HTTP proxy auth only |
| `network.cookie.cookieBehavior` | `5` (reject trackers + partition foreign / dFPI) | PREFS:943 (from firefox.js) | see §12 |
| `network.cookie.cookieBehavior.pbmode` | `5` | PREFS:944 | private browsing only |
| `privacy.clearOnShutdown.cookies` / `_v2.cookiesAndStorage` | `true` | PREFS:362, 370 | only applied if `privacy.sanitize.sanitizeOnShutdown` is true (static pref, default false in Firefox — low) |
| `dom.disable_open_during_load` | `true` | PREFS (Zotero section) | content `window.open` during load blocked — SSO popups may fail |
| `browser.chromeURL` | `chrome://zotero/content/standalone/basicViewer.xhtml` | PREFS (Zotero section) | new windows from content open as basic viewers |
| `privacy.purge_trackers.enabled` | `true` | `.ref/omni-gre/greprefs.js:383` | purges cookies of *classified trackers* only; EZproxy/IdP unlikely affected (low) |

User profile `prefs.js` (read-only check): no overrides of cookie behaviour, findPDFs or proxies. (high)

---

## 11. Login window for EZproxy inside Zotero

- `Zotero.openInViewer(uri, {onLoad?, allowJavaScript?, userContextId?, customUserAgent?}) → Window`
  (`XPCOM/zotero.js:1309-1364`): opens `basicViewer.xhtml` (no URL bar) with a remote content `<browser>`; reuses an
  existing viewer with the same URI and `userContextId`; JS enabled by default; loads with the system principal as
  triggering principal (`basicViewer.js` `loadURI`). Without `userContextId` it uses the **default jar**, the same as
  `Zotero.HTTP.request`. (high)
- Detect completion by polling `Services.cookies.getCookiesFromHost(host, {})` (or `{userContextId}`) like
  `BrowserRequest._pollForCookie`, and/or watching `browser.currentURI` leave the proxy login host; close the window
  ourselves; handle the user closing it via a `Services.wm` listener (`_makeViewerCloseListener`, browserRequest.js:271).
- `HiddenBrowser` (`CONTENT/HiddenBrowser.mjs`) options: `allowJavaScript`, `docShell`, `blockRemoteResources`,
  `useHiddenFrame`, `userContextId`, `customUserAgent`; methods `load(url, {requireSuccessfulStatus})` (5 s load-start
  timeout), `waitForDocument()`, `getPageData(['characterSet','title','bodyText','documentHTML','cookie',
  'channelInfo'])`, `getDocument()`, `snapshot()`, `destroy()`. Useful for JS-rendered landing pages behind EZproxy
  (`await browser._createdPromise` before using browser properties). (high)

---

## 12. Cookie / privacy risks for EZproxy sessions

1. **Third-party partitioning (`cookieBehavior=5`)**: cookies set while the EZproxy/IdP page is the *top-level*
   document in a viewer window are first-party and stored unpartitioned. Requests from chrome (`Zotero.HTTP.request`
   from the system principal, `forceAllowThirdPartyCookie=true`) read the unpartitioned jar. Zotero itself depends on
   this (BrowserRequest hidden browser banks cookies, then plain `Zotero.HTTP.request` reuses them,
   attachments.js:2219-2234). (medium; verify empirically with a public cookie-setting page before relying on it)
2. **IdP iframes / popups**: silent-SSO iframes inside the login page are third-party and partitioned; `window.open`
   during load is blocked. KCL's SSO (Microsoft Entra ID likely) normally uses top-level redirects. (low)
3. **Session cookies**: EZproxy's session cookie is usually non-persistent; expect re-login after every Zotero restart
   (Zotero does not restore browsing sessions). (low)
4. **Bounce-tracking protection** (`privacy.bounceTrackingProtection.mode`, static pref, not visible in extracted
   prefs): SSO redirect chains are classic "bounces"; in Firefox standard mode this runs in dry-run, but read the pref
   at runtime and warn if it is in enforcing mode. (low)
5. **UA**: proxied publisher hosts get `… Firefox/140.0 Zotero/10.0.6`; Cloudflare-fronted hosts may challenge it
   (Zotero's own comment, browserRequest.js:125-127). Using `customUserAgent` on our viewer/HiddenBrowser is local and
   safe; `registerPlainUAHost` is global. (high for code, medium for impact)

---

## 13. Prior art and observed failure modes

Zotero staff statements (medium):
- The app "doesn't currently have any way of accessing gated PDFs except via IP-based (so on-campus or VPN)
  authorization"; adding web-proxy support is blocked because Zotero cannot know which domains an EZproxy instance
  supports (no EZproxy endpoint for the config; OCLC did not reply). — dstillman, 2023-04-05,
  https://forums.zotero.org/discussion/103321 (comment 432172). Same answer for OpenAthens/LibKey in 2025
  (https://forums.zotero.org/discussion/122975).
- Blog (2018): Find Available PDF tries the DOI/URL page first, then OA; "If you use a web-based proxy, only open-access
  PDFs will be automatically retrieved" (https://www.zotero.org/blog/improved-pdf-retrieval-with-unpaywall-integration/).

Failure modes seen in forums / issues (medium unless noted):
| failure | evidence | implication for us |
|---|---|---|
| IdP cookie bounce (Nature `idp.nature.com` redirects back to the same URL) was skipped as "already tried" | forum 81182; fix = only mark final URLs tried (code comment attachments.js:2261-2271, high) | do not dedupe redirect hops; let XHR follow redirects natively |
| `citation_pdf_url` points to an HTML page / needs a JS "PDF" button (Silverchair `watermark.silverchair.com?token=…`) | forum 81182 | need HiddenBrowser fallback + login/HTML detection |
| Cloudflare "verify human" loop on ScienceDirect | forum 125581; fixes `d1ea8a397b`, `fc37e524fe` (high) | allow-listed hosts only; expect CAPTCHA windows |
| 403 + "Too many redirects" + "Too many failed requests" on ScienceDirect/OUP even on campus; SAGE returns HTML `action/cookieAbsent`; Wiley/PNAS translator POST status 0; MIT Press relative-URL TypeError; PMC 203; Anubis challenge pages | forum 133907 (Zotero 10.0.3, Oct 2026) | log precise reasons; detect cookie-wall/challenge HTML in Rust; per-host backoff |
| Queue stalled for up to an hour on 5xx/Retry-After | forum 133703/133642; fixes `7b26a1a3fc`, `a72ffd6549` in 10.0.6 (high) | always pass `errorDelayMax`/`noRetryOnThrottle`; bounded timeouts |
| ProQuest JS redirect after EZproxy auth | `fd6c52ede4`, issue zotero#5442 (Set-Cookie before redirect ignored) (high) | follow redirects in-network so cookies persist |
| Atypon `?download=true` dropped by URL normalisation | issue zotero#4396 (fixed) | keep meaningful query params in our dedupe |
| Infinite 302 loops | issue zotero#2883 (fixed: limits) | cap redirects |
| JS-dependent translators (ACM) fail with XHR | issue zotero#4255 (open) | HiddenBrowser path for such hosts |
| MDPI Akamai bot interstitial | forum 132837; fixed 10.0.6 `4d4c2c1412` (high) | rely on `downloadFirstAvailableFile` fallback for these |
| UK institutions moving EZproxy → OpenAthens | library guides (e.g. uow.libguides.com/open-athens/updating-referencing-software) | support an OpenAthens redirector scheme (`go.openathens.net/redirector/<org>?url=%u`) as well |

Plugins studied (shadow-library plugins excluded by rule — e.g. `urschrei/pdferret` defaults to Sci-Hub/Anna's Archive
and was not studied):
- **Beaver** (github.com/jlegewie/beaver-zotero, AGPL-3.0; installed in the user's profile): prepends backend-ranked
  `{url, pageURL, accessMethod:'openalex', articleVersion}` candidates to `Zotero.Attachments.getFileResolvers(item)`
  and calls `addFileFromURLs`; learns the winning source via `onAccessMethodStart`; skips CAPTCHA hosts via
  `onBeforeRequest` throw (`src/utils/pdfResolvers.ts`, `src/utils/pdfChallengeUrls.ts`). Notes that Zotero's 6-URL
  cap applies only to function resolvers. (high)
- **zotero-openalex-pdf** (github.com/Littview/zotero-openalex-pdf, MIT, Zotero 7–10): matches works by DOI → PMID →
  exact title; builds resolvers best-first (direct PDFs before landing pages, published before preprints, OA before
  paywalled; "paywalled landing pages kept at the end because Zotero uses the browser's cookies"); uses
  `addFileFromURLs` for links and `importFromFile` for its own downloads; configurable `requestDelay`. (high)
- **zotero-reading-bridge** (github.com/volatile-static/zotero-reading-bridge,
  `packages/zotero/src/zotero/fulltext-download.ts`): calls `downloadFirstAvailableFile` directly to a temp path
  ("never call addFileFromURLs"), then re-sniffs `%PDF` itself because EPUB is also accepted, and checks non-zero size.
  (high)

---

## 14. Answer to "can we just call `addFileFromURLs(item, ourResolvers)`?"

**Yes, it works and is what other plugins do** — Zotero then handles redirects, meta refresh, translators on the
landing page, the bot-challenge allow-list, type enforcement, renaming and saving, with the default cookie jar (so a
prior EZproxy login in a Zotero viewer window is honoured). **But it is not enough on its own** for KCL proxy access:

| gap | why it matters | our mitigation |
|---|---|---|
| no proxy rewriting | must pass already-proxied `pageURL`/`url` (e.g. `https://<proxy>/login?url=https://doi.org/…` or HttpsHyphens host form) | Rust rewrites; JS passes proxied URLs |
| no login detection | expired session = silent "No file found" | pre-flight one `Zotero.HTTP.request` to a proxied URL; Rust classifies login page; open viewer; retry |
| opaque result (`Item|false`) | user sees nothing actionable | run our own pipeline first; use Zotero's as fallback; capture `onAccessMethodStart`/`onRequestError` |
| static DOM only | JS-rendered PDF links fail | HiddenBrowser fallback for listed hosts |
| `onAfterRequest` not forwarded; stall paths (§3) | queue can hang | call `downloadFirstAvailableFile` directly with `errorDelayMax:0, noRetryOnThrottle:true` and our own throttle callbacks |
| EPUB accepted; `%PDF-` must be at byte 0 | not our target; some PDFs have leading bytes | re-sniff in Rust (search first 1024 bytes) |
| always renames; stores proxied final URL in `url` | privacy of proxy host / tokens | save ourselves via `createURLAttachmentFromTemporaryStorageDirectory` with a clean URL |

Suggested flow per item (JS shell, Rust decides):
1. Eligibility (§8). 2. Build candidates in Rust: OA sources (direct PDFs first), then proxied publisher landing page.
3. For each candidate: `Zotero.HTTP.request('GET', url, {responseType:'arraybuffer', successCodes:false,
timeout:20000, errorDelayMax:0, noRetryOnThrottle:true})` → pass `{status, responseURL, contentType, bytes}` to Rust
→ `Pdf | LoginPage | Challenge | Landing{pdfLinks[]} | Error`. 4. `LoginPage` → pause queue, `Zotero.openInViewer(
loginURL)` and poll cookies; resume. 5. `Landing` with no extractable link → hand `{pageURL}` to
`Zotero.Attachments.downloadFirstAvailableFile([...], tmp, {enforceFileType:true, shouldDisplayCaptcha:<user
setting>, errorDelayMax:0, noRetryOnThrottle:true, onBeforeRequest, onRequestError})`. 6. Save via §7A. Port the
proven heuristics to Rust: URL normalisation for "tried" set, meta-refresh parsing (≤15 s), `Retry-After` parsing
(seconds or HTTP-date, cap 3600 s), 1 s same-host spacing, 5-consecutive-failure host cut-off, 10 s back-off on
429/5xx, 6-URL cap per source, https upgrade.

Licensing note: Zotero is AGPL-3.0. Re-implementing behaviour described here is fine; copying code verbatim into our
Rust/JS makes the plugin AGPL-bound. (medium)

---

## 15. Open questions

1. Does a cookie set by a top-level EZproxy page in `Zotero.openInViewer` reach `Zotero.HTTP.request` /
   `HTTP.download` under `cookieBehavior=5` in practice? (Test with a harmless public cookie-setting URL in a
   Zotero dev profile, not the user's profile.)
2. Does KCL's EZproxy rewrite `citation_pdf_url` meta tags (absolute URLs) into proxied form? If not, translator output
   on proxied pages may be unproxied or mis-proxied (§9 caveat).
3. KCL proxy flavour: EZproxy (HttpsHyphens? cookie name?) vs OpenAthens redirector — affects rewriting rules and
   login detection.
4. Rate limits of `services.zotero.org/oa/search` (undocumented; private).
5. `privacy.bounceTrackingProtection.mode` value in Zotero 10.0.6 at runtime (static pref, not in extracted defaults).
6. Whether `Zotero.Attachments.createURLAttachmentFromTemporaryStorageDirectory` and
   `downloadFirstAvailableFile` stay stable in Zotero 11 (reading-bridge says it checked "Zotero 11 … at db41b99f");
   wrap in feature detection.
