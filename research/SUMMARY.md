# SUMMARY: zotero-fulltext research synthesis (critic pass, 2026-10-20)

Inputs: briefs `01`..`07` in `research/`, `03-probe-result.json`, `fixtures/html` (31 pages + `expected.json`), `fixtures/oa` (81 files + `manifest.json`).
The critic re-checked key claims in `.ref/omni-app` (Zotero 10.0.6) and ran 3 unauthenticated GETs to Europe PMC (marked "critic").
Confidence: **H** = verified in source or empirically, **M** = official docs, **L** = inferred. `[0N]` = brief file `research/0N-*.md`.

## 0. Verdict
1. **Feasible.** A thin JS shell plus a pure Rust/WASM core runs in every Zotero 10 JS context. A cookie set in a Zotero viewer window is reused by `Zotero.HTTP.request`, `downloadFile` and `addFileFromURLs` (local simulation) [03] H.
2. **Premise correction.** KCL's EZproxy proxies only ~19 hosts. The medical publishers (Elsevier, Wiley, OUP, LWW, NEJM, JAMA, Springer/Nature, T&F, SAGE, Karger, Thieme, Cambridge) are **not** proxied. They are reached through SAML/OpenAthens or campus IP [04 §2.4] H. So the institutional route is built around **browser sessions on publisher domains**, entered through the OpenAthens redirector. EZproxy is secondary.
3. **The main practical blocker is bot walls, not URL patterns.** About 12 of 20 target publishers serve a Cloudflare, Akamai or proof-of-work page to non-browser clients [06 §5.1] H (curl). A browser-capture tier is therefore mandatory.
4. **One BLOCKING unknown.** Can a KCL user finish Entra/OpenAthens SSO inside a Zotero Gecko window, and do the resulting session cookies authorise later downloads? Only the user can test this, because of the no-login rule (§h B1).

## Contradictions found and how they resolve
| # | Conflict | Resolution |
|---|---|---|
| C1 | Europe PMC PDF. [05 §2.4]: `europepmc.org/articles/PMCn?pdf=render` returns 403 Cloudflare. [06 §3.1]: it 302s to `/api/getPdf` and returns %PDF. | Critic re-probe 2026-10-20 (Firefox UA): `?pdf=render` gave **403 `cf-mitigated: challenge`**. `https://europepmc.org/api/getPdf?pmcid=PMC2707599` gave **200 `application/pdf`**. The AAM `PMC4221854` gave **500 JSON**. **Call `/api/getPdf` directly.** It is undocumented, so treat it as flaky. H (one probe each) |
| C2 | HTML parser. [06 §8] recommends `tl` or `lol_html`. [07 §2] measured `tl` as wrong (drops unquoted attributes, tokenises inside script, misses `<link>`) and `lol_html` as not entity-decoding (+388 KB). | Take [07] (empirical): **html5gum 0.8.4**. H |
| C3 | WASM loading. [01 §2.4] recommends wasm-bindgen `--target web` + `initSync({module})` (Beaver). [03 §6] found that `initSync` fails cross-compartment and that `instantiateStreaming` over chrome:// fails. [01 §2.3] says loading from rootURI jar: breaks on the next ESR. | Use a **hand ABI** with zero imports [07 §3]. The glue is a classic IIFE subscript loaded from `chrome://`. Get the wasm with `fetch(chrome://…wasm)` → `arrayBuffer()` → `WebAssembly.compile`. That path is MIME-agnostic and works in every context [03 §5]. H |
| C4 | Who drives downloads. [02 §14]: our own pipeline, with Zotero `downloadFirstAvailableFile` as fallback and our own save. [03 §11]/[05 §6]: hand everything to `addFileFromURLs`. [06 §0]: run Zotero translators first. | **Own orchestrator.** It is needed for login detection, licence limits, clean URLs and diagnostics. Run Zotero's translators on the *same* fetched page (`blobToHTMLDocument` + `getFileFromDocument`, `utilities_internal.js:966,1475`) next to the Rust extractor. Keep `downloadFirstAvailableFile` as a last resort only. Reasons verified by the critic: the landing-page request lacks `noRetryOnThrottle` (`attachments.js:2157-2159`), and `Retry-After` is honoured with no cap, before `errorDelayMax` (`http.js:1595-1607`). `addFileFromURLs` also stores the final proxied or signed URL and always renames [02 §2.7]. |
| C5 | Login window. [01 §7] wants a custom XHTML window. [02 §11]/[03 §11] use `Zotero.openInViewer`. | Critic verified that in basicViewer, a left-click whose target is `<a>`/`<area>` opens the **system browser** (`ExternalLinkHandlerChild.mjs:5-24`, group `basicViewer` at `ActorManager.mjs:61`). Default: **custom window**. Spike B1 tests both. |
| C6 | Login success signal. [02] polls for the EZproxy cookie. [04 §3.3] says cookie names are unverified and not to rely on them. | Use **URL state + Rust `classify()` + a cheap no-follow probe**. Cookies are a hint only. |
| C7 | PDF sniff. [02]/[05] look within the first 1 KiB. [06] wants offset 0, as Zotero does. | Zotero requires offset 0 (`mime.js:33-34`, critic H). Our own save path accepts %PDF within 1 KiB. Anything handed back to Zotero must satisfy offset 0. |
| C8 | Primary OA index. [02] uses Zotero OA via `getFileResolvers(['oa'])`. [05] uses OpenAlex. | **OpenAlex singleton is primary.** Zotero OA is an optional toggle, called through `Zotero.Utilities.Internal.getOpenAccessPDFURLs`, never a hand-rolled POST. |
| C9 | Beaver's `initSync({module})` works [01] / the same call fails from the sandbox [03]. | Moot after C3. |

