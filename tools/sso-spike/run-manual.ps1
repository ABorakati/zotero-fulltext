<#
.SYNOPSIS
  sso-spike MANUAL mode: YOU sign in. Opens a visible throwaway Zotero (fresh temp profile and
  data directory; your real Zotero and ~/Zotero are not touched) with a control window.

.DESCRIPTION
  For each step the control window opens a sign-in window (a custom window or Zotero's own
  viewer). You sign in with your KCL account and MFA. When the article page or PDF shows, press
  "Finished signing in: run checks". The plugin then tests whether Zotero's download paths reuse
  the session. Press "Write report and quit" when done.

  Privacy: the report keeps cookie NAMES and flags, redacted URLs and pass/fail facts. It never
  keeps passwords, cookie values, SAML messages, tokens or page text. The throwaway profile is
  set to forget passwords and to clear cookies, cache and history on quit, and this script then
  deletes the whole run directory (profile, data, logs) unless you pass -KeepRunDir.

.EXAMPLE
  pwsh -NoProfile -File tools\sso-spike\run-manual.ps1
#>
[CmdletBinding()]
param(
    [int]$TimeoutMin = 120,
    [switch]$KeepRunDir
)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$harness = Join-Path (Split-Path $here -Parent) 'zotero-harness\run.ps1'
$runName = 'sso-manual-' + (Get-Date -Format 'yyyyMMdd-HHmmss')
$resultsDir = Join-Path $here 'results'
New-Item -ItemType Directory -Force $resultsDir | Out-Null

$prefs = @{
    'extensions.sso-spike.mode'                     = 'manual'
    # never store or offer to store credentials or form data in the throwaway profile
    'signon.rememberSignons'                        = $false
    'signon.autofillForms'                          = $false
    'browser.formfill.enable'                       = $false
    # wipe session state when this Zotero quits
    'privacy.sanitize.sanitizeOnShutdown'           = $true
    'privacy.clearOnShutdown.cookies'               = $true
    'privacy.clearOnShutdown.cache'                 = $true
    'privacy.clearOnShutdown.sessions'              = $true
    'privacy.clearOnShutdown.history'               = $true
    'privacy.clearOnShutdown.formdata'              = $true
    'privacy.clearOnShutdown.offlineApps'           = $true
    'privacy.clearOnShutdown_v2.cookiesAndStorage'  = $true
    'privacy.clearOnShutdown_v2.cache'              = $true
}

# -NoDebugText keeps Zotero's verbose debug output (which can contain full URLs) out of the logs.
& $harness -PluginDir (Join-Path $here 'plugin') -PluginId 'sso-spike@zotero-fulltext.local' `
    -RunName $runName -TimeoutSec ($TimeoutMin * 60) -Headless:$false -GraceSec 45 -NoDebugText `
    -LogTailLines 0 -ExtraPrefs $prefs
$code = $LASTEXITCODE

$runsRoot = Join-Path (Split-Path (Split-Path $here -Parent) -Parent) '.ref\harness-runs'
$runDir = Join-Path $runsRoot $runName
$result = Join-Path $runDir 'result.json'
$partial = "$result.partial.json"
$saved = $null
if (Test-Path $result) { $saved = Join-Path $resultsDir "$runName.json"; Copy-Item $result $saved }
elseif (Test-Path $partial) { $saved = Join-Path $resultsDir "$runName.partial.json"; Copy-Item $partial $saved }

if (-not $KeepRunDir -and (Test-Path $runDir)) {
    Remove-Item -Recurse -Force $runDir
    Write-Host "[sso-spike] deleted the throwaway run directory $runDir"
}
if ($saved) {
    Write-Host "[sso-spike] report: $saved"
    Write-Host '[sso-spike] Please look through it before sharing. It should hold no secrets; tell Claude if you spot one.'
} else {
    Write-Warning '[sso-spike] no report was written.'
}
exit $code
