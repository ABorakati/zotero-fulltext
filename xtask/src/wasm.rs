//! Inspection of the built `.wasm` BINARY: a hand-written section reader (no dependencies) plus the checks
//! `cargo xtask build` applies before the module is accepted.
//!
//! The reader only understands what the checks need: the import section (id 2) and the export section (id 7),
//! with unsigned LEB128 integers and UTF-8 names. Every other section is skipped by its declared size. All reads
//! are bounds-checked and return [`ParseError`]; nothing here indexes a slice.
use std::error::Error;
use std::fmt;
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// The exports the plugin module must have, exactly. `memory`, `__data_end` and `__heap_base` come from wasm-ld.
pub const RELEASE_EXPORTS: [&str; 11] = [
    "memory",
    "__data_end",
    "__heap_base",
    "fx_abi_version",
    "fx_alloc",
    "fx_free",
    "fx_inspect",
    "fx_step",
    "fx_reset",
    "fx_last_panic",
    "fx_login_state",
];
/// The extra export of the `--debug-panic` test build.
pub const DEBUG_EXPORT: &str = "fx_debug_panic";

/// Warn above this many raw bytes (1 MiB = 1,048,576 B).
pub const WARN_RAW_BYTES: usize = 1024 * 1024;
/// Fail above this many raw bytes (1.5 MiB = 1,572,864 B).
pub const MAX_RAW_BYTES: usize = 1536 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The input ended inside the named element.
    Truncated(&'static str),
    BadMagic,
    BadVersion(u32),
    /// A LEB128 integer longer than its type allows.
    BadLeb(&'static str),
    BadUtf8(&'static str),
    /// An import or export kind byte outside 0..=4.
    BadKind(u8),
    /// A section whose entries do not fill its declared size.
    TrailingBytes(u8),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated(what) => write!(f, "truncated while reading {what}"),
            Self::BadMagic => f.write_str("not a wasm module (bad magic)"),
            Self::BadVersion(v) => write!(f, "unsupported wasm version {v}"),
            Self::BadLeb(what) => write!(f, "malformed LEB128 in {what}"),
            Self::BadUtf8(what) => write!(f, "invalid UTF-8 in {what}"),
            Self::BadKind(k) => write!(f, "unknown import/export kind {k}"),
            Self::TrailingBytes(id) => write!(f, "section {id} has bytes after its last entry"),
        }
    }
}

impl Error for ParseError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub module: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    /// 0 func, 1 table, 2 memory, 3 global, 4 tag.
    pub kind: u8,
}

/// What [`parse`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Module {
    pub imports: Vec<Import>,
    pub exports: Vec<Export>,
}

/// A cursor over a byte slice; every read advances it or fails.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    fn byte(&mut self, what: &'static str) -> Result<u8, ParseError> {
        let (&first, rest) = self.rest.split_first().ok_or(ParseError::Truncated(what))?;
        self.rest = rest;
        Ok(first)
    }

    fn take(&mut self, len: usize, what: &'static str) -> Result<&'a [u8], ParseError> {
        let (head, tail) = self
            .rest
            .split_at_checked(len)
            .ok_or(ParseError::Truncated(what))?;
        self.rest = tail;
        Ok(head)
    }

    /// Unsigned LEB128, at most 10 bytes, value must fit in `u64`.
    fn leb_u64(&mut self, what: &'static str) -> Result<u64, ParseError> {
        let mut value = 0_u64;
        for shift in (0..70).step_by(7) {
            let byte = self.byte(what)?;
            let payload = u64::from(byte & 0x7f);
            // The tenth byte may only carry the single remaining bit.
            if shift == 63 && payload > 1 {
                return Err(ParseError::BadLeb(what));
            }
            value |= payload << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(ParseError::BadLeb(what))
    }

    fn leb_u32(&mut self, what: &'static str) -> Result<u32, ParseError> {
        u32::try_from(self.leb_u64(what)?).map_err(|_| ParseError::BadLeb(what))
    }

    fn name(&mut self, what: &'static str) -> Result<String, ParseError> {
        let len = self.leb_u32(what)?;
        let bytes = self.take(
            usize::try_from(len).map_err(|_| ParseError::BadLeb(what))?,
            what,
        )?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ParseError::BadUtf8(what))
    }

    /// Skip a `limits` (min, optional max); the flag byte says whether a max follows.
    fn limits(&mut self, what: &'static str) -> Result<(), ParseError> {
        let flags = self.byte(what)?;
        self.leb_u64(what)?;
        if flags & 1 != 0 {
            self.leb_u64(what)?;
        }
        Ok(())
    }

    /// Skip the type descriptor of one import, after its kind byte.
    fn import_descriptor(&mut self, kind: u8) -> Result<(), ParseError> {
        match kind {
            0 => self.leb_u32("function import type").map(drop),
            1 => {
                self.byte("table element type")?;
                self.limits("table limits")
            }
            2 => self.limits("memory limits"),
            3 => {
                self.byte("global type")?;
                self.byte("global mutability").map(drop)
            }
            4 => {
                self.byte("tag attribute")?;
                self.leb_u32("tag type").map(drop)
            }
            other => Err(ParseError::BadKind(other)),
        }
    }
}

