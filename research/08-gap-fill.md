# 08: Gap fill for B1 (KCL single sign-on inside Zotero 10)

**Date:** 2026-10-20. **Zotero:** 10.0.6 (Gecko 140.17.0 ESR), Windows 11.
**Method:** source reading (`.ref/omni-app`, `.ref/omni-gre`, Gecko ESR140 on GitHub), official
docs, and two runs of a new probe plugin in throwaway Zotero instances through
`tools/zotero-harness/run.ps1`. The probe is `tools/sso-spike/`.
**Rules kept:** nobody logged in, no credentials were entered, and no SSO flow was completed. In
the automated runs, every non-GET request to an identity-provider host was cancelled inside
Zotero before it left the process. The user's Zotero, profile and data directory were not
touched: the harness confirmed the user's PIDs were still alive and the ini hashes unchanged.

Confidence key: **high** = verified in source or empirically. **medium** = official docs.
**low** = inferred.

Raw data:
- `tools/sso-spike/results/sso-auto-20261020-155444.json`: run 2, the primary run.
- `tools/sso-spike/results/sso-auto-20261020-154345.json`: run 1. Its page inspection was
  broken (see section 3.7), but its network data is valid.
- `tools/sso-spike/results/sso-smoke-*.json`: the validation of the manual mode (section 4.2).

---

## 0. Verdict

1. **B1 cannot be closed without a real KCL sign-in.** The no-login rule forbids that for
   Claude. B1 is now down to one **20-30 minute run by the user** with a ready kit:
   `pwsh -NoProfile -File tools\sso-spike\run-manual.ps1`. A headless smoke test with no human
   and no login has exercised every code path of that kit (section 4).
2. **Everything up to the password prompt works in Zotero 10.0.6, in both a custom XHTML window
   and `Zotero.openInViewer`.** *(high, empirical)*
   - The OpenAthens redirector chains for Nature, Elsevier (Lancet via DOI) and Wiley reach
     KCL's Entra page: `login.microsoftonline.com/8370cf14-…/saml2`, title "Sign in to your
     account". The page has the `loginfmt` and password inputs and KCL's own branding text.
   - The page shows **no unsupported-browser message**. This held with Zotero's UA and with a
     plain Firefox UA.
   - The EZproxy `login?qurl=` page loads in both window types, and its auto-submit script fires.
     The guard cancelled the resulting POST to Entra's SAML endpoint.
3. **New blocker found and fixed in the design: Cloudflare Turnstile at Wiley's SSO start
   rejects Zotero's default User-Agent.** *(high, empirical)*
   - With the default UA (`… Firefox/140.0 Zotero/10.0.6`), the redirector's hop to
     `onlinelibrary.wiley.com/action/ssostart` stayed on the 403 "Just a moment..." page in both
     window types. It did not clear within the 15-30 s observed in any of 4 runs, so login never
     started.
   - With `Zotero.VersionHeader.getPlainFirefoxUA()` set as the browsing context's
     `customUserAgent`, the challenge cleared with no interaction: in about 2-6 s in 4 runs, and
     after about 130 s in a later smoke-test run. Cloudflare decisions are risk-based and vary
     from run to run. The flow then continues through Wiley's ScienceConnect OIDC hop and
     `iam.atypon.com` to OpenAthens and Entra.
   - So **every login window and every follow-up download to a Cloudflare-fronted host should
     use the plain Firefox UA**. The login window must also stay visible and interactive, so the
     user can click a Turnstile checkbox when Cloudflare escalates. This matches Zotero's own
     comment and workaround (`xpcom/browserRequest.js:125-127`, `:71-76`, `:674-679`).
4. **Cross-site cookie sharing now holds with real servers, not just the local simulation.**
   *(high, empirical)*
   - Cookies set by real hosts during top-level cross-site redirects in a login window go into
     the default, **unpartitioned** jar: `secure.nature.com` `JSESSIONID`;
     `login.openathens.net` `oacookie` and `oaloginorg`; Entra `buid`, `fpc` and `esctx`.
   - They are then sent by `Zotero.HTTP.request`, by a `HiddenBrowser` document load, and by
     `fetch(…, {credentials:'include'})`. A plain `fetch()` does not send them.
   - Only cookies set inside third-party iframes are partitioned. In these runs that was Entra's
     Seamless-SSO iframe on `autologon.microsoftazuread-sso.com`, with partition key
     `(https,microsoftonline.com)`.
5. **No Gecko-level blocker for the SAML POST-back.** *(high: runtime prefs and Gecko source)*
   - `network.cookie.sameSite.laxByDefault=false` and `noneRequiresSecure=true`.
   - `privacy.bounceTrackingProtection.mode=3` (`MODE_ENABLED_DRY_RUN`): it classifies but never
     purges.
   - `cookieBehavior=5` (dFPI) only partitions third-party contexts.
   - So OpenAthens' `oacookie` (no SameSite attribute) is sent on Entra's cross-site
     SAMLResponse POST, exactly as in desktop Firefox.