## (a) Zotero 10 plugin facts we rely on
- **Manifest.** Use `manifest_version: 2` and `applications.zotero.{id, update_url, strict_max_version}`. All three are mandatory (`omni-gre/modules/Extension.sys.mjs:1874-1889`). Release builds enforce `strict_max_version`: zotron (`9.*`) is appDisabled in 10.0.6 [01 §1] H. Plan: id `zotero-fulltext@local` (not an email), placeholder `update_url`, min `10.0`, max `10.*`.
- **Bootstrap.**
  - Hooks are called as `func.call(sandbox, {id, version, rootURI}, reason)`, and returned promises are awaited (`xpcom/plugins.js:224-292`).
  - The sandbox is **reused on disable→enable** (`plugins.js:133-136`), so `startup()`/`shutdown()` must be idempotent. Skip teardown on `APP_SHUTDOWN` [01 §2] H.
  - `onMainWindowLoad` **never fires for the initial window**; this was seen in 6 of 6 runs. Initialise windows from `startup()` via `Zotero.getMainWindows()` with a WeakSet guard, and do not block on `uiReadyPromise` [03 §9] H.
- **Sandbox globals.**
  - Present: `fetch`, `XMLHttpRequest`, `DOMParser`, `TextEncoder`/`TextDecoder`, `URL`, `crypto`, `IOUtils`, `PathUtils`, `Services`, `Cc`/`Ci`/`Cu`, `ChromeWorker`, `Zotero`.
  - **Missing:** `console`, `AbortController`, `structuredClone` and streams. Use `Zotero.debug`, and use `cancellerReceiver` to abort requests [01 §2.2, 03 §7] H.
- **Code loading.** Register a chrome package with `aomStartup.registerChrome`, which gives `chrome://zotero-fulltext/content/`. Load scripts with `loadSubScriptWithOptions(chromeURL, {target, ignoreCache:true})`. Never load from rootURI jar:/file:, because Fx153 / Mozilla bug 1974213 breaks that [01 §2.3] H.
- **Menu.** Call `Zotero.MenuManager.registerMenu({menuID, pluginID, target:'main/library/item', menus:[{menuType:'menuitem', l10nID, onShowing(e,ctx), onCommand(e,ctx)}]})`; it returns a key.
  - `ctx.items` holds the selection; `ctx.setVisible()`/`ctx.setEnabled()` control the item.
  - Only synchronous throws are caught. Plugin menus are hidden when annotations are selected, and registrations are auto-removed on shutdown (`pluginAPI/menuManager.js:30-254`, `zoteroPane.js:4679-4705`) [01 §3; 03 §8 live] H.
  - Use a label distinct from the built-in "Find Full Text" [02].
- **l10n, prefs and progress.**
  - Ship one `locale/en-US/zotero-fulltext.ftl`: unique name, no subfolder, `.label` attributes. Call `insertFTLIfNeeded` for each window (`plugins.js:569-750`).
  - Defaults go in `prefs.js`. Read and write with `Zotero.Prefs.get/set('extensions.zotero-fulltext.x', true)`. The prefs pane uses `Zotero.PreferencePanes.register({pluginID, src, scripts})` with `preference="full.key"`.
  - For progress use `Zotero.ProgressWindow` + `ItemProgress(itemType, text)` or `Zotero.ProgressQueues.create`. There is no toast API [01 §4-6] H.
- **HTTP.** Call `Zotero.HTTP.request(method, url, {responseType:'arraybuffer', successCodes:false, timeout:20000, errorDelayMax:0, noRetryOnThrottle:true, cancellerReceiver})`. It resolves to the XHR; the final URL is `xhr.responseURL` (`xpcom/http.js:179-582`) [02 §6] H.
  - `successCodes:false` means no `UnexpectedStatusException`, so the uncapped Retry-After loop (`http.js:1595-1607`) never runs (critic H).
- **Cookie jar.** All of these share the **default jar** (userContextId 0): viewer, HiddenBrowser, `HTTP.request`, `HTTP.download` and `fetch(...,{credentials:'include'})`.
  - A top-level HttpOnly cookie was stored unpartitioned under `cookieBehavior=5`.
  - Plain `fetch` and sandbox XHR send **no** cookies [03 §10] H. This was tested locally on 127.0.0.1 only; a cross-site SAML chain is untested.
- **User-Agent.**
  - Every non-zotero.org request is sent as `…Firefox/140.0 Zotero/10.0.6`.
  - `Zotero.VersionHeader.registerPlainUAHost(host)` strips `Zotero/` for that host, globally and with no unregister. Zotero's own comment says later requests must use the UA that passed Turnstile (`xpcom/zotero.js:2252-2292`, critic H).
  - `clearChallenge` uses a plain-UA hidden browser (`browserRequest.js:127`).
