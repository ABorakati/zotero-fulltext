//! Packaging of the plugin directory into a deterministic `.xpi` (a zip archive).
//!
//! Determinism: entries sorted by their `/`-joined relative path, fixed 1980-01-01 timestamps, fixed permissions,
//! deflate level 9, files only (no directory entries), no extra fields. The same input tree gives the same bytes.
use crate::R;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Environment variable that points the build at a different plugin root (tests, throwaway manifests).
pub const PLUGIN_DIR_VAR: &str = "ZFT_PLUGIN_DIR";

/// The plugin root: `override_dir` (resolved against `root` when relative), else `<root>/plugin`.
pub fn plugin_dir(root: &Path, override_dir: Option<OsString>) -> PathBuf {
    match override_dir.filter(|v| !v.is_empty()) {
        Some(dir) => root.join(dir), // `join` keeps an absolute `dir` as is
        None => root.join("plugin"),
    }
}

/// [`plugin_dir`] with `ZFT_PLUGIN_DIR` read from the environment.
pub fn plugin_dir_from_env(root: &Path) -> PathBuf {
    plugin_dir(root, std::env::var_os(PLUGIN_DIR_VAR))
}

/// `\d+\.\d+\.\d+`, nothing else.
pub fn is_plain_semver(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The `version` of `<plugin>/manifest.json`; an error when the file is missing or the version is not `x.y.z`.
pub fn read_version(plugin: &Path) -> R<String> {
    let manifest_path = plugin.join("manifest.json");
    let text = fs::read(&manifest_path).map_err(|e| {
        format!(
            "cannot read {} ({e}); the XPI needs a manifest with a version",
            manifest_path.display()
        )
    })?;
    let manifest: serde_json::Value = serde_json::from_slice(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", manifest_path.display()))?;
    let version = manifest
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{} has no string \"version\"", manifest_path.display()))?;
    if !is_plain_semver(version) {
        return Err(format!(
            "{}: version {version:?} is not <major>.<minor>.<patch> (digits only)",
            manifest_path.display()
        )
        .into());
    }
    Ok(version.to_owned())
}

/// Files that never belong in the archive: source maps, Markdown, `.gitkeep` and OS litter.
pub fn is_excluded(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".map")
        || lower.ends_with(".md")
        || matches!(name, ".gitkeep" | ".DS_Store" | "Thumbs.db")
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) -> R<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel_path = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let path = entry.path();
        if path.is_dir() {
            walk(&path, &rel_path, out)?;
        } else if !is_excluded(&rel_path) {
            out.push((rel_path, path));
        }
    }
    Ok(())
}

/// Every packable file under `src` as `(archive path, disk path)`, sorted by archive path.
pub fn collect(src: &Path) -> R<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    walk(src, "", &mut files)?;
    files.sort();
    Ok(files)
}

/// What [`pack`] wrote.
#[derive(Debug, PartialEq, Eq)]
pub struct Packed {
    pub files: usize,
    pub bytes: u64,
}

