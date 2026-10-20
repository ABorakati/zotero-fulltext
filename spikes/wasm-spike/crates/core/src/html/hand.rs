//! Hand-rolled tag scanner (~200 lines, zero deps). Not a spec tokenizer:
//! handles comments, doctype/PI, raw-text elements (script/style/textarea/title/
//! xmp/noembed/noframes/plaintext), quoted/unquoted attributes, case-insensitive
//! names and a small named-entity table plus numeric references.

use super::{Collector, Extracted};

const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title", "xmp", "noembed", "noframes", "plaintext"];

pub fn extract(html: &str) -> Extracted {
    let mut c = Collector::default();
    let b = html.as_bytes();
    let mut i = 0usize;
    let n = b.len();
    while i < n {
        let Some(off) = memchr(b'<', b.get(i..).unwrap_or_default()) else {
            if c.wants_text() {
                c.text(&decode_entities(html.get(i..).unwrap_or_default()));
            }
            break;
        };
        if off > 0 {
            if c.wants_text() {
                c.text(&decode_entities(html.get(i..i + off).unwrap_or_default()));
            }
        }
        i += off; // at '<'
        let rest = b.get(i..).unwrap_or_default();
        if rest.starts_with(b"<!--") {
            i = find(b, i + 4, b"-->").map_or(n, |p| p + 3);
        } else if rest.starts_with(b"<!") || rest.starts_with(b"<?") {
            i = find(b, i + 2, b">").map_or(n, |p| p + 1);
        } else if rest.starts_with(b"</") {
            let (name, after) = read_name(b, i + 2);
            if name.is_empty() {
                i = find(b, i + 2, b">").map_or(n, |p| p + 1); // bogus comment
            } else {
                c.end_tag(&name);
                i = find(b, after, b">").map_or(n, |p| p + 1);
            }
        } else if rest.get(1).is_some_and(u8::is_ascii_alphabetic) {
            let (name, mut j) = read_name(b, i + 1);
            let mut attrs: Vec<(String, String)> = Vec::new();
            // attributes
            loop {
                while b.get(j).is_some_and(|ch| ch.is_ascii_whitespace() || *ch == b'/') {
                    j += 1;
                }
                match b.get(j) {
                    None => break,
                    Some(b'>') => {
                        j += 1;
                        break;
                    }
                    _ => {}
                }
                let start = j;
                while b.get(j).is_some_and(|ch| !ch.is_ascii_whitespace() && !matches!(ch, b'=' | b'>' | b'/')) {
                    j += 1;
                }
                let aname = html.get(start..j).unwrap_or_default().to_ascii_lowercase();
                if j == start {
                    j += 1; // stray '=' etc.
                    continue;
                }
                while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                    j += 1;
                }
                let mut value = String::new();
                if b.get(j) == Some(&b'=') {
                    j += 1;
                    while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                        j += 1;
                    }
                    match b.get(j) {
                        Some(q @ (b'"' | b'\'')) => {
                            let end = memchr(*q, b.get(j + 1..).unwrap_or_default()).map_or(n, |p| j + 1 + p);
                            value = decode_entities(html.get(j + 1..end).unwrap_or_default());
                            j = (end + 1).min(n);
                        }
                        _ => {
                            let s = j;
                            while b.get(j).is_some_and(|ch| !ch.is_ascii_whitespace() && *ch != b'>') {
                                j += 1;
                            }
                            value = decode_entities(html.get(s..j).unwrap_or_default());
                        }
                    }
                }
                if !attrs.iter().any(|(k, _)| *k == aname) {
                    attrs.push((aname, value)); // first attribute wins (spec)
                }
            }
            let get = |k: &str| attrs.iter().find(|(an, _)| an == k).map(|(_, v)| v.clone());
            c.start_tag(&name, &get);
            i = j;
            if RAW_TEXT.contains(&name.as_str()) {
                // skip to the matching end tag, case-insensitively
                let close = format!("</{name}");
                let end = find_ci(b, i, close.as_bytes()).unwrap_or(n);
                if name == "title" || name == "textarea" {
                    // RCDATA: text is real text, but no tags inside
                    if c.wants_text() {
                c.text(&decode_entities(html.get(i..end).unwrap_or_default()));
            }
                }
                i = end;
            }
        } else {
            c.text("<");
            i += 1;
        }
    }
    c.finish()
}

fn read_name(b: &[u8], mut j: usize) -> (String, usize) {
    let s = j;
    while b.get(j).is_some_and(|ch| !ch.is_ascii_whitespace() && !matches!(ch, b'>' | b'/')) {
        j += 1;
    }
    let name = b.get(s..j).map(|v| String::from_utf8_lossy(v).to_ascii_lowercase()).unwrap_or_default();
    (name, j)
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&x| x == needle)
}

fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    b.get(from..)?.windows(pat.len()).position(|w| w == pat).map(|p| p + from)
}

fn find_ci(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    b.get(from..)?.windows(pat.len()).position(|w| w.eq_ignore_ascii_case(pat)).map(|p| p + from)
}

/// Small named-entity table: the ones that actually show up in hrefs/link text.
const ENTITIES: &[(&str, char)] = &[
    ("amp", '&'), ("lt", '<'), ("gt", '>'), ("quot", '"'), ("apos", '\''), ("nbsp", '\u{a0}'),
    ("ndash", '\u{2013}'), ("mdash", '\u{2014}'), ("hellip", '\u{2026}'), ("copy", '\u{a9}'),
    ("reg", '\u{ae}'), ("trade", '\u{2122}'), ("eacute", '\u{e9}'), ("egrave", '\u{e8}'),
    ("aacute", '\u{e1}'), ("oacute", '\u{f3}'), ("uuml", '\u{fc}'), ("ouml", '\u{f6}'),
    ("auml", '\u{e4}'), ("szlig", '\u{df}'), ("ccedil", '\u{e7}'), ("rsquo", '\u{2019}'),
    ("lsquo", '\u{2018}'), ("rdquo", '\u{201d}'), ("ldquo", '\u{201c}'), ("middot", '\u{b7}'),
    ("times", '\u{d7}'), ("thinsp", '\u{2009}'), ("ensp", '\u{2002}'), ("emsp", '\u{2003}'),
];

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(rest.get(..p).unwrap_or_default());
        let after = rest.get(p + 1..).unwrap_or_default();
        let semi = after.find(';').filter(|&q| q <= 32);
        let decoded = semi.and_then(|q| {
            let ent = after.get(..q)?;
            if let Some(num) = ent.strip_prefix('#') {
                let cp = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                    u32::from_str_radix(hex, 16).ok()?
                } else {
                    num.parse::<u32>().ok()?
                };
                Some((char::from_u32(cp).unwrap_or('\u{fffd}'), q + 1))
            } else {
                ENTITIES.iter().find(|(k, _)| *k == ent).map(|(_, ch)| (*ch, q + 1))
            }
        });
        match decoded {
            Some((ch, consumed)) => {
                out.push(ch);
                rest = after.get(consumed..).unwrap_or_default();
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}
