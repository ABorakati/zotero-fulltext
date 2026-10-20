//! `cargo xtask harness [--gui] [--dry-run]`: run the unpacked plugin inside a THROWAWAY Zotero via
//! `tools/zotero-harness/run.ps1` (see that folder's README for what the script itself guarantees).
//!
//! This module adds its own guards on top of the script's:
//! * the run root is a directory this command creates fresh (`create_dir`, never `create_dir_all` on the leaf), under
//!   the workspace target directory, so the profile (`<run>/run/profile`) and data dir (`<run>/run/data`) can only be
//!   brand-new directories;
//! * the command refuses to start when the run root resolves inside `%APPDATA%\Zotero`, `%LOCALAPPDATA%\Zotero` or
//!   `~/Zotero` (the user's real profile and default data directory), or outside the workspace target directory;
//! * it never passes credentials, preferences or URLs: only the plugin directory, the add-on id and the run paths.
use crate::R;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Environment variable that overrides the Zotero binary (otherwise the default inside `run.ps1` is used).
pub const ZOTERO_EXE_VAR: &str = "ZOTERO_EXE";
/// Name of the single run folder inside the fresh run root.
const RUN_NAME: &str = "run";

/// `harness` flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Show the Zotero window instead of running headless.
    pub gui: bool,
    /// Check everything and print the command, but build nothing, create nothing, start nothing.
    pub dry_run: bool,
}

impl Options {
    /// Parse the flags after `harness`.
    pub fn parse(args: &[String]) -> R<Self> {
        let mut options = Self::default();
        for arg in args {
            match arg.as_str() {
                "--gui" => options.gui = true,
                "--dry-run" => options.dry_run = true,
                other => {
                    return Err(format!(
                        "harness: unknown option {other:?}; use [--gui] [--dry-run]"
                    )
                    .into());
                }
            }
        }
        Ok(options)
    }
}

/// The default of the `$ZoteroExe` parameter, read from the text of `run.ps1`.
pub fn script_default_zotero_exe(script: &str) -> Option<String> {
    let line = script.lines().find(|l| l.contains("[string]$ZoteroExe"))?;
    line.split('\'').nth(1).map(str::to_owned)
}

/// Directories a throwaway run must never live in.
pub fn forbidden_roots() -> Vec<PathBuf> {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let mut roots = Vec::new();
    for base in [var("APPDATA"), var("LOCALAPPDATA")].into_iter().flatten() {
        roots.push(base.join("Zotero"));
    }
    for home in [var("USERPROFILE"), var("HOME")].into_iter().flatten() {
        roots.push(home.join("Zotero"));
    }
    roots
}

/// Canonical (when the path exists) lower-case `/` form without the Windows `\\?\` prefix, for containment tests.
fn normal_form(path: &Path) -> String {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy().replace('\\', "/").to_lowercase();
    text.strip_prefix("//?/")
        .unwrap_or(&text)
        .trim_end_matches('/')
        .to_owned()
}

/// `path` is `root` or lies below it.
pub fn is_inside(path: &Path, root: &Path) -> bool {
    let (p, r) = (normal_form(path), normal_form(root));
    !r.is_empty() && (p == r || p.starts_with(&format!("{r}/")))
}

/// Refuse a run root that is outside `target_dir` or inside any of `forbidden`.
pub fn check_run_root(run_root: &Path, target_dir: &Path, forbidden: &[PathBuf]) -> R<()> {
    if let Some(root) = forbidden.iter().find(|f| is_inside(run_root, f)) {
        return Err(format!(
            "refusing to run: {} is inside {}",
            run_root.display(),
            root.display()
        )
        .into());
    }
    if !is_inside(run_root, target_dir) {
        return Err(format!(
            "refusing to run: {} is not inside the build directory {}",
            run_root.display(),
            target_dir.display()
        )
        .into());
    }
    Ok(())
}

/// A run-root name unique to this invocation: unix seconds, process id and a per-process counter.
pub fn run_root_name() -> String {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("run-{seconds}-{}-{n}", std::process::id())
}

