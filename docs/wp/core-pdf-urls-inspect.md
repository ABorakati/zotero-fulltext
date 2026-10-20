# core-pdf-urls-inspect: PDF sniffing, proxy and institutional URLs, inspect dispatch rules, login_state

Package id `core-pdf-urls-inspect`. Implementer: Sonnet. Depends on: nothing for its own logic. Behaviour that needs the parsers or
the classifier (`inspect` end to end, `login_state` from raw URLs) is verified by `core-integration`, not here.
Read first: `docs/design/core.md` sections 2 (decisions 3-5), 3.2; `research/04-kcl-institutional-access.md` sections 2.3, 3, 4.2-4.4, 6.2-6.5;
`research/06-publisher-pdf-patterns.md` section 4; `research/SUMMARY.md` sections (c), (f); `docs/design/contracts/model.rs`.

## Files you own
`crates/fulltext-core/src/pdf.rs`, `src/urls.rs`, `src/text.rs`, `src/inspect.rs`, `crates/fulltext-core/tests/pdf.rs`,
`tests/urls.rs`, `tests/inspect.rs`. Read-only: everything else. Environment: `CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\core-pdf-urls-inspect`.
Several skeleton items are FINAL and used by other packages; keep their names, signatures and behaviour (add tests only):
`urls::{parse_http, host_of, host_key, resolve, canonical_key, percent_encode_component, percent_decode, doi_url, redirector_url, ezproxy_login_url, libkey_url}`, all of `text.rs`.

## Rules for all code
No `unwrap`/`expect`/`panic!`/indexing/string slicing outside tests. Total functions. Remove every `SKELETON` marker.

## 1. pdf.rs
```rust
pub const SNIFF_WINDOW: usize = 1024;
pub fn sniff(head: &[u8], tail: &[u8], total_len: u64) -> PdfVerdict;
pub fn verdict(resp: &RespMeta, body: &[u8]) -> PdfVerdict;
```
* `is_pdf`: `%PDF-` occurs within the first `SNIFF_WINDOW` bytes of `head`. `junk_prefix` = its offset (0 = clean). Zotero itself needs offset 0, so the engine trims `junk_prefix` bytes before saving (research/SUMMARY.md C7). Examples: BOM + `%PDF-1.7` -> offset 3; 1500 bytes of junk then `%PDF-` -> NOT a PDF; `%PDF` without the dash -> not a PDF.
* `has_eof`: `%%EOF` anywhere in `tail` (the last 1024 bytes; callers stage `head ++ tail`).
* `encrypted`: the byte string `/Encrypt` occurs in `head` or `tail` (informational only).
* `looks_html`: after optional UTF-8 BOM and ASCII whitespace the body starts with `<!doctype html`, `<html`, `<head`, `<body`, or `<?xml` followed later by `<html`; compare case-insensitively on the first 256 bytes. JSON (`{`), plain text and PDFs are false. `looks_html` is independent of `is_pdf` (a PDF never looks like HTML).
* `len`: the FULL body length (`total_len`); never the staged length.
* `verdict(resp, body)`: if `resp.head_len == Some(n)` the staged body is `head(n bytes) ++ tail`: split at `n` (clamped to `body.len()`), pass both parts and `resp.body_len`; otherwise `head = body`, `tail = body` (so `%%EOF` is searched in the last `SNIFF_WINDOW` bytes) and `len = resp.body_len.max(body.len() as u64)` when `body_len == 0`.
Tests (`tests/pdf.rs`): every example above, empty input, 1-byte input, `%PDF-` exactly at offset 1023 and 1024, split staging with a 1024-byte head and 1024-byte tail of a synthetic 5 MB file, the real PDF magic of `fixtures/oa` is not available (no PDF fixtures exist): build synthetic files. HTML fixtures from `fixtures/html` and `fixtures/oa` (`pmc-web__pdf-pow-interstitial.html`, `europepmc-web__...html`) must give `looks_html == true` and `is_pdf == false`; JSON fixtures give `looks_html == false`.

