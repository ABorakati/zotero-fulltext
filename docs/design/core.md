# core: workspace, fulltext-core, fulltext-wasm, xtask

Status: skeleton complete (compiles, `cargo test -p fulltext-core` green, wasm builds, Node ABI smoke test green).
Contracts: `docs/design/contracts/model.rs` and `proto.rs` are frozen. Background: `research/SUMMARY.md`
(cited below as S.a .. S.h), `docs/design/judgements.json`. Work packages: `docs/wp/core-*.md`.

## 1. What this area owns

| path | role |
|---|---|
| `Cargo.toml`, `.cargo/config.toml`, `clippy.toml`, `.gitignore` | workspace, lints, `cargo xtask` alias |
| `crates/fulltext-core` | pure, total, I/O-free logic (rlib, native-tested, fuzzable) |
| `crates/fulltext-wasm` | the only crate with `#[no_mangle]`; hand ABI, zero imports (cdylib only) |
| `xtask` | `build`, `test`, `xpi`, `harness` |
| `tests/js/abi.test.mjs` | Node smoke test of the real `.wasm` |

The engine (`crates/fulltext-engine`) and the JS plugin belong to other areas. The engine depends on
`fulltext-core` as a plain rlib: it calls `urls::*`, `ids::*`, `rank::rank`, `oa::*` natively. Only
`fx_inspect` / `fx_login_state` are reached through wasm, because JS owns the bytes.

```
          JS host (I/O, Zotero)                          wasm32 module (no imports)
  response bytes + RespMeta ──fx_inspect──▶ fulltext-wasm ─▶ fulltext_core::inspect::inspect ─▶ Inspection
  window URL / html        ──fx_login_state▶              ─▶ fulltext_core::inspect::login_state ─▶ LoginState
  StepIn {commands,completions} ──fx_step─▶              ─▶ fulltext_engine::Engine::step ─▶ StepOut {effects}
```

## 2. Decisions taken here (additions to the frozen architecture)

1. **`oa::*::parse` takes `&RespMeta`, not a bare status.** `parse(resp, body, requested) -> ApiInspection`.
   Reason: 429/503 `Retry-After` and the final URL (S3 key, PMC id) live in the response facts. `oa::gate` does the
   status-first mapping (404/410 NotFound, 429 Throttled, other non-2xx Unparseable) before any body parsing,
   because error bodies are HTML, XML or text (S.d "Errors are often not JSON").
2. **Crossref is reported as `Source::Doi`.** The frozen `Source` enum has no Crossref variant and adding one could
   break exhaustive matches in the engine. `Source::Doi` therefore means "DOI-registry metadata". If the engine ever
   wants a distinct trace label, add `Source::Crossref` additively (owner: core-ids-oa) and re-run `cargo check`.
3. **`login_state` rides on an ADDITIVE export `fx_login_state`.** The pinned ABI has only `fx_inspect`, and `Inspection`
   has no login variant. The additive export takes UTF-8 `url` and optional `html` and returns `Envelope<LoginState>`.
   The same classification is also reachable as `InspectSpec::Probe` (which returns `PageClass`).
4. **Response-shape rule** (`inspect.rs` module docs): the returned `Inspection` describes what the bytes ARE, not what
   was expected. `Page` spec + PDF bytes gives `Inspection::Pdf`; `Pdf` spec + HTML bytes gives `Inspection::Page`
   (so a challenge or login page is classified, not merely "not a PDF"). The engine must accept both for both specs.
5. **Institution knowledge is built in.** `InspectMeta` carries no `Institution`, so `page::classify` and
   `urls::url_class` hard-code the KCL values (login host `login.kcl.idm.oclc.org`, suffix `kcl.idm.oclc.org`,
   `go.openathens.net`, `login.microsoftonline.com`, `login.live.com`, `login.openathens.net`, `kclidp.kcl.ac.uk`)
   plus generic patterns (any host ending `.idm.oclc.org` with path `/login`, `/Shibboleth.sso/`, SAML form fields).
   Pure helpers that need config (`proxify`, `unproxify`, `handoff`) take it as arguments (`ProxyConf`, `HandoffConf`).
