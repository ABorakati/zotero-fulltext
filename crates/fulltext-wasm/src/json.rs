//! JSON-in / JSON-out layer: bytes in, bytes out, no pointers, so it is unit-testable on the host.
//!
//! Contract of every function here: the result is a complete, non-empty `Envelope<T>` as UTF-8 JSON bytes
//! (`{"ok":...}` or `{"err":{"code","message"}}`), whatever the input. Hostile input (empty, truncated, invalid
//! UTF-8, megabytes of `{`, 10 000 nested `[`, integers outside their type) becomes an `err` value: serde_json caps
//! recursion at 128 levels, so nothing here can overflow the 1 MiB wasm stack. Panics are the engine's and the core's
//! problem, and they trap (`panic = "abort"`); the host then discards the instance (see `panic_hook`).
use fulltext_core::inspect::{inspect, login_state};
use fulltext_core::model::InspectMeta;
use fulltext_engine::Engine;
use fulltext_engine::proto::{Envelope, ErrBody, StepIn};
use serde::Serialize;
use std::cell::RefCell;

thread_local! {
    /// The engine owned by this wasm instance. `None` until the first `fx_step` after instantiation or `reset`.
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

/// Static fallback when even the error envelope cannot be serialized.
const SERIALIZE_FAILED: &[u8] =
    br#"{"err":{"code":"serialize","message":"failed to serialize the result"}}"#;

/// Serialize `Envelope::Ok(value)`.
pub fn ok<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(&Envelope::Ok(value)).unwrap_or_else(|_| SERIALIZE_FAILED.to_vec())
}

/// Serialize `Envelope::Err { code, message }`.
pub fn err(code: &str, message: impl Into<String>) -> Vec<u8> {
    err_body(ErrBody {
        code: code.to_owned(),
        message: message.into(),
    })
}

/// Serialize `Envelope::Err(body)`.
fn err_body(body: ErrBody) -> Vec<u8> {
    serde_json::to_vec(&Envelope::<()>::Err(body)).unwrap_or_else(|_| SERIALIZE_FAILED.to_vec())
}

/// `fx_inspect`: `meta` is JSON `InspectMeta`, `body` the staged response bytes.
pub fn inspect_bytes(meta: &[u8], body: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<InspectMeta>(meta) {
        Ok(m) => ok(&inspect(&m, body)),
        Err(e) => err("bad_json", format!("inspect meta: {e}")),
    }
}

/// `fx_login_state`: `url` and `html` are UTF-8 text; an empty `html` means "no document yet".
pub fn login_state_bytes(url: &[u8], html: &[u8]) -> Vec<u8> {
    let url = String::from_utf8_lossy(url);
    let html = String::from_utf8_lossy(html);
    let html = (!html.is_empty()).then_some(html.as_ref());
    ok(&login_state(url.trim(), html))
}

/// `fx_step`: parse `StepIn`, run the global engine once, serialize `StepOut`.
pub fn step_bytes(input: &[u8]) -> Vec<u8> {
    let step_in = match serde_json::from_slice::<StepIn>(input) {
        Ok(s) => s,
        Err(e) => return err("bad_json", format!("step input: {e}")),
    };
    ENGINE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut slot) => match slot.get_or_insert_with(Engine::new).step(step_in) {
            Ok(out) => ok(&out),
            Err(e) => err_body(e.to_err_body()),
        },
        Err(_) => err("reentered", "the engine is already running"),
    })
}