fn parse_imports(payload: &[u8], out: &mut Vec<Import>) -> Result<(), ParseError> {
    let mut r = Reader::new(payload);
    for _ in 0..r.leb_u32("import count")? {
        let module = r.name("import module name")?;
        let name = r.name("import name")?;
        let kind = r.byte("import kind")?;
        r.import_descriptor(kind)?;
        out.push(Import { module, name });
    }
    if r.is_empty() {
        Ok(())
    } else {
        Err(ParseError::TrailingBytes(2))
    }
}

fn parse_exports(payload: &[u8], out: &mut Vec<Export>) -> Result<(), ParseError> {
    let mut r = Reader::new(payload);
    for _ in 0..r.leb_u32("export count")? {
        let name = r.name("export name")?;
        let kind = r.byte("export kind")?;
        if kind > 4 {
            return Err(ParseError::BadKind(kind));
        }
        r.leb_u32("export index")?;
        out.push(Export { name, kind });
    }
    if r.is_empty() {
        Ok(())
    } else {
        Err(ParseError::TrailingBytes(7))
    }
}

/// Read the import and export sections of a binary module.
pub fn parse(bytes: &[u8]) -> Result<Module, ParseError> {
    let mut r = Reader::new(bytes);
    if r.take(4, "magic")? != b"\0asm" {
        return Err(ParseError::BadMagic);
    }
    let version = r
        .take(4, "version")?
        .iter()
        .rev()
        .fold(0_u32, |acc, &b| (acc << 8) | u32::from(b));
    if version != 1 {
        return Err(ParseError::BadVersion(version));
    }
    let mut module = Module::default();
    while !r.is_empty() {
        let id = r.byte("section id")?;
        let size = r.leb_u32("section size")?;
        let payload = r.take(
            usize::try_from(size).map_err(|_| ParseError::BadLeb("section size"))?,
            "section payload",
        )?;
        match id {
            2 => parse_imports(payload, &mut module.imports)?,
            7 => parse_exports(payload, &mut module.exports)?,
            _ => {}
        }
    }
    Ok(module)
}

/// `needle` found in `haystack`, comparing ASCII letters without regard to case (Windows paths).
fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle))
}

/// Size of `bytes` after deflate level 9, measured as a one-entry zip in memory (about the cost of the wasm inside
/// the XPI, or of a gzip transfer, give or take the ~100 bytes of zip headers).
pub fn deflated_len(bytes: &[u8]) -> Result<usize, Box<dyn Error>> {
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(9));
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("module.wasm", opts)?;
    zip.write_all(bytes)?;
    Ok(zip.finish()?.into_inner().len())
}

