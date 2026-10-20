//! `cargo xtask <command>`: the project's build driver. Owned by core-wasm-xtask (spec: `docs/wp/core-wasm-xtask.md`).
//!
//! * `build [--debug-panic]`: `cargo build -p fulltext-wasm --release --target wasm32-unknown-unknown` with the builder's
//!   home, cargo, rustup, workspace and target directories remapped out of the binary. The module is VERIFIED before
//!   it is accepted (`wasm.rs`): no imports; exactly the documented exports; no builder path fragments anywhere in the
//!   bytes; size (warn above 1 MiB raw, fail above 1.5 MiB). One summary line is printed. Only a module that passes
//!   is staged to `<plugin>/content/fulltext.wasm` (atomic rename). With `--debug-panic` the `fx_debug_panic` test
//!   export is compiled in and the result goes to `<target>/wasm32-unknown-unknown/release/fulltext_wasm_debug.wasm`
//!   and `<target>/xtask/fulltext-debug.wasm`, NEVER into the plugin.
//! * `test`: `cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; `build`;
//!   `build --debug-panic`; `node --test "tests/js/*.test.mjs"` (with `ZFT_DEBUG_WASM` set). Stops at the first failure
//!   and says which step failed.
//! * `check`: `cargo fmt --all --check` (skipped with a note when rustfmt is missing), everything in `test`, then
//!   `cargo clippy -p fulltext-core -p fulltext-wasm --target wasm32-unknown-unknown -- -D warnings` (also with
//!   `--features debug-panic` for the wasm crate).
//! * `xpi`: refuses to run unless `<plugin>/manifest.json` has a `version` of the form `x.y.z`; `build`; then a
//!   deterministic zip of the plugin directory into `dist/zotero-fulltext-<version>.xpi` (entries sorted, fixed
//!   timestamps, deflate 9; `*.map`, `*.md`, `.gitkeep` excluded). Prints the SHA-256 of the archive.
//! * `harness [--gui] [--dry-run]`: `build`, then `tools/zotero-harness/run.ps1` loads the unpacked plugin into a
//!   throwaway Zotero profile and data directory freshly created under `<target>/xtask/harness/`; refuses to start if
//!   that location is inside the user's Zotero directories. Headless unless `--gui`. The script's exit code is the
//!   command's exit code (0 result written, 2 timed out, 3 Zotero exited without a result) and the result file path is
//!   printed. The Zotero binary is the default inside `run.ps1`, or `ZOTERO_EXE`. `--dry-run` only prints the plan.
//!
//! Environment: `ZFT_PLUGIN_DIR` selects another plugin directory (default `plugin`; relative paths are resolved
//! against the workspace root) for `build`, `xpi` and `harness`; the unit tests use it for a throwaway manifest.
//! The user's global cargo config sets `rustc-wrapper = "kache"` and a global `CARGO_TARGET_DIR`; the output
//! location is therefore always asked from `cargo metadata`, never assumed to be `<root>/target`.
mod harness;
mod sha256;
mod wasm;
mod xpi;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

pub(crate) type R<T> = Result<T, Box<dyn Error>>;

const WASM_CRATE_FILE: &str = "fulltext_wasm.wasm";
const WASM_TRIPLE: &str = "wasm32-unknown-unknown";
const USAGE: &str = "usage: cargo xtask <command>\n  build [--debug-panic]   build, verify and stage the wasm module\n  test                    cargo tests, clippy, build, Node tests\n  check                   test plus rustfmt and wasm32 clippy\n  xpi                     build and write dist/zotero-fulltext-<version>.xpi\n  harness [--gui] [--dry-run]\n                          run the plugin in a throwaway Zotero";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// Run `cmd` in `dir`; an unsuccessful exit becomes an error naming `what`.
fn run(what: &str, cmd: &mut Command, dir: &Path) -> R<()> {
    let status = cmd
        .current_dir(dir)
        .status()
        .map_err(|e| format!("cannot start {what}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed ({status})").into())
    }
}

