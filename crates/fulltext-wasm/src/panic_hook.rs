//! Panic hook: keeps the last panic message readable after the `panic = "abort"` trap.
//!
//! Under `panic = "abort"` the hook still runs before the abort, so the message survives in static memory and the
//! host can read it with `fx_last_panic` even though the instance is dead. Nothing here allocates on the read path.
//!
//! Lifetime of the stored buffer: it belongs to the instance that panicked and is never freed or moved while that
//! instance exists (`fx_free` ignores its address, see `abi::release`). A second panic on the same instance replaces
//! the string, so a host must copy the bytes out before it calls anything else on a poisoned instance. A NEW instance
//! of the same module has its own memory and its own (empty) buffer: nothing carries over.
use fulltext_engine::proto::ErrBody;
use std::sync::{Mutex, Once};

static INSTALL: Once = Once::new();
/// JSON `{"code":"panic","message":...}`; empty until a panic happens. (`Mutex` is an uncontended no-op on wasm32.)
static LAST: Mutex<String> = Mutex::new(String::new());

/// The JSON stored for a panic with `message` (pure: tested natively without touching the global hook).
fn panic_json(message: &str) -> String {
    let body = ErrBody {
        code: "panic".to_owned(),
        message: message.to_owned(),
    };
    serde_json::to_string(&body)
        .unwrap_or_else(|_| r#"{"code":"panic","message":"unprintable panic"}"#.to_owned())
}

/// Install the hook once per instance (called lazily by the first export that runs).
pub fn install() {
    INSTALL.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let json = panic_json(&info.to_string());
            if let Ok(mut slot) = LAST.lock() {
                *slot = json;
            }
        }));
    });
}

/// Run `f` on the stored panic JSON (empty string when no panic happened).
pub fn with_last<R>(f: impl FnOnce(&str) -> R) -> R {
    match LAST.lock() {
        Ok(slot) => f(slot.as_str()),
        Err(_) => f(""),
    }
}

/// `(address, length)` of the stored message for `fx_last_panic`; `(0, 0)` when nothing is stored.
pub fn buffer() -> (usize, usize) {
    with_last(|s| {
        if s.is_empty() {
            (0, 0)
        } else {
            (s.as_ptr() as usize, s.len())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn panic_json_is_a_valid_error_body() {
        let text =
            "panicked at src/x.rs:1:2:\nquote \" backslash \\ tab \t nul \u{0} snowman \u{2603}";
        let parsed: Value = serde_json::from_str(&panic_json(text)).unwrap();
        assert_eq!(parsed["code"], "panic");
        assert_eq!(parsed["message"], text);
    }

    #[test]
    fn nothing_is_stored_before_a_panic() {
        // No test in this crate panics through the installed hook, so the buffer stays empty.
        assert_eq!(buffer(), (0, 0));
        assert!(with_last(str::is_empty));
    }
}