/// What a module must satisfy to be accepted.
pub struct Policy<'a> {
    /// Allowed export names, exactly (see [`RELEASE_EXPORTS`]).
    pub exports: Vec<&'a str>,
    /// Strings that must not occur anywhere in the binary (the builder's directories).
    pub forbidden: Vec<String>,
    pub warn_raw: usize,
    pub max_raw: usize,
}

impl Policy<'_> {
    /// The policy for the plugin module, or the `--debug-panic` test module (one more export).
    pub fn new(debug_panic: bool, forbidden: Vec<String>) -> Self {
        let mut exports = RELEASE_EXPORTS.to_vec();
        if debug_panic {
            exports.push(DEBUG_EXPORT);
        }
        Self {
            exports,
            forbidden,
            warn_raw: WARN_RAW_BYTES,
            max_raw: MAX_RAW_BYTES,
        }
    }
}

/// A module that passed [`verify`].
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub raw: usize,
    pub deflated: usize,
    pub exports: usize,
    pub warnings: Vec<String>,
}

impl Report {
    /// The single summary line `cargo xtask build` prints.
    pub fn summary(&self) -> String {
        format!(
            "wasm ok: {} B raw, ~{} B deflated, 0 imports, {} exports, no builder paths",
            self.raw, self.deflated, self.exports
        )
    }
}

