<#
.SYNOPSIS
  Runs a Zotero plugin inside a THROWAWAY Zotero instance (fresh temp profile + temp data dir),
  waits for the plugin to write a result file, then shuts that instance down.

.DESCRIPTION
  Safety model (the user's own Zotero is usually running and must never be touched):
    * The throwaway instance gets its own profile (-profile) and data dir (-datadir + prefs),
      is started with -no-remote, and has the connector HTTP server (port 23119) disabled.
    * Only processes that provably belong to this run are ever stopped: the PID returned by
      Start-Process -PassThru, plus processes whose command line contains this run's unique
      directory, or which descend from those PIDs (Gecko launcher/content processes).
      Processes that existed before the run (the user's Zotero) are excluded by PID.
      Nothing is ever killed by image name.
    * After the run, the user's pre-existing zotero.exe PIDs are re-checked and reported, and the
      user's profiles.ini / installs.ini are hash-checked (read-only) to prove they were not modified.

.PARAMETER PluginDir
  Unpacked plugin directory containing manifest.json (installed via an extension proxy file),
  or a path to a packed .xpi (copied into the temp profile as <PluginId>.xpi).
.PARAMETER PluginId
  The add-on id (manifest.json applications.zotero.id).
.PARAMETER ResultFile
  File the plugin writes when done. Default: <runDir>\result.json. Passed to the plugin as pref
  "extensions.zotero-harness.resultFile" and env var ZOTERO_HARNESS_RESULT_FILE.
.PARAMETER TimeoutSec
  Max seconds to wait for the result file (default 120).
.PARAMETER Headless
  On by default (MOZ_HEADLESS=1 and -headless). Use -Headless:$false for a normal window.
.PARAMETER ExtraPrefs
  Hashtable of extra prefs for user.js, e.g. @{ 'extensions.zotero.debug.level' = 5 }.
.EXAMPLE
  pwsh -NoProfile -File run.ps1 -PluginDir .\probe-plugin -PluginId probe@zotero-fulltext.local
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string]$PluginDir,
    [Parameter(Mandatory)] [string]$PluginId,
    [string]$ResultFile,
    [int]$TimeoutSec = 120,
    [switch]$Headless,
    [string]$RunName,
    [string]$RunsRoot,
    [string]$ZoteroExe = 'C:\Program Files\Zotero\zotero.exe',
    [hashtable]$ExtraPrefs = @{},
    [int]$GraceSec = 30,
    [int]$LogTailLines = 40,
    [switch]$NoDebugText,
    [switch]$Quiet
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3.0
$HeadlessOn = if ($PSBoundParameters.ContainsKey('Headless')) { [bool]$Headless } else { $true }
$utf8NoBom = [Text.UTF8Encoding]::new($false)

function Say([string]$msg) { if (-not $Quiet) { Write-Host "[harness] $msg" } }

# ---------------------------------------------------------------- inputs
if (-not (Test-Path $ZoteroExe)) { throw "Zotero executable not found: $ZoteroExe" }
$PluginDir = (Resolve-Path $PluginDir).Path
$isXpi = (Test-Path $PluginDir -PathType Leaf) -and ($PluginDir -match '\.xpi$')
if (-not $isXpi) {
    $manifestPath = Join-Path $PluginDir 'manifest.json'
    if (-not (Test-Path $manifestPath)) { throw "No manifest.json in $PluginDir" }
    try {
        $manifest = Get-Content $manifestPath -Raw | ConvertFrom-Json
        $mid = $manifest.applications.zotero.id
        if ($mid -and $mid -ne $PluginId) { Write-Warning "manifest id '$mid' != -PluginId '$PluginId'" }
    } catch { Write-Warning "Could not parse manifest.json: $_" }
}

if (-not $RunsRoot) { $RunsRoot = Join-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) '.ref\harness-runs' }
if (-not $RunName) { $RunName = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + ($PluginId -replace '[^A-Za-z0-9._-]', '_') }
$runDir = Join-Path $RunsRoot $RunName
if (Test-Path $runDir) { throw "Run dir already exists: $runDir (pick another -RunName)" }
$profileDir = Join-Path $runDir 'profile'
$dataDir = Join-Path $runDir 'data'
$logDir = Join-Path $runDir 'logs'
New-Item -ItemType Directory -Force $profileDir, $dataDir, $logDir, (Join-Path $profileDir 'extensions') | Out-Null
$runDir = (Resolve-Path $runDir).Path
$profileDir = (Resolve-Path $profileDir).Path
$dataDir = (Resolve-Path $dataDir).Path
if (-not $ResultFile) { $ResultFile = Join-Path $runDir 'result.json' }
$ResultFile = [IO.Path]::GetFullPath($ResultFile)
New-Item -ItemType Directory -Force (Split-Path $ResultFile -Parent) | Out-Null
Remove-Item -Force -ErrorAction SilentlyContinue $ResultFile, "$ResultFile.tmp"
$stdoutLog = Join-Path $logDir 'zotero-stdout.log'
$stderrLog = Join-Path $logDir 'zotero-stderr.log'

