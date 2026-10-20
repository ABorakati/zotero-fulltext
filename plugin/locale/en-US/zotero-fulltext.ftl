# zotero-fulltext Fluent strings (en-US). One file, plugin-unique name, directly in locale/en-US (Zotero registers it
# automatically; xpcom/plugins.js registerLocales). Message ids are a CONTRACT: the Rust engine emits the
# status and action ids below in proto.rs ProgressRow.l10n and RowAction.l10n (docs/design/plugin-js.md section 8);
# tests/js/ftl.test.mjs fails if any "zotero-fulltext-..." string literal in plugin/ or crates/ has no message here.

## Menus (MenuManager l10nID: the label is the .label attribute)

zotero-fulltext-menu-get =
    .label = Get Full Text
zotero-fulltext-menu-get-oa =
    .label = Get Full Text (open access only)
zotero-fulltext-menu-signin =
    .label = Sign In to King's for Full Text{ "\u2026" }
zotero-fulltext-menu-progress =
    .label = Show Full Text Progress
zotero-fulltext-menu-prefs =
    .label = Full Text Preferences{ "\u2026" }

## Row status text. These ids are emitted by the Rust engine (crates/fulltext-engine/src/l10n.rs, ProgressRow.l10n);
## the arguments are the Fluent variables named in that file. tests/js/ftl.test.mjs fails if an id is missing here.

zotero-fulltext-status-queued = Waiting in the queue
zotero-fulltext-status-waiting-confirm = Waiting for your confirmation
zotero-fulltext-status-checking = Checking the item
zotero-fulltext-status-searching-oa = Looking for an open-access copy
zotero-fulltext-status-downloading = Downloading from { $host }
zotero-fulltext-status-waiting-host = Waiting { $seconds } s before contacting { $host }
zotero-fulltext-status-waiting-login = Waiting for you to sign in
zotero-fulltext-status-signing-in = Signing in
zotero-fulltext-status-institutional = Trying institutional access
zotero-fulltext-status-challenge = The publisher is asking for a human check
zotero-fulltext-status-saving = Attaching the PDF
zotero-fulltext-status-network-down = No network connection; waiting

## Terminal row text, one per Outcome variant

zotero-fulltext-status-saved-oa = Attached an open-access PDF
zotero-fulltext-status-saved-inst = Attached a PDF through your institution
zotero-fulltext-status-skipped-has-pdf = Skipped: already has a PDF
zotero-fulltext-status-skipped-not-downloaded = Skipped: its PDF is synced but not downloaded yet
zotero-fulltext-status-not-editable = Skipped: the library does not allow file changes
zotero-fulltext-status-skipped-not-regular = Skipped: not a top-level item
zotero-fulltext-status-skipped-in-progress = Skipped: already being processed
zotero-fulltext-status-no-identifier = No DOI, PMID, PMCID or arXiv ID to search with
zotero-fulltext-status-not-found = No full text found
zotero-fulltext-status-not-found-oa-only = No open-access copy found (institutional access was off for this run)
zotero-fulltext-status-not-entitled = Your institution has no access to this article
zotero-fulltext-status-needs-login = Sign-in needed
zotero-fulltext-status-login-declined = Sign-in was skipped
zotero-fulltext-status-login-timeout = Sign-in took too long
zotero-fulltext-status-login-refused = Your institution refused the sign-in ({ $code })
zotero-fulltext-status-login-ineffective = Signed in, but the publisher still did not grant access
zotero-fulltext-status-misconfigured = Institution settings need attention
zotero-fulltext-status-needs-user = Needs you: open it in your browser
zotero-fulltext-status-no-pdf-link = Found the article page but no PDF link
zotero-fulltext-status-group-library = Not saved into a group library without your confirmation
zotero-fulltext-status-engine-restarted = The full-text engine restarted; run this item again
zotero-fulltext-status-blocked = { $host } blocked automated access
zotero-fulltext-status-cap-reached = { $cap ->
        [hourly] Waiting: hourly institutional download limit reached
        [daily] Waiting: daily institutional download limit reached
        [mb_per_hour] Waiting: institutional data limit for this hour reached
       *[batch] Not started: batch size limit reached
    }
zotero-fulltext-status-halted = Institutional access paused to protect your library's licence
zotero-fulltext-status-cancelled = Cancelled
zotero-fulltext-status-timed-out = The publisher took too long to answer
zotero-fulltext-status-failed = Failed ({ $code })

## Row action buttons (RowAction kinds and RowAction.OpenBrowser.l10n)

zotero-fulltext-action-open-browser = Open in browser
zotero-fulltext-action-open-redirector = Open with King's sign-in
zotero-fulltext-action-open-ezproxy = Open through the library proxy
zotero-fulltext-action-open-libkey = Open in LibKey
zotero-fulltext-action-open-doi = Open the article page
zotero-fulltext-action-attach-file = Attach a file you downloaded{ "\u2026" }
zotero-fulltext-action-sign-in = Sign in
zotero-fulltext-action-retry = Try again
zotero-fulltext-action-cancel-item = Cancel this item

