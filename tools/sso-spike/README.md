# sso-spike

A throwaway-Zotero probe for research gap **B1**: can a KCL user finish single sign-on
inside a Zotero 10 window? And after sign-in, do Zotero's download paths reuse the session?
Those paths are `Zotero.HTTP.request`, a `HiddenBrowser` with PDF capture, and
`Zotero.Attachments.addAvailableFile`.

It runs on the reusable harness in `tools/zotero-harness/`. That harness uses a fresh temp
profile and a temp data directory, and it never touches your running Zotero, your profile or
`~/Zotero`. Findings are written up in `research/08-gap-fill.md`.

## Two modes

| Mode | Who | Login? | What it shows |
|---|---|---|---|
| `auto` (`run-auto.ps1`) | Claude or anyone | **No.** Any non-GET request to an identity-provider host (Entra, OpenAthens, EZproxy, kcl.ac.uk) is cancelled inside Zotero before it is sent. | The redirect chain up to the sign-in form in both window types: whether the Entra page renders, which cookies are set (names and flags only, plus partition key), whether cross-site cookies are reused by `Zotero.HTTP.request` and `HiddenBrowser`, and pre-login download baselines. |
| `manual` (`run-manual.ps1`) | **You** | **Yes, you sign in yourself.** | Whether KCL SSO and MFA complete in a custom window and in `Zotero.openInViewer`, and whether T1-T6 below then return the entitled PDF. |

## Manual run (about 20-30 minutes)

```powershell
pwsh -NoProfile -File C:\Users\abora\zotero-fulltext\tools\sso-spike\run-manual.ps1
```

1. A second, empty Zotero window opens, plus a **control window**.
2. Run the four non-optional steps in order. Press **Start** for one step. A sign-in window opens:
   - **custom window**: has a bar showing the real origin and TLS state;
   - **Zotero viewer**: Zotero's own `openInViewer` window, which has **no address bar**. Check
     the window title, or skip this kind if you are not comfortable.
3. Sign in as you normally would: KCL username, password, MFA approval. Only type your
   password when the origin is `https://login.microsoftonline.com`.
   On Wiley you may first see Cloudflare's "Just a moment...". It usually clears by itself
   within seconds, but in one test it took about 2 minutes. If a "Verify you are human"
   checkbox appears, click it.
   "Stay signed in?" can be answered either way; note which in the notes box.
4. When the article page or PDF appears, press **Finished signing in: run checks**. Checks take
   1-4 minutes. If sign-in cannot be completed (an error page, a "browser not supported" page,
   an MFA prompt that never appears, a page that never loads), write what you saw in the notes
   box and press **Abandon this step**. That is a valid result too.
5. "Clear all cookies before each step" is ticked by default, so each step needs a fresh
   sign-in and the four results stay independent. Untick it for one extra run if you want to see
   silent re-use, for example running the viewer step right after the custom-window step.
6. Press **Write report and quit**. The script copies the report to
   `tools\sso-spike\results\sso-manual-<timestamp>.json` and deletes the throwaway run
   directory (profile, data, logs). Pass `-KeepRunDir` to keep it.

Check the report before sharing it. It should hold only redacted URLs, cookie names and
flags, and pass/fail facts.

## What each check means

| Check | Mechanism | A pass means |
|---|---|---|
| baseline | `Zotero.HTTP.request` on the PDF URL before sign-in | (control) should **not** be a PDF; if it is, the article is free or IP-entitled and the step proves nothing about SSO |
| T1 | `Zotero.HTTP.request(GET pdfUrl)` | the plain XHR path sends the new session cookies and gets `%PDF-` |
| T2 | `Zotero.BrowserRequest.downloadPDF(pdfUrl)` (HiddenBrowser + `application/pdf` MIME capture) | a hidden browser in the default cookie jar gets the PDF (and passes Cloudflare) |
| T3 | `Zotero.Attachments.addAvailableFile(item, {methods:['doi']})` (OpenAthens flows) or `['url']` with the proxied landing URL (EZproxy flows) | Zotero's own "Find Full Text" pipeline attaches the entitled PDF |
| T4 | `HiddenBrowser` loads the **start URL for a second article** (redirector or `login?qurl=`) | after one interactive login, the plugin can run the SSO hop invisibly for further articles (ends on the publisher, not on an IdP page) |
| T5 | `Zotero.HTTP.request` on the second article's PDF URL | the publisher or EZproxy session covers further articles |
| T6 | `Zotero.HTTP.request` on the proxied landing page (EZproxy flows) | the EZproxy session cookie is sent by plain XHR to the proxied host |

## Targets (defaults; override with pref `extensions.sso-spike.configFile` = path to a JSON with `flows`)

| Flow | Start URL | Why this article |
|---|---|---|
| `oa-wiley` | `go.openathens.net/redirector/kcl.ac.uk?url=https://doi.org/10.1111/jgs.17623` | J Am Geriatr Soc 2022. OpenAlex says closed. KCL's Alma resolver lists "Wiley journals current" (checked 2026-10-20). |
| `ezp-ajp` | `login.kcl.idm.oclc.org/login?qurl=https://psychiatryonline.org/doi/10.1176/appi.ajp.20260398` | Am J Psychiatry 2026, closed. Alma lists "PsychiatryOnline Premium Package", and the free package excludes the latest year. `psychiatryonline.org` is configured in KCL's EZproxy. |
| `oa-elsevier` (optional) | redirector + `https://doi.org/10.1016/S0140-6736(19)31138-9` | Lancet 2019. Alma lists Elsevier ScienceDirect. |
| `ezp-bmj` (optional) | `login?qurl=https://www.bmj.com/content/368/bmj.m1091` | The BMJ article the research critic suggested. It is **free to read**, so it proves login and proxying but not entitlement. |

## Files

- `plugin/bootstrap.js`: all logic (recorder, IdP POST guard, checks, auto and manual drivers).
- `plugin/content/login.xhtml`, `login.js`: the custom sign-in window. It has no
  `messagemanagergroup`, so Zotero's `ExternalLinkHandler` does not attach. It shows the origin
  and TLS state, and it shows popups visibly.
- `plugin/content/control.xhtml`, `control.js`: the manual-mode control panel.
- `plugin/content/SsoSpikeChild.mjs`: a JSWindowActor that reports page facts (Entra form
  present, Cloudflare challenge, WebAuthn availability, UA). It never reports form values.
- `run-auto.ps1`, `run-manual.ps1`: harness wrappers. Results go to `results/`.