- **Browser primitives.**
  - `Zotero.openInViewer(uri, {userContextId, customUserAgent})` returns the window (`zotero.js:1309-1364`).
  - `HiddenBrowser({userContextId, customUserAgent, allowJavaScript})` offers `.load()`, `.getPageData(['documentHTML','channelInfo'])` and `.destroy()` [01 §10, 02 §11] H.
  - PDF capture uses `Zotero.MIMETypeHandler.addHandlers('application/pdf', handler, true)`, scoped by `topFrameElement` (`browserRequest.js:525-565`) [01 §7] H.
- **`Zotero.BrowserRequest`** (internal, new in 10.0). Feature-detect it.
  - `CHALLENGE_URLS` covers sciencedirect, worldcat, pmc and mdpi.
  - `clearChallenge(url, {entry:{captchaLocator:null}, allowViewer})`: an `entry` must always be passed, because the code dereferences `entry.captchaLocator` (`browserRequest.js:162-185`, critic H).
  - `downloadPDF(url, path, {shouldDisplayCaptcha})` uses a default-UA HiddenBrowser plus PDF capture (`:567-614`) [02 §4, 06 §1] H.
- **Saving** (same storage shape as Find Full Text) [02 §7] H.
  - Steps: `createTemporaryStorageDirectory()` → `IOUtils.write(path, Uint8Array)` → `createURLAttachmentFromTemporaryStorageDirectory({directory, libraryID, parentItemID, filename, title, url, contentType})` (`attachments.js:682,2853`).
  - Filename: `getFileBaseNameFromItem(parent,{attachmentTitle})+'.pdf'` (`:2380`).
  - Title: `Zotero.getString('attachment.fullText'|'attachment.acceptedVersion'|'attachment.submittedVersion')`.
  - `Zotero.File.putContentsAsync` does not handle ArrayBuffer.
- **Eligibility and identifiers.**
  - An item is eligible if it is a regular, non-feed item in a `filesEditable` library with `numFileAttachmentsWithContentType('application/pdf')==0`.
  - Skip the item when `getBestAttachmentState()` reports a PDF with `exists:false`, which means synced but not downloaded (`data/item.js:2850-2862,4297-4385`) [02 §8] H.
  - Schema v44: `journalArticle` has native `DOI`, `PMID` and `PMCID` fields, and `preprint` has `DOI` and `archiveID` (`resource/schema/global/schema.json`, critic H). Also read DOI from Extra via `getExtraField`.
- **Reusable Zotero page logic.** `getFileFromDocument(doc)` runs only the first matching translator. `getHTMLMetaRefreshURL` follows the **first** refresh of 15 s or less and has **no `<noscript>` filter** (`http.js:1045-1072`, critic H). Strip `<noscript>` before calling it, or run our own loop [06 §2.2].
- **Dev harness.** `tools/zotero-harness/run.ps1` runs a throwaway profile and data dir with safety prefs: `extensions.zotero.firstRun2=false`, Word/LibreOffice `skipInstallation=true`, `httpServer.enabled=false`, `extensions.autoDisableScopes=0`. A proxy file or XPI loads on first start, and the user's Zotero is never touched [03 §2] H.

## (b) WASM feasibility verdict: GO
- **Works in every context.** A real Rust cdylib (82 KB, zero imports) passed all 13 steps in the bootstrap sandbox, main window, ESM global, ChromeWorker and Worker. Sync compile took 2-6 ms, 2000 JSON round trips took 45-137 ms, and nothing leaked [03 §3-4] H. All wasm prefs are at their defaults (`omni-gre/greprefs.js:391-394`) H.
- **Load path.**
  - `fetch(chrome://…)` and `arrayBuffer()` + `instantiate` work.
  - `instantiateStreaming` over chrome:// **fails** (`application/wasm;charset=UTF-8`).
  - `IOUtils.read` fails for a packed XPI [03 §5] H.
- **ABI.**
  - The hand ABI (alloc/free + `(ptr,len)` JSON in, packed `u64` out) was proven inside real Zotero [03 §4] and under Node [07 §3]. `i64` arrives as BigInt in every Zotero context, which answers [07]'s open question.
  - Reject wasm-bindgen: it needs ESM plus `Cu.cloneInto`, adds imports and gives no speed gain [03 §6, 07 §3.2].
- **Size.**
  - The full core with html5gum and `idna_adapter` 1.0.0 is **308 KB raw / 110 KB gzip**. With the hand scanner it is 190 / 75 KB [07 §1.4] H.
  - Compile time at that size inside Gecko is not measured. 82 KB took 2-6 ms, so the risk is low (L).
- **Where it runs.**
  - HTML parsing must happen in Rust if it ever runs in a worker, because workers have no `DOMParser`.
  - A typical page costs 1-5 ms on the main thread. Cap HTML input at about 2 MB; a 6 MB page took 140-320 ms. A ChromeWorker is optional [03 §11, 07 §2.2].
- **Panics.** A panic traps the instance. A panic hook writes the message to `fx_last_panic`, then JS marks the instance poisoned and re-instantiates it from the cached `WebAssembly.Module` (0.2-0.9 ms) [07 §4] H.