6. **`<noscript>` content is invisible to the extractor by construction.** html5gum with `naively_switch_states(true)`
   treats `<noscript>` as raw text, like a browser with scripting on. LWW, ScienceDirect and Europe PMC put a
   false-positive `<meta refresh>` there (S.e). Zotero's own DOMParser path does the opposite (S.a), so keep it this way.
7. **Tiers are numbers in `rank::tier`** (1 PMC S3, 2 Europe PMC getPdf, 3 arXiv, 4 rxiv early, 5 publisher direct,
   6 repository direct, 7 landing, 8 browser only, 9 paid API) and mirror S.d "Phase 2". The engine sorts by
   (`tier`, `-score`); `Candidate.tier == 0` means "unranked".
8. **Totality contract.** No function in `fulltext-core` or `fulltext-wasm` may panic: workspace clippy denies
   `unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `string_slice`, `unreachable`, `todo`, `unimplemented`
   (`clippy.toml` re-allows them inside tests; integration-test files add a file-level `#![allow]`). The release profile
   is `panic = "abort"`, so a panic poisons the wasm instance. Bad input becomes a value:
   `ApiStatus::Unparseable`, `PageClass::Html`, `Inspection::Failed { reason }`, `Envelope::Err`.
9. **`#![forbid(unsafe_code)]` in `fulltext-core`.** The only `unsafe` in the area is `crates/fulltext-wasm/src/abi.rs`.
10. **MSRV 1.88** (let chains, `is_none_or`); built and tested with Rust 1.96.

## 3. fulltext-core

### 3.1 Module map

| module | public surface (all total) | package |
|---|---|---|
| `model` | frozen contract, verbatim copy of `contracts/model.rs` | (frozen) |
| `ids` | `normalise(&ItemFields) -> Ids`, `clean_doi/pmid/pmcid/arxiv`, `arxiv_from_doi`, `is_rxiv_doi`, `extra_field`, `apply_learned`, `same_doi` | core-ids-oa |
| `oa` | `parse(Source, &RespMeta, &[u8], &Ids) -> ApiInspection`; per source `openalex`, `europepmc`, `biorxiv`, `pmc_s3`, `arxiv`, `unpaywall`, `semantic_scholar`, `core_api`, `crossref`, `alma`, `zotero_oa`; helpers `gate`, `empty`, `unparsed`, `version_from_str`, `host_kind`, `s3_to_https` | core-ids-oa |
| `rank` | `new_candidate`, `merge`, `score`, `rank`, `RankPrefs`, `tier::*` | core-ids-oa |
| `page::html` | `parse(&str) -> Doc` (title, metas, links, anchors, frames, forms, inputs, scripts, refreshes) | core-page |
| `page::classify` | `classify(&Facts) -> PageClass` | core-page |
| `page::extract` | `extract(&PageCtx) -> Extraction`, `page_ids` | core-page |
| `page::rules` | `RULES`, `rule_for(host_key)` | core-page |
| `page` | `inspect_page(&RespMeta,&Ids,Route,&[u8]) -> PageInspection`, `classify_response(&RespMeta,&[u8]) -> PageClass` | core-page |
| `pdf` | `sniff(head, tail, len) -> PdfVerdict`, `verdict(&RespMeta,&[u8])` | core-pdf-urls-inspect |
| `urls` | `parse_http`, `host_of`, `host_key`, `resolve`, `canonical_key`, `percent_encode_component`, `percent_decode`, `doi_url`, `redirector_url`, `ezproxy_login_url`, `libkey_url` (all implemented); `proxify`, `unproxify`, `url_class`, `strip_tracking`, `handoff` (to do) | core-pdf-urls-inspect |
| `wayfless` | `wayfless_url(proxy, entity_id, url)`, `wayfless_for(proxy, entity_id, doi, urls)`, `target_for_doi`, `platform_of`, `valid_entity_id`, `is_supported` (publisher SAML / Shibboleth sign-in links, section 3.4) | institutions-saml |
| `text` | `decode`, `head`, `tail`, `contains_ci`, `MAX_TEXT_BYTES` (implemented) | core-pdf-urls-inspect |
| `inspect` | `inspect(&InspectMeta,&[u8]) -> Inspection`, `login_state(&str,Option<&str>) -> LoginState` | core-pdf-urls-inspect |