/// Create the brand-new run root `<target>/xtask/harness/<name>`. Fails if it already exists, so a directory that
/// this command did not create is never reused.
pub fn create_run_root(target_dir: &Path, name: &str) -> R<PathBuf> {
    let base = target_dir.join("xtask").join("harness");
    fs::create_dir_all(&base)?;
    let run_root = base.join(name);
    fs::create_dir(&run_root).map_err(|e| {
        format!(
            "cannot create the fresh run root {}: {e}",
            run_root.display()
        )
    })?;
    Ok(run_root)
}

/// Everything needed to start `run.ps1`, except the run root (created later).
#[derive(Debug)]
pub struct Prepared {
    pub script: PathBuf,
    pub plugin_dir: PathBuf,
    pub plugin_id: String,
    pub zotero_exe: Option<PathBuf>,
    pub options: Options,
}

/// The `pwsh` argument list for a run rooted at `run_root` (documented parameters of `run.ps1` only).
pub fn script_args(p: &Prepared, run_root: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> =
        vec!["-NoProfile".into(), "-File".into(), p.script.clone().into()];
    let mut pair = |name: &str, value: OsString| {
        args.push(name.into());
        args.push(value);
    };
    pair("-PluginDir", p.plugin_dir.clone().into());
    pair("-PluginId", p.plugin_id.clone().into());
    pair("-RunsRoot", run_root.as_os_str().to_owned());
    pair("-RunName", RUN_NAME.into());
    pair("-ResultFile", result_file(run_root).into_os_string());
    if let Some(exe) = &p.zotero_exe {
        pair("-ZoteroExe", exe.clone().into());
    }
    if p.options.gui {
        // Headless is the script's default; a switch parameter is turned off with `-Headless:$false`.
        args.push("-Headless:$false".into());
    }
    args
}

fn result_file(run_root: &Path) -> PathBuf {
    run_root.join(RUN_NAME).join("result.json")
}

/// Cheap checks that need no build: the script, the manifest id and the Zotero binary.
pub fn prepare(root: &Path, plugin_dir: &Path, options: Options) -> R<Prepared> {
    let script = root.join("tools").join("zotero-harness").join("run.ps1");
    let script_text = fs::read_to_string(&script)
        .map_err(|e| format!("cannot read {}: {e}", script.display()))?;

    let manifest_path = plugin_dir.join("manifest.json");
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(&manifest_path)
            .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?,
    )?;
    let plugin_id = manifest
        .pointer("/applications/zotero/id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{} has no applications.zotero.id", manifest_path.display()))?
        .to_owned();

    let override_exe = std::env::var_os(ZOTERO_EXE_VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let exe = override_exe
        .clone()
        .or_else(|| script_default_zotero_exe(&script_text).map(PathBuf::from));
    match &exe {
        Some(path) if path.is_file() => {}
        found => {
            let shown = found.as_ref().map_or_else(
                || "(no default in run.ps1)".to_owned(),
                |p| p.display().to_string(),
            );
            let message = format!(
                "Zotero executable not found: {shown}\n  Install Zotero 10, or point the harness at it:\n    PowerShell: $env:{ZOTERO_EXE_VAR} = 'C:\\path\\to\\zotero.exe'\n    cmd:        set {ZOTERO_EXE_VAR}=C:\\path\\to\\zotero.exe"
            );
            if !options.dry_run {
                return Err(message.into());
            }
            eprintln!("harness (dry run): {message}");
        }
    }
    Ok(Prepared {
        script,
        plugin_dir: plugin_dir.to_path_buf(),
        plugin_id,
        zotero_exe: override_exe,
        options,
    })
}

/// Print the command line a run would use.
pub fn describe(args: &[OsString]) -> String {
    let quoted: Vec<String> = args
        .iter()
        .map(|a| {
            let text = a.to_string_lossy();
            if text.contains(' ') || text.contains('$') {
                format!("\"{text}\"")
            } else {
                text.into_owned()
            }
        })
        .collect();
    format!("pwsh {}", quoted.join(" "))
}

