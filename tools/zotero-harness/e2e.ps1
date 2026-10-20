<#
.SYNOPSIS
  End-to-end test of zotero-fulltext in a THROWAWAY Zotero 10 (temp profile + temp data dir), open-access sources only.

.DESCRIPTION
  Builds nothing: it assumes plugin\content\fulltext.wasm exists (build it first).

  Loads two plugins into the throwaway profile:
    * zotero-fulltext@aborakati   the real plugin, from <repo>\plugin (unpacked, extension proxy file)
    * e2e@zotero-fulltext.local   the driver, from tools\zotero-harness\e2e\plugin
  run.ps1 installs only one plugin, so this script wraps it: for the duration of the call it shadows Start-Process with a
  function that writes the second proxy file into the profile just before zotero.exe starts, then delegates to the real
  cmdlet. run.ps1 itself is not modified. Everything else (isolation, process safety, user-Zotero PID and ini checks) is
  run.ps1's. The user's own Zotero, profile and ~/Zotero are never touched. No login, no SSO, no institutional access,
  no Unpaywall email (so no personal address is sent anywhere).

  The driver (e2e\plugin\bootstrap.js) waits for zotero-fulltext, switches institutional access off by prefs, creates four
  items (gold OA 10.1371/journal.pmed.1000097, arXiv 10.48550/arXiv.1706.03762, medRxiv 10.1101/2023.05.31.23290770,
  paywalled 10.1097/SLA.0000000000005603), selects them and clicks the real "Get Full Text" entry of the item context
  menu, waits for the RunReport, inspects the PDF attachments, windows, dialogs, console errors, debug log and event-loop
  lag, writes <run>\result.json and quits Zotero.

.PARAMETER TimeoutSec
  Seconds the driver waits for the RunReport (default 240). The harness timeout is this + 180.
.PARAMETER Mode
  'full' (the "Get Full Text" entry, default) or 'oa_only' (the "open access only" entry). Institutional access is off either way.
.PARAMETER Cases
  Comma list of case keys to run: gold, arxiv, medrxiv, paywalled (default: all four).
.PARAMETER Headless
  On by default. Use -Headless:$false for a visible window.
.PARAMETER NoSmoke
  Skip zotero-fulltext's own smoke checks (modules, wasm, prefs, menu, menu_ui, prefpane, ftl) after the run.
.PARAMETER NoQuit
  Leave Zotero running after the result is written (run.ps1 then stops it after its grace period).
.PARAMETER Trace
  Turn on the plugin's engine trace pref (debug.trace) for more detail in the debug log.
.PARAMETER PluginDir
  Directory of the zotero-fulltext plugin to load (default: <repo>\plugin), or a packed .xpi (copied into the profile like a real install).
.PARAMETER ShowHarness
  Show run.ps1's own console output (it is quiet by default; the summary below uses harness-summary.json).

.EXAMPLE
  pwsh -NoProfile -File tools\zotero-harness\e2e.ps1
.EXAMPLE
  pwsh -NoProfile -File tools\zotero-harness\e2e.ps1 -Cases gold,paywalled -Headless:$false

  Exit codes: 0 PASS (warnings allowed), 1 FAIL, 2 no result file (timeout or Zotero exited).