6. **Device-based Conditional Access is the main residual risk.**
   - Microsoft supports Firefox 91+ for device policies only through "Windows SSO". That is
     Gecko pref `network.http.windows-sso.enabled`, implemented in core networking
     (`nsHttpChannel.cpp:476-491`), so it exists in Zotero. **It defaults to `false`.**
     *(high, source and runtime)*
   - MFA-only policies "work with all browsers" *(medium, Microsoft docs)*.
   - KCL documents MFA through Microsoft Authenticator, SMS or phone call. None of these needs a
     browser feature that Zotero lacks *(medium, KCL page; low for the "none needs" inference)*.
   - Whether KCL applies a device-compliance policy to the OpenAthens or EZproxy enterprise apps
     is unknowable without signing in. The user's run answers it: an Entra error page with code
     53000 or similar means "device not compliant".
7. **`Zotero.openInViewer` is the wrong window for credentials.** *(high, source)*
   - It has no address bar or TLS indicator (`standalone/basicViewer.xhtml:245-256`).
   - Plain `<a>` clicks go to the **system browser**: the `ExternalLinkHandler` actor is
     registered for `messagemanagergroup="basicViewer"` (`actors/ActorManager.mjs:51-61`,
     `ExternalLinkHandlerChild.mjs`).
   - `window.open` and `target=_blank` content goes into a **hidden** `<browser>`
     (`browserWindowShim.js:25-31`).
   - The plugin should ship its own login window. `tools/sso-spike/plugin/content/login.xhtml`
     is a working template with an origin and TLS bar and visible popups.

---

## 1. What B1 asks, and what can be shown without logging in

B1 asks two things. Can a KCL user finish the OpenAthens to Entra (MFA, Conditional Access) to
publisher ACS flow, and the EZproxy `login?qurl=` to Entra flow, inside a custom XHTML
`<browser>` window and inside `Zotero.openInViewer`? And afterwards, do `Zotero.HTTP.request` and
a HiddenBrowser with PDF MIME capture download the entitled PDF?

| Part of B1 | Testable without login? | Status |
|---|---|---|
| Redirect chain to the IdP renders in each window type, with each UA | yes | **done** (section 3) |
| Entra sign-in page accepts this browser (no unsupported-browser page, form present) | yes | **done** |
| Cookie storage across a cross-site SAML chain (jar, partitioning, SameSite) | partly (pre-login hops) | **done** for all hops up to Entra |
| Cookies from login windows reused by HTTP.request / HiddenBrowser / fetch | yes (pre-login cookies) | **done** with real servers |
| Password, MFA and Conditional Access outcome | **no** | user run (section 4) |
| SAMLResponse POST-back to OpenAthens and EZproxy, publisher session creation | **no** | user run |
| Entitled PDF via T1-T6 after sign-in | **no** | user run |

---

## 2. Static evidence (source and documentation)

### 2.1 `Zotero.openInViewer` and `basicViewer`
- `Zotero.openInViewer(uri, {onLoad, allowJavaScript, userContextId, customUserAgent})`
  (`xpcom/zotero.js:1321-1364`).
  - It reuses an open viewer with the same `uri` and `userContextId` (`:1327-1333`).
  - It opens `chrome://zotero/content/standalone/basicViewer.xhtml` with
    `"chrome,dialog=yes,resizable,centerscreen,menubar,scrollbars"` (`:1344-1345`). *(high)*
- The window has one `<browser type="content" remote="false" maychangeremoteness="true"
  disableglobalhistory="true" messagemanagergroup="basicViewer">` (`basicViewer.xhtml:247-254`)
  and **no URL bar**. *(high)*
- `basicViewer.js:34-61, 77-91`: `customUserAgent` is set as
  `browser.browsingContext.customUserAgent` before load. The load uses a system triggering
  principal. *(high)*
- Popups: `browserWindowShim.js:22-31, 219` installs `window.browserDOMWindow`. Its
  `_openURIInNewTab()` appends a `<browser>` with `hidden = true` and no `openWindowInfo`. Any
  SSO step that opens a window would be invisible. `browser.link.open_newwindow=3` (runtime) and
  `dom.disable_open_during_load=true` (`defaults/preferences/zotero.js:355, 1461`). *(high)*
- Links: `ExternalLinkHandlerChild.mjs` turns left clicks whose `event.target` is an `<a>`,
  `<area>` or `label.text-link` into `Zotero.launchURL(href)`
  (`ExternalLinkHandlerParent.mjs`). Same-page `#hash` and `javascript:` links are exempt. In
  basicViewer, a "Sign in with your institution" anchor would therefore move the session into
  the system browser. *(high)*