Dependency direction (no cycles): `model, text` <- `ids, urls` <- `oa, rank, page, pdf` <- `inspect`.
Skeleton-implemented helpers that several packages rely on and that must keep their signatures: `ids::clean_doi`,
`urls::{parse_http, host_of, host_key, resolve, canonical_key, percent_*}`, `oa::{gate, empty, unparsed}`,
`rank::new_candidate`, `text::*`, `page::html::*`.

### 3.2 Pipelines, in the order the engine uses them

1. **Phase 0.** `ids::normalise(&snapshot.fields)` gives `Ids` (DOI from field, Extra, URL or archive id; PMID/PMCID;
   arXiv incl. `10.48550` DOIs; `rxiv` for `10.1101/`). The engine builds request URLs from `Ids`.
2. **Phase 1 (parallel APIs).** JS performs each GET, stages the body, calls `fx_inspect` with
   `InspectSpec::Api { source, requested }`. `oa::parse` returns `ApiInspection { status, learned, candidates, oa_status,
   note, retry_after_s }`. `ids::apply_learned` folds `learned` ids into `Ids` (never overwriting).
3. **Phase 2 (rank).** `rank::merge` pools candidates from all sources and the page extractor; `rank::rank` dedupes
   (canonical URL INCLUDING query), maps aliases, scores, assigns `tier`, sorts.
4. **Attempt.** `InspectSpec::Page` for landing pages (`page::inspect_page`: classify, then extract for
   `Landing`/`Html`/`ReaderShell`), `InspectSpec::Pdf` for direct files (`pdf::verdict`), `InspectSpec::Probe` for
   no-follow probes (`page::classify_response`). `PageClass::Redirect { next }` tells the engine to follow.
5. **Institutional.** `urls::redirector_url` / `ezproxy_login_url` / `proxify` build entry URLs; `urls::unproxify`
   recovers the real URL to store; `urls::handoff` builds the "open in browser" links; the login window polls
   `login_state`.

### 3.3 Publisher rules and generic extraction

Order (S.e): publisher rule > `citation_pdf_url` (name-suffix match) > `link[rel=alternate][type=application/pdf]` >
other head metas > JSON state > scored anchors > frames > interstitial redirects. Rules match on
`urls::host_key(host)` of the UNPROXIED host, and also on its proxified form by prefix. Rejections are reported, not
silently dropped (`PageInspection.rejected`), so the failure catalogue can assert them. Acceptance oracle:
`fixtures/html/expected.json` (31 pages; 16 are archived, so tests assert structure).

### 3.4 SAML / Shibboleth: WAYFless links and generic identity-provider pages

Institutions that are not on OpenAthens (Imperial, Oxford, Cambridge, QMUL, Manchester, Edinburgh ...) sign in through
the UK Access Management Federation: the publisher's own "institutional login" with the university's IdP `entityID`.
`wayfless::wayfless_url(proxy, entity_id, url)` builds the publisher deep link that already names the IdP (no "find your
institution" page). Input is an article/landing URL (EZproxy-proxified or OpenAthens-wrapped input is un-proxified
first with `urls::unproxify`; detection uses `urls::host_key`) or, through `wayfless_for` / `target_for_doi`, a DOI.
The target is always the real https article URL without tracking parameters or fragment; IP literals and non-default
ports give `None`. `E` below is `percent_encode_component(entityID)`, `T` the same for the target.

