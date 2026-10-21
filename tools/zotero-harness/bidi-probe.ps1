<#
.SYNOPSIS
  Probe whether a THROWAWAY headless Zotero 10 accepts a WebDriver BiDi connection (Firefox Remote Agent).
  Fresh temp profile + temp data dir; never touches the user's Zotero. Kills only the PID it started.
#>
param([string[]]$PluginDirs = @(), [int]$Port = 9333, [int]$TimeoutSec = 60, [switch]$KeepRunning)
$ErrorActionPreference = 'Stop'
$run = Join-Path $PSScriptRoot "..\..\.ref\harness-runs\bidi-probe"
if (Test-Path $run) { Remove-Item $run -Recurse -Force }
$profileDir = Join-Path $run 'profile'; $dataDir = Join-Path $run 'data'; $tmp = Join-Path $run 'tmp'
New-Item -ItemType Directory -Force $profileDir, $dataDir, $tmp | Out-Null
$dd = $dataDir -replace '\\', '\\'
@"
user_pref("extensions.zotero.dataDir", "$dd");
user_pref("extensions.zotero.useDataDir", true);
user_pref("extensions.zotero.httpServer.enabled", false);
user_pref("extensions.zotero.sync.autoSync", false);
user_pref("app.update.enabled", false);
user_pref("app.update.auto", false);
user_pref("extensions.zotero.firstRunGuidance", false);
user_pref("remote.log.level", "Info");
user_pref("marionette.port", 2829);
user_pref("extensions.autoDisableScopes", 0);
user_pref("extensions.zoteroWinWordIntegration.skipInstallation", true);
user_pref("extensions.zoteroOpenOfficeIntegration.skipInstallation", true);
user_pref("xpinstall.signatures.required", false);
"@ | Set-Content -Encoding utf8 (Join-Path $profileDir 'user.js')

$ext = Join-Path $profileDir 'extensions'; New-Item -ItemType Directory -Force $ext | Out-Null
(Resolve-Path (Join-Path $PSScriptRoot 'remote-unlock')).Path | Set-Content -NoNewline -Encoding ascii (Join-Path $ext 'remote-unlock@zotero-fulltext.local')
foreach ($pd in $PluginDirs) { $id = (Get-Content (Join-Path $pd 'manifest.json') -Raw | ConvertFrom-Json).applications.zotero.id; (Resolve-Path $pd).Path | Set-Content -NoNewline -Encoding ascii (Join-Path $ext $id) }
$env:TEMP = $tmp; $env:TMP = $tmp; $env:MOZ_HEADLESS = '1'
$stdout = Join-Path $run 'stdout.log'; $stderr = Join-Path $run 'stderr.log'
$p = Start-Process -FilePath 'C:\Program Files\Zotero\zotero.exe' -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr `
  -ArgumentList @('-headless', '-no-remote', '-profile', "`"$profileDir`"", '--remote-debugging-port', "$Port", '--remote-allow-system-access', '-marionette')
"started zotero pid $($p.Id)"
$ok = $false
for ($i = 0; $i -lt $TimeoutSec; $i++) {
  Start-Sleep -Seconds 1
  try { $r = Invoke-WebRequest -UseBasicParsing -TimeoutSec 2 "http://127.0.0.1:$Port/json/version"; "HTTP /json/version: $($r.StatusCode) $($r.Content)"; $ok = $true; break } catch { }
  if ($p.HasExited) { "zotero exited early: $($p.ExitCode)"; break }
}
if (-not $ok) { "no HTTP answer on port $Port" }
"--- stderr (remote lines):"; if (Test-Path $stderr) { Get-Content $stderr | Where-Object { $_ -match 'Remote|WebDriver|BiDi|listening|error' } | Select-Object -First 15 }
"--- stdout (remote lines):"; if (Test-Path $stdout) { Get-Content $stdout | Where-Object { $_ -match 'Remote|WebDriver|BiDi|listening' } | Select-Object -First 15 }
if (-not $KeepRunning) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force; "stopped pid $($p.Id)" } } else { "left running: pid $($p.Id) port $Port" }