/// Write the deterministic archive of `src` to `out` (parent directories are created).
pub fn pack(src: &Path, out: &Path) -> R<Packed> {
    let files = collect(src)?;
    if files.is_empty() {
        return Err(format!("{} has no files to pack", src.display()).into());
    }
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir)?;
    }
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(9))
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    let mut zip = ZipWriter::new(File::create(out)?);
    for (rel, path) in &files {
        zip.start_file(rel.as_str(), opts)?;
        zip.write_all(&fs::read(path)?)?;
    }
    zip.finish()?;
    Ok(Packed {
        files: files.len(),
        bytes: fs::metadata(out)?.len(),
    })
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh directory under the OS temp dir, removed on drop.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("xtask-{label}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }

        pub fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TempDir;
    use super::*;
    use std::io::Read;

    fn sample_plugin() -> TempDir {
        let dir = TempDir::new("plugin");
        dir.write("manifest.json", br#"{"version":"1.2.3"}"#);
        dir.write("bootstrap.js", b"var x = 1;\r\n");
        dir.write("content/z.js", &[b'z'; 5000]);
        dir.write("content/a.js", b"a");
        dir.write("content/nested/deep/file.txt", b"deep");
        dir.write(
            "content/fulltext.wasm",
            &(0..=255).cycle().take(40_000).collect::<Vec<u8>>(),
        );
        dir.write("content/fulltext.js.map", b"{}");
        dir.write("README.md", b"# docs");
        dir.write("content/NOTES.MD", b"# docs");
        dir.write("content/icons/.gitkeep", b"");
        dir.write("content/Thumbs.db", b"x");
        dir
    }

    #[test]
    fn zipping_the_same_directory_twice_gives_identical_bytes() {
        let plugin = sample_plugin();
        let out = TempDir::new("out");
        let a = out.path().join("a.xpi");
        let b = out.path().join("sub").join("b.xpi");
        let first = pack(plugin.path(), &a).unwrap();
        let second = pack(plugin.path(), &b).unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
        // ...also when the files were written in a different order and touched later.
        let copy = TempDir::new("plugin-copy");
        for (rel, path) in collect(plugin.path()).unwrap().into_iter().rev() {
            copy.write(&rel, &fs::read(path).unwrap());
        }
        let c = out.path().join("c.xpi");
        pack(copy.path(), &c).unwrap();
        assert_eq!(fs::read(&a).unwrap(), fs::read(&c).unwrap());
    }

    #[test]
    fn the_archive_is_sorted_clean_and_round_trips() {
        let plugin = sample_plugin();
        let out = TempDir::new("out");
        let xpi = out.path().join("p.xpi");
        let packed = pack(plugin.path(), &xpi).unwrap();
        let mut archive = zip::ZipArchive::new(File::open(&xpi).unwrap()).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "bootstrap.js",
                "content/a.js",
                "content/fulltext.wasm",
                "content/nested/deep/file.txt",
                "content/z.js",
                "manifest.json"
            ]
        );
        assert_eq!(packed.files, names.len());
        let mut wasm = Vec::new();
        archive
            .by_name("content/fulltext.wasm")
            .unwrap()
            .read_to_end(&mut wasm)
            .unwrap();
        assert_eq!(
            wasm,
            fs::read(plugin.path().join("content").join("fulltext.wasm")).unwrap()
        );
        let entry = archive.by_name("manifest.json").unwrap();
        assert_eq!(entry.last_modified().unwrap().year(), 1980);
    }

    #[test]
    fn exclusions() {
        for excluded in [
            "a.map",
            "dir/b.js.map",
            "README.md",
            "x/NOTES.MD",
            ".gitkeep",
            "a/.gitkeep",
            ".DS_Store",
            "Thumbs.db",
        ] {
            assert!(is_excluded(excluded), "{excluded}");
        }
        for kept in [
            "bootstrap.js",
            "content/mapper.js",
            "content/markdown.js",
            "locale/en-US/x.ftl",
            "gitkeep",
            "a.mdx",
        ] {
            assert!(!is_excluded(kept), "{kept}");
        }
    }

    #[test]
    fn semver_check() {
        for good in ["0.1.0", "10.20.30", "1.0.0"] {
            assert!(is_plain_semver(good), "{good}");
        }
        for bad in [
            "",
            "1",
            "1.0",
            "1.0.0.0",
            "1.0.x",
            "v1.0.0",
            "1.0.0-beta",
            "1..0",
            "1.0.0 ",
            "-1.0.0",
            "+1.0.0",
            "١.٠.٠",
        ] {
            assert!(!is_plain_semver(bad), "{bad:?}");
        }
    }

    #[test]
    fn read_version_validates_the_manifest() {
        let dir = TempDir::new("manifest");
        assert!(
            read_version(dir.path())
                .unwrap_err()
                .to_string()
                .contains("cannot read")
        );
        dir.write("manifest.json", b"not json");
        assert!(
            read_version(dir.path())
                .unwrap_err()
                .to_string()
                .contains("not valid JSON")
        );
        dir.write("manifest.json", br#"{"name":"x"}"#);
        assert!(
            read_version(dir.path())
                .unwrap_err()
                .to_string()
                .contains("no string")
        );
        dir.write("manifest.json", br#"{"version":"1.0"}"#);
        assert!(
            read_version(dir.path())
                .unwrap_err()
                .to_string()
                .contains("not <major>")
        );
        dir.write("manifest.json", br#"{"version":3}"#);
        assert!(read_version(dir.path()).is_err());
        dir.write("manifest.json", br#"{"version":"4.5.6"}"#);
        assert_eq!(read_version(dir.path()).unwrap(), "4.5.6");
    }

    #[test]
    fn plugin_dir_override() {
        let root = Path::new("/repo");
        assert_eq!(plugin_dir(root, None), root.join("plugin"));
        assert_eq!(plugin_dir(root, Some(OsString::new())), root.join("plugin"));
        assert_eq!(plugin_dir(root, Some("other".into())), root.join("other"));
        let abs = std::env::temp_dir().join("elsewhere");
        assert_eq!(plugin_dir(root, Some(abs.clone().into_os_string())), abs);
    }

    #[test]
    fn packing_an_empty_directory_is_refused() {
        let empty = TempDir::new("empty");
        empty.write("only/.gitkeep", b"");
        let out = TempDir::new("out");
        assert!(pack(empty.path(), &out.path().join("x.xpi")).is_err());
    }
}