## Progress window

zotero-fulltext-progress-title = Get Full Text
zotero-fulltext-progress-header = { $done } of { $total } finished { "\u00B7" } { $working } in progress { "\u00B7" } { $waiting } waiting for you
zotero-fulltext-progress-cancel = Cancel all
zotero-fulltext-progress-close = Close

## Final report

zotero-fulltext-report-summary = { $saved } attached { "\u00B7" } { $needs } need you { "\u00B7" } { $notfound } not found { "\u00B7" } { $failed } failed { "\u00B7" } { $skipped } skipped
zotero-fulltext-report-group-attached = Attached
zotero-fulltext-report-group-needs-you = Needs you
zotero-fulltext-report-group-not-found = Not found
zotero-fulltext-report-group-failed = Failed
zotero-fulltext-report-group-skipped = Skipped
zotero-fulltext-report-ledger = Institutional downloads: { $hour } of { $hour_cap } this hour, { $day } of { $day_cap } in 24 hours, { $mb } of { $mb_cap } MB this hour
zotero-fulltext-report-copy-diagnostics = Copy diagnostics

## Sign-in window

zotero-fulltext-login-title = Sign in for full text
zotero-fulltext-login-waiting = { $count ->
        [one] { $count } item is waiting for this sign-in
       *[other] { $count } items are waiting for this sign-in
    }
zotero-fulltext-login-skip = Skip sign-in
zotero-fulltext-login-keep-waiting = Keep waiting
zotero-fulltext-login-open-browser = Open in my browser
zotero-fulltext-login-challenge-hint = The site is asking for a human check. Please complete it in this window.
zotero-fulltext-login-ca-blocked = Your institution blocked this sign-in ({ $code }). Try the browser or the library VPN.
zotero-fulltext-login-popups = Pop-ups opened: { $count }
zotero-fulltext-login-secure = Secure connection
zotero-fulltext-login-not-secure = Connection NOT secure

## Confirmation dialogs

zotero-fulltext-confirm-large-batch-title = Get full text for { $n } items?
zotero-fulltext-confirm-large-batch-body = You selected { $n } items. Institutional downloads are limited to { $cap } per batch to respect your library's licence; open-access copies are not limited.
zotero-fulltext-confirm-group-title = Save into a group library?
zotero-fulltext-confirm-group-body = Some items are in the group library "{ $library }". Publisher PDFs obtained through your institution may not be shared with a group.
zotero-fulltext-confirm-yes = Continue
zotero-fulltext-confirm-save-anyway = Save anyway
zotero-fulltext-confirm-oa-only = Open access only
zotero-fulltext-confirm-cancel = Cancel

## Notices

zotero-fulltext-notice-no-eligible = Select at least one top-level item.
zotero-fulltext-notice-wasm-load-failed = The full-text engine could not be loaded. Please restart Zotero; if this continues, report it.
zotero-fulltext-notice-engine-failed = The full-text engine stopped unexpectedly. Items that were not finished are marked so you can run them again.
zotero-fulltext-notice-start-failed = Could not start: { $reason }

## Preferences pane

