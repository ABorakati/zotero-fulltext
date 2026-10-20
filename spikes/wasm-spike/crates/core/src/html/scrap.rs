//! scraper 0.27 (html5ever 0.39) backend: full browser-grade tree builder.

use super::{Anchor, Extracted, Link, Meta, collapse_ws};
use scraper::{Html, Selector};

pub fn extract(html: &str) -> Extracted {
    let doc = Html::parse_document(html);
    let mut out = Extracted::default();
    let sel = |s: &str| Selector::parse(s).ok();
    if let Some(s) = sel("meta") {
        for el in doc.select(&s) {
            let v = el.value();
            if let (Some(k), Some(c)) = (v.attr("name").or_else(|| v.attr("property")), v.attr("content")) {
                out.metas.push(Meta { name: k.trim().to_ascii_lowercase(), content: c.trim().to_owned() });
            }
        }
    }
    if let Some(s) = sel("link[href]") {
        for el in doc.select(&s) {
            let v = el.value();
            out.links.push(Link {
                rel: v.attr("rel").unwrap_or_default().to_owned(),
                r#type: v.attr("type").unwrap_or_default().to_owned(),
                href: v.attr("href").unwrap_or_default().trim().to_owned(),
            });
        }
    }
    if let Some(s) = sel("a[href]") {
        for el in doc.select(&s) {
            let text: String = el.text().collect();
            out.anchors.push(Anchor {
                href: el.value().attr("href").unwrap_or_default().trim().to_owned(),
                text: collapse_ws(&text),
            });
        }
    }
    if let Some(s) = sel("iframe[src], frame[src], embed[src]") {
        for el in doc.select(&s) {
            out.iframes.push(el.value().attr("src").unwrap_or_default().trim().to_owned());
        }
    }
    if let Some(s) = sel("base[href]") {
        out.base_href = doc.select(&s).next().and_then(|e| e.value().attr("href")).map(|h| h.trim().to_owned());
    }
    out
}