## (c) KCL access mechanics, login detection, proxify algorithm
| Route | Entry | Covers | Conf. |
|---|---|---|---|
| Campus IP / existing session | direct publisher URL, default jar | everything KCL licenses, when on campus or VPN | M |
| **OpenAthens redirector (primary)** | `https://go.openathens.net/redirector/kcl.ac.uk?url=<enc https://doi.org/DOI>` | WAYFless deep links for Elsevier/Lancet (via DOI), Wiley, OUP, Springer, Nature, T&F, SAGE, NEJM, JAMA, BMJ, Karger, Thieme, Cambridge. LWW passes through unchanged. `thelancet.com` URLs give 404, so use the DOI | H [04 §4.3-4.4] |
| EZproxy (secondary) | `https://login.kcl.idm.oclc.org/login?qurl=<encodeURIComponent(target)>` | about 19 configured hosts: doi.org, www.bmj.com, ATS, AJR, Liebert, JNeurosci, PsychiatryOnline, JOSPT, JoVE, Web of Science … | H [04 §2.4] |
| Alma CTO (entitlement oracle, no key) | `https://librarysearch.kcl.ac.uk/view/uresolver/44KCL_INST/openurl?ctx_ver=Z39.88-2004&url_ver=Z39.88-2004&rft_id=info:doi/<DOI>&svc_dat=CTO` | `getFullTxt` services with `package_public_name`, `Availability`, `Is_free`, `resolution_url` | H [04 §4.5] |
| LibKey API (library 286) | needs a Third Iron token (401 without one) | — | H [04 §5] |

- **Identity.**
  - EZproxy authenticates by SAML straight to Entra tenant `8370cf14-16f3-4c16-b83c-724071654356`; the SP is `https://kcl.idm.oclc.org/shibboleth`.
  - The UKfed IdP `https://kclidp.kcl.ac.uk/idp/shibboleth` is hosted by OpenAthens (org 71807903), which in turn uses Entra [04 §2.1, 4.1] H.
- **Sessions.** EZproxy idle timeout defaults to 120 min [04 §3.4] M. OpenAthens sessions last about 8 h (M). EZproxy cookie names and domain are unverified (L), so do not depend on them.
- **`classify(status, final_url, headers, body_head)`.** Rules are checked in order. XHR follows redirects, so classify the final URL; 3xx rules apply only to no-follow probes [04 §6.5, 06 §5.2]:
  1. `Pdf`: content type `application/pdf`, or `%PDF-` within 1 KiB.
  2. `EzproxyNeedLogin`: final host = login host with path `/login`, and the body has `name='EZproxyForm'`, title `Shibboleth Authentication Request` or `RelayState value='ezp.`.
  3. `EzproxyHostNotConfigured`: final URL `https://login.kcl.idm.oclc.org/<same path>` returning 404 `<H1>404 Not found</H1>`.
  4. `EzproxyErrorPage`: 200 HTML from `/login` without EZproxyForm. The page body was never seen (L).
  5. `IdpLogin`: host `login.microsoftonline.com` (`/<tenant>/saml2`, `/kmsi`, `/login.srf`), `login.live.com`, `login.openathens.net/auth/…` or `kclidp.kcl.ac.uk`; title "Sign in to your account".
  6. `SamlAutoPost`: a POST form with `SAMLRequest`/`SAMLResponse` and an onload submit.
  7. `OpenAthensWait`: title "OpenAthens / Please wait...". HEAD-poll every 2.5 s for up to 30 s until 201 or 302 with `Location`.
  8. `OpenAthensUnknownResource` (404 "OpenAthens: 404") and `OpenAthensBadOrg` (403).
  9. `PublisherSsoHop`: paths such as `/action/ssostart`, `/Shibboleth.sso/Login`, `auth.elsevier.com/ShibAuth`, `fsso.springer.com`, `sp.nature.com/saml`, `oup-sp.sams-sigma.com`.
  10. `Challenge`:
      - Cloudflare: `cf-mitigated: challenge`, `_cf_chl_opt`, "Just a moment...".
      - Proof of work and JS interstitials: PMC "Preparing to download ...", Thieme "Please wait", ScienceDirect `craft/challenge/pdf`.
      - Akamai `bm-verify`.
  11. `Blocked`: Cloudflare "Sorry, you have been blocked", or Akamai "Access Denied" / `errors.edgesuite.net`.
  12. `Redirect`: linkinghub `input#redirectURL`, or a meta refresh of 15 s or less that is **not** inside `<noscript>`.
  13. `ReaderShell`: `/doi/epdf/`, `/doi/reader/`, `.pdf+html`.
  14. Otherwise `Html` (landing page).
- **Proxify (Rust, pure)** [04 §6.3-6.4, 06 §4.1] H.
  - `login_url(t)`: `https://{login_host}/login?qurl=` + full percent-encoding of `t`. Never use `login?url=` when the target has a query string.
  - `proxify_host(t)`, steps in order:
    1. Return the URL unchanged if it is already proxied.
    2. Return None for an IP host or a non-default port; those go through `login_url`.
    3. https: lowercase the host, then `label = host.replace('.','-')`. Existing hyphens are **kept, not doubled**. Result: `https://{label}.{suffix}{path}{?query}{#frag}`.
    4. http: use the dotted form `http://{host}.{suffix}…`.
  - `unproxify(u)`:
    - `qurl`: decode once.
    - `url=`: take the rest of the query verbatim.
    - Proxied host label: look it up in the learned map (from observed `qurl` 302s), then the static hyphen table (thieme-connect, oecd-ilibrary…), then fall back to `Ambiguous` with a naive `-`→`.` replacement.
    - Also unwrap the OpenAthens redirector.
  - Match publisher rules on `host_key = host.replace('.','-')` rather than decoding.