## 2. urls.rs: to implement
```rust
pub struct ProxyConf<'a> { pub login_host: &'a str, pub suffix: &'a str }
pub fn proxify(conf: &ProxyConf<'_>, target: &str) -> Option<String>;
pub fn unproxify(conf: &ProxyConf<'_>, url: &str, learned: &BTreeMap<String, String>) -> Unproxied;   // Unproxied { url, kind: UnproxyKind }
pub fn url_class(conf: Option<&ProxyConf<'_>>, url: &str) -> UrlClassKind;
pub fn strip_tracking(url: &str) -> String;
pub struct HandoffConf<'a> { pub openathens_domain: &'a str, pub ezproxy: Option<ProxyConf<'a>>, pub libkey_library_id: Option<u32> }
pub fn handoff(conf: &HandoffConf<'_>, ids: &Ids, landing: Option<&str>) -> Handoff;
```
**proxify** (HttpsHyphens; research/04 section 6.3), steps in order: (1) the target host is the login host or ends with `.<suffix>` -> return `Some(target)` unchanged; (2) scheme https: a port other than 443 -> `None`; an IP-literal host -> `None`; otherwise `https://<host lower-cased with every '.' replaced by '-'>.<suffix><path><?query><#fragment>` (existing hyphens are kept, NOT doubled; IDN hosts use the punycode form from the `url` crate); (3) scheme http: port other than 80 -> `None`, else `http://<host>.<suffix><path...>` (dotted form); (4) other schemes -> `None`. Vectors: `https://doi.org/10.1038/nature12373` -> `https://doi-org.kcl.idm.oclc.org/10.1038/nature12373`; `https://www.bmj.com/content/372/bmj.n71?x=1#a` -> `https://www-bmj-com.kcl.idm.oclc.org/content/372/bmj.n71?x=1#a`; `https://www.thieme-connect.de/a` -> `https://www-thieme-connect-de.kcl.idm.oclc.org/a` (existing hyphens are kept, never doubled, so the label is ambiguous on the way back: see unproxify); `http://doi.org/x` -> `http://doi.org.kcl.idm.oclc.org/x`; `https://example.org:8443/x` -> `None`; `https://10.0.0.1/x` -> `None`.
**unproxify** (research/04 section 6.4, exact): (A) host == login host: path (case-insensitive) `/login` or `/connect`: query key `qurl` -> percent-decode ONCE and parse (`Qurl`); else a query containing `url=` at the start or after `&` -> take EVERYTHING after the first `url=` verbatim (do not split on `&`; if it does not parse, percent-decode once) (`UrlParam`); other login-host paths (`/menu`, `/logout`) -> `NotProxied`. (B) host ends with `.<suffix>`: label = host minus `.<suffix>`; label `login` -> `NotProxied`; http scheme or label containing `.` -> real host = label (`Label`); https and no `.`: 1. `learned.get(label)` (`Label`), 2. built-in table of hyphenated publisher hosts keyed by `host_key` (`www.thieme-connect.de`, `www.thieme-connect.com`, `www.oecd-ilibrary.org`, `www.pep-web.org`, `www.wiley-vch.de`, `www.e-elgar.com`, `www.jbe-platform.com`, `www.un-ilibrary.org`, `wrds-www.wharton.upenn.edu`) (`Label`), 3. else replace `-` with `.` (`Ambiguous`; also `Ambiguous` for double-hyphen labels). Keep path, query and fragment; drop the port; lower-case the host first. (C) host `go.openathens.net` with path `/redirector/<d>`: decode the `url` query value once (`Redirector`). (D) anything else -> `NotProxied` with the input unchanged. Vectors (all must pass): `https://doi-org.kcl.idm.oclc.org/10.1038/nature12373` -> `https://doi.org/10.1038/nature12373` (Ambiguous without a learned map, Label with `doi-org -> doi.org`); `https://login.kcl.idm.oclc.org/login?qurl=https://doi.org%2fx%3fa%3d1%26b%3d2` -> `https://doi.org/x?a=1&b=2`; `https://login.kcl.idm.oclc.org/login?url=https://www.bmj.com/x?a=1&b=2` -> `https://www.bmj.com/x?a=1&b=2`; `http://doi.org.kcl.idm.oclc.org/x` -> `http://doi.org/x`; `https://WWW-BMJ-COM.kcl.idm.oclc.org/` -> `https://www.bmj.com/`; `https://www-thieme-connect-de.kcl.idm.oclc.org/a` -> `https://www.thieme-connect.de/a` (Label via the table); `https://login.kcl.idm.oclc.org/menu` -> unchanged `NotProxied`; `https://go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fdoi.org%2F10.1002%2Fbjs.11646` -> `https://doi.org/10.1002/bjs.11646`. Round trip property: for 20 assorted URLs, `unproxify(proxify(u))` returns `u` when the host has no hyphen, and an `Ambiguous` or `Label` result otherwise.
**url_class** (for `BrowserReq.hold_while` and `login_state`): `OpenathensRedirector` = host `go.openathens.net` and path starts `/redirector/`; `IdpLogin` = hosts `login.microsoftonline.com`, `login.live.com`, `login.microsoft.com`, `device.login.microsoftonline.com`, `kclidp.kcl.ac.uk`, or `login.openathens.net` with path `/auth/`; `EzproxyLogin` = host equals `conf.login_host` (default `login.kcl.idm.oclc.org` when `conf` is `None`) with path `/login` or `/connect`; `PublisherSsoHop` = URL contains any of `/Shibboleth.sso/Login`, `/Shibboleth.sso/discovery`, `/action/ssostart`, `auth.elsevier.com/ShibAuth`, `fsso.springer.com`, `sp.nature.com/saml`, `secure.nature.com/saml2`, `oup-sp.sams-sigma.com`, `shibboleth.highwire.org/session/init`, `profile.thieme.de/HTML/sso`, `karger.com/ExternalAuthentication`, `shibboleth.cambridge.org`; else `Other`. Order: redirector, IdP, EZproxy, SSO hop.
**strip_tracking**: remove query pairs whose name is `utm_*`, `fbclid`, `gclid`, `dclid`, `msclkid`, `mc_cid`, `mc_eid`, `ref_src`, `_ga`, `igshid`, `spm`, `cmpid`, `WT.mc_id` (case-insensitive); keep other pairs byte-for-byte and in order; drop `?` when nothing remains; keep the fragment; unparseable input is returned unchanged. Never reorder or re-encode surviving pairs (signed publisher URLs depend on it).
**handoff**: target = `doi_url(ids.doi)` when a DOI is known, else `landing`, else `ids.url` (http(s) only); no target -> `Handoff::default()`. `redirector_url` = `urls::redirector_url(conf.openathens_domain, target)` when the domain is non-empty; `ezproxy_url` = `ezproxy_login_url(p.login_host, target)` when `conf.ezproxy` is `Some`; `libkey_url` when both `libkey_library_id` and a DOI exist; `doi_url` when a DOI exists. Tests include a Lancet DOI with parentheses (`10.1016/S0140-6736(20)30183-5`), a DOI with a query-hostile character, and the BJS example from the model's `Outcome` docs.
Tests (`tests/urls.rs`): all vectors above, the skeleton's helpers (`canonical_key` keeps the query, `percent_*` round trips including multi-byte UTF-8, `doi_url`), 200 randomly generated URLs for the `unproxify`/`proxify` totality property (seeded xorshift, no `rand` crate).