/// `target_directory` as cargo resolves it (honours `CARGO_TARGET_DIR` and config files).
fn target_dir(root: &Path) -> R<PathBuf> {
    let out = Command::new(cargo())
        .current_dir(root)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let dir = meta
        .get("target_directory")
        .and_then(|v| v.as_str())
        .ok_or("cargo metadata has no target_directory")?;
    Ok(PathBuf::from(dir))
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `CARGO_ENCODED_RUSTFLAGS` that keep the builder's home, cargo, rustup, workspace and target directories out of
/// panic `Location` strings inside the shipped `.wasm`. General prefixes first: the LAST matching remap wins.
fn remap_flags(root: &Path, target: &Path) -> String {
    let sep = std::path::MAIN_SEPARATOR;
    let home = env_nonempty("USERPROFILE")
        .or_else(|| env_nonempty("HOME"))
        .unwrap_or_default();
    let cargo_home = env_nonempty("CARGO_HOME").unwrap_or_else(|| format!("{home}{sep}.cargo"));
    let rustup_home = env_nonempty("RUSTUP_HOME").unwrap_or_else(|| format!("{home}{sep}.rustup"));
    let mut flags = Vec::new();
    if !home.is_empty() {
        flags.push(format!("--remap-path-prefix={home}=~"));
    }
    flags.push(format!(
        "--remap-path-prefix={cargo_home}{sep}registry{sep}src=/cargo"
    ));
    flags.push(format!(
        "--remap-path-prefix={rustup_home}{sep}toolchains=/rustup"
    ));
    flags.push(format!("--remap-path-prefix={}=/src", root.display()));
    flags.push(format!("--remap-path-prefix={}=/target", target.display()));
    flags.join("\u{1f}")
}

/// Fragments that must not appear in the built module: the builder's own directories and the usual Windows ones.
fn builder_path_needles(root: &Path, target: &Path) -> Vec<String> {
    let mut needles: Vec<String> = ["\\.cargo\\", "/.cargo/", ".rustup", "C:\\Users", "C:/Users"]
        .map(str::to_owned)
        .to_vec();
    let own = [root.display().to_string(), target.display().to_string()];
    let env = ["USERPROFILE", "HOME", "CARGO_HOME", "RUSTUP_HOME"]
        .iter()
        .filter_map(|name| env_nonempty(name));
    for path in own.into_iter().chain(env) {
        // Shorter than 4 characters would match innocent text (`/`, `~`); both separator styles are checked.
        if path.chars().count() >= 4 {
            needles.push(path.replace('/', "\\"));
            needles.push(path.replace('\\', "/"));
        }
    }
    needles.sort();
    needles.dedup();
    needles
}

/// Write `bytes` to `dest` through a temporary sibling and a rename, so a reader never sees half a file.
fn write_atomic(dest: &Path, bytes: &[u8]) -> R<()> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = dest.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, dest).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Build, verify and place the wasm module. Returns the path it was placed at.
///
/// The plugin build stages `<plugin>/content/fulltext.wasm`. The `--debug-panic` build writes
/// `<target>/wasm32-unknown-unknown/release/fulltext_wasm_debug.wasm` and `<target>/xtask/fulltext-debug.wasm` and
/// never touches the plugin directory.
fn build(root: &Path, plugin: &Path, debug_panic: bool) -> R<PathBuf> {
    let started = Instant::now();
    let target = target_dir(root)?;
    let mut cmd = Command::new(cargo());
    cmd.env("CARGO_ENCODED_RUSTFLAGS", remap_flags(root, &target))
        .args([
            "build",
            "-p",
            "fulltext-wasm",
            "--release",
            "--target",
            WASM_TRIPLE,
        ]);
    if debug_panic {
        cmd.args(["--features", "debug-panic"]);
    }
    run("cargo build (wasm)", &mut cmd, root)?;

    let release_dir = target.join(WASM_TRIPLE).join("release");
    let built = release_dir.join(WASM_CRATE_FILE);
    let bytes = fs::read(&built)
        .map_err(|e| format!("cannot read the built module {}: {e}", built.display()))?;
    let policy = wasm::Policy::new(debug_panic, builder_path_needles(root, &target));
    let report = wasm::verify(&bytes, &policy).map_err(|problems| {
        let list: String = problems.iter().map(|p| format!("\n  - {p}")).collect();
        format!(
            "the built module is not acceptable ({}):{list}",
            built.display()
        )
    })?;
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }

    let placed = if debug_panic {
        let named = release_dir.join("fulltext_wasm_debug.wasm");
        write_atomic(&named, &bytes)?;
        let shared = target.join("xtask").join("fulltext-debug.wasm");
        write_atomic(&shared, &bytes)?;
        // The feature build overwrote cargo's default output name; remove it so nothing mistakes the test build for
        // the plugin build (the next normal `cargo build` re-creates it).
        let _ = fs::remove_file(&built);
        shared
    } else {
        let staged = plugin.join("content").join("fulltext.wasm");
        write_atomic(&staged, &bytes)?;
        staged
    };
    println!(
        "{} in {:.1}s -> {}",
        report.summary(),
        started.elapsed().as_secs_f32(),
        placed.display()
    );
    Ok(placed)
}