- **Runtime probes.**
  - Is a host configured? GET the proxified `https://<label>.kcl.idm.oclc.org/` without following redirects. A `Location` of `login?qurl=` means configured; a `Location` on the login host with the same path means not configured. Cache the answer for 7 days.
  - Is a session alive? GET `https://doi-org.kcl.idm.oclc.org/10.1000/1` without following redirects [04 §6.3-6.5] H.
- **Per-item institutional flow.** OA runs first; then [04 §7] + critic:
  1. Direct fetch: campus IP or an existing session.
  2. Alma CTO. Skip the institutional route if there is no `getFullTxt` service.
  3. Run the redirector in a HiddenBrowser.
  4. On `IdpLogin`, open a **visible window at the entry URL**, not at the IdP URL, because the EZproxy→Entra hop is an auto-POST.
  5. Once the browser lands on the publisher, extract the PDF link, then fetch it with the same jar **and the same UA**, or capture it through the MIME handler.
  6. Use EZproxy only for hosts confirmed as configured.
  7. Store the un-proxied URL. Never store `login?`, redirector or SAML URLs.
- **Licence limits for institutional routes** [04 §8.2], basis: OCLC rules (logs above 50 PDFs per 5 min) and Jisc licence clauses 4.1.7/4.1.9 (M):
  - User-initiated only.
  - At most 20 items per batch; confirm above 10.
  - 1 request per publisher host and 2 overall at a time.
  - At least 5 s plus jitter between PDFs from the same host.
  - At most 30 PDFs per hour, 80 per rolling 24 h and 300 MB per hour.
  - At most 4 fetches per item.
  - No automatic retries; abort the batch on 429, a challenge loop or a suspend page.
  - Warn before saving into a group library.

## (d) OA source order and fields [05 §2, §6]
- **Phase 0 (Rust `normalise_identifiers`).**
  - DOI from the field or Extra, cleaned and compared case-insensitively.
  - PMID and PMCID from their fields.
  - arXiv id from `10.48550/arXiv.X`, `archiveID` or an arxiv.org URL.
  - `10.1101/` marks bioRxiv/medRxiv.
- **Phase 1, in parallel** (8-10 s timeouts, one backoff on 429/5xx):
  - **OpenAlex** `GET https://api.openalex.org/works/doi:{doi}`: free, keyless; an optional key goes in `Authorization: Bearer`.
    - PDF fields: `locations[].pdf_url`, `best_oa_location.pdf_url`, `primary_location.pdf_url`. Landing: `landing_page_url`.
    - Per location: `version`, `license`, `is_oa`, `source.type`, `id` (`doi:`/`pmid:`/`pmh:`).
    - Work-level: `open_access.{is_oa,oa_status,oa_url}`, `ids.pmcid`, `has_content.pdf`, `content_urls.pdf`.
    - Errors: a 404 comes back as HTML. H
  - **Europe PMC REST** `GET https://www.ebi.ac.uk/europepmc/webservices/rest/search?query=DOI:"{doi}"&resultType=core&format=json`. Fields: `pmcid`, `isOpenAccess`, `license`, `fullTextUrlList.fullTextUrl[]{documentStyle, availabilityCode, url}`. H
  - **bioRxiv** `GET https://api.biorxiv.org/details/{biorxiv|medrxiv}/{doi}/na/json` → `collection[]{version,date,license,published}`. `published` can be stale. H
  - Optional sources:
    - Zotero OA via `Zotero.Utilities.Internal.getOpenAccessPDFURLs(doi)` → `[{url,pageURL,version}]`. This is a private API, so give it a toggle.
    - Unpaywall: **only if the user sets an email**. The email is required, so no placeholder ships.
    - Semantic Scholar and CORE: off unless the user adds a key.
  - Then **PMC S3** `https://pmc-oa-opendata.s3.amazonaws.com/metadata/PMC{n}.1.json`. If that is missing, use ListObjectsV2 `?list-type=2&prefix=PMC{n}.`. Rewrite `pdf_url` from `s3://…?md5=` to https; it is served as `binary/octet-stream`, so sniff it. An AAM with `license_code "TDM"` has no PDF. H
- **Phase 2, sequential, stop at the first verified %PDF:**
  1. PMC S3 PDF.
  2. `https://europepmc.org/api/getPdf?pmcid=PMC{n}` (C1).
  3. `https://arxiv.org/pdf/{id}`.
  4. medRxiv/bioRxiv early path `https://www.{server}.org/content/{server}/early/YYYY/MM/DD/{suffix}.full.pdf`, built from the latest version's `date`. The canonical medRxiv URL is WAF-blocked intermittently.
  5. Direct `pdf_url`s: publisher gold/hybrid first, then repositories.
  6. Landing pages: Rust extractor plus Zotero translators on the same document.
  7. Challenge-walled copies (PMC website) via BrowserRequest or HiddenBrowser only.
  8. OpenAlex content API, only with the user's key ($0.01 each).
  9. Hand over to the institutional phase with Crossref `message.link[]` (`content-type: application/pdf`) and `resource.primary.URL`.