**Evidence (2026-10-21).** The IdP used for verification is KCL's, `https://kclidp.kcl.ac.uk/idp/shibboleth`, taken
from `https://md.seamlessaccess.org/entities/?q=king%27s%20college%20london` (JSON `entityID`; the same search finds
Oxford `https://registry.shibboleth.ox.ac.uk/idp` and Imperial `https://shibboleth.imperial.ac.uk/shibboleth`). Every
request was an unauthenticated `GET` (redirects followed by hand until the first IdP host; nothing posted, no
credentials, no CAPTCHA bypass). Two kinds of evidence:

* **chain**: our URL was followed to the IdP's SSO endpoint (`login.openathens.net/saml/2/sso/kcl.ac.uk/...`, which hosts
  KCL's IdP; for Oxford `idp.shibboleth.ox.ac.uk/idp/profile/SAML2/Redirect/SSO`, for Imperial an Entra tenant).
* **redirector**: the publisher answers non-browser clients with a Cloudflare "Just a moment..." challenge (403), so the
  chain cannot be followed; the OpenAthens redirector (`go.openathens.net/redirector/kcl.ac.uk?url=<target>`, which
  publishes the WAYFless link it knows for each target) returned exactly this URL shape for a real article of the host.
  The URL form is therefore the publisher's own, but the last hop (the publisher answering a browser) is unverified.

| platform (`Platform`) | hosts | URL | evidence |
|---|---|---|---|
| `Atypon` | `onlinelibrary.wiley.com`, `tandfonline.com`, `journals.sagepub.com`, `nejm.org`, `science.org`, `pnas.org`, `ascopubs.org`, `embopress.org`, `ahajournals.org` | `https://<host>/action/ssostart?idp=E&redirectUri=T` | redirector (all nine) |
| `Elsevier` | `sciencedirect.com`, `linkinghub.elsevier.com` (page as is); `thelancet.com`, `cell.com` (PII from the path, return via linkinghub); `10.1016/S...` DOIs (PII from the DOI) | `https://auth.elsevier.com/ShibAuth/institutionLogin?entityID=E&appReturnURL=T` | chain: 200 with the auto-POST form to `/SHIRE/Login?SAMLDS=1&entityID=E` (not submitted); redirector for sciencedirect and DOI. No PII, no link |
| `Springer` | `link.springer.com`, `springer.com` | `https://fsso.springer.com/federation/init?entityId=E&returnUrl=T` | chain to the IdP |
| `Nature` | `nature.com` | `https://sp.nature.com/saml/login?idp=E&targetUrl=T` | chain to the IdP (KCL, Oxford, Imperial) |
| `Oup` | `academic.oup.com` (BJS and other OUP journals) | `https://oup-sp.sams-sigma.com/Shibboleth.sso/Login?entityID=E&target=enc(https://oup-sp.sams-sigma.com/shib?dest=https://www.academic.oup.com/SHIBBOLETH?dest=<T raw>)` | chain to the IdP (KCL, Oxford, Imperial) |
| `ShibbolethSso` | `jamanetwork.com`, `pubs.acs.org`, `emerald.com` | `https://<host>/Shibboleth.sso/Login?entityID=E&target=T` | redirector |
| `Cambridge` | `cambridge.org` | `https://shibboleth.cambridge.org/Shibboleth.sso/discovery?SAMLDS=1&target=enc(https://shibboleth.cambridge.org/CJOShibb2/index?app=https://www.cambridge.org/core/shibboleth?ref=<T raw>)&entityID=E` | chain to the IdP |
| `HighWire` | `bmj.com` and every `*.bmj.com` | `https://shibboleth.highwire.org/session/init?entityID=E&hw-shib-return-uri=T&subcode=bmjjournals&env=prod` | chain to the IdP (needs cookies: 4 hops through `/shibboleth-v2/`, `/applications/secure-sp/Shibboleth.sso/Login`); redirector for `www`, `bmjopen`, `gut` |
| `Karger` | `karger.com` | `https://karger.com/ExternalAuthentication/KeystoneDeeplink?entityId=E&destination=T` | redirector |
| `Thieme` | `thieme-connect.com`, `thieme-connect.de` | `https://profile.thieme.de/HTML/sso/ejournals/shibboleth.htm?federationid=http%3A%2F%2Fukfederation.org.uk&entityid=E&hook_url=T` | chain to the IdP (UK federation only) |
| `AnnualReviews` | `annualreviews.org` | `https://www.annualreviews.org/session/ext/athens?url=T&entityId=E` | redirector (Annual Reviews is Atypon, but not by the `ssostart` form) |