/// Check `bytes` against `policy`. Returns every violation at once, so one build shows all the problems.
pub fn verify(bytes: &[u8], policy: &Policy<'_>) -> Result<Report, Vec<String>> {
    let module = parse(bytes).map_err(|e| vec![format!("cannot read the module: {e}")])?;
    let mut problems = Vec::new();

    // (a) zero imports: JS must not be able to hand the module anything.
    for import in &module.imports {
        problems.push(format!(
            "imports `{}.{}`; the module must import nothing",
            import.module, import.name
        ));
    }

    // (b) exactly the documented export set.
    let found: Vec<&str> = module.exports.iter().map(|e| e.name.as_str()).collect();
    for name in &policy.exports {
        if !found.contains(name) {
            problems.push(format!("missing export `{name}`"));
        }
    }
    for name in &found {
        if !policy.exports.contains(name) {
            problems.push(format!("unexpected export `{name}`"));
        }
    }
    if let Some(dup) = found
        .iter()
        .enumerate()
        .find_map(|(i, n)| found.iter().skip(i + 1).find(|m| *m == n))
    {
        problems.push(format!("export `{dup}` appears twice"));
    }

    // (c) no builder directories (they would leak through panic locations and embedded paths).
    for needle in &policy.forbidden {
        if contains_ignore_case(bytes, needle.as_bytes()) {
            problems.push(format!(
                "the binary contains the builder path fragment `{needle}`"
            ));
        }
    }

    // (d) size.
    let raw = bytes.len();
    let mut warnings = Vec::new();
    if raw > policy.max_raw {
        problems.push(format!(
            "{raw} B raw exceeds the hard limit of {} B",
            policy.max_raw
        ));
    } else if raw > policy.warn_raw {
        warnings.push(format!(
            "{raw} B raw is above the {} B budget (hard limit {} B)",
            policy.warn_raw, policy.max_raw
        ));
    }

    if !problems.is_empty() {
        return Err(problems);
    }
    let deflated =
        deflated_len(bytes).map_err(|e| vec![format!("cannot measure the deflated size: {e}")])?;
    Ok(Report {
        raw,
        deflated,
        exports: module.exports.len(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(module (type (func)) (func (type 0)) (memory 1) (export "memory" (memory 0)) (export "fx_abi_version" (func 0)))`
    /// assembled by hand: header, type, function, memory, export and code sections.
    #[rustfmt::skip]
    const TINY: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,                   // \0asm, version 1
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00,                               // type section: [] -> []
        0x03, 0x02, 0x01, 0x00,                                           // function section: one function, type 0
        0x05, 0x03, 0x01, 0x00, 0x01,                                     // memory section: min 1 page
        0x07, 0x1b, 0x02,                                                 // export section, 2 entries
        0x06, b'm', b'e', b'm', b'o', b'r', b'y', 0x02, 0x00,             //   "memory" memory 0
        0x0e, b'f', b'x', b'_', b'a', b'b', b'i', b'_', b'v', b'e', b'r', b's', b'i', b'o', b'n', 0x00, 0x00, // "fx_abi_version" func 0
        0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b,                               // code section: one empty body
    ];

    /// `(module (type (func)) (import "env" "f" (func (type 0))) (import "env" "mem" (memory 1 2)))` plus a custom section.
    #[rustfmt::skip]
    const WITH_IMPORTS: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x05, 0x04, b'n', b'a', b'm', b'e',                         // custom section "name" (skipped)
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
        0x02, 0x15, 0x02,                                                 // import section, 2 entries
        0x03, b'e', b'n', b'v', 0x01, b'f', 0x00, 0x00,                   //   env.f func type 0
        0x03, b'e', b'n', b'v', 0x03, b'm', b'e', b'm', 0x02, 0x01, 0x01, 0x02, // env.mem memory min 1 max 2
    ];

    #[test]
    fn reads_exports_of_a_tiny_module() {
        let m = parse(TINY).unwrap();
        assert!(m.imports.is_empty());
        assert_eq!(
            m.exports,
            vec![
                Export {
                    name: "memory".into(),
                    kind: 2
                },
                Export {
                    name: "fx_abi_version".into(),
                    kind: 0
                }
            ]
        );
    }

    #[test]
    fn reads_imports_and_skips_custom_sections() {
        let m = parse(WITH_IMPORTS).unwrap();
        assert_eq!(
            m.imports,
            vec![
                Import {
                    module: "env".into(),
                    name: "f".into()
                },
                Import {
                    module: "env".into(),
                    name: "mem".into()
                }
            ]
        );
        assert!(m.exports.is_empty());
    }

    #[test]
    fn rejects_malformed_input_without_panicking() {
        assert_eq!(parse(b""), Err(ParseError::Truncated("magic")));
        assert_eq!(parse(b"\0asx\x01\0\0\0"), Err(ParseError::BadMagic));
        assert_eq!(parse(b"\0asm\x02\0\0\0"), Err(ParseError::BadVersion(2)));
        // Every strict prefix of a valid module either parses (cut at a section boundary) or errors; none panics.
        for cut in 0..TINY.len() {
            let _ = parse(TINY.get(..cut).unwrap());
        }
        for cut in 0..WITH_IMPORTS.len() {
            let _ = parse(WITH_IMPORTS.get(..cut).unwrap());
        }
        // A section that claims more bytes than the file has.
        assert_eq!(
            parse(b"\0asm\x01\0\0\0\x07\x7f\x00"),
            Err(ParseError::Truncated("section payload"))
        );
        // An 11-byte LEB128 section size.
        assert!(matches!(
            parse(b"\0asm\x01\0\0\0\x07\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x01"),
            Err(ParseError::BadLeb(_))
        ));
        // Export kind 9 does not exist.
        assert_eq!(
            parse(b"\0asm\x01\0\0\0\x07\x05\x01\x01x\x09\x00"),
            Err(ParseError::BadKind(9))
        );
        // Bytes left in a section after its declared entries.
        assert_eq!(
            parse(b"\0asm\x01\0\0\0\x07\x03\x00\x00\x00"),
            Err(ParseError::TrailingBytes(7))
        );
    }

    #[test]
    fn leb128_edge_cases() {
        let mut r = Reader::new(&[0xe5, 0x8e, 0x26]);
        assert_eq!(r.leb_u64("t"), Ok(624_485));
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
        assert_eq!(r.leb_u64("t"), Ok(u64::MAX));
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]);
        assert_eq!(r.leb_u64("t"), Err(ParseError::BadLeb("t")));
        let mut r = Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x10]);
        assert_eq!(r.leb_u32("t"), Err(ParseError::BadLeb("t")));
        let mut r = Reader::new(&[0x80]);
        assert_eq!(r.leb_u64("t"), Err(ParseError::Truncated("t")));
    }

    fn policy(forbidden: &[&str]) -> Policy<'static> {
        let mut p = Policy::new(false, forbidden.iter().map(|s| (*s).to_owned()).collect());
        p.exports = vec!["memory", "fx_abi_version"];
        p
    }

    #[test]
    fn verify_accepts_a_conforming_module() {
        let report = verify(TINY, &policy(&["C:\\Users"])).unwrap();
        assert_eq!(report.raw, TINY.len());
        assert_eq!(report.exports, 2);
        assert!(report.warnings.is_empty());
        assert!(
            report.summary().starts_with("wasm ok: "),
            "{}",
            report.summary()
        );
    }

    #[test]
    fn verify_reports_every_violation() {
        let problems = verify(WITH_IMPORTS, &policy(&[])).unwrap_err();
        assert!(
            problems.iter().any(|p| p.contains("imports `env.f`")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("imports `env.mem`")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("missing export `fx_abi_version`")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("missing export `memory`")),
            "{problems:?}"
        );

        let mut p = policy(&[]);
        p.exports = vec!["memory"];
        let problems = verify(TINY, &p).unwrap_err();
        assert_eq!(
            problems,
            vec!["unexpected export `fx_abi_version`".to_owned()]
        );
    }

    #[test]
    fn verify_finds_builder_paths_ignoring_case() {
        // Text can only hide inside a valid module as section content: append it as a custom section.
        let text = b"panicked at c:\\users\\bob\\.cargo\\registry\\src\\x.rs";
        let mut leaky = TINY.to_vec();
        leaky.extend_from_slice(&[0x00, u8::try_from(text.len() + 2).unwrap(), 0x01, b'x']);
        leaky.extend_from_slice(text);
        assert!(parse(&leaky).is_ok());
        let problems =
            verify(&leaky, &policy(&["C:\\Users", "\\.cargo\\", ".rustup"])).unwrap_err();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems.iter().all(|p| p.contains("builder path")),
            "{problems:?}"
        );
        assert!(verify(&leaky, &policy(&[".rustup"])).is_ok());
    }

    #[test]
    fn verify_enforces_the_size_budget() {
        let mut p = policy(&[]);
        p.warn_raw = TINY.len() - 1;
        let warned = verify(TINY, &p).unwrap();
        assert_eq!(warned.warnings.len(), 1);
        p.max_raw = TINY.len() - 1;
        let problems = verify(TINY, &p).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|m| m.contains("exceeds the hard limit")),
            "{problems:?}"
        );
    }

    #[test]
    fn verify_refuses_garbage() {
        let problems = verify(b"<html>not wasm</html>", &policy(&[])).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|m| m.contains("cannot read the module")),
            "{problems:?}"
        );
    }

    #[test]
    fn the_debug_policy_allows_exactly_one_more_export() {
        assert_eq!(
            Policy::new(true, vec![]).exports.len(),
            Policy::new(false, vec![]).exports.len() + 1
        );
        assert!(!Policy::new(false, vec![]).exports.contains(&DEBUG_EXPORT));
        assert!(Policy::new(true, vec![]).exports.contains(&DEBUG_EXPORT));
    }

    #[test]
    fn deflated_len_is_smaller_for_compressible_input() {
        let zeros = vec![0_u8; 100_000];
        assert!(deflated_len(&zeros).unwrap() < 1_000);
    }

    /// The module staged by `cargo xtask build`, when present: it must parse and satisfy the export/import rules.
    #[test]
    fn the_staged_module_conforms_when_present() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("plugin")
            .join("content")
            .join("fulltext.wasm");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("note: {} not built yet; skipping", path.display());
            return;
        };
        let module = parse(&bytes).unwrap();
        assert!(module.imports.is_empty(), "{:?}", module.imports);
        let mut names: Vec<&str> = module.exports.iter().map(|e| e.name.as_str()).collect();
        let mut want = RELEASE_EXPORTS.to_vec();
        names.sort_unstable();
        want.sort_unstable();
        assert_eq!(names, want);
    }
}