/// Counts the steps of a multi-step command and names the failing one.
struct Progress {
    done: usize,
    total: usize,
}

impl Progress {
    fn step<T>(&mut self, label: &str, f: impl FnOnce() -> R<T>) -> R<T> {
        self.done += 1;
        println!("==> [{}/{}] {label}", self.done, self.total);
        f().map_err(|e| format!("step {}/{} `{label}` failed: {e}", self.done, self.total).into())
    }
}

fn cargo_step(root: &Path, args: &[&str]) -> R<()> {
    run(
        &format!("cargo {}", args.join(" ")),
        Command::new(cargo()).args(args),
        root,
    )
}

/// Is `rustfmt` installed for the active toolchain?
fn rustfmt_installed(root: &Path) -> bool {
    Command::new(cargo())
        .current_dir(root)
        .args(["fmt", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `test` (and, with `full`, `check`).
fn test(root: &Path, plugin: &Path, full: bool) -> R<()> {
    let mut progress = Progress {
        done: 0,
        total: if full { 8 } else { 5 },
    };
    if full {
        if rustfmt_installed(root) {
            progress.step("cargo fmt --all --check", || {
                cargo_step(root, &["fmt", "--all", "--check"])
            })?;
        } else {
            progress.done += 1;
            println!(
                "==> [{}/{}] cargo fmt --all --check: skipped (rustfmt is not installed; `rustup component add rustfmt`)",
                progress.done, progress.total
            );
        }
    }
    progress.step("cargo test --workspace", || {
        cargo_step(root, &["test", "--workspace"])
    })?;
    progress.step("cargo clippy --workspace --all-targets", || {
        cargo_step(
            root,
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        )
    })?;
    if full {
        progress.step("cargo clippy (wasm32)", || {
            cargo_step(
                root,
                &[
                    "clippy",
                    "-p",
                    "fulltext-core",
                    "-p",
                    "fulltext-wasm",
                    "--target",
                    WASM_TRIPLE,
                    "--",
                    "-D",
                    "warnings",
                ],
            )
        })?;
        progress.step("cargo clippy (wasm32, debug-panic)", || {
            cargo_step(
                root,
                &[
                    "clippy",
                    "-p",
                    "fulltext-wasm",
                    "--target",
                    WASM_TRIPLE,
                    "--features",
                    "debug-panic",
                    "--",
                    "-D",
                    "warnings",
                ],
            )
        })?;
    }
    progress.step("build", || build(root, plugin, false))?;
    let debug_wasm = progress.step("build --debug-panic", || build(root, plugin, true))?;
    progress.step("node --test", || {
        run(
            "node --test",
            Command::new("node")
                .env("ZFT_DEBUG_WASM", &debug_wasm)
                .args(["--test", "tests/js/*.test.mjs"]),
            root,
        )
    })
}

fn xpi_command(root: &Path, plugin: &Path) -> R<()> {
    let version = xpi::read_version(plugin)?;
    build(root, plugin, false)?;
    let out = root
        .join("dist")
        .join(format!("zotero-fulltext-{version}.xpi"));
    let packed = xpi::pack(plugin, &out)?;
    let digest = sha256::hex_digest(&fs::read(&out)?);
    println!(
        "xpi: {} bytes, {} files -> {}",
        packed.bytes,
        packed.files,
        out.display()
    );
    println!("xpi: sha256 {digest}");
    Ok(())
}

fn harness_command(root: &Path, plugin: &Path, args: &[String]) -> R<u8> {
    let options = harness::Options::parse(args)?;
    let prepared = harness::prepare(root, plugin, options)?;
    if options.dry_run {
        let target = target_dir(root)?;
        let sample_root = target
            .join("xtask")
            .join("harness")
            .join(harness::run_root_name());
        harness::check_run_root(&sample_root, &target, &harness::forbidden_roots())?;
        println!(
            "harness (dry run, nothing built or started): {}",
            harness::describe(&harness::script_args(&prepared, &sample_root))
        );
        return Ok(0);
    }
    build(root, plugin, false)?;
    harness::launch(&prepared, &target_dir(root)?)
}

/// Run the command line; the value is the process exit code.
fn dispatch(args: &[String]) -> R<u8> {
    let root = workspace_root();
    let plugin = xpi::plugin_dir_from_env(&root);
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("build") => match rest {
            [] => build(&root, &plugin, false).map(|_| 0),
            [flag] if flag == "--debug-panic" => build(&root, &plugin, true).map(|_| 0),
            _ => Err("build: the only option is --debug-panic".into()),
        },
        Some("test") if rest.is_empty() => test(&root, &plugin, false).map(|()| 0),
        Some("check") if rest.is_empty() => test(&root, &plugin, true).map(|()| 0),
        Some("xpi") if rest.is_empty() => xpi_command(&root, &plugin).map(|()| 0),
        Some("harness") => harness_command(&root, &plugin, rest),
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            Ok(0)
        }
        other => Err(format!("unknown command or arguments {other:?} {rest:?}\n{USAGE}").into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_flags_cover_every_builder_directory_with_the_general_prefix_first() {
        let flags = remap_flags(Path::new("S:/work/repo"), Path::new("S:/cache/target"));
        let parts: Vec<&str> = flags.split('\u{1f}').collect();
        assert!(
            parts.iter().all(|p| p.starts_with("--remap-path-prefix=")),
            "{parts:?}"
        );
        let at = |suffix: &str| parts.iter().position(|p| p.ends_with(suffix)).unwrap();
        assert!(at("=/cargo") < at("=/src") && at("=/rustup") < at("=/src"));
        assert!(
            at("=/src") < at("=/target"),
            "the target remap is the most specific and goes last"
        );
        assert!(
            parts.iter().any(|p| p.ends_with("S:/work/repo=/src")),
            "{parts:?}"
        );
    }

    #[test]
    fn needles_include_the_fixed_fragments_and_the_roots_in_both_slash_styles() {
        let needles =
            builder_path_needles(Path::new("S:/work/repo"), Path::new("S:\\cache\\target"));
        for want in [
            "\\.cargo\\",
            "/.cargo/",
            ".rustup",
            "C:\\Users",
            "C:/Users",
            "S:\\work\\repo",
            "S:/work/repo",
            "S:\\cache\\target",
            "S:/cache/target",
        ] {
            assert!(
                needles.iter().any(|n| n == want),
                "missing {want:?} in {needles:?}"
            );
        }
        assert!(
            needles.iter().all(|n| n.chars().count() >= 4),
            "short needles match innocent text: {needles:?}"
        );
        let mut sorted = needles.clone();
        sorted.dedup();
        assert_eq!(sorted, needles, "no duplicates");
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp_file() {
        let dir = xpi::testing::TempDir::new("atomic");
        let dest = dir.path().join("sub").join("m.wasm");
        write_atomic(&dest, b"one").unwrap();
        write_atomic(&dest, b"two").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"two");
        assert_eq!(fs::read_dir(dest.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn unknown_commands_and_arguments_are_errors() {
        for args in [
            vec![],
            vec!["nope"],
            vec!["build", "--release"],
            vec!["test", "extra"],
            vec!["xpi", "-x"],
            vec!["harness", "--bad"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
            assert!(dispatch(&args).is_err(), "{args:?}");
        }
        assert_eq!(dispatch(&["help".to_owned()]).unwrap(), 0);
    }

    #[test]
    fn progress_names_the_failing_step() {
        let mut progress = Progress { done: 0, total: 3 };
        assert_eq!(progress.step("one", || Ok(7)).unwrap(), 7);
        let err = progress
            .step::<()>("two", || Err("boom".into()))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "step 2/3 `two` failed: boom");
    }
}