#>
[CmdletBinding()]
param(
    [string]$RunName,
    [string]$ResultFile,
    [int]$TimeoutSec = 240,
    [int]$HarnessTimeoutSec = 0,
    [ValidateSet('full', 'oa_only')] [string]$Mode = 'full',
    [string]$Cases = '',
    [switch]$Headless,
    [switch]$NoSmoke,
    [switch]$NoQuit,
    [switch]$Trace,
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
$e2eDir = Join-Path $harnessDir 'e2e\plugin'

function Get-PluginId([string]$dir) {
    if ($dir -match '\.xpi$') {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($dir)
        try {
            $entry = $zip.GetEntry('manifest.json')
            if (-not $entry) { throw "No manifest.json in $dir" }
            $reader = [IO.StreamReader]::new($entry.Open())
            try { $manifestText = $reader.ReadToEnd() } finally { $reader.Dispose() }
        } finally { $zip.Dispose() }
        $xpiId = ($manifestText | ConvertFrom-Json).applications.zotero.id
        if (-not $xpiId) { throw "No applications.zotero.id in manifest.json of $dir" }
        return $xpiId
    }
    $manifest = Join-Path $dir 'manifest.json'
    if (-not (Test-Path $manifest)) { throw "No manifest.json in $dir" }
    $id = (Get-Content $manifest -Raw | ConvertFrom-Json).applications.zotero.id
    if (-not $id) { throw "No applications.zotero.id in $manifest" }
    $id
}
# Safe property read for objects parsed from JSON (Set-StrictMode throws on a missing property).
function Prop($obj, [string]$name) {
    if ($null -ne $obj -and $obj.PSObject.Properties[$name]) { return $obj.$name }
    return $null
}

$zftId = Get-PluginId $zftDir
$e2eId = Get-PluginId $e2eDir
$zftIsXpi = $zftDir -match '\.xpi$'
$wasm = Join-Path $zftDir 'content\fulltext.wasm'
if (-not $zftIsXpi -and (-not (Test-Path $wasm) -or (Get-Item $wasm).Length -lt 1024)) {
    throw "plugin\content\fulltext.wasm is missing or empty ($wasm). Build the wasm first; e2e.ps1 builds nothing."
}

if (-not $RunName) { $RunName = 'e2e-' + (Get-Date -Format 'yyyyMMdd-HHmmss') }
$runDir = Join-Path (Join-Path $repoRoot '.ref\harness-runs') $RunName
if (-not $ResultFile) { $ResultFile = Join-Path $runDir 'result.json' }
$ResultFile = [IO.Path]::GetFullPath($ResultFile)
if ($HarnessTimeoutSec -le 0) { $HarnessTimeoutSec = $TimeoutSec + 180 }

# user.js prefs on top of run.ps1's. The harness result pref is cleared on purpose: zotero-fulltext's built-in smoke harness
# (ZFT.harness.maybeRun) wakes up when it is set, would write the same file and quit Zotero before the driver is done. The
# driver reads its own pref instead (and runs the same smoke checks itself, after the run).
$extraPrefs = @{
    'extensions.zotero-harness.resultFile' = ''
    'extensions.zotero-e2e.resultFile'     = $ResultFile
    'extensions.zotero-e2e.timeoutSec'     = $TimeoutSec
    'extensions.zotero-e2e.mode'           = $Mode
    'extensions.zotero-e2e.cases'          = $Cases
    'extensions.zotero-e2e.smoke'          = (-not $NoSmoke.IsPresent)
}
if ($NoQuit) { $extraPrefs['extensions.zotero-harness.quitWhenDone'] = $false }
if ($Trace) { $extraPrefs['extensions.zotero-fulltext.debug.trace'] = $true }

$runArgs = @{
    PluginDir  = $e2eDir
    PluginId   = $e2eId
    ResultFile = $ResultFile
    TimeoutSec = $HarnessTimeoutSec
    RunName    = $RunName
    ExtraPrefs = $extraPrefs
    GraceSec   = 45
}
if ($PSBoundParameters.ContainsKey('Headless')) { $runArgs.Headless = [bool]$Headless }
if ($ZoteroExe) { $runArgs.ZoteroExe = $ZoteroExe }
if (-not $ShowHarness) { $runArgs.Quiet = $true }

# ---------------------------------------------------------------- second plugin via a Start-Process shim
# State lives in a global because the shim runs in run.ps1's scope chain, where $script: would mean run.ps1's own scope.
$global:ZftE2eShim = @{ PluginId = $zftId; PluginDir = $zftDir; IsXpi = $zftIsXpi; Installed = $false; Profile = '' }
function Start-Process {
    [CmdletBinding()]
    param(
        [Parameter(Position = 0)] [string]$FilePath,
        [Parameter(Position = 1)] [string[]]$ArgumentList,
        [switch]$PassThru,
        [string]$RedirectStandardOutput,
        [string]$RedirectStandardError
    )
    $shim = $global:ZftE2eShim
    if (-not $shim.Installed) {
        $line = $ArgumentList -join ' '
        if ($line -notmatch '-profile\s+"([^"]+)"') { throw 'e2e shim: no -profile "<dir>" in the Zotero command line' }
        $profileDir = $Matches[1]
        $extDir = Join-Path $profileDir 'extensions'
        New-Item -ItemType Directory -Force $extDir | Out-Null
        if ($shim.IsXpi) { Copy-Item -LiteralPath $shim.PluginDir -Destination (Join-Path $extDir ($shim.PluginId + '.xpi')) -Force }
        else { [IO.File]::WriteAllText((Join-Path $extDir $shim.PluginId), $shim.PluginDir, [Text.UTF8Encoding]::new($false)) }
        $shim.Installed = $true
        $shim.Profile = $profileDir
    }
    Microsoft.PowerShell.Management\Start-Process @PSBoundParameters
}

Write-Host "[e2e] run $RunName | mode $Mode | cases '$Cases' | driver timeout $TimeoutSec s | harness timeout $HarnessTimeoutSec s"
Write-Host "[e2e] plugins: $zftId ($zftDir) + $e2eId ($e2eDir)"
$harnessExit = $null
try {
    & $runPs1 @runArgs
    $harnessExit = $LASTEXITCODE
} catch {
    Write-Host "[e2e] run.ps1 threw: $_" -ForegroundColor Red
    $harnessExit = 99
} finally {
    Remove-Item Function:\Start-Process -ErrorAction SilentlyContinue
}
$shim = $global:ZftE2eShim
Remove-Variable ZftE2eShim -Scope Global -ErrorAction SilentlyContinue

# ---------------------------------------------------------------- report
$summaryFile = Join-Path $runDir 'harness-summary.json'
$hs = $null
if (Test-Path $summaryFile) { $hs = Get-Content $summaryFile -Raw | ConvertFrom-Json }
$logDir = Join-Path $runDir 'logs'
Write-Host ''

if (-not (Test-Path $ResultFile)) {
    Write-Host '==================== NO RESULT FILE ====================' -ForegroundColor Red
    Write-Host "run.ps1 exit code $harnessExit (2 = timed out, 3 = Zotero exited without a result, 99 = run.ps1 threw)"
    Write-Host "second plugin proxy installed: $($shim.Installed) ($($shim.Profile))"
    foreach ($log in @((Join-Path $logDir 'zotero-stdout.log'), (Join-Path $logDir 'zotero-stderr.log'))) {
        if ((Test-Path $log) -and (Get-Item $log).Length -gt 0) {
            Write-Host "---- tail 40 of $log"
            Get-Content $log -Tail 40 | Write-Host
        }
    }
    Write-Host 'RESULT: FAIL (no result file)' -ForegroundColor Red
    exit 2
}

$json = Get-Content $ResultFile -Raw
Write-Host "==================== e2e result ($ResultFile) ===================="
Write-Host $json
$res = $json | ConvertFrom-Json

$nPass = 0; $nWarn = 0; $nFail = 0
function Write-Check([string]$status, [string]$name, [string]$detail) {
    $color = switch ($status) { 'pass' { 'Green' } 'warn' { 'Yellow' } default { 'Red' } }
    Write-Host ('  [{0}] {1,-30} {2}' -f $status.ToUpper().PadRight(4), $name, $detail) -ForegroundColor $color
    switch ($status) { 'pass' { $script:nPass++ } 'warn' { $script:nWarn++ } default { $script:nFail++ } }
}

Write-Host ''
Write-Host '==================== e2e summary ===================='
$meta = Prop $res 'meta'
$run = Prop $res 'run'
Write-Host ("Zotero {0} | headless {1} | mode {2} | run {3} took {4} ms | driver total {5} ms | finished by {6}" -f
    (Prop $meta 'zotero'), (Prop $meta 'headless'), (Prop $meta 'mode'), (Prop $run 'runID'), (Prop $run 'elapsedMs'), (Prop $meta 'elapsedMs'), (Prop $meta 'finishedBy'))
Write-Host 'Items:'
foreach ($it in @(Prop $res 'items')) {
    if ($null -eq $it) { continue }
    $o = Prop $it 'outcome'
    $kind = if ($o) { Prop $o 'kind' } else { '(no report)' }
    $via = if ($kind -eq 'saved') { '{0} / {1} / {2}' -f (Prop $o 'source'), (Prop $o 'route'), (Prop $o 'version') } elseif ($o) { (($o.PSObject.Properties | Where-Object { $_.Name -in 'reason', 'why', 'cap', 'host', 'code', 'by' } | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ' ') } else { '' }
    $atts = @(Prop $it 'attachments')
    $attText = if ($atts.Count -gt 0 -and $null -ne $atts[0]) { ($atts | ForEach-Object { '{0} {1:N0} B head {2} eof {3}' -f (Prop $_ 'contentType'), (Prop $_ 'size'), (Prop $_ 'head'), (Prop $_ 'eof') }) -join '; ' } else { 'no attachment' }
    Write-Host ('  {0,-10} {1,-10} {2,-40} {3}' -f (Prop $it 'key'), $kind, $via, $attText)
}
$lag = Prop $res 'eventLoop'
Write-Host ("Event loop during the run: max timer lag {0} ms over {1} ticks; {2} ticks over 250 ms" -f (Prop $lag 'maxMs'), (Prop $lag 'samples'), (Prop $lag 'over250'))
Write-Host ("Command route: {0}; prompt spy: {1}; windows opened during the run: {2}" -f (Prop (Prop $res 'invoke') 'via'), (Prop $res 'promptSpy'), @((Prop (Prop $res 'windows') 'opened')).Count)
Write-Host 'Checks:'
foreach ($c in @(Prop $res 'checks')) {
    if ($null -ne $c) { Write-Check (Prop $c 'status') (Prop $c 'name') (Prop $c 'detail') }
}

# harness-level safety (run.ps1)
Write-Host 'Harness:'
if ($null -eq $hs) {
    Write-Check 'warn' 'harness_summary' 'harness-summary.json missing'
} else {
    $missing = @(Prop $hs 'userZoteroPidsMissing' | Where-Object { $null -ne $_ })
    if ($missing.Count -gt 0) { Write-Check 'fail' 'user_zotero_untouched' "user's Zotero PIDs no longer running: $($missing -join ', ')" }
    elseif ((Prop $hs 'userIniHashesUnchanged') -eq $false) { Write-Check 'fail' 'user_zotero_untouched' "the user's profiles.ini/installs.ini changed" }
    else { Write-Check 'pass' 'user_zotero_untouched' "user PIDs alive: $((@(Prop $hs 'userZoteroPidsAlive')) -join ', '); ini hashes unchanged" }
    if ((Prop $hs 'cleanExit') -eq $true) { Write-Check 'pass' 'clean_shutdown' "Zotero quit by itself (root exit code $(Prop $hs 'rootExitCode'))" }
    else { Write-Check 'warn' 'clean_shutdown' "Zotero had to be stopped by the harness (killed PIDs: $((@(Prop $hs 'killedPids')) -join ', '))" }
}
if (-not $shim.Installed) { Write-Check 'fail' 'second_plugin_installed' 'the Start-Process shim never installed the zotero-fulltext proxy file' }
elseif (-not (Test-Path (Join-Path $shim.Profile $(if ($zftIsXpi) { "extensions\$zftId.xpi" } else { "extensions\$zftId" })))) { Write-Check 'fail' 'second_plugin_installed' 'proxy file / xpi missing after the run' }
else { Write-Check 'pass' 'second_plugin_installed' "profile\extensions\$zftId -> $zftDir$(if ($zftIsXpi) { ' (packed xpi)' } else { '' })" }

Write-Host ''
Write-Host "run dir: $runDir  (logs\zotero-stdout.log, result.zotero-debug.txt, harness-summary.json)"
$verdict = if ($nFail -eq 0 -and (Prop $res 'verdict') -eq 'PASS') { 'PASS' } else { 'FAIL' }
$color = if ($verdict -eq 'PASS') { 'Green' } else { 'Red' }
Write-Host ("RESULT: {0}  ({1} pass, {2} warn, {3} fail)" -f $verdict, $nPass, $nWarn, $nFail) -ForegroundColor $color
if ($verdict -eq 'PASS') { exit 0 } else { exit 1 }
