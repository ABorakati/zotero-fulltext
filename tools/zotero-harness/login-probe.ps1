<#
.SYNOPSIS
  Opens zotero-fulltext's REAL sign-in window on PUBLIC pages in a THROWAWAY Zotero 10 and records what it renders.

.DESCRIPTION
  Same two-plugin trick as e2e.ps1 (real plugin + tools\zotero-harness\login-probe\plugin driver, via a Start-Process shim).
  The driver calls ZFT.login.open() once per scenario (a 'challenge' and a 'login' request by default), waits -WaitSec seconds,
  then records browser.currentURI, remoteness, the origin/TLS bar, the banner, the page title, focus state, the
  "[zotero-fulltext] login" debug lines and a PNG of the window (<run>\shots\). It never types into a page and never signs in.
  The user's own Zotero, profile and ~/Zotero are never touched (run.ps1's safety model).

.PARAMETER Scenarios
  JSON array of {name, kind ('challenge'|'login'), url, waitSec?}. Default: example.org as challenge and login, and a
  Cloudflare-protected Europe PMC PDF URL as challenge.
.PARAMETER WaitSec
  Seconds each window is left open before it is inspected (default 10).
.PARAMETER PluginDir
  zotero-fulltext plugin: a directory (default <repo>\plugin) or a packed .xpi.
.PARAMETER Headless
  OFF by default here (rendering needs a real window). Use -Headless for headless.

.EXAMPLE
  pwsh -NoProfile -File tools\zotero-harness\login-probe.ps1 -PluginDir dist\zotero-fulltext-0.1.0.xpi