# ---------------------------------------------------------------- baseline (read-only)
# zotero.exe plus Gecko helper executables that a Zotero instance can spawn
function Get-ZoteroProcs {
    @(Get-CimInstance Win32_Process -Filter "Name='zotero.exe' OR Name='crashhelper.exe' OR Name='plugin-container.exe'" |
        Select-Object ProcessId, ParentProcessId, Name, CommandLine, CreationDate)
}
$baseline = Get-ZoteroProcs
$baselinePids = @($baseline | ForEach-Object { [int]$_.ProcessId })
$userIniFiles = @(
    (Join-Path $env:APPDATA 'Zotero\Zotero\profiles.ini'),
    (Join-Path $env:APPDATA 'Zotero\Zotero\installs.ini')
)
function Get-IniHashes { $h = [ordered]@{}; foreach ($f in $userIniFiles) { $h[$f] = if (Test-Path $f) { (Get-FileHash $f -Algorithm SHA256).Hash } else { '(absent)' } }; $h }
$iniBefore = Get-IniHashes
Say "user's zotero.exe PIDs before run: $($baselinePids -join ', ')"

# ---------------------------------------------------------------- profile prefs (user.js)
function Format-PrefValue($v) {
    if ($v -is [bool]) { return $(if ($v) { 'true' } else { 'false' }) }
    if ($v -is [int] -or $v -is [long]) { return [string]$v }
    return ([string]$v | ConvertTo-Json -Compress)   # JSON string literal == valid JS string literal
}
$prefs = [ordered]@{
    # harness <-> plugin contract
    'extensions.zotero-harness.resultFile'                      = $ResultFile
    'extensions.zotero-harness.runDir'                          = $runDir
    # isolated data directory (also passed as -datadir)
    'extensions.zotero.dataDir'                                 = $dataDir
    'extensions.zotero.useDataDir'                              = $true
    'extensions.zotero.warnOnUnsafeDataDir'                     = $false
    # never clash with the user's running Zotero (connector server on 23119)
    'extensions.zotero.httpServer.enabled'                      = $false
    'extensions.zotero.httpServer.localAPI.enabled'             = $false
    # no sync / no background network
    'extensions.zotero.sync.autoSync'                           = $false
    'extensions.zotero.sync.reminder.setUp.enabled'             = $false
    'extensions.zotero.sync.reminder.autoSync.enabled'          = $false
    'extensions.zotero.sync.fulltext.enabled'                   = $false
    'extensions.zotero.streaming.enabled'                       = $false
    'extensions.zotero.automaticScraperUpdates'                 = $false
    'extensions.zotero.retractions.enabled'                     = $false
    # first-run UI: firstRun2=true would open zotero.org/start in the user's real browser
    'extensions.zotero.firstRun2'                               = $false
    'extensions.zotero.firstRunGuidance'                        = $false
    'browser.startup.homepage_override.mstone'                  = 'ignore'
    'browser.shell.checkDefaultBrowser'                         = $false
    # Word/LibreOffice plugin auto-installers would write outside the temp profile
    'extensions.zoteroWinWordIntegration.skipInstallation'      = $true
    'extensions.zoteroOpenOfficeIntegration.skipInstallation'   = $true
    # app updates (Zotero stores app.update.auto as a normal profile pref)
    'app.update.enabled'                                        = $false
    'app.update.auto'                                           = $false
    'app.update.checkInstallTime'                               = $false
    'app.update.staging.enabled'                                = $false
    'app.update.service.enabled'                                = $false
    'app.update.BITS.enabled'                                   = $false
    'app.update.background.scheduling.enabled'                  = $false
    'app.update.langpack.enabled'                               = $false
    # add-ons: load the sideloaded plugin without prompts, never phone home
    'extensions.autoDisableScopes'                              = 0
    'extensions.enabledScopes'                                  = 5
    'xpinstall.signatures.required'                             = $false
    'extensions.update.enabled'                                 = $false
    'extensions.update.autoUpdateDefault'                       = $false
    'extensions.getAddons.cache.enabled'                        = $false
    'extensions.logging.enabled'                                = $true
    # telemetry / crash handling (we may kill the process on timeout)
    'toolkit.telemetry.enabled'                                 = $false
    'datareporting.policy.dataSubmissionEnabled'                = $false
    'datareporting.healthreport.uploadEnabled'                  = $false
    'toolkit.startup.max_resumed_crashes'                       = -1
    'browser.sessionstore.resume_from_crash'                    = $false
    # logging
    'browser.dom.window.dump.enabled'                           = $true
    'devtools.console.stdout.chrome'                            = $true
    'extensions.zotero.debug.store'                             = $true
    'extensions.zotero.debug.store.limit'                       = 2000000
}
foreach ($k in $ExtraPrefs.Keys) { $prefs[$k] = $ExtraPrefs[$k] }
$userJs = ($prefs.GetEnumerator() | ForEach-Object { "user_pref(`"$($_.Key)`", $(Format-PrefValue $_.Value));" }) -join "`n"
[IO.File]::WriteAllText((Join-Path $profileDir 'user.js'), "// generated by zotero-harness run.ps1`n$userJs`n", $utf8NoBom)

# ---------------------------------------------------------------- install the plugin
if ($isXpi) {
    Copy-Item $PluginDir (Join-Path $profileDir "extensions\$PluginId.xpi")
    $installMode = 'xpi-copy'
} else {
    # extension proxy file: file named <id>, content = absolute path of the unpacked plugin dir
    [IO.File]::WriteAllText((Join-Path $profileDir "extensions\$PluginId"), $PluginDir, $utf8NoBom)
    $installMode = 'proxy-file'
}
Say "run dir: $runDir ($installMode, headless=$HeadlessOn)"

# ---------------------------------------------------------------- launch
$argLine = "-profile `"$profileDir`" -no-remote -datadir `"$dataDir`""
if (-not $NoDebugText) { $argLine += ' -ZoteroDebugText' }
if ($HeadlessOn) { $argLine += ' -headless' }
$savedEnv = @{}
# TEMP/TMP point into the run dir: Zotero extracts translators to %TEMP%\Zotero and deletes that folder at shutdown, so sharing
# the real temp dir would race with the user's running Zotero (and with the next throwaway instance).
$tmpDir = Join-Path $runDir 'tmp'
New-Item -ItemType Directory -Force $tmpDir | Out-Null
$envForChild = @{ 'ZOTERO_HARNESS_RESULT_FILE' = $ResultFile; 'MOZ_CRASHREPORTER_DISABLE' = '1'; 'TEMP' = $tmpDir; 'TMP' = $tmpDir }
if ($HeadlessOn) { $envForChild['MOZ_HEADLESS'] = '1' }
foreach ($k in $envForChild.Keys) { $savedEnv[$k] = [Environment]::GetEnvironmentVariable($k); [Environment]::SetEnvironmentVariable($k, $envForChild[$k]) }
$t0 = Get-Date
try {
    $proc = Start-Process -FilePath $ZoteroExe -ArgumentList $argLine -PassThru `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog
} finally {
    foreach ($k in $savedEnv.Keys) { [Environment]::SetEnvironmentVariable($k, $savedEnv[$k]) }
}
$rootPid = $proc.Id
Say "started zotero.exe PID $rootPid : $argLine"

# Processes that belong to THIS run: root PID, anything whose command line names our run dir,
# and descendants (by ParentProcessId or Gecko's -parentPid) -- never anything in the baseline.
$seenOurs = [Collections.Generic.HashSet[int]]::new()
[void]$seenOurs.Add($rootPid)
function Update-OurProcs {
    $all = Get-ZoteroProcs
    $changed = $true
    while ($changed) {
        $changed = $false
        foreach ($p in $all) {
            $procId = [int]$p.ProcessId
            if ($baselinePids -contains $procId -or $seenOurs.Contains($procId)) { continue }
            $cl = [string]$p.CommandLine
            $isOurs = ($cl -and $cl.IndexOf($runDir, [StringComparison]::OrdinalIgnoreCase) -ge 0) -or
                      $seenOurs.Contains([int]$p.ParentProcessId)
            if (-not $isOurs -and $cl -match '-parentPid (\d+)') { $isOurs = $seenOurs.Contains([int]$Matches[1]) }
            if ($isOurs) { [void]$seenOurs.Add($procId); $changed = $true }
        }
    }
    @($all | Where-Object { $seenOurs.Contains([int]$_.ProcessId) })
}
function Test-AnyOursAlive { (@(Update-OurProcs)).Count -gt 0 }

# ---------------------------------------------------------------- wait
$resultSeen = $false; $timedOut = $false
$deadline = $t0.AddSeconds($TimeoutSec)
while ((Get-Date) -lt $deadline) {
    if (Test-Path $ResultFile) { $resultSeen = $true; break }
    if (-not (Test-AnyOursAlive)) { break }
    Start-Sleep -Milliseconds 500
}
if (-not $resultSeen -and (Get-Date) -ge $deadline) { $timedOut = $true }
$resultAtSec = [math]::Round(((Get-Date) - $t0).TotalSeconds, 1)

# give the plugin's clean quit a chance
$cleanExit = $false
if ($resultSeen) {
    $graceEnd = (Get-Date).AddSeconds($GraceSec)
    while ((Get-Date) -lt $graceEnd) {
        if (-not (Test-AnyOursAlive)) { $cleanExit = $true; break }
        Start-Sleep -Milliseconds 500
    }
}

# ---------------------------------------------------------------- stop only our processes
$killed = @()
$leftovers = @(Update-OurProcs)
foreach ($p in $leftovers) {
    $procId = [int]$p.ProcessId
    if ($baselinePids -contains $procId) { continue }   # belt and braces
    try { Stop-Process -Id $procId -Force -ErrorAction Stop; $killed += $procId } catch { }
}
if ($killed.Count) { Say "stopped our leftover PIDs: $($killed -join ', ')"; Start-Sleep -Seconds 2 }
$exitCode = $null
try { if ($proc.HasExited) { $exitCode = $proc.ExitCode } } catch { }

# ---------------------------------------------------------------- verify the user's Zotero is untouched
$after = Get-ZoteroProcs
$afterPids = @($after | ForEach-Object { [int]$_.ProcessId })
$userAlive = @($baselinePids | Where-Object { $afterPids -contains $_ })
$userMissing = @($baselinePids | Where-Object { $afterPids -notcontains $_ })
$iniAfter = Get-IniHashes
$iniUnchanged = $true
foreach ($k in $iniBefore.Keys) { if ($iniBefore[$k] -ne $iniAfter[$k]) { $iniUnchanged = $false } }
$oursStillAlive = @($after | Where-Object { $seenOurs.Contains([int]$_.ProcessId) } | ForEach-Object { [int]$_.ProcessId })

$summary = [ordered]@{
    runDir                  = $runDir
    profileDir              = $profileDir
    dataDir                 = $dataDir
    resultFile              = $ResultFile
    installMode             = $installMode
    headless                = $HeadlessOn
    args                    = $argLine
    rootPid                 = $rootPid
    ourPidsSeen             = @($seenOurs)
    resultSeen              = $resultSeen
    resultAtSec             = $resultAtSec
    timedOut                = $timedOut
    cleanExit               = $cleanExit
    killedPids              = $killed
    ourPidsStillAlive       = $oursStillAlive
    rootExitCode            = $exitCode
    userZoteroPidsBefore    = $baselinePids
    userZoteroPidsAlive     = $userAlive
    userZoteroPidsMissing   = $userMissing
    userIniHashesUnchanged  = $iniUnchanged
    durationSec             = [math]::Round(((Get-Date) - $t0).TotalSeconds, 1)
}
$summary | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 (Join-Path $runDir 'harness-summary.json')

# ---------------------------------------------------------------- report
if (-not $Quiet) {
    Write-Host ''
    Write-Host '==================== harness summary ===================='
    $summary.GetEnumerator() | ForEach-Object { Write-Host ("{0,-24} {1}" -f $_.Key, ($_.Value -join ', ')) }
    Write-Host ''
    if ($resultSeen) {
        Write-Host "==================== result file ($ResultFile) ===================="
        Get-Content $ResultFile -Raw | Write-Host
    } else {
        Write-Host '==================== NO RESULT FILE ===================='
    }
    Write-Host ''
    foreach ($log in @($stdoutLog, $stderrLog, ($ResultFile -replace '\.json$', '') + '.zotero-debug.txt')) {
        if ((Test-Path $log) -and (Get-Item $log).Length -gt 0) {
            Write-Host "==================== tail $LogTailLines of $log ===================="
            Get-Content $log -Tail $LogTailLines | Write-Host
        }
    }
}
if ($userMissing.Count) { Write-Warning "user's Zotero PIDs no longer running: $($userMissing -join ', ') (they were NOT targeted by this script)" }
if ($oursStillAlive.Count) { Write-Warning "our PIDs still alive: $($oursStillAlive -join ', ')" }
if ($resultSeen) { exit 0 } elseif ($timedOut) { exit 2 } else { exit 3 }
