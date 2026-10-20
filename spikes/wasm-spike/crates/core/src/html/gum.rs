//! html5gum 0.8.4 backend: WHATWG tokenizer, with naive state switching so
//! script/style/textarea contents are not tokenized as tags.

use super::{Collector, Extracted};
use html5gum::{DefaultEmitter, Token, Tokenizer};

pub fn extract(html: &str) -> Extracted {
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut c = Collector::default();
    for token in Tokenizer::new_with_emitter(html, emitter) {
        let Ok(token) = token; // &str input: error type is Infallible
        match token {
            Token::StartTag(tag) => {
                let name = String::from_utf8_lossy(&tag.name).into_owned(); // already lower-case
                let get = |k: &str| {
                    tag.attributes.get(k.as_bytes()).map(|v| String::from_utf8_lossy(&v.value).into_owned())
                };
                c.start_tag(&name, &get);
            }
            Token::EndTag(tag) => c.end_tag(&String::from_utf8_lossy(&tag.name)),
            Token::String(s) => {
                if c.wants_text() {
                    c.text(&String::from_utf8_lossy(&s.value));
                }
            }
            _ => {}
        }
    }
    c.finish()
}