/// Create the fresh run root, apply the guards, run the script and relay its exit code (0 result written, 2 timed
/// out, 3 Zotero exited without a result).
pub fn launch(p: &Prepared, target_dir: &Path) -> R<u8> {
    let run_root = create_run_root(target_dir, &run_root_name())?;
    check_run_root(&run_root, target_dir, &forbidden_roots())?;
    let args = script_args(p, &run_root);
    println!("harness: {}", describe(&args));
    let status = Command::new("pwsh").args(&args).status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "pwsh (PowerShell 7) was not found on PATH; install it to run the harness".to_owned()
        } else {
            format!("cannot start pwsh: {e}")
        }
    })?;
    let code = status.code().unwrap_or(1);
    let meaning = match code {
        0 => "result written",
        2 => "timed out waiting for the plugin's result file",
        3 => "Zotero exited without writing a result",
        _ => "harness script failed",
    };
    let result = result_file(&run_root);
    println!("harness: exit code {code} ({meaning})");
    if result.is_file() {
        println!("harness: result file: {}", result.display());
    } else {
        println!(
            "harness: no result file was written (expected at {})",
            result.display()
        );
    }
    println!(
        "harness: summary: {}",
        run_root
            .join(RUN_NAME)
            .join("harness-summary.json")
            .display()
    );
    println!(
        "harness: logs: {}",
        run_root.join(RUN_NAME).join("logs").display()
    );
    Ok(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xpi::testing::TempDir;

    fn prepared(gui: bool, exe: Option<&str>) -> Prepared {
        Prepared {
            script: PathBuf::from("S:/repo/tools/zotero-harness/run.ps1"),
            plugin_dir: PathBuf::from("S:/repo/plugin"),
            plugin_id: "zotero-fulltext@aborakati".into(),
            zotero_exe: exe.map(PathBuf::from),
            options: Options {
                gui,
                dry_run: false,
            },
        }
    }

    fn text(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn options() {
        assert_eq!(Options::parse(&[]).unwrap(), Options::default());
        let all = Options::parse(&["--gui".into(), "--dry-run".into()]).unwrap();
        assert!(all.gui && all.dry_run);
        assert!(Options::parse(&["--headless".into()]).is_err());
    }

    #[test]
    fn default_zotero_path_comes_from_the_script() {
        let sample = "param(\n    [string]$ZoteroExe = 'D:\\Apps\\Zotero\\zotero.exe',\n    [int]$GraceSec = 30\n)";
        assert_eq!(
            script_default_zotero_exe(sample).as_deref(),
            Some("D:\\Apps\\Zotero\\zotero.exe")
        );
        assert_eq!(script_default_zotero_exe("param([int]$x)"), None);
        // The real script keeps the parameter this tool depends on.
        let real = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("tools")
            .join("zotero-harness")
            .join("run.ps1");
        let real_text = fs::read_to_string(real).unwrap();
        let exe = script_default_zotero_exe(&real_text).unwrap();
        assert!(exe.to_lowercase().ends_with("zotero.exe"), "{exe}");
        for parameter in [
            "PluginDir",
            "PluginId",
            "RunsRoot",
            "RunName",
            "ResultFile",
            "ZoteroExe",
            "Headless",
        ] {
            assert!(
                real_text.contains(&format!("${parameter}")),
                "run.ps1 lost parameter {parameter}"
            );
        }
    }

    #[test]
    fn headless_by_default_and_gui_on_request() {
        let root = Path::new("S:/target/xtask/harness/run-1-2");
        let headless = text(&script_args(&prepared(false, None), root));
        assert_eq!(
            &headless[..3],
            [
                "-NoProfile",
                "-File",
                "S:/repo/tools/zotero-harness/run.ps1"
            ]
        );
        assert!(
            !headless.iter().any(|a| a.starts_with("-Headless")),
            "{headless:?}"
        );
        assert!(!headless.contains(&"-ZoteroExe".to_owned()));
        let gui = text(&script_args(&prepared(true, Some("C:/Z/zotero.exe")), root));
        assert_eq!(gui.last().map(String::as_str), Some("-Headless:$false"));
        let at = gui.iter().position(|a| a == "-ZoteroExe").unwrap();
        assert_eq!(gui.get(at + 1).map(String::as_str), Some("C:/Z/zotero.exe"));
    }

    #[test]
    fn script_args_pass_only_documented_parameters_and_paths() {
        let root = Path::new("S:/target/xtask/harness/run-1-2");
        let args = text(&script_args(&prepared(true, Some("C:/Z/zotero.exe")), root));
        let names: Vec<&str> = args
            .iter()
            .map(String::as_str)
            .filter(|a| a.starts_with('-'))
            .collect();
        assert_eq!(
            names,
            [
                "-NoProfile",
                "-File",
                "-PluginDir",
                "-PluginId",
                "-RunsRoot",
                "-RunName",
                "-ResultFile",
                "-ZoteroExe",
                "-Headless:$false"
            ]
        );
        let value = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap()
        };
        assert_eq!(value("-PluginDir"), "S:/repo/plugin");
        assert_eq!(value("-PluginId"), "zotero-fulltext@aborakati");
        assert_eq!(value("-RunsRoot"), "S:/target/xtask/harness/run-1-2");
        assert_eq!(value("-RunName"), RUN_NAME);
        assert!(
            value("-ResultFile")
                .replace('\\', "/")
                .ends_with("run-1-2/run/result.json"),
            "{}",
            value("-ResultFile")
        );
        // No credentials, prefs or URLs ever travel on this command line.
        assert!(
            !args
                .iter()
                .any(|a| a.contains("http") || a.contains("pass") || a.contains("ExtraPrefs")),
            "{args:?}"
        );
    }

    #[test]
    fn containment_is_boundary_and_case_aware() {
        assert!(is_inside(
            Path::new("C:/Users/me/AppData/Roaming/Zotero/profile"),
            Path::new("c:/users/me/appdata/roaming/zotero")
        ));
        assert!(is_inside(Path::new("C:/a/b"), Path::new("C:/a/b")));
        assert!(
            !is_inside(Path::new("C:/a/bc"), Path::new("C:/a/b")),
            "prefix without a separator boundary"
        );
        assert!(!is_inside(Path::new("C:/a"), Path::new("C:/a/b")));
        assert!(!is_inside(Path::new("C:/a"), Path::new("")));
    }

    #[test]
    fn guards_refuse_the_users_zotero_directories_and_foreign_roots() {
        let target = TempDir::new("target");
        let user_zotero = TempDir::new("home");
        let zotero_dir = user_zotero.path().join("Zotero");
        fs::create_dir_all(&zotero_dir).unwrap();
        let forbidden = vec![
            zotero_dir.clone(),
            PathBuf::from("Z:/does/not/exist/Zotero"),
        ];

        let inside_target = target.path().join("xtask").join("harness").join("run-1-1");
        fs::create_dir_all(&inside_target).unwrap();
        assert!(check_run_root(&inside_target, target.path(), &forbidden).is_ok());

        let err = check_run_root(&zotero_dir.join("harness"), target.path(), &forbidden)
            .unwrap_err()
            .to_string();
        assert!(err.contains("is inside"), "{err}");
        let err = check_run_root(&zotero_dir, &zotero_dir, &forbidden)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("is inside"),
            "forbidden wins even when it is also the target: {err}"
        );
        let err = check_run_root(user_zotero.path(), target.path(), &forbidden)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not inside the build directory"), "{err}");
        let err = check_run_root(
            Path::new("Z:/does/not/exist/Zotero/x"),
            Path::new("Z:/does/not"),
            &forbidden,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("is inside"), "{err}");
    }

    #[test]
    fn each_run_root_is_fresh_and_below_the_target_dir() {
        let target = TempDir::new("target");
        let first = create_run_root(target.path(), "run-1-1").unwrap();
        assert!(first.is_dir());
        assert!(is_inside(&first, target.path()));
        assert_eq!(fs::read_dir(&first).unwrap().count(), 0, "created empty");
        // A name that already exists is refused instead of reused.
        assert!(create_run_root(target.path(), "run-1-1").is_err());
        assert!(create_run_root(target.path(), "run-1-2").is_ok());
        assert!(run_root_name().starts_with("run-"));
    }

    #[test]
    fn real_forbidden_roots_end_in_zotero() {
        for root in forbidden_roots() {
            assert!(root.ends_with("Zotero"), "{}", root.display());
        }
    }

    #[test]
    fn prepare_reads_the_manifest_id_and_reports_a_missing_binary() {
        let repo = TempDir::new("repo");
        repo.write(
            "tools/zotero-harness/run.ps1",
            b"param(\n  [string]$ZoteroExe = 'Z:\\definitely\\missing\\zotero.exe'\n)\n",
        );
        let plugin = TempDir::new("plugin");
        plugin.write(
            "manifest.json",
            br#"{"applications":{"zotero":{"id":"x@y"}}}"#,
        );

        let err = prepare(repo.path(), plugin.path(), Options::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("Zotero executable not found"), "{err}");
        assert!(
            err.contains(ZOTERO_EXE_VAR) && err.contains("missing"),
            "{err}"
        );

        let dry = prepare(
            repo.path(),
            plugin.path(),
            Options {
                gui: false,
                dry_run: true,
            },
        )
        .unwrap();
        assert_eq!(dry.plugin_id, "x@y");

        plugin.write("manifest.json", br#"{"applications":{}}"#);
        assert!(
            prepare(
                repo.path(),
                plugin.path(),
                Options {
                    gui: false,
                    dry_run: true
                }
            )
            .unwrap_err()
            .to_string()
            .contains("applications.zotero.id")
        );
    }

    /// A stand-in for `run.ps1` that records how it was called. Exits with `exit_code` after optionally writing the
    /// result file.
    fn stub_script(repo: &TempDir, write_result: bool, exit_code: i32) -> PathBuf {
        let write = if write_result {
            "New-Item -ItemType Directory -Force (Split-Path $ResultFile -Parent) | Out-Null\n\"headless=$on id=$PluginId exe=$ZoteroExe name=$RunName\" | Set-Content $ResultFile"
        } else {
            ""
        };
        let script = format!(
            "[CmdletBinding()]\nparam([string]$PluginDir,[string]$PluginId,[string]$RunsRoot,[string]$RunName,[string]$ResultFile,[string]$ZoteroExe,[switch]$Headless)\n$on = if ($PSBoundParameters.ContainsKey('Headless')) {{ [bool]$Headless }} else {{ $true }}\n{write}\nexit {exit_code}\n"
        );
        repo.write("stub/run.ps1", script.as_bytes());
        repo.path().join("stub").join("run.ps1")
    }

    fn pwsh_available() -> bool {
        Command::new("pwsh")
            .args(["-NoProfile", "-Command", "exit 0"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn launch_relays_the_script_exit_code_and_the_gui_switch() {
        if !pwsh_available() {
            eprintln!("note: pwsh not available; skipping");
            return;
        }
        let repo = TempDir::new("stub");
        let target = TempDir::new("target");
        let mut prepared = prepared(true, Some("Q:/Z/zotero.exe"));
        prepared.plugin_dir = repo.path().to_path_buf();

        // Result written -> exit 0, and the stub saw -Headless:$false, the id, the exe and the fixed run name.
        prepared.script = stub_script(&repo, true, 0);
        assert_eq!(launch(&prepared, target.path()).unwrap(), 0);
        let run_roots: Vec<PathBuf> = fs::read_dir(target.path().join("xtask").join("harness"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(run_roots.len(), 1);
        let result = fs::read_to_string(result_file(&run_roots[0])).unwrap();
        assert!(
            result.contains("headless=False") && result.contains("id=zotero-fulltext@aborakati"),
            "{result}"
        );
        assert!(
            result.contains("exe=Q:/Z/zotero.exe") && result.contains("name=run"),
            "{result}"
        );

        // Headless is the default: no switch is passed, the stub's default applies.
        prepared.options.gui = false;
        assert_eq!(launch(&prepared, target.path()).unwrap(), 0);
        let newest = fs::read_dir(target.path().join("xtask").join("harness"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p != &run_roots[0])
            .collect::<Vec<_>>();
        if let Some(root) = newest.first() {
            assert!(
                fs::read_to_string(result_file(root))
                    .unwrap()
                    .contains("headless=True")
            );
        }

        // The script's own exit codes (2 timed out, 3 no result) come back unchanged.
        prepared.script = stub_script(&repo, false, 3);
        assert_eq!(launch(&prepared, target.path()).unwrap(), 3);
    }

    #[test]
    fn describe_quotes_arguments_with_spaces_or_dollars() {
        let args: Vec<OsString> = vec![
            "-File".into(),
            "C:/a b/run.ps1".into(),
            "-Headless:$false".into(),
        ];
        assert_eq!(
            describe(&args),
            "pwsh -File \"C:/a b/run.ps1\" \"-Headless:$false\""
        );
    }
}