Differences from the brief: Atypon's `redirectUri` is the full article URL (not a path); ACS is not an `ssostart` site
(it runs a Shibboleth SP, `ShibbolethSso`); Springer's `wayf.springernature.com?redirect_uri=&entityID=` is only a
discovery page ("Access through your institution") and is NOT used (`fsso.springer.com/federation/init` is the
WAYFless form); `wayf.springernature.com/sso` is a 404. Other identity providers were checked for Nature and OUP only
(Oxford and Imperial: the chain reached `idp.shibboleth.ox.ac.uk` and Imperial's Entra tenant); everything else is
assumed to differ from the KCL check only in `E`.
`Elsevier` with an arbitrary `appReturnURL` (even `https://example.com/`) is accepted by the first hop, so the
allow-list (if any) acts after authentication and was not tested: only Elsevier hosts (and linkinghub PII links, the
form the redirector itself emits for DOIs) are ever used as the return target.

**Not supported (`None`)**: Wolters Kluwer / LWW `journals.lww.com` (the redirector passes the target through
unchanged: no WAYFless form), OUP/JAMA/Cambridge/Karger/LWW and non-`S` Elsevier DOIs from a DOI alone (the article URL
is not a function of the DOI), any other host. `target_for_doi` covers 10.1002/10.1111 (Wiley), 10.1080, 10.1177,
10.1056, 10.1126, 10.1073, 10.1200, 10.15252, 10.1161, 10.1021, 10.1146, 10.1055, 10.1007 (`article` or, for
`978-`, `chapter`), 10.1038, `10.1016/S...` and `10.1136/bmj.*`.

**Generic IdP pages.** `urls::is_generic_idp(host, path)` (used by `url_class`, and by `page::classify` for the
`Idp::Other` label) recognises a Shibboleth IdP v2-v5 / CAS / AD FS / SimpleSAMLphp / NetIQ page by path
(`/idp/profile/`, `/idp/Authn/`, `/idp/shibboleth`, `/adfs/ls`, `/simplesaml/saml2/idp/`, ...) or by a host whose first
label is `idp`, `idp1`, `idp2`... (not by a bare `shibboleth.` host: `shibboleth.cambridge.org` and
`shibboleth.highwire.org` are SPs). `classify` also recognises the IdP's own views by markup on any host: a form posting to
`/idp/profile/...`, or Shibboleth flow fields (`j_username`, `j_password`, `_eventId*`, `shib_idp_*`), at a status
below 400, as `IdpLogin { idp: Other }` (the Entra, OpenAthens and KCL hosts keep their labels). Publisher-side
Shibboleth SP paths (`/Shibboleth.sso/...`, any endpoint) are `PublisherSsoHop` / `UrlClassKind::PublisherSsoHop`, not
IdP pages (the brief's "/Shibboleth.sso/ as IdpLogin" was read as "interactive page": an SP endpoint is a hop that
leads to one). `state_of` maps `IdpLogin` to `Pending`, so the visible window keeps waiting while the IdP is shown.
Fixtures: `crates/fulltext-core/tests/data/page/shibboleth_idp_login.html` (hand-made, shaped like the v4/v5 default
login view) and `shibboleth_idp_session_loading.html` (the real "Loading Session Information" interstitial of an
Oxford-style IdP, captured by an unauthenticated GET, institution name and per-request tokens replaced).

**Hand-off.** `Handoff.wayfless_url` (additive, `skip_serializing_if = None`) carries the link; `urls::handoff` fills it
from `HandoffConf.saml_entity_id` (reached page, else item URL, else DOI). Contract additions of this work: `Realm::Saml`,
`Source::Saml`, `Handoff.wayfless_url`, and in the engine protocol `Institution.saml_entity_id`,
`Institution.ezproxy_all_hosts`, `InstPrefs.use_saml` (see `engine.md` section 8).

### 3.5 Alma link resolver (`oa::alma`): full-text links, not only "held"

Live captures (2026-10-21, unauthenticated GET, no login) of
`https://librarysearch.kcl.ac.uk/view/uresolver/44KCL_INST/openurl?ctx_ver=Z39.88-2004&url_ver=Z39.88-2004&rft_id=info:doi/<DOI>&svc_dat=CTO`
are in `tests/data/oa/alma-{wiley,lww,lancet,oup,nature,springer-not-held,unknown-doi}.xml` (trimmed: context keys and
service keys reduced, `performance_counters` dropped, long `Availability` cut). The service target is **always** an
Alma redirector, never the publisher URL itself and never EZproxy- or OpenAthens-wrapped (`proxy_enabled` is `false`
everywhere):

`<resolution_url>` = `https://kcl.alma.exlibrisgroup.com/view/action/uresolver.do?operation=resolveService&package_service_id=<id>&institutionId=6881&customerId=6880`
(an element after `<keys>`; the key form is also tolerated). Unauthenticated GET gives a 302 to the platform; the
hop depends on the package parser (`parser_program`):

| DOI | service package | first 302 target |
|---|---|---|
| 10.1111/nep.13250 | Wiley journals current | `https://onlinelibrary.wiley.com/doi/full/10.1111/nep.13250` (article-level; then 403, bot wall) |
| 10.1093/bjs/znab385 | OUP Read and Publish | `https://academic.oup.com/bjs/article-lookup/doi/10.1093/bjs/znab385` (article-level; then 403) |
| 10.1038/s41586-020-2649-2 | Nature | `https://www.nature.com/openurl?id=doi:...&genre=article`, then 303 to `idp.nature.com/authorize` (login, stopped there) |
| 10.1016/S0140-6736(20)30183-5 | Elsevier ScienceDirect | `https://www.sciencedirect.com/science?_ob=GatewayURL&_origin=SFX&_method=citationSearch&_volkey=...` (journal/issue-level gateway, not the article; then 403) |
| 10.1097/SLA.0000000000005603 | Journals@Ovid LWW | `https://ovidsp.ovid.com/athens/ovidweb.cgi?CSC=Y&...&PAGE=fulltext` (article search by ISSN/volume/page/DOI, Athens entry; 403) |
| same | Wolters Kluwer transitional OA | `https://ovidsp.ovid.com/ovidweb.cgi?T=JS&...&PAGE=toc` (journal TOC only) |
| same | PubMed Central (current_access=false) | `https://www.ncbi.nlm.nih.gov/pmc/ivip/0003-4932/276/4/579` -> 404 (coverage ended 2007) |
| same | Canadian Libraries (Is_free=1, `Bulk::BULK`) | `http://www.archive.org/details/annalsofsurgery571philuoft` (a 1913 volume: wrong for this article) |

So a candidate URL must be opened **in the user's browser session** (`needs_browser`): the redirector needs no cookie
itself, but the publisher it lands on does (OpenAthens/Shibboleth, or the publisher cookie), and several targets are
only journal/issue-level. A 5-service Nature response is about 6 KB; an unknown or not-held DOI returns
`<context_services/>` and `full_text_indicator=false` (`Empty`, note `not_held`).

Output contract (pinned for the engine): per `getFullTxt` service with an http(s) target one `Candidate`
`{ url: resolution_url, kind: Pdf if the URL is evidently a file else Landing, source: Alma, version: Published,
host_kind: Publisher, record_doi: requested DOI, landing_url: None, signal: "alma:<package_public_name>" (80 chars at
most), score: 0, tier: 0, needs_browser: true }`. `Found` when at least one exists. Extra rules, all derived from the
captures: services with `current_access=false` (coverage window ended before the article, e.g. PubMed Central 1885-2007
for a 2022 paper) are dropped when any service is current, and kept only when nothing is current (so the "held" signal
survives); remaining services are ordered paid before free (the free `Bulk` archives rarely hold the article), then in
document order, de-duplicated by URL, at most 5. `oa_status = "free"` when a retained service has `Is_free` 1.

## 4. fulltext-wasm ABI v1

Exports (all `u32`, UTF-8 JSON, results `(ptr << 32) | len` as BigInt in JS, JS frees every result with `fx_free`):

| export | contract |
|---|---|
| `fx_abi_version() -> u32` | 1 |
| `fx_alloc(len) -> ptr` | `Layout{len,1}`; `len == 0` returns a dangling non-null; `0` only on OOM |
| `fx_free(ptr, len)` | no-op for `ptr == 0`, `len == 0`, or the static panic buffer |
| `fx_inspect(meta_ptr, meta_len, body_ptr, body_len) -> u64` | `InspectMeta` JSON + raw body; `Envelope<Inspection>`; callee frees both inputs |
| `fx_step(in_ptr, in_len) -> u64` | `StepIn` JSON; `Envelope<StepOut>`; callee frees the input; engine is a `thread_local RefCell<Option<Engine>>` |
| `fx_reset()` | drops the engine; the next `fx_step` calls `Engine::new()` |
| `fx_last_panic() -> u64` | `{"code":"panic","message":...}` of the last panic or len 0; allocation-free, valid after a trap |
| `fx_login_state(url_ptr,url_len,html_ptr,html_len) -> u64` | ADDITIVE; `Envelope<LoginState>`; `html_len == 0` = no document |

Rules: JS writes only into buffers from `fx_alloc` and re-creates its `Uint8Array` views after every call (memory
may grow); every export returns an envelope and never traps on bad input; panics install a hook lazily (first export
call) that stores the message for `fx_last_panic` before the abort trap; JS marks the instance poisoned on
`WebAssembly.RuntimeError`, re-instantiates the cached module, and fails the current run if it was the engine
instance (no journal replay in v1). `xtask` and `tests/js/abi.test.mjs` assert: zero imports, exports are exactly the
documented set plus `memory`, `__data_end`, `__heap_base`; steady-state calls do not grow memory.

Code layout: `json.rs` (bytes in, bytes out, native unit tests), `panic_hook.rs`, `abi.rs` (wasm32 only, the only
`unsafe`). Size at skeleton stage: 305 KB raw with the engine skeleton; budget for the finished build is about
420 KB raw / 150 KB gzip (S.f: core 308 KB with html5gum and `idna_adapter` 1.0.0, plus engine and futures-util).

## 5. xtask

`cargo xtask build` : `cargo build -p fulltext-wasm --release --target wasm32-unknown-unknown` with
`CARGO_ENCODED_RUSTFLAGS` remapping home, cargo, rustup and workspace paths (no `C:\Users\...` in the binary), output
located via `cargo metadata` (the user's global config sets `CARGO_TARGET_DIR` and the `kache` wrapper), copied to
`plugin/content/fulltext.wasm`. `test` runs `cargo test --workspace`, `build`, `node --test tests/js`.
`xpi` builds then writes a deterministic zip (sorted entries, 1980-01-01 timestamps, deflate 9) to
`dist/zotero-fulltext-<version>.xpi`, version from `plugin/manifest.json`. `harness` is a stub until core-wasm-xtask
wires `tools/zotero-harness/run.ps1` (throwaway profile only).

## 6. Testing strategy

| layer | where | what |
|---|---|---|
| unit | `#[cfg(test)]` in each module | small vectors |
| contract | `tests/model_contract.rs` | wire shapes of the model types |
| fixtures | `tests/oa_fixtures.rs`, `tests/page_fixtures.rs`, `tests/classify.rs` | every `fixtures/oa` and `fixtures/html` file through the real parser, structural assertions (`tests/common` loads body + status + URL) |
| vectors | `tests/urls.rs`, `tests/ids.rs`, `tests/rank.rs`, `tests/pdf.rs` | proxify / unproxify (research/04 6.3-6.4), DOI forms, ranking cases, PDF sniffing |
| e2e | `tests/e2e.rs`, `tests/totality.rs` | `inspect()` on every fixture with every spec; random/mutated bytes never panic |
| wasm | `crates/fulltext-wasm` unit tests, `tests/js/abi.test.mjs`, `tests/js/abi-recovery.test.mjs`, `tests/js/core-wasm-fixtures.test.mjs` | envelopes, ABI, trap recovery, fixtures through the real module |

Commands: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo clippy -p fulltext-core -p fulltext-wasm --target wasm32-unknown-unknown -- -D warnings`, `cargo xtask build`,
`node --test tests/js`, `cargo xtask xpi`. Parallel agents set `CARGO_TARGET_DIR=...\target\<package-id>`.

## 7. Work packages

| id | files | depends on | deliverable |
|---|---|---|---|
| core-ids-oa | `ids.rs`, `oa/**`, `rank.rs`, `tests/{ids,oa_fixtures,rank}.rs` | none | identifiers, 11 parsers, ranking |
| core-page | `page/**`, `tests/{page_fixtures,classify}.rs` | none | tokenizer, classifier, extractor, rules |
| core-pdf-urls-inspect | `pdf.rs`, `urls.rs`, `text.rs`, `inspect.rs`, `tests/{pdf,urls,inspect}.rs` | none (logic); `inspect` rules reference page/oa behaviour | PDF sniff, proxy URLs, dispatch rules, `login_state` |
| core-wasm-xtask | `crates/fulltext-wasm/**`, `xtask/**`, `tests/js/abi.test.mjs`, `tests/js/abi-recovery.test.mjs` | none | ABI hardening, `debug-panic` trap-recovery build, xtask wasm verification, `check`, harness command |
| core-integration | `crates/fulltext-core/tests/{e2e,totality}.rs`, `tests/js/core-wasm-fixtures.test.mjs` | the four above | cross-module acceptance, totality fuzzing, fixtures through the real wasm, size budget, final clippy on both targets |

## 8. Hand-offs to other areas

* **Engine:** accept both `Inspection::Pdf` and `Inspection::Page` for `Page` and `Pdf` specs (decision 4); read the
  published DOI of a preprint from `ApiInspection.note` (`published_doi:<doi>`); use `rank::tier::*` as the attempt order;
  call `urls::*` and `ids::*` directly (rlib). `EngineError::to_err_body()` is what `fx_step` serializes.
* **JS glue:** instantiate the same module twice; copy result bytes out before the next call; call `fx_free(ptr,len)` for
  every result, including `fx_last_panic`; map BigInt results with `Number(p >> 32n)`, `Number(p & 0xffffffffn)`; use
  `fx_login_state` for the login window poll; evaluate `BrowserReq.hold_while` with a tiny host matcher or via
  `fx_login_state(url)` (`Pending` means still in SSO); no other exports exist (debug exports only in test builds).
* **Packaging:** `cargo xtask xpi` expects `plugin/manifest.json` with a `version`.