### 2.2 HiddenBrowser and BrowserRequest (for the post-login download checks)
- `new HiddenBrowser({userContextId, customUserAgent, allowJavaScript, …})`
  (`HiddenBrowser.mjs:46-131`).
  - It creates a `remote="true"` `<browser>` inside a windowless `HiddenFrame` and sets
    `docShell.allowImages=false`.
  - **Without `userContextId` it uses the default jar**, the same one as both login windows.
    *(high)*
- `Zotero.BrowserRequest.downloadPDF(url, path, {shouldDisplayCaptcha})`
  (`browserRequest.js:567-614`).
  - It uses `new HiddenBrowser()` with the default UA, then `_loadAndSettle` with a PDF MIME
    handler (`:444-523`, `_makePDFMIMETypeHandler` `:525-559`).
  - `PLAIN_UA_HOSTS` (`:71-76`) is limited to `challenges.cloudflare.com`, `www.sciencedirect.com`,
    `pdf.sciencedirectassets.com`, `search.worldcat.org` and `www.mdpi.com`. **Wiley,
    PsychiatryOnline and BMJ are not on it.** *(high)*

### 2.3 User-Agent handling, including a Zotero bug
- Base UA: `… Gecko/20100101 Zotero/10.0.6`. `Zotero.VersionHeader` (`zotero.js:2238-2336`)
  observes `http-on-modify-request` and, in mode `full`, inserts `Firefox/140.0 ` before
  `Zotero/` (`:2311-2318`). Mode `plain` removes `Zotero/x` and adds `Firefox/x` only if missing
  (`:2306-2310`). *(high)*
- **Bug (observed):** `full` mode has no idempotency check. On redirects Gecko copies the
  already-rewritten `User-Agent` into the replacement channel, so every hop adds another
  `Firefox/140.0`.
  - Run 2 recorded `… Firefox/140.0 Firefox/140.0 Firefox/140.0 Firefox/140.0 Firefox/140.0
    Zotero/10.0.6` on the fifth hop of the OpenAthens-to-Entra chain.
  - It is harmless for Entra (the form rendered) but it is a bot-detection signal.
  - The plain UA avoids it. *(high: empirical plus source)*
- `customUserAgent` did not cover every request. A fetch from the out-of-process Cloudflare
  iframe (`hagen.challenges.cloudflare.com`) still carried `Zotero/10.0.6`. The challenge passed
  anyway. *(high, empirical)*

### 2.4 Gecko cookie and privacy settings at runtime
Read inside the throwaway instance; all are defaults.

| Pref | Value | Meaning for SAML chains |
|---|---|---|
| `network.cookie.cookieBehavior` | 5 (`zotero.js:943`) | dFPI. Partitions third-party iframe cookies only. Top-level hops are first-party. |
| `network.cookie.sameSite.laxByDefault` | false (Gecko default `StaticPrefList.yaml:13158-13161`) | Cookies without SameSite are sent on cross-site POSTs, e.g. the SAMLResponse POST to OpenAthens or EZproxy. |
| `network.cookie.sameSite.noneRequiresSecure` | true | `SameSite=None` needs `Secure`. Entra, Atypon and OpenAthens comply (all https). |
| `privacy.bounceTrackingProtection.mode` | 3 = `MODE_ENABLED_DRY_RUN` (`StaticPrefList.yaml:16331-16337`; `nsIBounceTrackingProtection.idl:36-39`) | No purging of OpenAthens, Atypon or other bounce hosts. |
| `privacy.sanitize.sanitizeOnShutdown` | false | Persistent cookies survive restarts. Session cookies such as `oacookie` and EZproxy's do not. |
| `network.http.windows-sso.enabled` | **false** | Windows Hello / PRT SSO to Entra is off (section 2.5). |
| `network.http.windows-sso.container-enabled.0` | true | Would apply to the default container if enabled. |
| `security.webauth.webauthn`, `security.webauthn.ctap2` | true | WebAuthn is on. On Windows, Gecko hands off to the platform dialog. Firefox's own prompt UI (browser.js) is absent from Zotero (only `webauthnDialog.ftl` strings ship). Passkeys or FIDO keys for MFA are **untested** *(low)*. |
| `general.useragent.override` | absent | See section 2.3. |
| `extensions.zotero.browserRequest.timeout` / `onLoadTimeout` | 60000 / 3000 | Used by T2. |

### 2.5 Conditional Access and Windows SSO
- Microsoft: client-app conditions "work with all browsers". To satisfy a **device** policy, the
  supported browsers on Windows 10+ are Edge, Chrome, and **Firefox 91+** through Windows SSO.
  "The device check fails if the browser is running in private mode or if cookies are
  disabled." (learn.microsoft.com, *Conditional Access: Conditions*, "Supported browsers",
  updated 2026-06-03.) *(medium)*