/// `fx_reset`: drop the engine and everything it holds.
pub fn reset() {
    ENGINE.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = None;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serializer;
    use serde_json::{Value, json};

    const NONE_META: &str =
        r#"{"spec":{"expect":"none"},"resp":{"status":200,"final_url":"https://example.org/"}}"#;
    const PAGE_META: &str = r#"{"spec":{"expect":"page","requested":{},"route":"oa"},"resp":{"status":200,"final_url":"https://example.org/a"}}"#;
    const PDF_META: &str = r#"{"spec":{"expect":"pdf","requested":{},"route":"oa"},"resp":{"status":200,"final_url":"https://example.org/a.pdf"}}"#;

    fn parse(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    /// Every result must be a non-empty, valid UTF-8 JSON object with exactly one of `ok` / `err`.
    fn envelope(bytes: &[u8]) -> Value {
        assert!(!bytes.is_empty(), "empty result buffer");
        assert!(std::str::from_utf8(bytes).is_ok(), "result is not UTF-8");
        let value = parse(bytes);
        let object = value.as_object().unwrap();
        assert_eq!(
            object.len(),
            1,
            "envelope must have exactly one key: {value}"
        );
        assert!(
            object.contains_key("ok") || object.contains_key("err"),
            "{value}"
        );
        value
    }

    fn assert_err(bytes: &[u8], code: &str) -> Value {
        let value = envelope(bytes);
        assert_eq!(value["err"]["code"], code, "{value}");
        assert!(value["err"]["message"].is_string(), "{value}");
        value
    }

    fn assert_ok(bytes: &[u8]) -> Value {
        let value = envelope(bytes);
        assert!(value.get("ok").is_some(), "{value}");
        value
    }

    #[test]
    fn inspect_round_trips_an_api_spec() {
        let meta = json!({"spec": {"expect": "api", "source": "openalex", "requested": {"rxiv": false}},
                          "resp": {"status": 200, "final_url": "https://api.openalex.org/works/doi:10.1371/x", "body_len": 2}});
        let out = parse(&inspect_bytes(meta.to_string().as_bytes(), b"{}"));
        assert_eq!(out["ok"]["expect"], "api");
        assert_eq!(out["ok"]["source"], "openalex");
    }

    #[test]
    fn inspect_rejects_bad_meta_as_a_value() {
        let out = parse(&inspect_bytes(b"not json", b""));
        assert_eq!(out["err"]["code"], "bad_json");
    }

    #[test]
    fn step_rejects_bad_input_as_a_value() {
        assert_eq!(parse(&step_bytes(b"[]"))["err"]["code"], "bad_json");
    }

    #[test]
    fn step_returns_an_envelope_for_valid_input() {
        let out = parse(&step_bytes(br#"{"now":0,"wall":0}"#));
        assert!(out.get("ok").is_some() || out.get("err").is_some(), "{out}");
        reset();
    }

    #[test]
    fn login_state_envelope() {
        let out = parse(&login_state_bytes(
            b"https://login.microsoftonline.com/x",
            b"",
        ));
        assert!(out["ok"]["state"].is_string());
    }

    // ---- hostile input: every entry point answers with an envelope, never a panic or an empty buffer ----

    #[test]
    fn empty_inputs() {
        assert_err(&inspect_bytes(b"", b""), "bad_json");
        assert_err(&step_bytes(b""), "bad_json");
        assert_ok(&login_state_bytes(b"", b""));
        // Whitespace-only and literal `null` are not objects either.
        assert_err(&step_bytes(b"   \n"), "bad_json");
        assert_err(&step_bytes(b"null"), "bad_json");
        assert_err(&inspect_bytes(b"null", b""), "bad_json");
    }

    #[test]
    fn five_megabytes_of_open_braces() {
        let braces = vec![b'{'; 5_000_000];
        assert_err(&inspect_bytes(&braces, b""), "bad_json");
        assert_err(&step_bytes(&braces), "bad_json");
        // As a body or a document the same bytes are just data: every spec must still answer.
        for meta in [NONE_META, PAGE_META, PDF_META] {
            assert_ok(&inspect_bytes(meta.as_bytes(), &braces));
        }
        assert_ok(&login_state_bytes(b"https://example.org/", &braces));
        assert_ok(&login_state_bytes(&braces, &braces));
    }

    #[test]
    fn invalid_utf8() {
        assert_err(&inspect_bytes(&[0xff, 0xfe, 0xfd], b""), "bad_json");
        assert_err(&step_bytes(&[0xff, 0xfe, 0xfd]), "bad_json");
        // Invalid UTF-8 inside a JSON string is rejected as a value as well.
        assert_err(
            &step_bytes(b"{\"now\":0,\"wall\":0,\"commands\":[{\"type\":\"\xc3\x28\"}]}"),
            "bad_json",
        );
        // login_state takes plain text and repairs it instead of failing.
        assert_ok(&login_state_bytes(
            b"https://example.org/\xff",
            b"<html>\xc3\x28\xa0\xa1</html>",
        ));
        // Bodies are raw bytes: junk must classify, not crash.
        let junk: Vec<u8> = (0..4096_u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        for meta in [NONE_META, PAGE_META, PDF_META] {
            assert_ok(&inspect_bytes(meta.as_bytes(), &junk));
            assert_ok(&inspect_bytes(meta.as_bytes(), &[0xff; 3000]));
        }
    }

    #[test]
    fn deeply_nested_json_hits_the_recursion_limit_not_the_stack() {
        let deep_arrays = "[".repeat(10_000);
        assert_err(&step_bytes(deep_arrays.as_bytes()), "bad_json");
        // Nested inside a free-form `Value` (a completion payload): the 128-level recursion limit stops it, long
        // before the 1 MiB stack or the 10 000 levels matter.
        let payload = format!(r#"{{"now":0,"wall":0,"completions":[{{"op":1,"ok":{deep_arrays}"#);
        let out = assert_err(&step_bytes(payload.as_bytes()), "bad_json");
        assert!(
            out["err"]["message"]
                .as_str()
                .unwrap()
                .contains("recursion limit"),
            "{out}"
        );
        // Nested where a command, a completion or nothing at all is expected: a type or EOF error, still a value.
        for field in ["commands", "completions", "extra"] {
            let nested = format!(r#"{{"now":0,"wall":0,"{field}":{deep_arrays}"#);
            assert_err(&step_bytes(nested.as_bytes()), "bad_json");
        }
        let deep_objects = r#"{"a":"#.repeat(10_000);
        assert_err(&step_bytes(deep_objects.as_bytes()), "bad_json");
        assert_err(&inspect_bytes(deep_arrays.as_bytes(), b""), "bad_json");
        assert_err(&inspect_bytes(deep_objects.as_bytes(), b""), "bad_json");
        // Balanced, valid but absurdly deep: also an error value.
        let balanced = format!("{}{}", "[".repeat(5000), "]".repeat(5000));
        assert_err(&step_bytes(balanced.as_bytes()), "bad_json");
    }

    #[test]
    fn integers_outside_their_type_are_errors() {
        for now in [
            "18446744073709551616",
            "-1",
            "1e400",
            "0.5",
            "\"7\"",
            "null",
            "99999999999999999999999999",
        ] {
            let input = format!(r#"{{"now":{now},"wall":0}}"#);
            assert_err(&step_bytes(input.as_bytes()), "bad_json");
        }
        // u16 status out of range in the inspect meta.
        let meta = r#"{"spec":{"expect":"none"},"resp":{"status":70000,"final_url":"https://example.org/"}}"#;
        assert_err(&inspect_bytes(meta.as_bytes(), b""), "bad_json");
        // The largest valid u64 passes the parser (the engine decides what to do with it).
        let max = format!(r#"{{"now":{},"wall":{}}}"#, u64::MAX, u64::MAX);
        envelope(&step_bytes(max.as_bytes()));
        reset();
    }

    #[test]
    fn unknown_commands_and_shapes_are_errors() {
        assert_err(
            &step_bytes(br#"{"now":0,"wall":0,"commands":[{"type":"no_such_command"}]}"#),
            "bad_json",
        );
        assert_err(&step_bytes(br#"{"now":0}"#), "bad_json");
        assert_err(
            &inspect_bytes(br#"{"spec":{"expect":"nope"},"resp":{}}"#, b""),
            "bad_json",
        );
    }

    #[test]
    fn step_while_the_engine_is_borrowed_reports_reentry() {
        ENGINE.with(|cell| {
            let _guard = cell.borrow_mut();
            assert_err(&step_bytes(br#"{"now":0,"wall":0}"#), "reentered");
            reset(); // must be a harmless no-op while borrowed
        });
        // Back to normal afterwards.
        envelope(&step_bytes(br#"{"now":0,"wall":0}"#));
        reset();
    }

    #[test]
    fn reset_then_step_starts_from_a_fresh_engine() {
        let input = br#"{"now":5,"wall":5}"#;
        let first = envelope(&step_bytes(input));
        reset();
        assert_eq!(envelope(&step_bytes(input)), first);
        reset();
        reset(); // idempotent
    }

    // ---- ok() / err() ----

    struct Failing;
    impl Serialize for Failing {
        fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("refuses to serialize"))
        }
    }

    #[test]
    fn ok_and_err_never_return_an_empty_buffer() {
        let outputs = [
            ok(&()),
            ok(&""),
            ok(&Vec::<u8>::new()),
            ok(&Value::Null),
            ok(&json!({})),
            err("", ""),
            err("code", String::new()),
            err("e", "x".repeat(100_000)),
        ];
        for bytes in &outputs {
            envelope(bytes);
        }
    }

    #[test]
    fn serialization_failure_falls_back_to_a_static_error_envelope() {
        let out = assert_err(&ok(&Failing), "serialize");
        assert_eq!(out["err"]["message"], "failed to serialize the result");
        assert_eq!(ok(&Failing), SERIALIZE_FAILED);
    }

    #[test]
    fn err_escapes_hostile_messages() {
        let message =
            "line\nbreak \"quoted\" back\\slash \u{0} nul \u{1f4a5} emoji \u{2028} separator";
        let out = envelope(&err("c\"ode", message));
        assert_eq!(out["err"]["code"], "c\"ode");
        assert_eq!(out["err"]["message"], message);
    }

    #[test]
    fn login_state_handles_large_non_ascii_documents() {
        let html = "<p>\u{65e5}\u{672c}\u{8a9e} \u{1f4a1} caf\u{e9}</p>".repeat(40_000);
        assert!(html.len() > 1_000_000);
        let out = assert_ok(&login_state_bytes(
            b"https://login.microsoftonline.com/x",
            html.as_bytes(),
        ));
        assert!(out["ok"]["state"].is_string());
    }
}
