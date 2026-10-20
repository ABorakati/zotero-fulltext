//! cargo xtask dist   -> build wasm (cdylib-only, release), stage into plugin-skel/, zip out/*.xpi
//! cargo xtask wasm   -> just build + stage the .wasm
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

type R<T> = Result<T, Box<dyn std::error::Error>>;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf).unwrap_or_default()
}

fn build_wasm() -> R<PathBuf> {
    let root = root();
    let target_dir = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
    let t = Instant::now();
    // Keep the builder's home directory out of panic Location strings in the .wasm.
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    let rustflags = if home.is_empty() { String::new() } else { format!("--remap-path-prefix={home}=~") };
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags) // overrides [target.*].rustflags
        .args(["rustc", "-q", "-p", "fulltext-core", "--lib", "--crate-type", "cdylib"])
        .args(["--release", "--target", "wasm32-unknown-unknown"])
        .status()?;
    if !status.success() {
        return Err("cargo rustc (wasm) failed".into());
    }
    let wasm = target_dir.join("wasm32-unknown-unknown/release/fulltext_core.wasm");
    let staged = root.join("plugin-skel/content/fulltext_core.wasm");
    fs::copy(&wasm, &staged)?;
    fs::copy(root.join("js/fulltext-core.js"), root.join("plugin-skel/content/fulltext-core.js"))?;
    println!("wasm: {} bytes in {:.1}s -> {}", fs::metadata(&staged)?.len(), t.elapsed().as_secs_f32(), staged.display());
    Ok(staged)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> R<()> {
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() { walk(&p, out)?; } else { out.push(p); }
    }
    Ok(())
}

fn zip_xpi() -> R<()> {
    let root = root();
    let src = root.join("plugin-skel");
    let xpi = root.join("out/zotero-fulltext-spike.xpi");
    let mut files = Vec::new();
    walk(&src, &mut files)?;
    files.sort(); // reproducible order
    let mut zip = ZipWriter::new(File::create(&xpi)?);
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(9))
        .last_modified_time(zip::DateTime::default()) // reproducible timestamps
        .unix_permissions(0o644);
    for f in &files {
        let rel = f.strip_prefix(&src)?.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"); // XPI paths use '/'
        zip.start_file(rel.as_str(), opts)?;
        zip.write_all(&fs::read(f)?)?;
        println!("  + {rel}");
    }
    zip.finish()?;
    println!("xpi: {} bytes -> {}", fs::metadata(&xpi)?.len(), xpi.display());
    Ok(())
}

fn main() -> R<()> {
    match std::env::args().nth(1).as_deref() {
        Some("wasm") => build_wasm().map(|_| ()),
        Some("dist") | None => {
            build_wasm()?;
            zip_xpi()
        }
        Some(other) => Err(format!("unknown task {other}; use: wasm | dist").into()),
    }
}