- Gecko: `if (StaticPrefs::network_http_windows_sso_enabled() && https && !LOAD_ANONYMOUS &&
  !private) { if (TYPE_DOCUMENT || TYPE_SUBDOCUMENT) AddWindowsSSO(this); }`
  (`netwerk/protocol/http/nsHttpChannel.cpp:476-491`, `HttpWinUtils.cpp:17-66`, ESR140).
  - It lives in libxul, so Zotero has it. It would apply to both login-window types and to
    HiddenBrowser document loads, but not to `Zotero.HTTP.request` (XHR).
  - Firefox exposes it through the `WindowsSSO` enterprise policy, which Zotero also ships
    (`omni-app/modules/policies/Policies.sys.mjs:2842-2844`). *(high)*
- This machine: `dsregcmd /status` shows `WorkplaceJoined: YES`, `AzureAdJoined: NO`,
  `DomainJoined: NO`. The device is Entra-*registered* to some tenant (not recorded). If that
  tenant is KCL and KCL requires a registered or compliant device, Windows SSO is the way to
  satisfy it. *(high for the flags; low for relevance)*
- Entra defaults: sign-in frequency is a rolling 90-day window, and "Stay signed in?" decides
  whether the Entra session cookie persists (learn.microsoft.com, *Conditional Access adaptive
  session lifetime policies*, updated 2026-04-08). *(medium)* The 8-hour OpenAthens session
  and 120-minute EZproxy idle timeout are from brief 04. So after one interactive login, later
  redirector runs should complete silently through the Entra cookie for as long as KCL's policy
  allows. Test T4 checks exactly this.
- KCL: "you will be asked to approve your sign-in … through an additional option, such as
  Microsoft Authenticator app, text, or phone call" (kcl.ac.uk, *Set Up Two-Factor
  Authentication for your IT Accounts*, 24 May 2024). *(medium)*

---

## 3. Empirical results (automated, no login)

### 3.1 Setup
- `tools/sso-spike/run-auto.ps1` runs a headless throwaway Zotero. For each start URL it opens
  (a) the custom window `chrome://ssospike/content/login.xhtml` and (b) `Zotero.openInViewer`.
  It follows the chain until Entra's form has rendered, a guarded POST stops it, or the URL is
  stable for 12 s.
- It records:
  - every document, XHR and fetch response through `http-on-examine-response`: method, status,
    redacted URL and Location, the **names** of cookies sent, and the Set-Cookie names with
    their attributes;
  - the cookie inventory (`Services.cookies.cookies`, names, flags and partition key only);
  - page facts through Zotero's `PageData` actor.
- **Guard:** in `http-on-modify-request`, any non-GET/HEAD request to
  `microsoftonline.com|microsoft.com|live.com|msauth.net|msftauth.net|openathens.net|idm.oclc.org|kcl.ac.uk`
  is cancelled with `NS_BINDING_ABORTED` before it is sent.
- Cookies are cleared before every run.

### 3.2 Results (run 2, 2026-10-20 14:54-15:00 UTC)

| Flow | Window | UA | Outcome | Time |
|---|---|---|---|---|
| OA redirector → Nature | custom | Zotero | **Entra form rendered** (`loginfmt`, password field, title "Sign in to your account", HTTP 200) | 5.9 s |
| OA redirector → Nature | viewer | Zotero | **Entra form rendered** | 5.5 s |
| OA redirector → Elsevier (Lancet DOI) | custom | Zotero | **Entra form rendered** (via `auth.elsevier.com` auto-POST to `SHIRE/Login`, then OpenAthens) | 10.1 s |
| OA redirector → Elsevier | viewer | Zotero | **Entra form rendered** | 5.2 s |
| OA redirector → Wiley (JAGS DOI) | custom | Zotero | **Stuck at Cloudflare** 403 "Just a moment..." on `onlinelibrary.wiley.com/action/ssostart` (Turnstile ran but issued no `cf_clearance`) | stable 12 s (run 1: 30 s) |
| OA redirector → Wiley | viewer | Zotero | **Stuck at Cloudflare** (same) | stable 12 s |
| OA redirector → Wiley | custom | **plain** | Cloudflare cleared, then **Entra form rendered** (15 document hops) | 11.5 s |
| OA redirector → Wiley | viewer | **plain** | Cloudflare cleared, then **Entra form rendered** | 10.2 s |
| EZproxy `login?qurl=` → PsychiatryOnline | custom / viewer | Zotero | EZproxy page "Shibboleth Authentication Request" (`EZproxyForm`, `SAMLRequest` field). Its auto-submit fired: **guard blocked POST** `login.microsoftonline.com/8370cf14-…/saml2` | 6.3 / 5.7 s |
| EZproxy `login?qurl=` → BMJ | custom / viewer | Zotero | same | 5.7 / 5.5 s |

- Entra's page text in every case began: "Sign in … Your username should be entered as your
  King's ID followed by @kcl.ac.uk …". There was no browser-support warning.
- Entra loaded a Seamless-SSO iframe, `autologon.microsoftazuread-sso.com/…/winauth/iframe`.
  Its `winauth/sso` XHR returned 401 (no Kerberos ticket, as expected off-domain).
