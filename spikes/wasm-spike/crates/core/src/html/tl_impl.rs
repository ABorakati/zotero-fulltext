//! tl 0.7.8 backend: builds a flat DOM, then walks it in document order.

use super::{Anchor, Collector, Extracted, collapse_ws};

pub fn extract(html: &str) -> Extracted {
    let Ok(dom) = tl::parse(html, tl::ParserOptions::default()) else {
        return Extracted::default();
    };
    let parser = dom.parser();
    let mut c = Collector::default();
    for node in dom.nodes() {
        let Some(tag) = node.as_tag() else { continue };
        let name = tag.name().as_utf8_str().to_ascii_lowercase();
        // tl keeps attribute keys as written; normalise and decode values ourselves.
        let attrs: Vec<(String, String)> = tag
            .attributes()
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.map(|v| decode(&v)).unwrap_or_default()))
            .collect();
        let get = |k: &str| attrs.iter().find(|(an, _)| an == k).map(|(_, v)| v.clone());
        if name == "a" {
            // tl gives us the subtree, so take inner_text directly instead of streaming.
            if let Some(href) = get("href") {
                let text = decode(&tag.inner_text(parser));
                c.out.anchors.push(Anchor { href: href.trim().to_owned(), text: collapse_ws(&text) });
            }
            continue;
        }
        c.start_tag(&name, &get);
    }
    c.finish()
}

fn decode(s: &str) -> String {
    // tl does not decode character references.
    #[cfg(feature = "hand")]
    {
        super::hand::decode_entities(s)
    }
    #[cfg(not(feature = "hand"))]
    {
        s.replace("&amp;", "&")
    }
}
