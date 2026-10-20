//! `EngineError`: what `Engine::step` returns when it cannot produce a `StepOut`.
//!
//! These are *engine-level* faults (bad protocol, not configured, executor full, internal invariant broken).
//! Per-item problems are never errors: they become `Outcome`s in the run report. The wasm crate maps an
//! `EngineError` to `Envelope::Err(ErrBody { code, message })`; JS then fails the current run cleanly (no replay).
use crate::proto::ErrBody;
use serde::Serialize;
use std::fmt;

/// Stable error codes (snake_case strings, shown in the run report and asserted by tests).
pub mod code {
    pub const PROTOCOL_MISMATCH: &str = "protocol_mismatch";
    pub const NOT_CONFIGURED: &str = "not_configured";
    pub const BAD_COMMAND: &str = "bad_command";
    pub const BUSY: &str = "busy";
    pub const EXECUTOR_FULL: &str = "executor_full";
    /// A `RefCell` was re-entered (`Shared::with` returned `None`): an engine bug, never user input.
    pub const REENTERED: &str = "reentered";
    pub const INTERNAL: &str = "internal";
    pub const NOT_IMPLEMENTED: &str = "not_implemented";
}

/// Engine-level failure. `code` is one of [`code`]; `message` is for logs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EngineError {
    pub code: String,
    pub message: String,
}

impl EngineError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    /// Placeholder error used by skeleton bodies. Work packages replace every use.
    pub fn not_implemented(what: &str) -> Self {
        Self::new(code::NOT_IMPLEMENTED, format!("not implemented: {what}"))
    }

    /// The wire form (`Envelope::Err`).
    pub fn to_err_body(&self) -> ErrBody {
        ErrBody {
            code: self.code.clone(),
            message: self.message.clone(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for EngineError {}

impl From<&EngineError> for ErrBody {
    fn from(e: &EngineError) -> Self {
        e.to_err_body()
    }
}