- The guard blocked Entra's telemetry POSTs on load (`/common/instrumentation/dssostatus`,
  `eu-mobile.events.data.microsoft.com`). The form rendered regardless.

### 3.3 Observed chains (document hops; values redacted)

```
OA → Nature (all GET)
 302 go.openathens.net/redirector/kcl.ac.uk?url=…           → sp.nature.com/saml/login?idp=E
 302 sp.nature.com/saml/login                                → secure.nature.com/saml2/authenticate/<b64 E>   (set JSESSIONID, __VCAP_ID__)
 302 secure.nature.com/saml2/authenticate/…                  → login.openathens.net/saml/2/sso/kcl.ac.uk/o/71807903/c/oafed?SAMLRequest=…
 307 login.openathens.net/saml/2/sso/…                        → login.openathens.net/auth/kcl.ac.uk/o/71807903   (set oaxsrftkn [Strict], oacookie)
 302 login.openathens.net/auth/…   (sent oacookie)            → login.microsoftonline.com/8370cf14-…/saml2?SAMLRequest=…
 200 login.microsoftonline.com/…/saml2                        (set buid, esctx, fpc [SameSite=None], x-ms-gateway-slice, stsservicecookie)

OA → Wiley with plain UA (Cloudflare first)
 200 go.openathens.net "Please wait" → HEAD poll 201 → onlinelibrary.wiley.com/action/ssostart?idp=E&redirectUri=…
 403 ssostart (Cloudflare) → challenge XHRs → cf_clearance [SameSite=None; Partitioned] → POST 302 ssostart
 302 ssostart → action/oidcStart → wiley.scienceconnect.io/api/oauth/authorize → action/oidcCallback (error=… no Wiley account) → ssostart
 403 second Cloudflare challenge on ssostart → cleared → 302 → http://iam.atypon.com/action/ssostart
 302 iam.atypon.com/action/ssostart → login.openathens.net/saml/2/sso/kcl.ac.uk/o/71807903/c/oafed?SAMLRequest=…
 307/302 OpenAthens → 200 Entra (as above)

EZproxy (HTTP-POST binding)
 200 login.kcl.idm.oclc.org/login?qurl=… "Shibboleth Authentication Request" → onload auto-POST to login.microsoftonline.com/8370cf14-…/saml2 (blocked by the guard)
```

### 3.4 Cookie storage across the cross-site chain
After the OA → Nature chain in the custom window, `Services.cookies.cookies` held:

| Host | Names (SameSite) | Partition key |
|---|---|---|
| `secure.nature.com` | `JSESSIONID`, `__VCAP_ID__`, `__VCAP_ID_META__` (unset) | `""` |
| `login.openathens.net` | `oacookie` (unset, session), `oaloginorg` (unset), `oaxsrftkn` ×2 (Strict, path `/saml/2/sso/…`) | `""` |
| `login.microsoftonline.com` / `.login.microsoftonline.com` | `buid`, `fpc` (30 d), `esctx`, `esctx-<id>`, `stsservicecookie`, `x-ms-gateway-slice`, `brcap` (390 d), all SameSite=None | `""` |
| `autologon.microsoftazuread-sso.com` | `fpc`, `esctx`, `stsservicecookie`, `x-ms-gateway-slice` | **`(https,microsoftonline.com)`**: third-party iframe, partitioned |

Wiley's chain also stored `cf_clearance` (`.onlinelibrary.wiley.com`, SameSite=None, 365 d,
**Partitioned** attribute, set first-party so partition key `""`), plus Atypon's `MAID`,
`MACHINE_LAST_SEEN` and `JSESSIONID`. `userContextId` was 0 for all of them. *(high)*

### 3.5 Are the login-window cookies reused by the download mechanisms? (real servers)
First request to each host after the chain. Only cookie **names** were recorded.

| Mechanism | `https://secure.nature.com/` | `https://login.openathens.net/` |
|---|---|---|
| `Zotero.HTTP.request` (XHR, browserId 0) | `JSESSIONID, __VCAP_ID__, __VCAP_ID_META__` | `oacookie, oaloginorg` |
| `HiddenBrowser` document load (default jar) | `JSESSIONID, __VCAP_ID__, __VCAP_ID_META__` | `oacookie, oaloginorg` |
| `fetch(url, {credentials:'include'})` | same three | `oacookie, oaloginorg` |
| `fetch(url)` (default credentials) | **none** | **none** |

*(high, empirical; extends brief 03 section 10 from one local host to a real cross-site chain)*

