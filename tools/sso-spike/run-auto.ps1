<#
.SYNOPSIS
  sso-spike AUTO mode: no login, no human. Runs in a throwaway headless Zotero via the harness.
  Follows each unauthenticated start URL up to the identity provider's sign-in form and records
  the chain, cookies (names/flags only) and pre-login download baselines. Non-GET requests to
  identity-provider hosts are cancelled inside Zotero before they are sent.
.EXAMPLE
  pwsh -NoProfile -File tools\sso-spike\run-auto.ps1
#>
[CmdletBinding()]
param(
    [switch]$Visible,
    [int]$TimeoutSec = 900
)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$harness = Join-Path (Split-Path $here -Parent) 'zotero-harness\run.ps1'
$runName = 'sso-auto-' + (Get-Date -Format 'yyyyMMdd-HHmmss')
$resultsDir = Join-Path $here 'results'
New-Item -ItemType Directory -Force $resultsDir | Out-Null

& $harness -PluginDir (Join-Path $here 'plugin') -PluginId 'sso-spike@zotero-fulltext.local' `
    -RunName $runName -TimeoutSec $TimeoutSec -Headless:(-not $Visible) -LogTailLines 15 `
    -ExtraPrefs @{ 'extensions.sso-spike.mode' = 'auto' }
$code = $LASTEXITCODE

$runsRoot = Join-Path (Split-Path (Split-Path $here -Parent) -Parent) '.ref\harness-runs'
$result = Join-Path $runsRoot "$runName\result.json"
if (Test-Path $result) {
    Copy-Item $result (Join-Path $resultsDir "$runName.json")
    Write-Host "[sso-spike] result copied to $(Join-Path $resultsDir "$runName.json")"
}
exit $code
