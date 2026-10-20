# zotero-fulltext

A Zotero 10 plugin. Right-click one or more items and choose **Get Full Text**. It finds the PDF and attaches it,
using open-access sources first and then King's College London institutional access.

- **Open access:** OpenAlex, Europe PMC, PubMed Central (AWS open-data copy), arXiv, bioRxiv/medRxiv and Zotero's own
  OA lookup. Unpaywall, Semantic Scholar and CORE are used only if you add an email or key in the settings.
- **King's access:** first a direct request, which works on campus or VPN. Then the OpenAthens route
  (`go.openathens.net/redirector/kcl.ac.uk`), which goes through KCL's Microsoft sign-in to the publisher. KCL's
  EZproxy (`login.kcl.idm.oclc.org`) is used only for the ~19 sites it covers. The research found that the major
  medical publishers go through OpenAthens, not EZproxy (`research/04-kcl-institutional-access.md`).
- **Concurrent and async:** 4 items at a time by default. Each item queries its sources in parallel and the first
  valid PDF wins. Nothing blocks Zotero's window.
- **Rust where possible:** about 49,000 lines of Rust against about 8,600 lines of plugin JavaScript. The Rust is
  compiled to WebAssembly and makes every decision: identifiers, parsing source responses, ranking, page and
  sign-in detection, PDF-link rules, URL rewriting, scheduling, rate limits and the sign-in gate. The JavaScript only
  performs network requests, calls Zotero and draws the windows.

## Install

1. In Zotero: **Tools → Plugins**.
2. Click the gear icon, then **Install Plugin From File…**.
3. Choose `dist/zotero-fulltext-0.1.0.xpi`.

Requires Zotero 10.x.

## Use

| Menu entry (right-click on items) | What it does |
|---|---|
| Get Full Text | Open access first, then King's access if needed |
| Get Full Text (open access only) | Never uses King's access and never asks you to sign in |

Under **Tools** you'll also find **Sign In to King's for Full Text…**, **Show Full Text Progress** and **Full Text
Preferences…**.

**Signing in:** when a paper needs King's access, one sign-in window opens and every waiting item shares it. Type
your password only when the bar at the top of that window shows `https://login.microsoftonline.com`. After one
sign-in, later items reuse the session without asking again.

**Progress window:** shows one row per item. When something can't be fetched automatically, its row offers an
action: open it in your browser (OpenAthens or LibKey link), attach a PDF you downloaded yourself, sign in, or retry.

### Limits on institutional downloads

Library licences forbid bulk downloading, so the plugin enforces these limits:

| Limit | Default | Setting key (`extensions.zotero-fulltext.inst.*`) |
|---|---|---|
| Items per batch | 20 (asks you above 10) | `batch_max`, `confirm_above` |
| PDFs per hour / per day | 30 / 80 | `hourly_cap`, `daily_cap` |
| MB per hour | 300 | `mb_per_hour` |
| Minimum gap between PDFs from one publisher | 5 s | `min_gap_sec` |

It stops on rate-limit (429) and suspension pages, and it never solves CAPTCHAs. When a site shows a bot check, a
window opens and you complete it yourself.

## Settings (Zotero Settings → Full Text)

- **Unpaywall email:** empty by default. Unpaywall needs an email; add yours to turn it on.
- **OpenAlex API key:** optional.
- **Institution:** King's College London by default. You can change the OpenAthens domain and the EZproxy hosts.
- **Item concurrency:** default 4.

## Status — what has and has not been tested

Tested:

- `cargo test --workspace` passes, with 0 failures: 131 Rust core unit tests, 300 core integration tests, 572
  engine tests including simulations of the scheduler and sign-in gate, 19 WebAssembly bridge tests and 37 build-tool
  tests. Clippy runs with warnings as errors on the native and wasm32 targets and is clean.
- `node --test "tests/js/*.test.mjs"`: all 615 JavaScript tests pass. They use the real `.wasm` with Zotero
  stand-ins.
- **End to end in a throwaway Zotero 10.0.6, using the packed `.xpi`:** PASS, 22 checks, 0 warnings, 0 failures.
  - The PRISMA paper (PLOS Medicine), "Attention Is All You Need" (arXiv) and a medRxiv preprint were each saved as a
    valid PDF, and the text was checked to be the right paper.
  - A paywalled Annals of Surgery paper ended as "No full text found", with institutional access switched off.
  - The menu and the settings pane register, the UI stayed responsive (longest event-loop pause 58 ms) and Zotero
    shut down cleanly.

Not yet tested:

- **Signing in to King's inside Zotero.** It needs your password and MFA, so no automated test can do it. Earlier
  research showed the sign-in page loads inside a Zotero window and that the session cookies carry over to downloads.
  To check the full flow, run the sign-in test (20–30 minutes, uses a separate empty Zotero):
  `pwsh -NoProfile -File tools/sso-spike/run-manual.ps1`. Instructions are in `tools/sso-spike/README.md`.
- These only run on screen and were never exercised in the automated tests: the "Attach a file you downloaded…" file
  picker, and bot-check windows.
- `research/` and `docs/wp/_finish-results.json` list smaller known gaps. One example: the Alma `getFullTxt` links
  are not fetched yet.

## Build from source

Requires Rust with the `wasm32-unknown-unknown` target, and Node 24 for the JavaScript tests.

```bash
cargo xtask build      # wasm -> plugin/content/fulltext.wasm
cargo xtask xpi        # -> dist/zotero-fulltext-<version>.xpi (reproducible)
cargo xtask check      # fmt, tests, clippy (native + wasm32), build, node tests
pwsh -NoProfile -File tools/zotero-harness/e2e.ps1 -PluginDir dist/zotero-fulltext-0.1.0.xpi   # throwaway-Zotero e2e
```

The harness always uses a temporary profile and a temporary data folder. It never touches your Zotero.

## Layout

| Path | Contents |
|---|---|
| `crates/fulltext-core` | Pure logic: identifiers, OA parsers, page classification, PDF-link extraction, URLs, PDF checks |
| `crates/fulltext-engine` | The async engine: executor, scheduler, licence limits, sign-in gate, per-item pipeline. It sends network requests through the JS side instead of doing I/O itself |
| `crates/fulltext-wasm` | The WebAssembly bridge (zero imports) |
| `xtask` | Build, test and package |
| `plugin/` | The Zotero plugin: bootstrap, effect runner, Zotero calls, windows, settings, strings |
| `docs/design/` | Design docs and the JS↔Rust contracts |
| `research/` | The research the design is based on |
| `tools/zotero-harness`, `tools/sso-spike` | Throwaway-Zotero test harness and the manual sign-in test |