- **Ranking.**
  - Prefer published, then accepted, then submitted (configurable).
  - Rank every `locations[]` entry; never trust `best_oa_location` alone.
  - Down-weight locations whose record DOI differs from the requested DOI when the source is unknown. OpenAlex merges records: the arXiv DOI maps to `10.65215/…`, and medRxiv maps to PLOS ONE.
  - "Gold" does not imply a PDF URL (PLOS gives a landing page only).
  - Dedupe URLs **including** the query string [05 §5] H.
- **Politeness.**
  - OpenAlex ≤10 req/s.
  - Crossref 5 req/s with concurrency 1.
  - arXiv API 1 request per 3 s.
  - NCBI ≤3 req/s.
  - Publisher hosts 1 req/s; skip the domain after 5 consecutive failures.
  - About 4 items in flight [05 §6] M/L.
- **Errors are often not JSON.** Unpaywall and OpenAlex 404 as HTML, Crossref 404 as text, S3 as XML. Many "not found" answers are 200 with an empty result. Parse by status first [05 §8] H.

## (e) Publisher rules shortlist (Rust, data-driven) [06 §2-3]
| Host (unproxied) | PDF rule, in priority order | Gotcha |
|---|---|---|
| www.sciencedirect.com | JSON `__PRELOADED_STATE__…pdfDownload.urlMetadata` → `/science/article/pii/{pii}/pdfft?md5=&pid=`; then `/pdfft?download=true` | Signed `pdf.sciencedirectassets.com`: fetch immediately and never proxify. "Preparing your download" → `BrowserRequest.downloadPDF` |
| www.thelancet.com, www.cell.com | `citation_pdf_url` (`/pdfs/journals/<j>/PII<pii>.pdf`, `/<j>/pdf/<PII>.pdf`) | Avoid `/servlet/linkout…cf=pdf` and `mmc*.pdf`. The redirector only works via the DOI |
| onlinelibrary.wiley.com | `citation_pdf_url` rewritten `/doi/pdf/` → `/doi/pdfdirect/{doi}?download=true` | `/doi/epdf/` is a reader; never use it |
| Atypon: tandfonline, sagepub, nejm, atsjournals, liebertpub | `/doi/pdf/{doi}`, then `/doi/pdfdirect/{doi}?download=true` | T&F, SAGE and NEJM have no `citation_pdf_url`. Rewrite `/doi/reader/` → `/doi/pdf/` |
| link.springer.com (incl. BMC), www.nature.com | `citation_pdf_url` (`/content/pdf/{doi}.pdf`, `/articles/{id}.pdf`) | `idp.*` cookie dance. Without access the URL 302s back to HTML |
| academic.oup.com, jamanetwork.com, karger.com (Silverchair) | `citation_pdf_url` (file IDs cannot be built from the DOI) or Crossref `link[]` | JAMA: use `a#pdf-link[data-article-url]`. Watch for the "unusual traffic" page |
| journals.lww.com (Annals of Surgery) | `meta wkhealth_pdf_url` (`…/downloadpdf.aspx?…&an=<AN>`) | Zotero's translator ignores it. Cloudflare. No redirector deep link. Use Alma to tell LWW from Ovid |
| www.bmj.com (HighWire) | `citation_pdf_url`, `link[rel=alternate][type=application/pdf]` | The only proxied publisher at KCL. The SSO host has a weak-DH TLS problem (L) |
| thieme-connect, mdpi, pmc | `citation_pdf_url` (Thieme http→https; MDPI `/pdf?version=`) | JS/Akamai/proof-of-work walls. Skip MDPI `pdf-with-cover` |
| plos, frontiers, cambridge | `citation_pdf_url` | Not blocked (live %PDF) |

- **Generic ranking**, highest first:
  1. Publisher rule.
  2. `citation_pdf_url`: suffix match on the name, `content` or `contents`, first one wins.
  3. `link[rel=alternate][type=application/pdf]`.
  4. Other head metas (`wkhealth_pdf_url`, `fulltext_pdf`).
  5. JSON state.
  6. Scored anchors.
  7. iframe / embed / object.
  8. Interstitial redirects.
- **Reject:**
  - URLs carrying a different DOI or PII than the page's own.
  - Supplements (`suppl`, `MOESM`, `mmc\d`, `/attachment/`).
  - Citation exports.
  - `epdf`/`reader` shells.
  - `mailto:` and `javascript:` links.
  - Refreshes inside `<noscript>`.
- **Test oracle:** `fixtures/html/expected.json` (`kind`, `expected_pdf_url`, `alternates`, `must_not_select`) [06 §6] H. Of the 31 fixtures, 16 come from Wayback, so assert structure rather than live markup.

## (f) Toolchain and build recommendations [07]
- **Workspace.** Rust 1.96 targeting `wasm32-unknown-unknown`. The crate `fulltext-core` (rlib: logic + native tests) sits under a thin `fulltext-wasm` (cdylib with `extern "C"` only). A crate that is both rlib and cdylib silently drops LTO (+16%) [07 §1.3] H.
- **Release profile:** `opt-level="z"`, `lto=true`, `codegen-units=1`, `panic="abort"`, `strip=true`, `incremental=false`. Clippy: deny `unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`, `unimplemented`, `string_slice`; warn on `indexing_slicing` [07 §1.2, §4] H.
- **Dependencies.**
  - `serde`/`serde_json`.
  - `url` 2.5 with **`idna_adapter` pinned to 1.0.0** (`cargo update -p idna_adapter --precise 1.0.0`; −44%; ASCII hosts only).
  - **html5gum 0.8.4** with `DefaultEmitter::naively_switch_states(true)`.
  - Keep the 185-line hand scanner as the size fallback [07 §1.4, §2] H.