zotero-fulltext-prefs-title = Full Text
zotero-fulltext-prefs-oa-heading = Open-access sources
zotero-fulltext-prefs-oa-openalex = OpenAlex
zotero-fulltext-prefs-oa-openalex-key = OpenAlex API key (optional)
zotero-fulltext-prefs-oa-europepmc = Europe PMC
zotero-fulltext-prefs-oa-europepmc-get-pdf = Europe PMC PDF endpoint
zotero-fulltext-prefs-oa-biorxiv = bioRxiv and medRxiv
zotero-fulltext-prefs-oa-medrxiv-early = bioRxiv/medRxiv early-release PDFs
zotero-fulltext-prefs-oa-pmc-s3 = PubMed Central open data
zotero-fulltext-prefs-oa-arxiv = arXiv
zotero-fulltext-prefs-oa-zotero-oa = Zotero's open-access lookup
zotero-fulltext-prefs-oa-unpaywall-email = Unpaywall email (leave empty to disable Unpaywall)
zotero-fulltext-prefs-oa-semantic-scholar-key = Semantic Scholar API key (optional)
zotero-fulltext-prefs-oa-core-key = CORE API key (optional)
zotero-fulltext-prefs-oa-version-order = Preferred versions, in order
zotero-fulltext-prefs-inst-heading = Institutional access
zotero-fulltext-prefs-inst-enabled = Use institutional access when no open-access copy is found
zotero-fulltext-prefs-inst-try-direct = Try the publisher directly first (campus network or an existing session)
zotero-fulltext-prefs-inst-use-openathens = Use the OpenAthens redirector
zotero-fulltext-prefs-inst-use-ezproxy = Use the library proxy (EZproxy) for the hosts it covers
zotero-fulltext-prefs-inst-check-alma = Check the library catalogue before trying a publisher
zotero-fulltext-prefs-inst-login-mode = When sign-in is needed
zotero-fulltext-prefs-inst-login-mode-window = Open a sign-in window
zotero-fulltext-prefs-inst-login-mode-handoff = Give me links to open in my browser
zotero-fulltext-prefs-inst-auto-open-login = Open the sign-in window automatically
zotero-fulltext-prefs-inst-login-timeout = Wait for sign-in (seconds)
zotero-fulltext-prefs-inst-group-libraries = Items in group libraries
zotero-fulltext-prefs-inst-group-ask = Ask first
zotero-fulltext-prefs-inst-group-never = Never use institutional access
zotero-fulltext-prefs-inst-group-allow = Allow
zotero-fulltext-prefs-inst-challenge-windows = Let me complete human checks in a window
zotero-fulltext-prefs-limits-heading = Licence-friendly limits for institutional downloads
zotero-fulltext-prefs-inst-confirm-above = Ask for confirmation above (items)
zotero-fulltext-prefs-inst-batch-max = Maximum institutional items per batch
zotero-fulltext-prefs-inst-hourly-cap = Maximum per hour
zotero-fulltext-prefs-inst-daily-cap = Maximum per 24 hours
zotero-fulltext-prefs-inst-mb-per-hour = Maximum megabytes per hour
zotero-fulltext-prefs-inst-min-gap = Minimum seconds between PDFs from one publisher
zotero-fulltext-prefs-run-heading = Speed
zotero-fulltext-prefs-run-item-concurrency = Items processed at the same time
zotero-fulltext-prefs-run-hedge = Try a second source when the first is slow
zotero-fulltext-prefs-run-pdf-max-mb = Largest PDF to download (MB)
zotero-fulltext-prefs-institution-heading = Institution
zotero-fulltext-prefs-institution-name = Name
zotero-fulltext-prefs-institution-openathens-domain = OpenAthens domain
zotero-fulltext-prefs-institution-ezproxy-login-host = EZproxy login host
zotero-fulltext-prefs-institution-ezproxy-suffix = EZproxy host suffix
zotero-fulltext-prefs-institution-ezproxy-hosts = Hosts covered by EZproxy (comma-separated)
zotero-fulltext-prefs-institution-alma-base = Library catalogue (Alma) address
zotero-fulltext-prefs-institution-alma-inst = Library catalogue institution code
zotero-fulltext-prefs-institution-libkey-library-id = LibKey library number
zotero-fulltext-prefs-institution-idp-hosts = Sign-in provider hosts (comma-separated)
zotero-fulltext-prefs-institution-entra-tenant = Microsoft Entra tenant ID
zotero-fulltext-prefs-institution-passthrough-hosts = Publishers that need no redirector (comma-separated)
zotero-fulltext-prefs-institution-warmup-url = Sign-in start address (empty for the default)
zotero-fulltext-prefs-reset-institution = Restore King's College London defaults
zotero-fulltext-prefs-clear-state = Forget sign-in pauses and download counters
zotero-fulltext-prefs-open-data-folder = Open the plugin data folder
zotero-fulltext-prefs-privacy-note = The OpenAthens redirector learns which article addresses you open. Nothing is sent anywhere else, and no passwords are ever stored by this plugin.
zotero-fulltext-prefs-debug-heading = Troubleshooting
zotero-fulltext-prefs-debug-trace = Record detailed per-item traces in the Zotero debug output
zotero-fulltext-prefs-debug-inspect-in-worker = Analyse web pages in a background worker
zotero-fulltext-prefs-problem-openathens-domain = The OpenAthens domain is empty while institutional access is on.
zotero-fulltext-prefs-problem-login-timeout = The sign-in wait is too short (minimum 30 seconds).
zotero-fulltext-prefs-problem-unpaywall-email = The Unpaywall email does not look like an email address.

## Preferences pane: control texts and messages (js-shell)

zotero-fulltext-prefs-show-secret = Show
zotero-fulltext-prefs-hide-secret = Hide
zotero-fulltext-prefs-unavailable = The Full Text plugin is not running. Restart Zotero to change these settings.
zotero-fulltext-prefs-clear-state-confirm = Forget the saved sign-in pauses and the institutional download counters? The hourly and daily limits protect your library's licence, so only continue if you are sure.
zotero-fulltext-prefs-state-cleared = The saved sign-in pauses and download counters were cleared. Restart Zotero to apply this fully.
zotero-fulltext-prefs-institution-restored = King's College London defaults were restored.