### 3.6 Pre-login download baselines (what the checks see with no session)
- **Wiley** `agsjournals.onlinelibrary.wiley.com/doi/pdfdirect/10.1111/jgs.17623`:
  - `Zotero.HTTP.request` with the Zotero UA: **403 Cloudflare "Just a moment..."**.
  - With the plain UA: no challenge. The chain is `302 → action/oidcStart → scienceconnect →
    oidcCallback → pdfdirect → 302 /doi/abs/… → 200`, i.e. the abstract page, no PDF. The
    237 KB abstract page was served very slowly: 25-37 s in the smoke test, and **over the 60 s
    XHR timeout** twice in run 2.
  - Cloudflare's decision varied. In the smoke test, the Zotero-UA XHR was *not* challenged and
    reached the abstract page (28.9 s). That happened after a login window had obtained
    `cf_clearance`, but that clearance was bound to the plain UA, so the reason is unclear.
    *(high that it happened; low on why)*
  - `BrowserRequest.downloadPDF`: "Settled without receiving a PDF" with the Zotero UA. With the
    plain UA: the same, or a 60 s timeout.
- **PsychiatryOnline and BMJ via EZproxy** (proxied PDF URL): `302 →
  login.kcl.idm.oclc.org/login?qurl=… → 200` SAML page. **Unproxied**: 403 Cloudflare with both
  UAs, because an XHR cannot run the JS challenge.
- **ScienceDirect** `…/pdfft?isDTMRedir=true&download=true`: 403, a 1.2 MB "ScienceDirect" page
  with both UAs.

The manual run repeats these as controls, so a PDF after sign-in can be credited to the session.

### 3.7 Other observations
- **Plugin JSWindowActors cannot use a child module from the plugin's own chrome package in
  content processes.**
  - `ChromeUtils.registerWindowActor('SsoSpike', {child:{esModuleURI:'chrome://ssospike/content/SsoSpikeChild.mjs'}})`
    logged "Failed to load chrome://ssospike/content/SsoSpikeChild.mjs" for every web page
    (remote types `webIsolated=…`, `webCOOP+COEP=…`, `web`), and `sendQuery` never answered.
    It worked only for `about:blank` in the parent process.
  - Zotero's own `PageData` actor (`actors/ActorManager.mjs:4-8`, `PageDataChild.mjs`) works
    everywhere: `title`, `bodyText`, `documentHTML`, `querySelectorMatches`, `channelInfo`.
  - **Implication:** for live-DOM reads from a login or hidden browser, use `PageData` and parse
    the HTML in the Rust core. *(high, unpacked proxy-file install; packed XPI not tested)*
- **Viewer flake:** in run 1, one of seven `openInViewer` windows never started its load (no
  request at all, browser stayed `about:blank`). Run 2 had 0 of 6. The kit now detects this and
  calls basicViewer's `loadURI()` again after 4 s. *(high that it happened once; cause unknown)*
- Window remoteness: both window types start `remote=false` and switch to a remote browser
  (`webIsolated=https://microsoftonline.com`, `webIsolated=https://oclc.org`) on navigation.
  `browserId` is stable across the switch and is a reliable key for attributing channels to a
  window. *(high)*

---

## 4. Remaining work: the user's run

### 4.1 Kit
- `tools/sso-spike/` holds the plugin, `run-manual.ps1`, `run-auto.ps1` and a `README.md` with
  the runbook.
- Manual mode opens a control window with eight steps: four required (OA→Wiley and EZproxy→AJP,
  each in a custom window and in `openInViewer`) and four optional (Elsevier and BMJ).
- Login windows use the plain Firefox UA by default (section 0, item 3); config `loginUA` sets
  it.
- After the person signs in and presses "Finished signing in: run checks", the kit runs:

| Check | Call |
|---|---|
| baseline (before login) | `Zotero.HTTP.request` on the PDF URL, both UAs |
| T1 / T1p | `Zotero.HTTP.request('GET', pdfUrl, {responseType:'arraybuffer', successCodes:false})` with the Zotero UA / with `headers:{'User-Agent': plainUA}` |
| T2 / T2p | `Zotero.BrowserRequest.downloadPDF(pdfUrl, path)` / `new HiddenBrowser({customUserAgent: plainUA})` + `BrowserRequest._loadAndSettle(hb, url, {onPDF})` |
| T3 | `Zotero.Attachments.addAvailableFile(item, {methods:['doi']})` (OA flows) or `['url']` with the proxied landing URL (EZproxy flows), in the throwaway library |
| T4 | `HiddenBrowser` (plain UA) loads the **start URL for a second article**: does the hop complete silently? |
| T5 | `Zotero.HTTP.request` (plain UA) on the second article's PDF URL |
| T6 | `Zotero.HTTP.request` on the proxied landing page (EZproxy) |

- Privacy:
  - The report stores redacted URLs (allow-listed parameters only), cookie names and flags,
    statuses, and `%PDF-` yes/no with sizes. Downloaded PDFs are deleted after the check.
  - No passwords, cookie values, SAML messages or page text in manual mode.
  - The throwaway profile runs with `signon.rememberSignons=false` and clear-on-shutdown.
    `run-manual.ps1` then deletes the whole run directory.
  - `Zotero.launchURL` is wrapped, so a viewer `<a>` hand-off to the system browser is
    recorded and blocked.

