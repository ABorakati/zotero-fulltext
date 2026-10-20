# 04 — KCL institutional e-resource access and EZproxy mechanics

Research brief for the `zotero-fulltext` plugin (Zotero 10.0.6, Rust/WASM core with a JS shell).
Probes and web research were done on 2026-10-20, 13:20–14:00 UTC. All probes were unauthenticated GET or HEAD requests. No forms were submitted, no credentials were entered, and no SSO flow was completed. The tracer stopped before it fetched any `login.microsoftonline.com` page, with one exception: the first exploratory trace fetched the Entra sign-in page once.

Confidence key: **high** = verified in source code or by an empirical probe. **medium** = official documentation. **low** = inferred or second-hand.

---

## 0. Key findings

| # | Finding | Confidence |
|---|---|---|
| 1 | KCL has **three** access routes. (a) **Federated SAML "Institutional login"**: KCL says this covers the majority of e-resources. (b) An **OCLC-hosted EZproxy** at `login.kcl.idm.oclc.org`, which covers a small set of resources. (c) **On-campus IP**. KCL also licenses **LibKey Nomad** (library ID 286). | high (KCL FAQ + probes) |
| 2 | EZproxy login prefix: `https://login.kcl.idm.oclc.org/login?url=` (KCL's own documented links) or `…/login?qurl=<percent-encoded>`. EZproxy itself generates `qurl`. Proxied hosts use HttpsHyphens: `https://www-bmj-com.kcl.idm.oclc.org/…`. | high (probe) |
| 3 | EZproxy authenticates by **SAML 2.0 directly against Microsoft Entra ID**. Tenant `8370cf14-16f3-4c16-b83c-724071654356` (= `kcl.ac.uk`, "Managed", brand "King's College London"). SP entityID: `https://kcl.idm.oclc.org/shibboleth`. | high (decoded AuthnRequest) |
| 4 | The UK-federation IdP for KCL is entityID **`https://kclidp.kcl.ac.uk/idp/shibboleth`**. Its SSO endpoints are **hosted by OpenAthens**: `https://login.openathens.net/saml/2/sso/kcl.ac.uk/c/ukfed`. OpenAthens then authenticates against the same Entra tenant (OpenAthens org id `71807903`). | high (UKfed metadata + probe) |
| 5 | **The OpenAthens redirector works for KCL**: `https://go.openathens.net/redirector/kcl.ac.uk?url=<enc>`. It turns DOI and publisher URLs into the correct WAYFless SAML deep link for Elsevier, Wiley, OUP, Springer, Nature, T&F, SAGE, NEJM, JAMA, BMJ, Karger, Thieme and Cambridge. **This should be the primary institutional route.** | high (probe) |
| 6 | KCL's EZproxy config is **small**. Of about 100 hosts probed, these are configured: `doi.org`, `dx.doi.org`, `www.bmj.com`, `www.webofscience.com`, `www.liebertpub.com`, `www.jneurosci.org`, `www.psychiatryonline.org`, `psychiatryonline.org`, `www.atsjournals.org`, `www.ajronline.org`, `www.jospt.org`, `www.jove.com`, `www.elgaronline.com`, `infoweb.newsbank.com`, `global.factiva.com`, `www.portal.euromonitor.com`, `institutions.newscientist.com`, `www.lrb.co.uk` and `libguides.kcl.ac.uk`. **These are not configured**: ScienceDirect, Wiley, OUP, LWW, Lancet, NEJM, JAMA, Springer, Nature, T&F, SAGE, Karger, Thieme and Cambridge. | high (probe) |
| 7 | When **no session exists** (or the session is invalid), a request to a configured proxied host returns `302 → https://login.kcl.idm.oclc.org/login?qurl=<target>`. An **unconfigured** proxied host returns `302 → https://login.kcl.idm.oclc.org/<same path>`, which then gives `404 Not found`. That 404 is the "host not configured" signal before login. | high (probe) |
| 8 | EZproxy's HttpsHyphens rule is **dots → single hyphens. Existing hyphens are kept and NOT doubled.** For example, `www-thieme-connect-de.<proxy>` decodes to `www.thieme-connect.de` on the Penn and Oxford EZproxies. EZproxy resolves the ambiguity by looking the name up in its config. | high (probe) |
| 9 | Licence terms (KCL guidance and the Jisc/Elsevier UK licence) forbid **systematic or bulk downloading** and robots that run **continuously and automatically**. OCLC-hosted EZproxy logs these default thresholds: >50 PDFs per 5 min, >150 per hour, >300 per 24 h. It blocks above 2 GB per hour, >4 countries per day or >20 IP addresses per hour. | medium (docs) |

---

## 1. How KCL tells users to access e-resources

Sources: KCL LibAnswers "How do I access eresources?" (https://libanswers.kcl.ac.uk/faq/226697, last updated 30 Sep 2026); "King's access to useful eresources" (https://libanswers.kcl.ac.uk/faq/229474); "What is LibKey Nomad?" (https://libanswers.kcl.ac.uk/faq/280440, last updated 19 May 2025).

- **Majority of resources: Shibboleth / "Institutional Login"** (also called "UK Access Federation"). The user picks *King's College London* and signs in with their K-number@kcl.ac.uk and password. (medium, KCL FAQ 226697)
- **Some resources use "a proxy server".** KCL tells users to start from Library Search, otherwise "the proxy access route will not work correctly". (medium, FAQ 226697)
- KCL's own links use the EZproxy form `login?url=` with an unencoded target. (high, FAQ 229474)
  - `https://login.kcl.idm.oclc.org/login?url=https://www.science.org/journal/science`
  - `https://login.kcl.idm.oclc.org/login?url=https://institutions.newscientist.com`
  - `http://login.kcl.idm.oclc.org/login?url=https://www.portal.euromonitor.com`
  - Factiva follows the same pattern.
  - The passwords page is itself proxied: `https://libguides-kcl-ac-uk.kcl.idm.oclc.org/passwords-for-e-resources`.
- For JAMA, Lancet, Nature and NEJM, KCL says to use the publisher's "sign in via your institution" option. The Lancet is linked via `https://www.sciencedirect.com/journal/the-lancet`. (medium, FAQ 229474)
- **LibKey Nomad** is KCL's supported browser extension. It is preinstalled on KCL machines and available from thirdiron.com/downloadnomad. (medium, FAQ 280440)
- **NHS staff** use NHS OpenAthens. The NHS England redirector domain is `nhs`, with IdP `https://idp.eng.nhs.uk/openathens` (see §4.4). (medium, KCL FAQ; high, probe)
- I found **no KCL guidance** on Zotero Connector proxy settings, bookmarklets or Lean Library. KCL is not a Lean Library site. (low: absence of evidence)
- The Library Search platform is Ex Libris Primo VE / Alma: `https://librarysearch.kcl.ac.uk/discovery/search?vid=44KCL_INST:44KCL_INST`. (high)

---

## 2. KCL EZproxy: observed facts

### 2.1 Identity
| Item | Value | Source / confidence |
|---|---|---|
| Login host | `login.kcl.idm.oclc.org` (`Server: EZproxy`) | probe, high |
| Proxy suffix | `kcl.idm.oclc.org` (wildcard DNS `*.kcl.idm.oclc.org` → `198.137.190.73`) | nslookup, high |
| Auth method | EZproxy Shibboleth/SAML2 module. It auto-POSTs an AuthnRequest to Entra. | probe, high |
| SAML SP entityID | `https://kcl.idm.oclc.org/shibboleth` (the `<saml:Issuer>` of the AuthnRequest) | decoded, high |
| IdP SSO URL | `https://login.microsoftonline.com/8370cf14-16f3-4c16-b83c-724071654356/saml2` | probe, high |
| Tenant check | `https://login.microsoftonline.com/kcl.ac.uk/v2.0/.well-known/openid-configuration` returns the same tenant GUID. `getuserrealm.srf` (queried with a placeholder address) gives `NameSpaceType: Managed`, `FederationBrandName: King's College London`. | probe, high |
| Is the EZproxy SP in UKfed metadata? | No (searched the 89 MB aggregate). It is registered directly in KCL's Entra tenant. | probe, high |
| robots.txt | `User-agent: * / Disallow: /` | probe, high |
| Logout | `GET https://login.kcl.idm.oclc.org/logout` → 200 "You are now logged out." | probe, high |
| Other public pages | `/encodeurl` (EZproxy URL-encoding helper, 200). `/Shibboleth.sso/Metadata` (200 stub). `/menu`, `/status`, `/loggedin/`, `/shibboleth` → 302 to `login?url=https://login.kcl.idm.oclc.org/<path>`. | probe, high |

### 2.2 Unauthenticated login page (what `login?url=` / `login?qurl=` returns)
`GET https://login.kcl.idm.oclc.org/login?url=https://doi.org/10.1038/nature12373` returns `200 OK`, `Server: EZproxy`, `Content-Type: text/html`, `Cache-Control: no-store`. **No Set-Cookie is sent before authentication.** The body is about 1.5 KB.

```html
<title>Shibboleth Authentication Request</title></head><body onload='EZproxyCheckBack()'>
<form name='EZproxyForm' method='POST' action='https://login.microsoftonline.com/8370cf14-…/saml2'>
<input type='hidden' name='RelayState' value='ezp.2cjAkaHR0cHM6Ly9kb2kub3JnLzEwLjEwMzgvbmF0dXJlMTIzNzM-'>
<input type='hidden' name='SAMLRequest' value='PD94bWwg…'>   <!-- base64 AuthnRequest, HTTP-POST binding -->
… <form name='EZproxyTrack'> … function EZproxyCheckBack() {… document.EZproxyForm.submit(); }
```
- The **RelayState** format is `ezp.` + `2` + base64(`r0$` + targetURL), using a modified alphabet: `=`→`-`, `/`→`.`, and probably `+`→`_`. Decoding it recovers the target exactly, including the query string. With no target, it is `ezp.2bWVudQ--` = base64(`menu`). (high, decoded)
- The same 200 page is returned for **any** target, configured or not (tested `https://www.example.com/`). Before login you **cannot** tell from `/login` whether a target host is configured. Use the proxied-hostname probe in §2.3 instead. (high)
- `http://login.kcl.idm.oclc.org/login?…` (plain http) returns the same 200 SAML page. (high)

### 2.3 Proxied-hostname behaviour (no session) — LOGIN / NOT-CONFIGURED signals
| Request | Response | Meaning |
|---|---|---|
| `GET https://doi-org.kcl.idm.oclc.org/10.1038/nature12373` | `302 Location: https://login.kcl.idm.oclc.org/login?qurl=https://doi.org%2f10.1038%2fnature12373`. Body: `To access this site, go <a href='…'>here</a>` | **Configured host, no or expired session** |
| `GET https://www-nature-com.kcl.idm.oclc.org/articles/nature12373` | `302 Location: https://login.kcl.idm.oclc.org/articles/nature12373`, which then gives `404` with body `<H1>404 Not found</H1>` | **Host not configured** in KCL's EZproxy |
| `GET https://DOI-ORG.kcl.idm.oclc.org/x` | `302 → https://doi-org.kcl.idm.oclc.org/x` (host lowercased) | canonicalisation |
| `GET https://doi.org.kcl.idm.oclc.org/…` (dotted, https) | `302 → https://doi-org.kcl.idm.oclc.org/…` (the TLS cert does not match the dotted name, so curl needed `-k`) | dotted → hyphen redirect |
| `GET http://doi.org.kcl.idm.oclc.org/…` (dotted, http) | `302 → https://login.kcl.idm.oclc.org/login?qurl=http://doi.org%2f…` | http origins are proxied in **dotted** form |
| `GET http://www-science-org.kcl.idm.oclc.org/` | `302 → http://kcl.idm.oclc.org/` | hyphen form over http is not valid |
| Bogus cookies `ezproxy=…; ezproxyl=…; ezproxyn=…` sent to the doi-org host | Same `302 → login?qurl=…` | an invalid or expired session looks the same as no session |

All 302s carry `Server: EZproxy` and `Cache-Control: no-store`. In the generated `qurl`, EZproxy leaves `scheme://host` raw and percent-encodes the path and query with **lowercase** hex (`%2f`, `%3f`, `%3d`, `%26`), e.g. `qurl=https://doi.org%2fx%3fa%3d1%26b%3d2`. (high)

### 2.4 Which publisher hosts are proxied (KCL, unauthenticated probe, 2026-10-20)
Method: `GET https://<host with . → ->.kcl.idm.oclc.org/<path>` with no redirects followed. If the response was `login?qurl=…`, the host is configured. If it was `login.kcl.idm.oclc.org/<path>`, it is not.

**Configured (→ `login?qurl=`):** `doi.org`, `dx.doi.org`, `www.bmj.com` (but not `bmj.com`, `gut.bmj.com` or `bestpractice.bmj.com`), `www.webofscience.com`, `www.liebertpub.com`, `www.jneurosci.org`, `www.psychiatryonline.org`, `psychiatryonline.org`, `www.atsjournals.org`, `www.ajronline.org`, `www.jospt.org`, `www.jove.com`, `www.elgaronline.com`, `infoweb.newsbank.com`, `global.factiva.com`, `www.portal.euromonitor.com`, `institutions.newscientist.com`, `www.lrb.co.uk`, `libguides.kcl.ac.uk`.

**Not configured (→ `login.kcl.idm.oclc.org/<path>` → 404):**
- The publishers in the brief: `www.sciencedirect.com`, `onlinelibrary.wiley.com`, `academic.oup.com`, `journals.lww.com`, `www.thelancet.com`, `www.nejm.org`, `jamanetwork.com`, `link.springer.com`, `www.nature.com`, `nature.com`, `www.tandfonline.com`, `journals.sagepub.com`, `karger.com`, `www.karger.com`, `www.thieme-connect.com`, `www.thieme-connect.de`, `www.cambridge.org`.
- Other hosts probed: `www.cochranelibrary.com`, `www.science.org` (even though KCL links it via `login?url=`), `www.cell.com`, `www.jstor.org`, `ovidsp.ovid.com`, `journals.ovid.com`, `www.embase.com`, `www.scopus.com`, `pubs.rsna.org`, `www.ahajournals.org`, `www.clinicalkey.com`, `www.uptodate.com`, `www.annualreviews.org`, `search.ebscohost.com`, `www.proquest.com`, `pubmed.ncbi.nlm.nih.gov`, `www.jbjs.org`, `www.neurology.org`, `www.pnas.org`, `pubs.acs.org`, `ieeexplore.ieee.org`, and others.

Implications:
- `doi.org` is proxied, so `https://login.kcl.idm.oclc.org/login?qurl=https%3A%2F%2Fdoi.org%2F<doi>` is a generic EZproxy entry point. After login, EZproxy follows doi.org's 302:
  - If the publisher host **is** configured (BMJ, ATS, AJR, Liebert, …), EZproxy rewrites `Location` to the proxied host.
  - If it is **not** configured, EZproxy "leaves [the URL] alone" (OCLC technical details page). The user lands on the plain publisher URL **without** institutional rights, unless they are on campus or already have a SAML session there.
  - (low: inferred from EZproxy semantics and the config probe; post-login behaviour was not observed)
- Most medical publishers at KCL therefore need the **SAML/OpenAthens** route (§4), not EZproxy.
- The KCL EZproxy config can change. Do not hard-code the list. Probe at runtime and cache (see §6.3).

---

## 3. Generic EZproxy mechanics

### 3.1 Starting-point URLs
- `https://<login-host>/login?url=<target>`: the **raw** target. **Everything after `url=` is taken as the target**, including later `&k=v`. Tested: `login?url=…&x=y` put `&x=y` inside the decoded RelayState target. (high, probe; OCLC "Starting point URLs and config.txt", medium)
- `https://<login-host>/login?qurl=<percent-encoded target>`: the target is decoded once. EZproxy uses this form in its own redirects, and the Zotero Connector generates it. OCLC troubleshooting: if an encoded target gives the menu page, use `qurl=` rather than `url=`. (high, probe; medium, OCLC KB "Why am I getting the EZproxy menu page when using an encoded URL"; Zotero forum thread 2025-05, adomasven: Zotero generates `qurl`)
- A full `encodeURIComponent(target)` in `qurl` was accepted. The decoded RelayState matched the target, including `?a=1&b=two words`. (high, probe)
- `connect?session=…&url=` also exists. Unauthenticated, it returns the same SAML page. Not needed. (high, probe)
- `/menu` is the post-login menu, which also appears when the target is invalid. (medium, OCLC)
- **Recommendation:** always enter with `login?qurl=` + `encodeURIComponent(targetURL)`. Never build `login?url=` for targets that have a query string.

### 3.2 Proxy-by-hostname and HttpsHyphens
- `Option HttpsHyphens` is the default for https sites proxied by hostname with a wildcard cert. Each `.` in the hostname becomes `-`, giving a single label under the proxy suffix: `https://www.somedb.com` → `https://www-somedb-com.ezproxy.yourlib.org`. `NoHttpsHyphens` keeps the dots (`www.somedb.com.ezproxy.yourlib.org`) and causes certificate warnings. The option is position-dependent per stanza. (medium, OCLC "Option HttpsHyphens")
- **Existing hyphens are NOT doubled** (high, empirical):
  - Penn: `www-thieme-connect-de.proxy.library.upenn.edu` → `login?qurl=https://www.thieme-connect.de%2f`. `www-oecd-ilibrary-org.proxy.library.upenn.edu` → `https://www.oecd-ilibrary.org`. The double-hyphen forms (`www-thieme--connect-de`, `www-oecd--ilibrary-org`) fell through as **not configured**.
  - Oxford (`ezproxy-prd.bodleian.ox.ac.uk`): same result for thieme-connect.
  - So the encoding is lossy. EZproxy decodes by matching the label against its configured hosts.
- **http origins** are proxied in **dotted** form (`http://doi.org.kcl.idm.oclc.org/…`). (high, probe)
- **Host case:** an upper-case label gets a 302 to the lower-case form. (high)
- **Explicit `:443`** on the proxied host is accepted. (high, Oxford probe)
- **Non-default ports:** internally EZproxy names them `p8080-www.somedb.com` (http :8080) or `s8443-www.somedb.com` (https :8443) before `ProxyHostnameEdit` rules run (medium, OCLC "ProxyHostnameEdit"). The public HttpsHyphens form for a non-default port is **unverified**: `s8443-doi-org` and `doi-org-443` were both "not configured" at KCL. **Never construct proxied hostnames for URLs with a non-default port. Use `login?qurl=` and let EZproxy build the name.**
- `ProxyHostnameEdit find replace` can rename generated hostnames (for example, shortening long names). A site may therefore use labels that are **not** derivable from the host, which is one more reason to prefer `login?qurl=`. (medium, OCLC)
- Zotero's own code assumes hyphens can be undone blindly, which breaks hosts like thieme-connect:
  - `.ref/omni-app/chrome/content/zotero/xpcom/translate/src/proxy.js:277` and connector `src/common/proxy.js:739`: `properURL.replace(/-/g, '.')` with the comment "Potentially troublesome with domains that contain dashes".
  - `.ref/omni-app/chrome/content/zotero/xpcom/proxy.js:513` (legacy desktop proxy): `toProxy` does `hostPort.replace(/-/g, '.')`, i.e. the inverse direction. This looks like a bug. **Do not reuse the legacy desktop `Zotero.Proxy` for rewriting.** (high, source)

### 3.3 Session cookies
- EZproxy sets **three cookies with the same session value** (medium, OCLC "EZproxy Cookies"):
  - `ezproxyn`: `SameSite=None; Secure; HttpOnly`
  - `ezproxyl`: `SameSite=Lax; HttpOnly`
  - `ezproxy`: classic, no attributes
  - `LoginCookieSameSite Classic|Lax|None` selects which of them are sent.
- `LoginCookieName` renames the base cookie (max 16 alphanumerics, default `ezproxy`). (medium, OCLC "LoginCookieName")
- `LoginCookieDomain` default `DomainName` uses the **parent domain** of the server name, e.g. `ezproxy.yourlib.org` → `.yourlib.org`. `HostName` gives `.ezproxy.yourlib.org`. `Manual` and `None` also exist. (medium, OCLC "LoginCookieDomain")
- For KCL the real names and domain are **unverified**: no cookie is set before login, and I did not log in. Expect `ezproxy`/`ezproxyl`/`ezproxyn` on `.kcl.idm.oclc.org` or `.idm.oclc.org`. (low) Plugin rule: **do not depend on cookie names. Detect the session by probing (§6.3).** Cookies are `HttpOnly`, so they are only visible via `Services.cookies` in chrome code, which a Zotero plugin can use. Use them as a hint only.
- Domain cookies set by publishers are kept **server-side** in the EZproxy session and are not sent to the browser. (medium, OCLC "EZproxy technical details")

### 3.4 Session lifetime and expiry
- `MaxLifetime` (ML): idle timeout, **default 120 minutes**. A session ends after ML minutes of inactivity or on `/logout`. (medium, OCLC "MaxLifetime" and "Set limits for your institution")
- Other limits: `MaxSessions` 500, `MaxConcurrentTransfers` 200, `MaxVirtualHosts` 200. These are server-wide and irrelevant for one user. (medium)
- **Expired or invalid session:** proxied host → `302 Location: https://<login-host>/login?qurl=<target>`. The login host then serves the 200 SAML auto-POST page. No 401 status and no inline login HTML on the proxied host. (high for an invalid cookie, by probe; medium for an expired one)
- KCL's actual `MaxLifetime` is unknown.

### 3.5 Error pages
Error pages are overridable `docs/*.htm` files (medium, OCLC "Error pages"):

| File | Sent when |
|---|---|
| `needhost.htm` | **starting-point URL for a host unknown to EZproxy** (after authentication) |
| `badhost.htm` | the hostname points at EZproxy but is unknown. If the file is absent, the user is redirected to the server's proper name. This matches the "unconfigured → 302 to login host" behaviour seen at KCL. |
| `logup.htm` | group not authorised |
| `ms.htm` | MaxSessions exceeded |
| `mv.htm` | MaxVirtualHosts exceeded |
| `limit.htm` | user login limit exceeded |
| `reject.htm` | RejectIP |
| `cookie.htm` | cookie could not be set |
| `https.htm` | wrong protocol |
| `suspend.htm` | **usage-limit suspension** (from `UsageLimit -enforce`) |

- Special sequences available to these pages: `^H` (host), `^U` (URL), `^L` (login URL).
- The default and KCL-customised text of `needhost.htm` was **not observed** because it needs a login. Detection rule: a 200 `text/html` from `login.kcl.idm.oclc.org/login…` **without** `name='EZproxyForm'` and without a redirect means needhost, suspend or another error page. Treat it as "EZproxy cannot serve this target" and stop. (low)

### 3.6 Un-proxying
- From a login URL: the target is the decoded `qurl` value, or the raw rest-of-query after `url=`.
- From a proxied host: strip the suffix and decode the label. Hyphens are ambiguous (§3.2).
- The Zotero Connector learns `toProperScheme` = `%h.<suffix>/%p` and `toProxyScheme` = `https://<login-host>/login?qurl=%u`. Its EZproxy detector requires `Server: EZproxy`, path `/login` and a `url|qurl` parameter. (high, `zotero-connectors/src/common/proxy.js:1014-1068`, commit `1531bd76be`, 2026-08-18)

---

## 4. KCL OpenAthens / UK federation (SAML) route

### 4.1 IdP facts
| Item | Value | Source / confidence |
|---|---|---|
| KCL IdP entityID (UK federation) | `https://kclidp.kcl.ac.uk/idp/shibboleth`. Registered by `http://ukfederation.org.uk` on 2010-04-22. Scope `kcl.ac.uk`. Entity categories: REFEDS R&S, SIRTFI. | UKfed metadata aggregate (`http://metadata.ukfederation.org.uk/ukfederation-metadata.xml`, 89 MB, fetched 2026-10-20); SeamlessAccess `md.seamlessaccess.org/entities/?q=…`; high |
| IdP SSO endpoints | `HTTP-Redirect: https://login.openathens.net/saml/2/sso/kcl.ac.uk/c/ukfed`. Shib1 AuthnRequest: `https://login.openathens.net/saml/1/sso/kcl.ac.uk/c/ukfed` | UKfed metadata, high |
| Host `kclidp.kcl.ac.uk` | resolves to `193.61.202.17` but did **not** answer on 443 or 80 from here. The legacy entityID is kept; OpenAthens serves it. | probe, high |
| Dev IdP (hidden) | `https://kclidpdev.kcl.ac.uk/idp/shibboleth` | SeamlessAccess, high |
| OpenAthens org | domain `kcl.ac.uk`, org id `71807903` (seen in `login.openathens.net/saml/2/sso/kcl.ac.uk/o/71807903/c/oafed` and `/auth/kcl.ac.uk/o/71807903`) | probe, high |
| Upstream auth | OpenAthens → `302 https://login.microsoftonline.com/8370cf14-…/saml2?SAMLRequest=…` (Entra, title "Sign in to your account") | probe, high |

### 4.2 Observed redirector chain (Nature example, GET-only)
```
[1] 302 go.openathens.net/redirector/kcl.ac.uk?url=https%3A%2F%2Fwww.nature.com%2Farticles%2Fnature12373   (server: oaws)
[2] 302 sp.nature.com/saml/login?idp=https%3A%2F%2Fkclidp.kcl.ac.uk%2Fidp%2Fshibboleth&targetUrl=…
[3] 302 secure.nature.com/saml2/authenticate/<b64(entityID)>?targetUrl=…        (sets JSESSIONID)
[4] 307 login.openathens.net/saml/2/sso/kcl.ac.uk/o/71807903/c/oafed?SAMLRequest=…   (sets oaxsrftkn, oacookie)
[5] 302 login.openathens.net/auth/kcl.ac.uk/o/71807903?t=%2Fsaml%2F2%2Fsso%2F…        (sets oaloginorg, oaselorg, oalastorg)
[6] 200 login.microsoftonline.com/8370cf14-…/saml2?SAMLRequest=…   title "Sign in to your account"
        (sets buid, esctx, fpc, x-ms-gateway-slice, stsservicecookie)
```
After the user signs in, Entra POSTs a SAMLResponse to OpenAthens, and OpenAthens POSTs to the publisher ACS. These hops use **HTTP-POST binding with auto-submit forms**, so the plugin needs a real browser context, not plain XHR (see §7).

### 4.3 Redirector behaviour
- Syntax: `https://go.openathens.net/redirector/<domain>?url=<percent-encoded target>`. The target must include `http(s)://`. (medium, OpenAthens docs "Create redirector links", "How the redirector works")
- Routing (medium, docs):
  1. A bypass IP goes direct.
  2. If an OpenAthens session exists, the user goes straight to the target.
  3. Otherwise the user logs in, with no WAYF step because the domain identifies the org.
  4. If both federated and proxied versions of a resource exist, federated wins.
- **First request for a URL that is not cached** returns `200 text/html`, title **"OpenAthens / Please wait..."** (high, probe). Its inline JS does:
  - `XMLHttpRequest HEAD` to the same URL every **2500 ms** (timeout 2500 ms);
  - on **`201`**, navigate to `getResponseHeader('location')`;
  - on error, `window.location.reload()`;
  - give up after **30 s**.
  - A later GET returns `302` + `Location` (`content-type: application/json`, header `x-openathens-id`).
  - **Plugin: emulate this with HEAD polling every 2.5 s for at most 30 s, accepting 201 or 302 + Location.**
- **Unknown resource** (e.g. any `www.thelancet.com/...`): `404`, title **"OpenAthens: 404"**. Body: "The OpenAthens redirector is not aware of the resource you attempted to visit…", plus a plain link to the target. The HEAD poll also returns 404. (high)
- **Unknown or invalid org domain** (`nhs.net`, `nhs.uk`, `kcl.ac.uk.invalid`): `403`. (high)
- **No SAML deep link known** (LWW: `journals.lww.com/...` and `doi.org/10.1097/SLA…`): `302` back to the **unchanged** target URL. The redirector is a no-op; access then depends on IP or an existing session. (high)
- **DOIs work**. The redirector resolves `https://doi.org/<doi>` server-side and returns the publisher's WAYFless URL (high):
  - `doi.org/10.1016/S0140-6736(20)30183-5` (Lancet) → `auth.elsevier.com/ShibAuth/institutionLogin?entityID=…&appReturnURL=https://linkinghub.elsevier.com/retrieve/pii/S0140673620301835`. **This is the way to reach the Lancet**, because thelancet.com URLs give the redirector 404.
  - `doi.org/10.1002/bjs.11646` → OUP SAMS Sigma (BJS moved to OUP).
- The OpenAthens session lasts up to about **8 hours** in one browser (medium, OpenAthens FAQ https://www.openathens.net/faq/what-is-the-inactivity-logoff-time; several library guides). Publisher sessions are separate and vary.

### 4.4 WAYFless deep-link patterns
These are the redirector outputs for KCL, captured 2026-10-20 and shown decoded. In the real URLs, the parameter values are percent-encoded and nested.

| Publisher | WAYFless URL emitted by redirector (`E` = `https://kclidp.kcl.ac.uk/idp/shibboleth`, `T` = target) |
|---|---|
| Elsevier / ScienceDirect / Lancet (via DOI) | `https://auth.elsevier.com/ShibAuth/institutionLogin?entityID=E&appReturnURL=T`. It answers 200 with an auto-POST form to `https://auth.elsevier.com/SHIRE/Login?SAMLDS=1&entityID=E`. |
| Wiley (Atypon) | `https://onlinelibrary.wiley.com/action/ssostart?idp=E&redirectUri=T` |
| T&F, SAGE, NEJM (Atypon) | `https://<host>/action/ssostart?idp=E&redirectUri=T`. T&F came back as `http://` and then 301 to https. |
| OUP (Silverchair + SAMS Sigma) | `https://oup-sp.sams-sigma.com/Shibboleth.sso/Login?entityID=E&target=https://oup-sp.sams-sigma.com/shib?dest=https://www.academic.oup.com/SHIBBOLETH?dest=T` |
| JAMA (Silverchair) | `https://jamanetwork.com/Shibboleth.sso/Login?entityID=E&target=T` |
| Springer (link.springer.com) | `https://fsso.springer.com/federation/init?entityId=E&returnUrl=T` → `fsso.springer.com/saml2/authenticate/<b64(E)>?returnUrl=…` |
| Nature | `https://sp.nature.com/saml/login?idp=E&targetUrl=T` |
| BMJ (HighWire) | `https://shibboleth.highwire.org/session/init?entityID=E&hw-shib-return-uri=T&subcode=bmjjournals&env=prod`. Node's TLS stack refused it with `ERR_SSL_DH_KEY_TOO_SMALL`; Gecko may also refuse it. Untested. |
| Karger | `https://karger.com/ExternalAuthentication/KeystoneDeeplink?entityId=E&destination=T` |
| Thieme | `https://profile.thieme.de/HTML/sso/ejournals/shibboleth.htm?federationid=http://ukfederation.org.uk&entityid=E&hook_url=T` |
| Cambridge | `https://shibboleth.cambridge.org/Shibboleth.sso/discovery?SAMLDS=1&target=https://shibboleth.cambridge.org/CJOShibb2/index?app=https://www.cambridge.org/core/shibboleth?ref=T&entityID=E` |
| LWW (journals.lww.com) | no deep link: the redirector passes T through unchanged |
| The Lancet (thelancet.com URL) | redirector 404. Use the DOI, which goes to the Elsevier pattern. |

- Many of these publisher endpoints answer non-browser clients with **Cloudflare challenges**: `403`, title "Just a moment...", `<meta http-equiv="refresh" content="360">`. This happened for Wiley, T&F, SAGE, JAMA, Karger, LWW and BMJ (www). They need a JS-capable browser context. Zotero's `Zotero.BrowserRequest` already handles some of these hosts (`.ref/omni-app/chrome/content/zotero/xpcom/browserRequest.js:60-90`). (high)
- The redirector is maintained by OpenAthens, and the patterns drift. **Use the redirector itself rather than re-implementing the table.** Cache its `Location` answers per (domain, target) for at most a day as an optimisation only.
- NHS England: `https://go.openathens.net/redirector/nhs?url=…` → entityID `https://idp.eng.nhs.uk/openathens`. (high, probe)

### 4.5 Library entitlement oracle: Alma uresolver (no key, no login)
`GET https://librarysearch.kcl.ac.uk/view/uresolver/44KCL_INST/openurl?ctx_ver=Z39.88-2004&url_ver=Z39.88-2004&rft_id=info:doi/<DOI>&svc_dat=CTO` returns `200 text/xml` (Alma ContextObject, about 20 KB). (high, probe)
- One `<context_service service_type="getFullTxt" context_service_id="…">` per entitled portfolio. Its `<key id="…">` children include `package_public_name`, `interface_name`, `Availability` (coverage text), `Is_free` (0/1), `proxy_enabled` (`false` for every service seen, BMJ included), `proxy_selected`, `parser_program`, `portfolio_PID`, and `<resolution_url>`.
- Example: Nature 10.1038/nature12373 has 5 services: Nature, ProQuest Central UK/Ireland, Springer Nature JISC Nature TA 2024, Free E-journals, BHL. BMJ 10.1136/bmj.m1091 has 4: JSTOR, "at publisher website", 2× ProQuest.
- `resolution_url` = `https://kcl.alma.exlibrisgroup.com/view/action/uresolver.do?operation=resolveService&package_service_id=<id>&institutionId=6881&customerId=6880`. GET → 302 to the platform's OpenURL (e.g. `https://www.nature.com/openurl?id=doi:…&genre=article`). Alma did **not** wrap these targets in EZproxy.
- Use: decide before any login whether KCL is entitled at all, and on which platform (publisher vs aggregator). If there are no `getFullTxt` services, skip the institutional route and save the user a login. Keep it to 1 request per item.
- The same base is used by LibKey for KCL: `linkResolverUrlBase`.

---

## 5. Other routes

| Route | Usable by plugin? | Notes / confidence |
|---|---|---|
| **OpenAthens redirector** (`go.openathens.net/redirector/kcl.ac.uk?url=`) | **Yes, primary.** No key needed. | §4; high |
| **EZproxy** (`login.kcl.idm.oclc.org/login?qurl=`) | **Yes, secondary.** Use for configured hosts, or as a generic doi.org entry. | §2; high |
| **WAYFless built by hand** from §4.4 patterns + `E` | Possible, but duplicates the redirector and breaks when publishers change. Use only as a fallback if the redirector is down. | high (patterns observed) |
| **LibKey API** `https://public-api.thirdiron.com/public/v1/libraries/{id}/articles/doi/{doi}?access_token=KEY` | **No** unless KCL or Third Iron issues a key. Keys are library-specific or integration-partner keys, obtained from support@thirdiron.com. The request without a key returned `{"status":401}`. Test library id 1390. | medium (Third Iron docs); high (401 probe) |
| **LibKey library metadata** `https://api.thirdiron.com/v2/libraries` (public JSON:API, about 18 MB, 3199 libraries) | Yes, read-only. KCL = **id 286**. Subscriptions include libkey, nomad and nomadAddon. `linkResolverUrlBase` = Alma above. `libkeyFallbackTemplate` = `…/openurl/44KCL_INST/44KCL_INST:44KCL_INST?id=doi:{suspected_doi}`. `ssoEnabled=false`, `hasVPN=false`. | high (probe) |
| **libkey.io link** `https://libkey.io/libraries/286/<doi>` | Only as an "Open in browser" button. It is an SPA, so do not scrape it. | high (200 SPA) |
| **Lean Library** | KCL does not appear to subscribe. Skip. | low |
| **Zotero Connector proxies** | Stored in the browser extension, **not readable** from Zotero desktop. The legacy desktop `proxies` table (`xpcom/proxy.js:46`, served at `/connector/proxies`, `server_connector.js:1062-1074`) is normally empty in modern installs. The plugin needs its **own** institution settings. The Connector does support OpenAthens redirector proxies (`Zotero.Proxy.OpenAthensProxy`, connector `proxy.js:915`; 12 h re-redirect interval, `proxy.js:42`). | high (source) |

---

## 6. Algorithms for the Rust core (pure, no I/O)

### 6.1 Institution config (one JSON blob, editable in prefs)
```json
{
  "name": "King's College London",
  "openathens": { "domain": "kcl.ac.uk" },
  "ezproxy": { "loginHost": "login.kcl.idm.oclc.org", "suffix": "kcl.idm.oclc.org" },
  "saml": { "idpEntityId": "https://kclidp.kcl.ac.uk/idp/shibboleth",
            "entraTenant": "8370cf14-16f3-4c16-b83c-724071654356" },
  "alma": { "base": "https://librarysearch.kcl.ac.uk", "inst": "44KCL_INST" },
  "libkeyLibraryId": 286
}
```

### 6.2 Rust signatures (wasm-bindgen friendly)
```rust
pub struct Ezproxy { login_host: String, suffix: String }   // lowercase, no trailing dot

/// Entry URL. Always qurl= + full percent-encoding (encodeURIComponent semantics; encode '+', '&', '#', '?', '=').
pub fn login_url(cfg: &Ezproxy, target: &str) -> String;     // "https://{login_host}/login?qurl={enc}"

/// Direct proxied URL (only when it is safe), else None -> caller uses login_url().
pub fn proxify_host(cfg: &Ezproxy, target: &url::Url) -> Option<url::Url>;

pub enum Unproxied { Exact(url::Url), Ambiguous { best: url::Url, label: String }, NotProxied }
pub fn unproxify(cfg: &Ezproxy, u: &url::Url, known: &HostMap) -> Unproxied;

/// Learned label -> host map, fed from every observed 302 "…/login?qurl=" (EZproxy's own decoding).
pub struct HostMap { /* hyphen_label -> real host */ }
pub fn learn_from_redirect(cfg: &Ezproxy, from: &url::Url, location: &str, map: &mut HostMap);

pub fn redirector_url(oa_domain: &str, target: &str) -> String; // "https://go.openathens.net/redirector/{domain}?url={enc}"
pub fn classify(resp: &RespMeta) -> AccessState;               // §6.4
```

### 6.3 `proxify_host` (exact)
```
input: target URL t
1. If t.host is login_host or ends with "."+suffix: return Some(t)   // already proxied
2. If t.scheme == "https":
     a. If t.port is Some(p) and p != 443: return None                // port form unverified -> use login_url
     b. h = lowercase(t.host)   (IDN: use the ASCII/punycode form)
     c. If h is an IP literal: return None
     d. label = h.replace('.', '-')          // existing '-' kept as-is (NOT doubled)
     e. return https://{label}.{suffix}{t.path}{?t.query}{#t.fragment}
3. If t.scheme == "http":
     a. If t.port is Some(p) and p != 80: return None
     b. return http://{h}.{suffix}{path...}    // dotted form; EZproxy upgrades to login over https
4. else None
```
Notes:
- A constructed hostname only helps when a session already exists **and** the host is configured. Otherwise you get a 302 to login (configured) or a 302 to the login host with the same path, then a 404 (not configured).
- Runtime "is host configured?" probe: `GET proxify_host(https://<host>/)` without following redirects, then:
  - `Location` matches `^https://{login_host}/login\?(q)?url=` → **configured** (no session).
  - `Location` = `https://{login_host}/<path>` with a path other than `/login…` → **not configured**.
  - 200 or a redirect to a non-login host → **configured and session valid**.
  - Cache per host for about 7 days.

### 6.4 `unproxify` (exact)
```
input: u
A. host == login_host:
   - path (case-insensitive) in {"/login", "/connect"}:
       * raw query has key "qurl" -> return Exact(parse(percent_decode_once(value)))
       * raw query contains "url=" at start or after '&' -> take EVERYTHING after the first "url=" verbatim
         (do not split on '&'); if it doesn't parse, try percent_decode_once; return Exact
   - otherwise -> NotProxied (EZproxy's own page)
B. host ends with "." + suffix:
   label = host without ("." + suffix);  if label == "login" -> NotProxied
   - scheme http, or label contains '.'  -> real host = label (dotted form)   -> Exact
   - scheme https and no '.' in label:
       1. known.get(label)                           -> Exact  (learned from qurl redirects or our own proxify calls)
       2. static table of hyphenated publisher hosts -> Exact  (e.g. www.thieme-connect.de/.com,
          www.oecd-ilibrary.org, www.pep-web.org, www.wiley-vch.de, www.e-elgar.com, www.jbe-platform.com,
          www.un-ilibrary.org, wrds-www.wharton.upenn.edu)
       3. fallback: label.replace('-', '.')          -> Ambiguous{best}  (what Zotero does)
   keep path, query, fragment unchanged; drop port
C. else NotProxied
```
Edge cases and test vectors (from probes):
- `https://doi-org.kcl.idm.oclc.org/10.1038/nature12373` → `https://doi.org/10.1038/nature12373`
- `https://login.kcl.idm.oclc.org/login?qurl=https://doi.org%2fx%3fa%3d1%26b%3d2` → `https://doi.org/x?a=1&b=2`
- `https://login.kcl.idm.oclc.org/login?url=https://www.bmj.com/x?a=1&b=2` → `https://www.bmj.com/x?a=1&b=2`
- `https://www-thieme-connect-de.ezproxy-prd.bodleian.ox.ac.uk/a` → `https://www.thieme-connect.de/a`. This is Exact only via the table or learned map; the naive fallback gives `www.thieme.connect.de`.
- `http://doi.org.kcl.idm.oclc.org/x` → `http://doi.org/x`
- `https://WWW-BMJ-COM.kcl.idm.oclc.org/` → lowercase first → `https://www.bmj.com/`
- A double-hyphen label such as `www-thieme--connect-de` is **not** produced by EZproxy. Treat it as a literal label: fallback gives `www.thieme..connect.de`, which is invalid. Return Ambiguous and log it.
- `https://login.kcl.idm.oclc.org/menu` / `/logout` → NotProxied
- Also unwrap OpenAthens: `go.openathens.net/redirector/<d>?url=<enc>` → decode `url` once (connector does this at `proxy.js:1139-1148`).

### 6.5 `classify(resp)`: login and state detection
Input: request URL, status, the `Location` header, `Server`, `Content-Type`, and the first about 8 KB of the body. Check the rules in this order:

| State | Signals |
|---|---|
| `Pdf` | `Content-Type: application/pdf` or body starts with `%PDF-` |
| `EzproxyNeedLogin` | (a) 3xx from `*.{suffix}` (not the login host) with `Location` matching `^https?://{login_host}/login\?(q?url)=`. (b) 200 from `{login_host}` with path `/login`, and body contains `name='EZproxyForm'` or `<title>Shibboleth Authentication Request</title>` or `name='RelayState' value='ezp.`. (c) 3xx from `{login_host}/<x>` to `login?url=https://{login_host}/<x>`. |
| `EzproxyHostNotConfigured` | 3xx from `X.{suffix}` (X ≠ login) with `Location == https://{login_host}{same path+query}` (no `/login`). Also a 404 from `{login_host}` with body `<H1>404 Not found</H1>`. |
| `EzproxyErrorPage` (needhost/suspend/limit…) | 200 `text/html` from `{login_host}/login…` with **no** `EZproxyForm` and no redirect. Body unverified. Stop and show it to the user. |
| `IdpLogin` (user must interact) | host `login.microsoftonline.com` (path `/<tenant>/saml2`, `/common/…`, `/kmsi`, `/login.srf`), `login.live.com`, `login.microsoft.com`, `device.login.microsoftonline.com`. Title "Sign in to your account". Cookies `esctx`/`buid`/`fpc`. Also `login.openathens.net` with path starting `/auth/` (org login page), or `kclidp.kcl.ac.uk`. |
| `SamlAutoPost` (needs a browser to continue) | HTML containing a `<form method="POST">` with input `SAMLRequest` or `SAMLResponse` (often also `RelayState`) and `onload=…submit()`. Examples: EZproxy login page, `auth.elsevier.com/ShibAuth/institutionLogin` (form → `/SHIRE/Login`), OpenAthens → publisher ACS. |
| `OpenAthensWait` | host `go.openathens.net`, 200, title `OpenAthens / Please wait...` → HEAD-poll (§4.3) |
| `OpenAthensUnknownResource` | host `go.openathens.net`, 404, title `OpenAthens: 404` |
| `OpenAthensBadOrg` | host `go.openathens.net`, 403 |
| `PublisherSsoHop` | 3xx to a known SP-initiated SSO path: `/Shibboleth.sso/Login`, `/Shibboleth.sso/discovery`, `/action/ssostart`, `auth.elsevier.com/ShibAuth`, `fsso.springer.com`, `sp.nature.com/saml`, `secure.nature.com/saml2`, `oup-sp.sams-sigma.com`, `shibboleth.highwire.org/session/init`, `profile.thieme.de/HTML/sso`, `karger.com/ExternalAuthentication`, `shibboleth.cambridge.org` |
| `BotChallenge` | 403/503 + `Server: cloudflare` + title `Just a moment...` (or header `cf-mitigated: challenge`). Hand off to the Zotero `BrowserRequest` path, not the login path. |
| `Html` | anything else. Feed to the landing-page → PDF-link extractor. |

A "logged-in" check that sends no credentials:
- **EZproxy:** `GET https://doi-org.kcl.idm.oclc.org/10.1000/1` (or any cheap DOI) without following redirects. A `Location` to `login?qurl` means no session. A `Location` to a proxied or publisher host means a session exists.
- **OpenAthens:** there is no cheap check. The Connector re-runs the redirector every 12 h (`OPENATHENS_REDIRECT_INTERVAL`, connector `proxy.js:42`). For the plugin: run the redirector in a hidden browser. If it reaches `IdpLogin`, show the window to the user.

---

## 7. How the JS shell should run the institutional flow (summary)

1. **OA first** (other briefs).
2. **Direct fetch** of the publisher landing page or PDF with Zotero's cookie jar. This covers campus IP and any SAML session the user already has with that publisher.
3. **Alma CTO check** (optional, 1 GET). If there are no `getFullTxt` services, mark the item "not held by KCL" and skip steps 4–5.
4. **OpenAthens redirector**, with target `https://doi.org/<doi>` (preferred) or the item URL:
   - Load it in a **hidden browser** (Zotero `HiddenBrowser`, as `Zotero.BrowserRequest` does) so that redirects, SAML auto-POST forms and Cloudflare JS all run.
   - If the page settles on `IdpLogin`, open a **visible** Zotero browser window at that URL for the user to sign in (Entra, possibly MFA). Wait until the window's URL leaves the IdP hosts and reaches the publisher.
   - Then hand the final publisher page to the PDF-link extractor. Fetch the PDF with the same cookie context (default `userContextId`, so cookies are shared with `Zotero.HTTP.request`).
5. **EZproxy fallback** for hosts that are known to be configured, or a doi.org entry: `login?qurl=https://doi.org/<doi>`. Use the same hidden-then-visible window logic. The login completes on `login.kcl.idm.oclc.org` and then redirects to `*.kcl.idm.oclc.org`.
6. Store the attachment's source URL **un-proxied** (§6.4). Never store `login?…`, `go.openathens.net/…` or SAML URLs.

Never put user identifiers in URLs. The redirector and EZproxy only receive the target URL.

---

## 8. Licence and ToS constraints, and limits the plugin should enforce

### 8.1 What the rules say
- **KCL** (FAQ 226697, "Using eresources"; medium, paraphrased):
  - Use is for private study, teaching and research only. No commercial use.
  - You may save or print **a single copy** of an article or chapter.
  - Downloading large amounts, **manually or with robots/crawlers**, is treated as excessive and a breach of licence.
  - Do not share copies or post content online.
  - Do not copy more than one article from the same journal issue.
  - Publishers who detect unusual activity may **block access**.
- **Jisc / Elsevier ScienceDirect UK licence 2022–2025** (based on the PA/Jisc model licence; redacted copy at infodocket.com; medium; clause numbers from that text):
  - 4.1.7: no downloading "in a systematic or regular manner" or to build a collection of a material subset.
  - 4.1.9: no robots, spiders or crawlers that run "continuously and automatically" to search, scrape, extract, deep link or index.
  - 4.1.10: no substantial or systematic local storage or redistribution.
  - 4.1.4 and 4.1.6: no distribution beyond Authorised Users.
  - 7.3: on unauthorised use the publisher may **suspend access** at its source.
  - The current (2025+) agreement was not checked. Assume it is similar.
- **OCLC-hosted EZproxy default security rules** (medium, OCLC "security rules explained"). KCL may have changed them.

  | Action | Rule | Trigger |
  |---|---|---|
  | **block** | bytes | > 2 GB in 60 min |
  | **block** | countries | > 4 in 24 h |
  | **block** | IP addresses | > 20 in 60 min |
  | log | bytes | > 1 GB in 60 min |
  | log | PDF bytes | > 500 MB in 60 min or in 24 h |
  | log | **PDF downloads** | **> 50 in 5 min**, **> 150 in 60 min**, **> 300 in 24 h** |
  | log | failed logins | > 10 in 60 min |
  | log | IP addresses | > 10 in 60 min or in 24 h |
  | log | countries | > 2 in 24 h |

- `UsageLimit -enforce` can **suspend** a user (`suspend.htm`) by MB or by transfers per interval. The default interval is 1440 min. (medium, OCLC "UsageLimit")
- The KCL EZproxy `robots.txt` disallows everything. That is aimed at crawlers. An on-demand, user-initiated download for a selected item is the case the licences allow. (high, probe)

### 8.2 Recommended hard limits (applied to the institutional routes; OA routes can be looser)
| Limit | Value | Rationale |
|---|---|---|
| Trigger | **User-initiated only** (context menu on the selected items). No auto-fetch on item add, no background or scheduled runs via EZproxy/OpenAthens. | licence 4.1.9 "continuously and automatically" |
| Batch size | ≤ **20** items per invocation via institutional routes. Ask for confirmation above 10. | "systematic" / collection-building |
| Concurrency | **1** in-flight institutional request per publisher host; **2** globally | polite; one session |
| Pacing | ≥ **5 s** (+0–3 s jitter) between PDF downloads to the same host; ≥ **2 s** between page fetches | stays far below 50 PDFs/5 min |
| Hourly cap | ≤ **30** institutional PDFs/hour | well below OCLC 150/h |
| Daily cap | ≤ **80** institutional PDFs in a rolling 24 h (persisted in prefs) | well below OCLC 300/24 h |
| Bytes | stop at **300 MB/hour** via the proxy | below the 500 MB PDF log rule |
| Per-item fetch budget | ≤ **4** HTTP fetches (landing → PDF link → PDF, plus 1 retry). No spidering beyond the item's own pages. | no crawling |
| Retries | No automatic retry on 403/429/challenge from a publisher. Honour `Retry-After`. Back off ≥ 30 min after any block signal and **abort the batch**. | avoid escalating blocks |
| Block signals → stop | `EzproxyErrorPage`, any page containing "unusual activity", "excessive", "suspended" or "systematic", HTTP 429, repeated `BotChallenge` | |
| Identity | Use Zotero's normal UA (or a plain Firefox UA only where Zotero itself does, `browserRequest.js` `PLAIN_UA_HOSTS`). No UA rotation, no proxies or VPN hopping, never run on a server. | OCLC IP/country rules |
| Sharing | Warn when saving into a **group library**. The licence forbids giving copies to non-Authorised Users. | licence 4.1.4/4.1.6 |
| TDM | Do not offer bulk or "whole collection" modes. No full-text extraction for TDM via institutional routes. | licence |

---

## 9. Open questions / unverified
1. EZproxy behaviour **after** login was not observed (by design). Unverified items:
   - the needhost page text;
   - the session cookie names and domain (`ezproxy*` on `.kcl.idm.oclc.org` vs `.idm.oclc.org`);
   - whether the doi.org → unconfigured-publisher hop really drops out of the proxy;
   - KCL's `MaxLifetime` and UsageLimit settings.
2. Does Entra Conditional Access at KCL allow sign-in inside Zotero's embedded Gecko browser? It might require a compliant device or block "embedded" user agents. This needs a manual test by the user.
3. The BMJ HighWire SSO host uses weak DH parameters (Node refused it). Whether Gecko 140 accepts it is untested.
4. The public HttpsHyphens hostname form for non-default ports is unverified.
5. KCL's EZproxy host list is only partly known (about 20 hosts confirmed). Learn it at runtime.
6. Whether KCL has LibKey API keys it would share (library 286). Ask KCL Library (library@kcl.ac.uk) if an API route is wanted.
7. The current Jisc–Elsevier agreement (2025+) wording.

---

## 10. Sources

**Empirical (2026-10-20):** curl/Node probes from this machine. The scripts are in the scratch folder `C:\Users\abora\AppData\Local\Temp\claude\probe\scripts\` (`probe.sh`, `raw.sh`, `hops.mjs`).

**Local Zotero 10.0.6 source:**
- `.ref/omni-app/chrome/content/zotero/xpcom/proxy.js:183-234, 458-526` (legacy desktop proxies; line 513 inverse-hyphen bug)
- `.ref/omni-app/chrome/content/zotero/xpcom/translate/src/proxy.js:44-100` (`getPotentialProxies`, TLD heuristic incl. HttpsHyphens), `:114-149` (blacklist), `:250-282` (`toProper`), `:296-325` (`toProxy`, `.`→`-`)
- `.ref/omni-app/chrome/content/zotero/xpcom/browserRequest.js:60-90` (challenge hosts, `PLAIN_UA_HOSTS`)
- `.ref/omni-app/chrome/content/zotero/xpcom/attachments.js:2254` (ProQuest EZproxy intermediate redirect)
- `.ref/omni-app/chrome/content/zotero/xpcom/server/server_connector.js:1062-1074` (`/connector/proxies`)
- `.ref/omni-app/defaults/preferences/zotero.js:1775-1779` (`proxies.*` prefs)

**Zotero Connector** (GitHub `zotero/zotero-connectors`, `src/common/proxy.js` @ `1531bd76be`, 2026-08-18): lines 42, 739, 771, 779, 915-1000 (OpenAthensProxy), 1014-1068 (EZProxy detector), 1139-1160 (OpenAthens detector).

**Zotero docs and forum:** https://www.zotero.org/support/connector_preferences ; https://forums.zotero.org/discussion/comment/492872/ (`qurl` vs `url`, May 2025).

**KCL:** https://libanswers.kcl.ac.uk/faq/226697 ; https://libanswers.kcl.ac.uk/faq/229474 ; https://libanswers.kcl.ac.uk/faq/280440

**OCLC EZproxy:**
- https://help.oclc.org/Library_Management/EZproxy/Configure_resources/Option_HttpsHyphens
- …/Troubleshooting/EZproxy_Cookies
- …/Configure_resources/LoginCookieDomain
- …/Configure_resources/LoginCookieName
- …/Configure_resources/MaxLifetime_ML
- …/EZproxy_configuration/Set_limits_for_your_institution
- …/Manage_EZproxy/Error_pages
- …/Configure_resources/UsageLimit
- …/Troubleshooting/OCLC_EZproxy_security_rules_explained
- …/Configure_resources/ProxyHostnameEdit
- …/Get_started/EZproxy_technical_details
- …/EZproxy_configuration/Starting_point_URLs_and_config_txt
- …/Troubleshooting/Why_am_I_getting_the_EZproxy_menu_page_when_using_an_encoded_URL_as_the_target_URL

**OpenAthens:** https://docs.openathens.net/libraries/how-the-redirector-works ; https://docs.openathens.net/libraries/create-redirector-links ; https://docs.openathens.net/tpa/oclc-worldshare-management-service-and-the-openathens-redirector ; https://www.openathens.net/faq/what-is-the-inactivity-logoff-time

**Federation:** http://metadata.ukfederation.org.uk/ukfederation-metadata.xml ; https://md.seamlessaccess.org/entities/?q=king%27s%20college%20london

**Third Iron / LibKey:** https://thirdiron.atlassian.net/wiki/spaces/BrowZineAPIDocs/pages/2930671617 ; https://api.thirdiron.com/v2/libraries (KCL = 286)

**Licence:** https://www.infodocket.com/wp-content/uploads/2022/03/ScienceDirect_redacted-licence_2022_2025.pdf (clauses 4.1.4–4.1.10, 7.3)
