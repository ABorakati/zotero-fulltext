//! Landing-page extraction. Each backend fills the same `Extracted` struct with
//! raw (entity-decoded, NOT yet URL-resolved) values so they can be compared.

use serde::{Deserialize, Serialize};

#[cfg(feature = "hand")]
pub mod hand;
#[cfg(feature = "html5gum")]
pub mod gum;
#[cfg(feature = "lolhtml")]
pub mod lol;
#[cfg(feature = "scraper")]
pub mod scrap;
#[cfg(feature = "tl")]
pub mod tl_impl;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// `name` or `property` attribute, lower-cased.
    pub name: String,
    pub content: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub rel: String,
    pub r#type: String,
    pub href: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub href: String,
    /// Descendant text, whitespace-collapsed and trimmed.
    pub text: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extracted {
    pub metas: Vec<Meta>,
    /// Only `<link>` elements with an href.
    pub links: Vec<Link>,
    /// Only `<a>` elements with an href.
    pub anchors: Vec<Anchor>,
    pub iframes: Vec<String>,
    pub base_href: Option<String>,
}

/// Collapse runs of whitespace (incl. NBSP) to one space and trim.
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

/// Shared sink used by streaming backends: feeds tags in document order.
#[derive(Default)]
pub struct Collector {
    pub out: Extracted,
    /// Index into `out.anchors` of the currently open `<a href>`, plus its raw text.
    open_anchor: Option<(usize, String)>,
}

impl Collector {
    /// `name` must be lower-case; `attr(name)` returns the decoded attribute value.
    pub fn start_tag(&mut self, name: &str, attr: &dyn Fn(&str) -> Option<String>) {
        match name {
            "meta" => {
                let key = attr("name").or_else(|| attr("property"));
                if let (Some(k), Some(c)) = (key, attr("content")) {
                    self.out.metas.push(Meta { name: k.trim().to_ascii_lowercase(), content: c.trim().to_owned() });
                }
            }
            "link" => {
                if let Some(href) = attr("href") {
                    self.out.links.push(Link {
                        rel: attr("rel").unwrap_or_default(),
                        r#type: attr("type").unwrap_or_default(),
                        href: href.trim().to_owned(),
                    });
                }
            }
            "a" => {
                self.close_anchor(); // <a> cannot nest
                if let Some(href) = attr("href") {
                    self.out.anchors.push(Anchor { href: href.trim().to_owned(), text: String::new() });
                    self.open_anchor = Some((self.out.anchors.len() - 1, String::new()));
                }
            }
            "iframe" | "frame" | "embed" => {
                if let Some(src) = attr("src") {
                    self.out.iframes.push(src.trim().to_owned());
                }
            }
            "base" => {
                if self.out.base_href.is_none() {
                    self.out.base_href = attr("href").map(|h| h.trim().to_owned());
                }
            }
            _ => {}
        }
    }

    pub fn end_tag(&mut self, name: &str) {
        if name == "a" {
            self.close_anchor();
        }
    }

    pub fn wants_text(&self) -> bool {
        self.open_anchor.is_some()
    }

    /// Decoded character data.
    pub fn text(&mut self, t: &str) {
        if let Some((_, buf)) = self.open_anchor.as_mut() {
            buf.push_str(t);
        }
    }

    fn close_anchor(&mut self) {
        if let Some((i, buf)) = self.open_anchor.take()
            && let Some(a) = self.out.anchors.get_mut(i)
        {
            a.text = collapse_ws(&buf);
        }
    }

    pub fn finish(mut self) -> Extracted {
        self.close_anchor();
        self.out
    }
}

/// Backend chosen for the wasm export: first enabled one in this order.
pub fn extract_default(html: &str) -> Extracted {
    #[cfg(feature = "hand")]
    {
        return hand::extract(html);
    }
    #[cfg(all(not(feature = "hand"), feature = "html5gum"))]
    {
        return gum::extract(html);
    }
    #[cfg(all(not(feature = "hand"), not(feature = "html5gum"), feature = "tl"))]
    {
        return tl_impl::extract(html);
    }
    #[cfg(all(not(feature = "hand"), not(feature = "html5gum"), not(feature = "tl"), feature = "lolhtml"))]
    {
        return lol::extract(html);
    }
    #[cfg(all(
        not(feature = "hand"),
        not(feature = "html5gum"),
        not(feature = "tl"),
        not(feature = "lolhtml"),
        feature = "scraper"
    ))]
    {
        return scrap::extract(html);
    }
    #[allow(unreachable_code)]
    {
        let _ = html;
        Extracted::default()
    }
}