- **ABI.** Exports:
  - `fx_abi_version`, `fx_init`, `fx_alloc`/`fx_free`, `fx_last_panic`.
  - JSON exports take `(ptr,len)` and return a packed `u64`, wrapped as `{"ok":…}` or `{"err":{code,message}}`.
  - Raw two-buffer exports for HTML (2-2.3× faster), and `fx_sniff_pdf` [07 §3.1] H.
- **Glue.** A plain IIFE with no top-level `let`/`const`/`class`, loaded via chrome://. Cache the compiled `WebAssembly.Module` and re-create memory views after every call. Re-instantiate after a trap or a huge input [07 §3.1, §4] H.
- **Build.** `cargo xtask dist` uses `zip` 9 with `deflate-flate2-zlib-rs`, sorted entries and fixed timestamps, which gives a reproducible XPI.
  - **Always pass `--remap-path-prefix`.** Otherwise `C:\Users\abora\…` paths ship inside the `.wasm` [07 §6] H.
  - The user's global cargo config sets `rustc-wrapper=kache` and a global `CARGO_TARGET_DIR`, so the xtask must locate its output via `cargo metadata` [07 §1.1].
  - No wasm-opt: it saves at most 4% gzip. No Node build step. Zig is rejected [07 §7-8] H.
- **Tests.**
  - Native `cargo test` over `fixtures/html` and `fixtures/oa` (`manifest.json` holds the status codes).
  - Fuzz the extractor natively.
  - Integration runs through `tools/zotero-harness/run.ps1`: headless, packed XPI, local `httpd.sys.mjs` fakes for EZproxy, OpenAthens and the OA APIs [03 §2, §10].
- **XPI layout:** `manifest.json`, `bootstrap.js`, `prefs.js`, `locale/en-US/zotero-fulltext.ftl`, `content/{main.js, core-glue.js, fulltext_core.wasm, login.xhtml, preferences.xhtml, icons/}` [01 §8.3].

## (g) Risks
| Risk | Mitigation | Src |
|---|---|---|
| Embedded SSO fails: Entra Conditional Access/MFA, popups, `window.open` blocked, `ExternalLinkHandler` | Spike B1. Fallback: campus IP/VPN plus an "Open in browser" handoff | [01 §7], [04 §9], critic |
| Cloudflare `cf_clearance` is tied to the UA. Clearing in a plain-UA browser, then fetching with the Zotero UA, fails | Capture the PDF inside the browser (MIME handler), or `registerPlainUAHost` for the host (global). Keep one UA per host | `zotero.js:2283-2286`, critic M |
| About 12 of 20 publishers are bot-walled for plain HTTP | Tiered fetcher: plain HTTP → classify → HiddenBrowser capture → visible window. Never solve CAPTCHAs | [06 §5] H (curl; Gecko untested) |
| Zotero internals change (BrowserRequest, HiddenBrowser, downloadFirstAvailableFile, createURL…) | Feature-detect each; degrade to our own code | [02 §15], [06 §9] |
| Licence breach leads to institution-level suspension | Hard limits (§c) with persisted daily counters | [04 §8] M |
| `Zotero.HTTP` retry stalls (uncapped Retry-After, 1 h of 5xx retries) | `successCodes:false`, `errorDelayMax:0`, `noRetryOnThrottle:true`, our own timeouts | `http.js:1595-1622`, critic H |
| Wrong document attached (OpenAlex merges, mirrors, other-DOI links) | Check record DOI and source. Reject other-DOI URLs. Optionally match the PDF `/Title` later | [05 §2.3], [06 §2.2] |
| Signed or short-lived URLs (ScienceDirect, LWW) | Fetch immediately. Never cache or proxify | [06 §4.3] |
| Next ESR (Fx153) subscript rules | Load only via chrome:// | [01 §9] H |
| UI never appears because `onMainWindowLoad` does not fire for the first window; FTL name collision | Initialise from `startup()`; unique FTL filename | [03 §9], [01 §5] H |
| AGPL contamination | Re-implement behaviour; never copy Zotero code verbatim. Choose the plugin licence deliberately | [02 §14] M |
| Privacy: the redirector sees target URLs; local paths in the wasm; proxied URLs in attachments | Disclose in prefs; remap path prefixes; store un-proxied URLs | [04 §7], [07 §6] |
| Fixture and endpoint drift (Europe PMC `getPdf`, medRxiv early path, PMC proof of work) | Assert structure; make every source toggleable | [05 §8], C1 |

