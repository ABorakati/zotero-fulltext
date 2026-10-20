//! A tolerant tag scanner: just enough XML for the three tiny documents the OA parsers read (arXiv Atom, S3
//! listings and errors, Alma ContextObjects). It is not a parser: no namespaces, no nesting of the same tag, and
//! malformed input yields fewer elements, never a panic.

/// One `<name attrs>inner</name>` element; self-closing tags have an empty `inner`.
pub(super) struct Element<'a> {
    pub attrs: &'a str,
    pub inner: &'a str,
}

/// All elements called `name` (full tag name including any `prefix:`), in document order. Elements of the same name
/// do not nest in the formats we read; a nested one is simply part of the outer element's `inner`.
pub(super) fn elements<'a>(text: &'a str, name: &str) -> Vec<Element<'a>> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find(&open) {
        let after = rest.get(pos + open.len()..).unwrap_or("");
        // `<entry` must not match `<entryfoo`.
        if !after
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/')
        {
            rest = after;
            continue;
        }
        let Some(end) = tag_end(after) else { break };
        let head = after.get(..end).unwrap_or("");
        let body = after.get(end + 1..).unwrap_or("");
        if let Some(attrs) = head.strip_suffix('/') {
            found.push(Element { attrs, inner: "" });
            rest = body;
            continue;
        }
        let Some(close_at) = body.find(&close) else {
            break;
        };
        found.push(Element {
            attrs: head,
            inner: body.get(..close_at).unwrap_or(""),
        });
        rest = body.get(close_at + close.len()..).unwrap_or("");
    }
    found
}

/// Byte offset of the `>` that closes the opening tag in `s` (quotes may contain `>`).
fn tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// Value of attribute `name` in an element's attribute text (either quote style, entities decoded).
pub(super) fn attr(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    while let Some(pos) = rest.find(name) {
        let preceded_ok = rest
            .get(..pos)
            .and_then(|p| p.chars().next_back())
            .is_none_or(char::is_whitespace);
        rest = rest.get(pos + name.len()..).unwrap_or("");
        if !preceded_ok {
            continue;
        }
        let Some(after_eq) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let after_eq = after_eq.trim_start();
        let Some(quote) = after_eq.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
            continue;
        };
        let value = after_eq.get(1..)?;
        let end = value.find(quote)?;
        return value.get(..end).map(decode);
    }
    None
}

/// Text content of an element body: CDATA unwrapped, entities decoded, trimmed.
pub(super) fn text(inner: &str) -> String {
    let inner = inner.trim();
    match inner
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
    {
        Some(raw) => raw.trim().to_owned(),
        None => decode(inner).trim().to_owned(),
    }
}

/// Text of the first `<name>` element, if non-empty.
pub(super) fn first_text(doc: &str, name: &str) -> Option<String> {
    elements(doc, name)
        .into_iter()
        .map(|e| text(e.inner))
        .find(|t| !t.is_empty())
}

/// Replace `&amp; &lt; &gt; &quot; &apos; &#N; &#xH;`; unknown entities are kept literally.
pub(super) fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(rest.get(..i).unwrap_or(""));
        let tail = rest.get(i + 1..).unwrap_or("");
        let entity = tail
            .find(';')
            .filter(|&n| n <= 10)
            .and_then(|n| Some((entity_char(tail.get(..n)?)?, n)));
        match entity {
            Some((c, n)) => {
                out.push(c);
                rest = tail.get(n + 1..).unwrap_or("");
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity_char(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_elements_attributes_and_text() {
        let doc = r#"<feed><entry id="1"><link href="a&amp;b" rel='x'/><title> T &lt;1&gt; </title></entry><entryfoo/><entry/></feed>"#;
        let entries = elements(doc, "entry");
        assert_eq!(entries.len(), 2);
        let link = elements(entries[0].inner, "link");
        assert_eq!(attr(link[0].attrs, "href").as_deref(), Some("a&b"));
        assert_eq!(attr(link[0].attrs, "rel").as_deref(), Some("x"));
        assert_eq!(attr(link[0].attrs, "ref"), None);
        assert_eq!(
            first_text(entries[0].inner, "title").as_deref(),
            Some("T <1>")
        );
        assert_eq!(attr(entries[0].attrs, "id").as_deref(), Some("1"));
    }

    #[test]
    fn survives_malformed_input() {
        for doc in [
            "",
            "<",
            "<entry",
            "<entry>",
            "<entry a='b>",
            "<entry/",
            "&",
            "&#99999999999;",
            "<![CDATA[",
            "\0\0<a>",
        ] {
            let _ = elements(doc, "entry");
            let _ = first_text(doc, "entry");
            let _ = decode(doc);
            let _ = attr(doc, "a");
        }
        assert_eq!(decode("a &bogus; &#65; &#x42; &amp"), "a &bogus; A B &amp");
    }

    #[test]
    fn cdata_is_unwrapped() {
        assert_eq!(text(" <![CDATA[ a & b ]]> "), "a & b");
    }
}
