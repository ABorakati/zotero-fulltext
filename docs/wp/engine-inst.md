# WP engine-inst: the institutional route and hand-off links

Owner of: `crates/fulltext-engine/src/pipeline/inst.rs` and `pipeline/handoff.rs`. Touch nothing else. You call the
frozen APIs of the other packages: `attempt::{fetch_candidate, FetchPolicy, FetchEnd, PdfBlob}`,
`landing::extract_landing`, `save::save_pdf_blob`, `pool::CandidatePool`, `pure::*`, `requests::{get, probe}`,
`Slot` / `parked`, and the `Cx` methods in `sched/acquire.rs` (`acquire_inst`, `acquire_browser`, `reserve_licence`,
`halt_inst`, `ezproxy_known`/`learn_ezproxy`, `alma_known`/`learn_alma`, `ua_for`, `timeout_for`) and `login.rs`
(`session_admit`, `session_report`, `session_login`, `session_epoch`, `challenge_window`).
Read first: the header of `inst.rs`; `docs/design/engine.md` sections 5-6, 8-9; `research/04-kcl-institutional-access.md`
(sections 2-4, 6, 8.2), `research/08-gap-fill.md` sections 3.6, 5, 6; `research/SUMMARY.md` (c);
`docs/design/candidates/C-failure-first.md` sections 1 (F1-F5, F11, F20, F24-F26) and 4.3.
Depends on: engine-executor-ops, engine-sched-login (real gates and login), and uses engine-pipeline's
`fetch_candidate` (develop against its skeleton signature; integration tests need it).
Env: `CARGO_TARGET_DIR=C:\Users\abora\zotero-fulltext\target\engine-inst`.

## What to build
`run_institutional(cx, slot, ids, pool) -> Outcome`, a straight-line `async fn` over these steps (each step has
its own function in inst.rs so scenarios can target them):
1. **Gates before any request**: `cx.inst_halted()` -> `Outcome::Halted { handoff }`; no DOI and no usable URL ->
   `NotFound { NotHeld }`; per-item budget counter (4 fetches; browser load = 1; translators = 1) -> `NeedsUser {
   NoPdfLink }` when spent; `key = InstKey::for_doi(doi)`.
2. **Entitlement** (`cfg.inst.check_alma` and `institution.alma`): `alma_known(doi)` else one GET of the CTO URL
   (`{alma.base}/view/uresolver/{alma.inst}/openurl?ctx_ver=Z39.88-2004&url_ver=Z39.88-2004&rft_id=info:doi/{doi}
   &svc_dat=CTO`, UA default, `acquire_api(Source::Alma)`, `InspectSpec::Api { source: Source::Alma, .. }`; core's
   `oa::alma` returns `ApiStatus::Found` = held (each `getFullTxt` service is a `Landing` candidate with the
   resolution URL) and `Empty` = not held) -> `learn_alma(doi, held)`. Not held -> `NotEntitled { handoff }`.
   Alma error / `Unparseable` -> continue (unknown = try). Crossref `Source::Doi` candidates collected during OA
   discovery (publisher `link[]` PDFs) are tried here, in the institutional phase only, after the redirector lands.
3. **Direct** (`cfg.inst.try_direct`): `fetch_candidate` of `https://doi.org/<doi>` with `FetchPolicy::inst(key)`.
   `Pdf` -> save. `Landing` -> `extract_landing` -> fetch the best candidate (PDF) with the same policy.
   `AccessDenied`/`NeedsLogin` -> go on. Remember `access_denied_for(key)` on the item (do not retry direct).
