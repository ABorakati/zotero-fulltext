# zotero-harness

Runs a Zotero plugin inside a **throwaway Zotero instance** (fresh temp profile and temp
data directory), waits for the plugin to write a result file, then shuts that instance down.
It never touches the user's running Zotero, their profile, or their `~/Zotero` data directory.

Verified on Windows 11 with Zotero 10.0.6 (Gecko 140.17.0 ESR), headless and GUI, with
both an unpacked plugin directory and a packed `.xpi`.

## Files

| Path | What it is |
|---|---|
| `run.ps1` | The harness (PowerShell 7). |
| `build-probe.ps1` | Builds the Rust probe crates, copies `.wasm` and wasm-bindgen glue into `probe-plugin/`, packs `out/probe@zotero-fulltext.local.xpi`. |
| `probe-plugin/` | Runtime probe plugin (`probe@zotero-fulltext.local`): WebAssembly in every JS context, wasm loading methods, prefs, Zotero API shapes, `onMainWindowLoad` timing, local cookie-jar and attachment simulation. |
| `probe-wasm/` | Rust `cdylib` with a hand-written ABI (`zf_alloc`, `zf_dealloc`, `zf_process_json`), serde_json, no wasm-bindgen. |
| `probe-wasm-bindgen/` | Same logic through `wasm-bindgen = "=0.2.126"` (must match the installed CLI). |
| `test-plugins/noop-plugin/` | Never writes a result. Use it to test the timeout and kill path. |

Run output goes to `C:\Users\abora\zotero-fulltext\.ref\harness-runs\<RunName>\`:
`profile\`, `data\`, `logs\zotero-stdout.log` (the `-ZoteroDebugText` output),
`logs\zotero-stderr.log`, `result.json`, `result.zotero-debug.txt` (written by the probe from
`Zotero.Debug.get()`), and `harness-summary.json`.

## Usage

```powershell
# build the probe (Rust -> wasm32-unknown-unknown, wasm-bindgen glue, XPI)
pwsh -NoProfile -File tools\zotero-harness\build-probe.ps1 -Offline

# unpacked dir (installed through an extension proxy file), headless (the default)
pwsh -NoProfile -File tools\zotero-harness\run.ps1 `
  -PluginDir tools\zotero-harness\probe-plugin -PluginId probe@zotero-fulltext.local

# packed XPI (rootURI becomes jar:file:///...xpi!/ like a real install)
pwsh -NoProfile -File tools\zotero-harness\run.ps1 `
  -PluginDir tools\zotero-harness\out\probe@zotero-fulltext.local.xpi -PluginId probe@zotero-fulltext.local

