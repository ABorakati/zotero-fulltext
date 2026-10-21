<#
.SYNOPSIS
  Records EVERY window a real "Get Full Text" run opens, in a THROWAWAY Zotero 10.

.DESCRIPTION
  Same two-plugin trick as login-probe.ps1 / e2e.ps1 (real plugin + tools\zotero-harness\window-probe\plugin driver, via a
  Start-Process shim). The driver creates journal items for paywalled DOIs, selects them, builds the real item context menu and fires
  the real "Get Full Text" menu command, then logs every window (windowtype, documentURI, title, size, readyState, opener stack,
  PNG in <run>\shots) plus wrapped Services.ww.openWindow / Services.prompt / Zotero.openInViewer calls and console errors.
  It never types into a page and never signs in; sign-in windows are only sampled and closed.
  The user's own Zotero, profile and ~/Zotero are never touched (run.ps1's safety model).

.PARAMETER Dois
  JSON array of {doi, title?}, or the path of a JSON file with that array. Default: Wiley (x2), LWW, Lancet.
.PARAMETER Experiment
  Path of a JS file (body of an async function with Zotero, Services, S, sleep, plog, R, tick in scope) run once before the menu
  path. Throwaway profile only; used to reproduce a window-opening situation in isolation.
.PARAMETER SkipMenu
  Do not create items or fire the menu command (experiment only).
.PARAMETER RunSec
  Hard cap in seconds for the run phase (default 240).
.PARAMETER PluginDir
  zotero-fulltext plugin: a directory (default <repo>\plugin) or a packed .xpi.
.PARAMETER Headless
  ON by default here. Use -Headless:$false for a visible window.

.EXAMPLE
  pwsh -NoProfile -File tools\zotero-harness\window-probe.ps1 -PluginDir dist\zotero-fulltext-0.1.0.xpi
#>
[CmdletBinding()]
param(
    [string]$RunName,
    [string]$Dois = '',
    [string]$Experiment = '',
    [switch]$SkipMenu,
    [int]$RunSec = 240,
    [int]$TimeoutSec = 420,
    [bool]$Headless = $true,
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
$probeDir = Join-Path $harnessDir 'window-probe\plugin'

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
if (-not $RunName) { $RunName = 'window-probe-' + (Get-Date -Format 'yyyyMMdd-HHmmss') }
$runDir = Join-Path (Join-Path $repoRoot '.ref\harness-runs') $RunName
$resultFile = Join-Path $runDir 'result.json'
$shotDir = Join-Path $runDir 'shots'

$extraPrefs = @{
    'extensions.zotero-harness.resultFile' = ''
    'extensions.zotero-wprobe.resultFile'  = $resultFile
    'extensions.zotero-wprobe.shotDir'     = $shotDir
    'extensions.zotero-wprobe.runSec'      = $RunSec
    'extensions.zotero-wprobe.dois'        = $(if ($Dois -and $Dois.Trim()[0] -ne '[') { (Resolve-Path $Dois).Path } else { $Dois })
    'extensions.zotero-wprobe.experiment'  = $(if ($Experiment) { (Resolve-Path $Experiment).Path } else { '' })
    'extensions.zotero-wprobe.skipMenu'    = $(if ($SkipMenu) { 'true' } else { '' })
}

$runArgs = @{
    PluginDir  = $probeDir
    PluginId   = $probeId
    ResultFile = $resultFile
    TimeoutSec = $TimeoutSec
    RunName    = $RunName
    ExtraPrefs = $extraPrefs
    GraceSec   = 30
    Headless   = $Headless
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

Write-Host "[window-probe] run $RunName | headless $Headless | plugin $zftDir"
$harnessExit = $null
try {
    & $runPs1 @runArgs
    $harnessExit = $LASTEXITCODE
} catch {
    Write-Host "[window-probe] run.ps1 threw: $_" -ForegroundColor Red
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
function P($o, [string]$n) { if ($null -ne $o -and $o.PSObject.Properties[$n]) { $o.$n } else { '' } }
Write-Host "result: $resultFile"
Write-Host "screenshots: $shotDir"
if ($res.PSObject.Properties['fatal']) { Write-Host "FATAL: $($res.fatal)" -ForegroundColor Red }
Write-Host ''
Write-Host 'WINDOWS (excluding the main window):' -ForegroundColor Cyan
foreach ($w in @($res.windows)) {
    $l = if ($w.PSObject.Properties['latest']) { $w.latest } else { $w.atOpen }
    $title = P $l 'title'
    $sz = if ($l -and $l.PSObject.Properties['outer']) { ($l.outer -join 'x') } else { '' }
    Write-Host ("  {0} +{1}ms type='{2}' uri='{3}' title='{4}' outer={5} state={6}" -f $w.id, (P $w 'firstSeenMs'), (P $l 'windowtype'), (P $l 'documentURI'), $title, $sz, (P $l 'readyState'))
    $top = @(P $w 'openerStack')
    if ($top.Count) { Write-Host ("      opener: " + (($top | Select-Object -First 3) -join ' <- ')) -ForegroundColor DarkGray }
    foreach ($s in @(P $w 'shots')) { if ($s -and $s.PSObject.Properties['path']) { Write-Host "      png: $($s.path)" -ForegroundColor DarkGray } }
}
Write-Host ''
Write-Host "calls: $(@($res.calls).Count)  consoleErrors: $(@($res.consoleErrors).Count)"
exit 0