## (h) Open questions
**BLOCKING**
- **B1. Does KCL SSO complete inside Zotero's Gecko, and do the resulting sessions authorise downloads?**
  - Covers: the OpenAthens redirector → Entra (MFA, Conditional Access) → publisher ACS, and the EZproxy `login?qurl=` → Entra flow.
  - Test it in (i) a custom XHTML `<browser>` window and (ii) `Zotero.openInViewer`.
  - After sign-in, do `Zotero.HTTP.request` and HiddenBrowser PDF capture on the publisher or proxied host get the PDF?
  - How to resolve: **the user** runs a non-headless harness instance (throwaway profile and data dir) with a tiny probe plugin:
    1. Open the redirector for one entitled DOI (Wiley or Elsevier, Alma-confirmed) and for one BMJ URL via EZproxy.
    2. The user signs in themselves.
    3. The probe logs the final URLs, cookie hosts and names (no values) and the download results.
  - Claude cannot do this because of the no-login rule.

**Non-blocking** (decide defaults now; settle by spike)
- **N1. Gecko vs curl.** Do `Zotero.HTTP.request` calls to ScienceDirect, Wiley, OUP, LWW, JAMA, T&F, SAGE and NEJM get Cloudflare challenges? Test with an unauthenticated harness probe; this sets how often the browser tier runs [06 §9].
- **N2. Browser capture on unregistered hosts.** Does `BrowserRequest.downloadPDF` or a HiddenBrowser + MIME capture work on non-registered Cloudflare hosts (e.g. `europepmc ?pdf=render`)? Which UA policy works? Harness probe.
- **N3. `<noscript>` meta refresh.** The critic confirmed there is no `<noscript>` filter in code. Confirm DOMParser behaviour in the harness (LWW fixture).
- **N4. EZproxy after login.** Not observed: the needhost/suspend page text, cookie names and domain, `MaxLifetime`, and whether a doi.org→unconfigured-publisher hop leaves the proxy (L) [04 §9]. Answered as a side effect of B1.
- **N5. Annals of Surgery platform.** Is KCL's portfolio on LWW or Ovid? Check with the Alma CTO; with entitlement, check `downloadpdf.aspx` behaviour [06 §3.6].
- **N6. Untested entitled behaviour:** ScienceDirect challenge frequency, Wiley `pdfdirect`, Karger's DOI-based PDF form, SAGE `?download=true` [06 §9].
- **N7. Private or undocumented endpoints:** rate limits and acceptability of services.zotero.org/oa/search, Europe PMC `/api/getPdf` and the medRxiv early path. Make each toggleable [05 §9].
- **N8. Release decisions:** plugin ID, `update_url` host (GitHub raw?), `strict_max_version` (`10.*` vs `10.0.*` + update manifest), and the licence (MIT vs AGPL) [01].
- **N9. UX and keys:** an OpenAlex key and content-API spend (ask per download?), Unpaywall email, optional Crossref `mailto` (placeholder only where it is documented as optional) [05 §7].
- **N10. Runtime prefs and sessions:** `privacy.bounceTrackingProtection.mode` value (read it in the harness), and whether sessions persist across restarts (Entra "stay signed in" / OpenAthens ~8 h) [02 §12].
- **N11. Parser placement:** HTML input cap (2 MB?) and main thread vs ChromeWorker; measure compile and parse time of the 308 KB core in the harness [07 §10].
- **N12. LibKey:** would KCL provide an API key or endorse redirector use? Optional [04 §9].

---

## Appendix A. "Gap-fill results" section for `SUMMARY.md`

`research/SUMMARY.md` does not exist yet, and a harness hook refused this subagent's write to
it ("Subagents should return findings as text"). The synthesis step should paste the block
below into `SUMMARY.md`.

> ### Gap-fill results (B1: KCL SSO inside Zotero; detail in `08-gap-fill.md`)
> **Partly resolved.** Everything up to the password prompt is verified empirically in Zotero
> 10.0.6. The sign-in itself needs a real KCL login, which the no-login rule leaves to the user:
> `pwsh -NoProfile -File tools\sso-spike\run-manual.ps1` (20-30 min). The kit was validated by
> headless smoke tests with no login.
>
> - OpenAthens redirector chains (Nature, Elsevier, Wiley) reach KCL's Entra sign-in form in
>   both a custom window and `Zotero.openInViewer`, with no unsupported-browser message. EZproxy
>   `login?qurl=` loads, and its SAML auto-POST fires.
> - Cloudflare at Wiley's SSO start did not clear with Zotero's default UA, but did with
>   `getPlainFirefoxUA()`. Use the plain UA for login windows and Cloudflare-host fetches, and
>   keep the login window visible so the user can answer Turnstile.
> - Zotero bug: `VersionHeader` adds another `Firefox/140.0` on every redirect hop.
> - Cookies from real cross-site hops sit unpartitioned in the default jar. `Zotero.HTTP.request`,
>   `HiddenBrowser` and `fetch(…, {credentials:'include'})` send them; plain `fetch()` does not.
> - No Gecko setting blocks the SAML POST-back: `laxByDefault=false`, BTP dry-run,
>   `cookieBehavior=5` affects only third-party frames.
> - Do not use `openInViewer` for credentials. It has no address bar, sends `<a>` clicks to the
>   system browser, and hides popups. Ship the custom `login.xhtml`.
> - Plugin JSWindowActor child modules from the plugin's `chrome://` package do not load in
>   content processes. Use Zotero's `PageData` actor.
> - Residual risk is device-based Conditional Access. Windows SSO
>   (`network.http.windows-sso.enabled`) exists in Gecko but defaults to false. Fallback:
>   campus IP/VPN plus "Open in browser" plus "attach a downloaded PDF".