# visible window, extra prefs, custom result path
pwsh -NoProfile -Command "& tools\zotero-harness\run.ps1 -PluginDir <dir> -PluginId <id> -Headless:`$false -ExtraPrefs @{ 'extensions.zotero-harness.skipCookieSim' = `$true } -ResultFile C:\tmp\r.json"
```

Parameters: `-PluginDir` (dir with `manifest.json`, or a `.xpi`), `-PluginId`, `-ResultFile`
(default `<runDir>\result.json`), `-TimeoutSec` (default 120), `-Headless` (default on; turn
it off with `-Headless:$false`), `-RunName`, `-RunsRoot`, `-ZoteroExe`, `-ExtraPrefs`
(hashtable), `-GraceSec` (time allowed for a clean quit after the result appears, default 30),
`-LogTailLines`, `-NoDebugText`, `-Quiet`.

Exit codes: `0` result written, `2` timed out, `3` Zotero exited without writing a result.

## Contract with the plugin under test

- The plugin finds the result path in pref `extensions.zotero-harness.resultFile`, with env
  var `ZOTERO_HARNESS_RESULT_FILE` as a fallback.
- It writes the file with `IOUtils.writeUTF8(path, text, { tmpPath: path + '.tmp' })`. The
  rename is atomic, so the harness never reads a partial file.
- It then quits with `Services.startup.quit(Ci.nsIAppStartup.eAttemptQuit)`. Leave quitting
  out to keep Zotero running until `-GraceSec` expires.
  `extensions.zotero-harness.quitWhenDone=false` disables quitting in the probe.
- Probe-only prefs: `extensions.zotero-harness.skipCookieSim` (skips the local httpd
  simulation, which takes about 35 s).

## What the harness does

1. Records a baseline of `zotero.exe`, `crashhelper.exe` and `plugin-container.exe` PIDs
   (the user's Zotero). It also SHA-256s `%APPDATA%\Zotero\Zotero\profiles.ini` and
   `installs.ini` (read-only).
2. Creates `profile\`, `data\` and `logs\`, and writes `profile\user.js`:
   - isolation: `extensions.zotero.dataDir=<data>`, `extensions.zotero.useDataDir=true`,
     `extensions.zotero.httpServer.enabled=false` (port 23119 stays with the user's Zotero),
     `extensions.zotero.httpServer.localAPI.enabled=false`;
   - no sync or background network: `sync.autoSync=false`, `streaming.enabled=false`,
     `automaticScraperUpdates=false`, `retractions.enabled=false`;
   - no first-run side effects: `extensions.zotero.firstRun2=false`. If this is left `true`,
     `zoteroPane.js:640-646` loads the start URL, which opens zotero.org in the user's
     real browser. Also `firstRunGuidance=false` and
     `browser.startup.homepage_override.mstone=ignore`;
   - **no writes outside the temp profile**:
     `extensions.zoteroWinWordIntegration.skipInstallation=true` and
     `extensions.zoteroOpenOfficeIntegration.skipInstallation=true`. A fresh profile would
     otherwise auto-install `Zotero.dotm` into the Word STARTUP folder and the LibreOffice
     extension (`resource/word-processor-plugin-installer.mjs:64-98`);
   - updates: `app.update.enabled=false`, `app.update.auto=false`, plus staging, service,
     BITS, background scheduling and langpack off. Zotero keeps `app.update.auto` as a
     normal profile pref (`PER_INSTALLATION_PREFS_PLATFORMS = []` in
     `omni-gre/modules/UpdateUtils.sys.mjs:16`), so this cannot leak into the install-wide
     config;
   - add-ons: `extensions.autoDisableScopes=0`, `extensions.enabledScopes=5` (profile and
     app only), `xpinstall.signatures.required=false`, update checks off;
   - crash and kill tolerance: `toolkit.startup.max_resumed_crashes=-1`,
     `browser.sessionstore.resume_from_crash=false`;
   - logging: `browser.dom.window.dump.enabled=true`, `devtools.console.stdout.chrome=true`,
     `extensions.zotero.debug.store=true`.
3. Installs the plugin. A directory gets a proxy file `profile\extensions\<id>` whose content
   is the absolute dir path (UTF-8, no BOM). A `.xpi` is copied to `profile\extensions\<id>.xpi`.
4. Launches `zotero.exe -profile "<profile>" -no-remote -datadir "<data>" -ZoteroDebugText [-headless]`
   with `Start-Process -PassThru`, stdout and stderr redirected to `logs\`, and env
   `MOZ_HEADLESS=1` (headless), `MOZ_CRASHREPORTER_DISABLE=1` and
   `ZOTERO_HARNESS_RESULT_FILE`. The env is set only around the launch and restored afterwards.
5. Polls for the result file until `-TimeoutSec`, then allows `-GraceSec` for a clean exit.
6. Stops **only our processes**: the root PID, any process whose command line contains this
   run's unique directory, and descendants by `ParentProcessId` or Gecko's
   `-parentPid <pid>`. Baseline PIDs are excluded. Nothing is killed by image name.
7. Checks that every baseline PID is still alive and that the ini hashes are unchanged.
   Writes `harness-summary.json`, then prints the result file and log tails.

## Observed behaviour (runs 2026-10-20)

- Fresh-profile startup to result: about 18 s headless without the cookie simulation and
  about 52 s with it. Clean `eAttemptQuit` exit code 0 every time.
- Headless works fully. `zoteroPane.xhtml` loads (1000x600), and `ZoteroPane`, `itemsView`
  and `#zotero-itemmenu` all exist.
- No launcher-process hand-off: the PID from `Start-Process` is the main process for the
  whole run.
- Timeout path (noop plugin, `-TimeoutSec 25`): exit code 2. Only this run's 7 PIDs were
  stopped (the root process and its content processes), and the user's PIDs were untouched.
- Startup network traffic from Zotero itself: one proxy check to
  `https://zoteroproxycheck.s3.amazonaws.com/test`. No sync, no translator updates, no
  browser launch.