#>
[CmdletBinding()]
param(
    [string]$RunName,
    [string]$Scenarios = '',
    [int]$WaitSec = 10,
    [int]$TimeoutSec = 240,
    [switch]$Headless,
    [switch]$CookieTest,
    [switch]$ShowHarness,
    [string]$ZoteroExe,
    [string]$PluginDir
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3.0

$harnessDir = $PSScriptRoot
$repoRoot = (Resolve-Path (Join-Path $harnessDir '..\..')).Path
$runPs1 = Join-Path $harnessDir 'run.ps1'
$zftDir = if ($PluginDir) { (Resolve-Path $PluginDir).Path } else { Join-Path $repoRoot 'plugin' }
$probeDir = Join-Path $harnessDir 'login-probe\plugin'

function Get-PluginId([string]$dir) {
    if ($dir -match '\.xpi$') {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($dir)
        try {
            $entry = $zip.GetEntry('manifest.json')
            $reader = [IO.StreamReader]::new($entry.Open())
            try { $text = $reader.ReadToEnd() } finally { $reader.Dispose() }
        } finally { $zip.Dispose() }
        return ($text | ConvertFrom-Json).applications.zotero.id
    }
    (Get-Content (Join-Path $dir 'manifest.json') -Raw | ConvertFrom-Json).applications.zotero.id
}

$zftId = Get-PluginId $zftDir
$probeId = Get-PluginId $probeDir
$zftIsXpi = $zftDir -match '\.xpi$'
if (-not $RunName) { $RunName = 'login-probe-' + (Get-Date -Format 'yyyyMMdd-HHmmss') }
$runDir = Join-Path (Join-Path $repoRoot '.ref\harness-runs') $RunName
$resultFile = Join-Path $runDir 'result.json'
$shotDir = Join-Path $runDir 'shots'

$extraPrefs = @{
    'extensions.zotero-harness.resultFile' = ''
    'extensions.zotero-probe.resultFile'   = $resultFile
    'extensions.zotero-probe.shotDir'      = $shotDir
    'extensions.zotero-probe.waitSec'      = $WaitSec
    'extensions.zotero-probe.scenarios'    = $Scenarios
    'extensions.zotero-probe.cookieTest'   = $(if ($CookieTest) { 'true' } else { '' })
}

$runArgs = @{
    PluginDir  = $probeDir
    PluginId   = $probeId
    ResultFile = $resultFile
    TimeoutSec = $TimeoutSec
    RunName    = $RunName
    ExtraPrefs = $extraPrefs
    GraceSec   = 30
    Headless   = [bool]$Headless
}
if ($ZoteroExe) { $runArgs.ZoteroExe = $ZoteroExe }
if (-not $ShowHarness) { $runArgs.Quiet = $true }

$global:ZftProbeShim = @{ PluginId = $zftId; PluginDir = $zftDir; IsXpi = $zftIsXpi; Installed = $false }
function Start-Process {
    [CmdletBinding()]
    param(
        [Parameter(Position = 0)] [string]$FilePath,
        [Parameter(Position = 1)] [string[]]$ArgumentList,
        [switch]$PassThru,
        [string]$RedirectStandardOutput,
        [string]$RedirectStandardError
    )
    $shim = $global:ZftProbeShim
    if (-not $shim.Installed) {
        $line = $ArgumentList -join ' '
        if ($line -notmatch '-profile\s+"([^"]+)"') { throw 'probe shim: no -profile "<dir>" in the Zotero command line' }
        $extDir = Join-Path $Matches[1] 'extensions'
        New-Item -ItemType Directory -Force $extDir | Out-Null
        if ($shim.IsXpi) { Copy-Item -LiteralPath $shim.PluginDir -Destination (Join-Path $extDir ($shim.PluginId + '.xpi')) -Force }
        else { [IO.File]::WriteAllText((Join-Path $extDir $shim.PluginId), $shim.PluginDir, [Text.UTF8Encoding]::new($false)) }
        $shim.Installed = $true
    }
    Microsoft.PowerShell.Management\Start-Process @PSBoundParameters
}

Write-Host "[login-probe] run $RunName | wait $WaitSec s | headless $([bool]$Headless) | plugin $zftDir"
$harnessExit = $null
try {
    & $runPs1 @runArgs
    $harnessExit = $LASTEXITCODE
} catch {
    Write-Host "[login-probe] run.ps1 threw: $_" -ForegroundColor Red
    $harnessExit = 99
} finally {
    Remove-Item Function:\Start-Process -ErrorAction SilentlyContinue
    Remove-Variable ZftProbeShim -Scope Global -ErrorAction SilentlyContinue
}

if (-not (Test-Path $resultFile)) {
    Write-Host "NO RESULT FILE (run.ps1 exit $harnessExit). Logs: $runDir\logs" -ForegroundColor Red
    exit 2
}
$res = Get-Content $resultFile -Raw | ConvertFrom-Json
# Property read that tolerates a missing property (JSON drops undefined values; strict mode throws on them).
function P($o, [string]$n) { if ($null -ne $o -and $o.PSObject.Properties[$n]) { $o.$n } else { '' } }
Write-Host "result: $resultFile"
Write-Host "screenshots: $shotDir"
if ($res.PSObject.Properties['fatal']) { Write-Host "FATAL: $($res.fatal)" -ForegroundColor Red }
if ($res.PSObject.Properties['cookieTest']) { Write-Host ('cookieTest: ' + ($res.cookieTest | ConvertTo-Json -Compress -Depth 6)) }
foreach ($s in @($res.scenarios)) {
    Write-Host ''
    $what = if ($s.PSObject.Properties['url']) { $s.url } elseif ($s.PSObject.Properties['doi']) { 'DOI ' + $s.doi } else { '' }
    Write-Host ("== {0} ({1}) {2}" -f $s.name, $s.kind, $what) -ForegroundColor Cyan
    if ($s.PSObject.Properties['driverError']) { Write-Host "  driver error: $($s.driverError)" -ForegroundColor Red; continue }
    if ($s.PSObject.Properties['noWindow']) { Write-Host '  NO WINDOW OPENED' -ForegroundColor Red }
    if ($s.PSObject.Properties['result']) { Write-Host ("  hidden load result: " + ($s.result | ConvertTo-Json -Compress -Depth 4)) }
    if ($s.PSObject.Properties['error']) { Write-Host ("  hidden load error: " + ($s.error | ConvertTo-Json -Compress -Depth 4)) }
    $samples = if ($s.PSObject.Properties['samples']) { @($s.samples) } else { @() }
    foreach ($m in $samples) {
        if ($null -eq $m) { continue }
        $uri = [string](P $m 'currentURI'); if ($uri.Length -gt 100) { $uri = $uri.Substring(0, 100) + '...' }
        $extra = ''
        if ($m.PSObject.Properties['rect'] -and $m.rect) { $extra += " browser=$($m.rect.w)x$($m.rect.h)" }
        if ($m.PSObject.Properties['tip']) { $extra += " tip=$($m.tip)" }
        if ($m.PSObject.Properties['error'] -and $m.error) { $extra += " ERROR='$($m.error)'" }
        if ($m.PSObject.Properties['progressBanner'] -and $m.progressBanner) { $extra += " progressBanner=$($m.progressBanner | ConvertTo-Json -Compress)" }
        Write-Host ("  +{0,5} ms  uri={1}  remote={2}  title='{3}'  origin='{4}'  secure='{5}'  active={6}  hint='{7}'{8}" -f (P $m 'atMs'), $uri, (P $m 'remote'), (P $m 'pageTitle'), (P $m 'origin'), (P $m 'secure'), (P $m 'active'), (P $m 'hint'), $extra)
    }
    if ($s.PSObject.Properties['closedAtMs']) { Write-Host "  window CLOSED at $($s.closedAtMs) ms; open() settled: $($s.settledEarly | ConvertTo-Json -Compress -Depth 4)" }
    elseif ($s.PSObject.Properties['afterCancel']) { Write-Host "  still open at the end; after cancel: $($s.afterCancel | ConvertTo-Json -Compress -Depth 4)" }
    foreach ($l in @($s.debug.lines)) { Write-Host "  dbg: $l" -ForegroundColor DarkGray }
}
exit 0