## 3. inspect.rs
`inspect(meta, body)` stays a total dispatcher; implement the response-shape rule table in its module docs exactly:
* Guard first: `body.len() > 16 MiB` -> `Inspection::Failed { reason: "body_too_large" }`.
* `Api`: body starting with `%PDF-` -> `Failed { reason: "api_body_is_pdf" }`; else `Inspection::Api(oa::parse(source, &resp, body, &requested))`.
* `Page`: `pdf::verdict(..).is_pdf` -> `Inspection::Pdf(verdict)`; else `Inspection::Page(page::inspect_page(..))`.
* `Pdf`: `is_pdf` -> `Inspection::Pdf`; else when `looks_html` (or the status is >= 400 and the body is non-empty text) -> `Inspection::Page(page::inspect_page(&resp, &requested, route, body))`; else `Inspection::Pdf(verdict)` with `is_pdf == false`.
* `Probe`: `Inspection::Probe { class: page::classify_response(&resp, body) }`.
* `None`: `Inspection::None`.
`login_state(url, html)`: (1) fast path with no HTML: `urls::url_class(None, url)` of `OpenathensRedirector`, `IdpLogin`, `EzproxyLogin` or `PublisherSsoHop` -> `Pending`; (2) otherwise build `RespMeta { status: 200, final_url: url, body_len }` and call `page::classify_response(&resp, html.unwrap_or("").as_bytes())`; (3) map the class with a PUBLIC pure helper you add: `pub fn state_of(class: &PageClass) -> LoginState`: `IdpLogin { ca_error: Some(c) }` -> `Refused { code: Some(c) }`; `IdpLogin { None }`, `EzproxyNeedLogin`, `SamlAutoPost`, `OpenathensWait`, `PublisherSsoHop`, `Redirect` -> `Pending`; `Challenge` -> `ChallengeShown`; `OpenathensBadOrg` -> `Refused { Some("openathens_bad_org") }`; `OpenathensUnknownResource` -> `Refused { Some("openathens_unknown_resource") }`; `EzproxyHostNotConfigured` -> `Refused { Some("ezproxy_host_not_configured") }`; `EzproxyError { suspend: true }` -> `Refused { Some("ezproxy_suspended") }`, `{ suspend: false }` -> `Refused { Some("ezproxy_error") }`; `Blocked` -> `Refused { Some("blocked") }`; `RateLimited` -> `Refused { Some("rate_limited") }`; `LicenceStop` -> `Refused { Some("licence_stop") }`; `Pdf`, `Landing`, `Html`, `ReaderShell`, `NotFound`, `AccessDenied` -> `Landed`.
Tests (`tests/inspect.rs`): `state_of` for every `PageClass` variant (hand-constructed); dispatch cases that do not need other packages' behaviour: `Api` with PDF body -> `Failed`; `Pdf` spec with synthetic PDF bytes -> `Inspection::Pdf` with `is_pdf`; `Pdf` spec with `<html>` bytes -> `Inspection::Page`; `Page` spec with PDF bytes -> `Inspection::Pdf`; `None` -> `None`; 17 MiB body -> `Failed`; `head_len` staging through `inspect`; the serialized JSON of each result starts with the right `"expect"` tag. Cases that need real parsing (Probe class, fixtures) belong to `core-integration`.

## 4. Acceptance
```
set CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\core-pdf-urls-inspect
cargo test -p fulltext-core --test pdf --test urls --test inspect
cargo test -p fulltext-core
cargo clippy -p fulltext-core --all-targets -- -D warnings
git grep --no-index -n -E "SKELETON|not implemented" -- crates/fulltext-core/src/pdf.rs crates/fulltext-core/src/urls.rs crates/fulltext-core/src/inspect.rs crates/fulltext-core/src/text.rs
```
The last command must print nothing.
