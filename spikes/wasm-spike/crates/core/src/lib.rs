//! fulltext-core spike: pure logic for the Zotero "Get Full Text" plugin.
//! No I/O. Builds natively (cargo test) and as wasm32-unknown-unknown cdylib.

pub mod abi;
pub mod html;
pub mod logic;
