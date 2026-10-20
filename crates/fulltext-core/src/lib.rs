//! fulltext-core: the pure, total, I/O-free logic of the zotero-fulltext plugin.
//!
//! Nothing in this crate performs I/O, reads a clock, spawns a thread or panics. Every public function is total:
//! bad input produces a value (`ApiStatus::Unparseable`, `PageClass::Html`, `Inspection::Failed`, `None`), never a
//! panic, because the shipped wasm is built with `panic = "abort"` and a panic would poison the whole instance.
//!
//! Layout (design: `docs/design/core.md`):
//!
//! | module | role |
//! |---|---|
//! | [`model`] | frozen data contract shared with the engine and the JS host (copy of `docs/design/contracts/model.rs`) |
//! | [`ids`] | DOI / PMID / PMCID / arXiv normalisation from item fields |
//! | [`urls`] | host keys, EZproxy / OpenAthens / LibKey URL builders, proxify / unproxify, URL classification |
//! | [`oa`] | one parser per open-access API: `(RespMeta, body, requested Ids) -> ApiInspection` |
//! | [`rank`] | merge, dedupe, score and tier candidates |
//! | [`page`] | HTML tokenizer wrapper, response classifier, PDF-link extractor, per-publisher rules |
//! | [`pdf`] | PDF byte sniffing |
//! | [`inspect`] | the single entry point `inspect(meta, body)` plus `login_state` |
//! | [`text`] | tiny byte/text helpers shared by the above |
#![forbid(unsafe_code)]

pub mod ids;
pub mod inspect;
// The frozen contract is copied verbatim; hand-written `Default` impls on macro-generated enums are part of it.
#[allow(clippy::derivable_impls)]
pub mod model;
pub mod oa;
pub mod page;
pub mod pdf;
pub mod rank;
pub mod text;
pub mod urls;