### 4.2 Validation of the kit without a human
`run.ps1` with `extensions.sso-spike.mode=manual` and `extensions.sso-spike.smokeTest=true`
(headless, guard on) drives the control window through two steps, OA→Wiley (custom) and
EZproxy→AJP (viewer). Each step "finishes" without sign-in, and the run then writes the report
and quits. See section 4.3 for the outcome.

### 4.3 Smoke-test result
Files: `tools/sso-spike/results/sso-smoke-20261020-160127.json` and
`sso-smoke-ui-20261020-160802.json`.

- **Both runs completed and exited cleanly.** In both, the user's Zotero PIDs stayed alive and
  `profiles.ini` and `installs.ini` were unchanged.
- **The control window renders correctly.**
  - The table shows 8 step rows.
  - "Finished signing in" is disabled at start and enabled while a step waits.
  - Per-step status cells update, and the log area fills.
- **Every check ran end to end and returned the expected pre-login answer: no PDF.**
  - OA→Wiley (custom window, plain UA):
    - T1 and T1p: the Wiley abstract page, 200, 237 KB, `citation_pdf_url` present.
    - T2: settled without a PDF. T2p: 60 s timeout.
    - T3: `addAvailableFile` → `false` in 27.7 s.
    - T4: the hidden browser followed the redirector for the second article to the Entra
      sign-in page (`endedOnIdP: true`), which is the correct "login needed" signal.
    - T5: abstract page.
  - EZproxy→AJP (viewer, plain UA): T1, T1p, T5 and T6 all returned the 1.5 KB "Shibboleth
    Authentication Request" page. T2, T2p and T4 ended on the EZproxy login page. T3 →
    `false`.
- In this run the Wiley login window took about 130 s to clear Cloudflare with no interaction.
  It had not cleared when the step "finished" at 25 s. It then went on to Entra while the checks
  ran. For the person running it: **wait on "Just a moment..." or click the Turnstile box.**
- One smoke-test issue was fixed: the step table is populated after the window's `load`
  handler, so the first check of the row count read 0 rows. The UI run read 8.

---

## 5. How to read the user's results: outcome → architecture

| Outcome of the manual run | Meaning | Design consequence |
|---|---|---|
| Custom window: sign-in + MFA complete, lands on article; T1p/T2p give `%PDF-`; T4 ends on publisher | In-Zotero institutional access works end to end | Keep the planned design: custom login window (plain UA) → cookie jar → downloads. Run the redirector in a HiddenBrowser (plain UA) per item; open the login window only when T4-style detection lands on an IdP page. |
| Sign-in completes, but T1 (Zotero UA) fails and T1p (plain UA) succeeds on Cloudflare hosts | `cf_clearance` is bound to the UA | Send the plain UA on **every** request to Cloudflare-fronted publisher hosts (header on `Zotero.HTTP.request`, `customUserAgent` on browsers). Do not rely on `BrowserRequest.downloadPDF` as shipped for those hosts. |
| Sign-in completes, T1/T1p fail, T2p succeeds | Publisher needs a JS-capable context for the PDF | Use HiddenBrowser + PDF MIME capture (T2p mechanism) as the institutional download path. |
| Sign-in completes in custom window but not in viewer | Viewer pitfalls (links → system browser, hidden popups) | Custom window only (already recommended). |
| Entra shows a Conditional Access error ("You can't get there from here", or an AADSTS code that says the device must be managed or compliant, e.g. 53000; code list from memory, *low*) | Device-based CA | Offer an opt-in toggle that sets `network.http.windows-sso.enabled=true` (the Firefox "Windows SSO" mechanism), then re-test. If it still fails: campus IP/VPN + "Open in browser" hand-off (LibKey `libkey.io/libraries/286/<doi>` or the redirector URL) + "attach a PDF I downloaded". |
| MFA prompt never appears / passkey step fails | Missing browser feature (likely WebAuthn UI) | Tell users to pick Authenticator push/SMS ("Sign in another way"); document this. |
| EZproxy step fails, OA works | EZproxy app has different CA or SAML binding issue | EZproxy is secondary anyway (brief 04 §0): drop it to "Open in browser" only. |
| T4 lands on Entra after a successful first login | Silent SSO not reused (session cookie not persisted, or SIF=every time) | Expect one interactive login per item batch; queue items and open one login window per batch. |

---

## 6. Design recommendations, valid whatever the user run shows

1. **Ship a custom login window.** Use `tools/sso-spike/plugin/content/login.xhtml` and
   `login.js` as the base:
   - no `messagemanagergroup`, so there is no ExternalLinkHandler;
   - an origin and TLS bar, read from `browsingContext.secureBrowserUI.state`;
   - a visible popup pane through `window.browserDOMWindow`;
   - `loadURI` with a system triggering principal.
   Do not use `Zotero.openInViewer` for credentials. *(high)*