4. **OpenAthens** (`cfg.inst.use_openathens`): the sequence in the file header with `session_admit` ->
   hidden `browser_load(Route::Inst, barrier: true, BrowserReq { url: pure::redirector_url(..), ua: Plain,
   capture_pdf: true, want_page: true, settle_ms: 3000, timeout_ms: policy BrowserLoad, hold_while:
   [OpenathensRedirector] }, InspectSpec::Page { .. })` under `acquire_inst(key, Page)` + `acquire_browser()`.
   Outcome by the returned inspection/class: PDF captured -> verify + save; publisher landing -> `session_report(
   LoggedIn)` -> extract -> `fetch_candidate` PDF; `IdpLogin` / `SamlAutoPost` / `PublisherSsoHop` stuck ->
   `session_report(NeedsLogin)`; then `parked(cx, slot, cx.session_login(Openathens, LoginRequest { entry_url:
   redirector URL, item_title, seen_epoch, capture_pdf: true }))` and map `SessionVerdict`: `Retry { epoch }` ->
   repeat this step ONCE under the new epoch (a second bounce => `Ineffective` => `NeedsUser { LoginIneffective }`);
   `Declined` -> `NeedsUser { LoginDeclined }`; `TimedOut` -> `LoginTimeout`; `Refused` -> `LoginRefused`;
   `Ineffective`; `Misconfigured`; `Suspended` -> `Halted`. `OpenathensWait` on the HTTP tier: HEAD-poll every
   `OPENATHENS_POLL_MS` up to `OPENATHENS_POLL_MAX_MS` (accept 201 / 302+Location). `OpenathensUnknownResource` ->
   step 5 if applicable else `NotEntitled`; `OpenathensBadOrg` -> `Misconfigured`.
5. **EZproxy** (`cfg.inst.use_ezproxy`, `institution.ezproxy`): only when the publisher host is in
   `ezproxy_hosts` or `ezproxy_known(host) == Some(true)` or a no-follow probe of the proxified host says configured
   (`pure::proxify`, `requests::probe`, 7-day cache via `learn_ezproxy`). Entry `pure::ezproxy_login_url(inst,
   doi_url)` through a hidden browser exactly like step 4 with `Realm::Ezproxy`. `EzproxyHostNotConfigured` ->
   learn false and stop; `EzproxyErrorPage { suspend: true }` / `LicenceStop` -> `cx.halt_inst(..)`, `Outcome::Halted`.
6. **Stop signals**: `FetchEnd::RateLimited` -> `halt_inst(RateLimited, retry_after)` + `Halted`; `Blocked` counts a
   block signal (3 => halt); a second `Challenge` on a host after the browser tier -> `halt_inst(ChallengeLoop)`;
   `Challenge { interactive: true }` with `interactive && cfg.inst.challenge_windows` -> `challenge_window`.
   `Refused(Cap(hit))` from `reserve_licence` -> `Deferred { cap: hit.kind }`; `Refused(InstHalted)` -> `Halted`.
7. **Saved**: `save::save_pdf_blob(cx, blob, ids)` with `route = Inst` (the blob carries the `LedgerTicket`, which the
   save commits). Group library: `claim_inst_slot` already decided; if it is `Ask` call `cx.group_consent().await`
   before the first licence reservation.
8. **Every non-saved terminal** carries `handoff::handoff(&cfg.institution, ids)`.

`handoff.rs` already builds the links from core's URL builders (test included). If `fulltext_core::urls::handoff`
is implemented when you start, delegate to it and keep the test.

## Tests (native, SimHost + ScriptResponder; one per scenario; names `inst_*`)
F1 (20 items -> one LoginWindow), F2 (expiry re-prompt then hand-off), F3 (Conditional Access), F4 (OpenAthens wait
poll), F5 (EZproxy not configured learned + suspend halts all items), F11 (429 halts the route, no retry),
F20 (group library Ask/Never), F24 (login timeout), F25 (ineffective sign-in), F26 (cap -> Deferred with ledger
reset), silent retry after another item signed in, hand-off links on every failure, no hedge ever, one inst request
per family at a time, PDF spacing, `Persist` ledger before each barrier fetch, cancel mid-fetch counts the sent
reservation.

## Acceptance
```
cargo test -p fulltext-engine pipeline::inst pipeline::handoff
cargo clippy -p fulltext-engine --all-targets -- -D warnings
cargo check -p fulltext-engine --target wasm32-unknown-unknown
```
