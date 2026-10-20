//! fulltext-engine: the sans-IO async engine of zotero-fulltext.
//!
//! The engine never performs I/O. JS feeds it `proto::StepIn { now, wall, commands, completions }`;
//! it answers with `proto::StepOut { effects, next_timer, more, ignored, stats }`. Inside, one async task per
//! item runs the whole pipeline (`pipeline::run_item`) on a tiny single-threaded executor with real `Waker`s;
//! every leaf `.await` is an effect that JS performs concurrently and a completion that wakes the task.
//!
//! Module map (see docs/design/engine.md):
//! - `proto`    frozen JS <-> engine contract (verbatim copy of docs/design/contracts/proto.rs)
//! - `error`    `EngineError`
//! - `cx`       `Shared` state cell, `State`, `Cx` / `ItemCx` (what pipeline futures hold), `Ctl`
//! - `executor` task table, `Waker`, poll budget, cancel-by-owner
//! - `ops`      reactor (op slots, virtual timers), leaf futures, typed op wrappers, `Body` RAII
//! - `sched`    per-host gates, token buckets, licence ledger, halts, host knowledge, timeouts
//! - `login`    single-flight institutional session gate (epochs) + interactive window gate
//! - `run`      run manager: StartRun / cancel / user actions / confirms / reports
//! - `pipeline` the per-item async pipeline (OA discovery, attempts, institutional route, save)
//! - `l10n`     Fluent ids and `ProgressRow` builders
//! - `engine`   `Engine::new` / `Engine::step`
//! - `sim`      (tests only) deterministic simulation harness and failure-catalogue scenarios

pub mod error;
// The contract is frozen: Effect has a few large variants and we do not restructure it.
#[allow(clippy::large_enum_variant)]
pub mod proto;

pub(crate) mod cx;
pub(crate) mod executor;
pub(crate) mod l10n;
pub(crate) mod login;
pub(crate) mod ops;
pub(crate) mod pipeline;
pub(crate) mod run;
pub(crate) mod sched;

mod engine;
#[cfg(test)]
mod sim;

pub use engine::Engine;
pub use error::EngineError;