2. **Use the plain Firefox UA for login windows and for institutional fetches to
   Cloudflare-fronted hosts.** Get it from `Zotero.VersionHeader.getPlainFirefoxUA()`
   (`zotero.js:2327-2331`). For `Zotero.HTTP.request`, pass `headers:{'User-Agent': ua}`;
   VersionHeader leaves a UA without `Zotero/` unchanged. Never call the internal
   `registerPlainUAHost()`, because it changes Zotero's behaviour globally. *(high)*
3. **Keep the default cookie jar** (no `userContextId`), so the login window, HiddenBrowser,
   `Zotero.HTTP.request` and `addFileFromURLs` share one session. Never use plain `fetch()`
   without `credentials:'include'`. *(high, empirical)*
4. **Detect state from URLs and HTML, not cookie names.** The Rust core classifies the redirect
   or landing state from the URL host and path plus `PageData.documentHTML`:
   - `login.microsoftonline.com/<tenant>/saml2` means an IdP page;
   - `EZproxyForm` means an EZproxy login;
   - `<title>Just a moment...` means a Cloudflare challenge;
   - an OpenAthens "Please wait" page means the redirector is still resolving.
   Cookie names are a hint only (brief 04 §3.3). *(high)*
5. **Do not register a plugin JSWindowActor** that needs a plugin-packaged child module in
   content processes. Use Zotero's `PageData` actor. *(high, unpacked install)*
6. **Windows SSO toggle** (opt-in, off by default, with a security note). It only helps when
   KCL's CA requires a registered or compliant device and the device is joined or registered to
   KCL. *(medium)*

---

## 7. Open questions (all need the user's run or KCL IT)
1. Does Entra apply device-based Conditional Access to the OpenAthens (`login.openathens.net`)
   or EZproxy (`https://kcl.idm.oclc.org/shibboleth`) enterprise apps for this user?
2. Does the SAMLResponse POST-back complete in Zotero for OpenAthens, then Atypon (Wiley), and
   for EZproxy? (Gecko prefs say yes; not observed.)
3. Do OCLC-hosted EZproxy fetches of `psychiatryonline.org` pass the publisher's Cloudflare,
   given that the unproxied host challenges every XHR?
4. After login, does Wiley `pdfdirect` return a PDF to an XHR with the plain UA, or is a browser
   context needed (T1p vs T2p)?
5. Is the Wiley `/doi/abs/` body hang (section 3.6) reproducible, and does it affect entitled
   sessions?
6. Do plugin JSWindowActor child modules load from a **packed XPI** (`jar:`) install?

---

## 8. Sources
- Local Zotero 10.0.6. Paths are relative to `.ref/omni-app/chrome/content/zotero/` unless
  noted:
  - `xpcom/zotero.js:1310-1364` (openInViewer), `:2233-2336` (VersionHeader);
  - `standalone/basicViewer.xhtml:41-48, 245-256`; `standalone/basicViewer.js:34-91`;
    `browserWindowShim.js:22-31, 97-180, 217-220`;
  - `actors/ActorManager.mjs:4-8, 51-61`; `actors/ExternalLinkHandlerChild.mjs`;
    `actors/ExternalLinkHandlerParent.mjs`; `actors/PageDataChild.mjs`;
  - `HiddenBrowser.mjs:46-131`; `xpcom/browserRequest.js:28-77, 114-186, 444-559, 567-671, 674-679`;
  - `xpcom/http.js:180-240` (request options);
  - `.ref/omni-app/defaults/preferences/zotero.js:355, 943-944, 1461`;
  - `.ref/omni-app/modules/policies/Policies.sys.mjs:1833, 2842-2844`.
- Gecko ESR140 (`raw.githubusercontent.com/mozilla-firefox/firefox/esr140/…`):
  - `netwerk/protocol/http/nsHttpChannel.cpp:476-491`;
  - `netwerk/protocol/http/HttpWinUtils.cpp:17-66`;
  - `modules/libpref/init/StaticPrefList.yaml:13074-13077, 13158-13161, 13460-13468, 16331-16337`;
  - `toolkit/components/antitracking/bouncetrackingprotection/nsIBounceTrackingProtection.idl:13-41`.
  - Searchfox `firefox-esr140` was used to locate these.
- Microsoft Learn:
  - *Conditional Access: Conditions*, "Supported browsers":
    https://learn.microsoft.com/en-us/entra/identity/conditional-access/concept-conditional-access-conditions
  - *Conditional Access adaptive session lifetime policies*:
    https://learn.microsoft.com/en-us/entra/identity/conditional-access/concept-session-lifetime
- KCL, *Set Up Two-Factor Authentication for your IT Accounts* (24 May 2024):
  https://www.kcl.ac.uk/students/set-up-two-factor-authentication-for-your-it-accounts
- Entitlement and OA status of the test articles:
  - KCL Alma uresolver `svc_dat=CTO` (brief 04 §4.5);
  - OpenAlex `api.openalex.org/works?filter=…` (no key, no email).
- Empirical runs: `tools/sso-spike/results/*.json` and `.ref/harness-runs/sso-auto-*`.

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
