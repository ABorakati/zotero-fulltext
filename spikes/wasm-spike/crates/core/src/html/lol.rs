//! lol_html 3.0.1 backend: streaming rewriter with CSS-selector handlers.
//! Note: lol_html does NOT decode entities in attribute values or text chunks.

use super::{Anchor, Extracted, Link, Meta, collapse_ws};
use lol_html::{HtmlRewriter, Settings, element, text};
use std::cell::RefCell;
use std::rc::Rc;

pub fn extract(html: &str) -> Extracted {
    // end-tag handlers must be 'static, so shared state lives in Rc<RefCell<_>>.
    let out = Rc::new(RefCell::new(Extracted::default()));
    let anchor_text = Rc::new(RefCell::new(String::new()));
    {
        let settings = Settings::new()
            .append_element_content_handler(element!("meta", |el| {
                let key = el.get_attribute("name").or_else(|| el.get_attribute("property"));
                if let (Some(k), Some(c)) = (key, el.get_attribute("content")) {
                    out.borrow_mut().metas.push(Meta { name: k.trim().to_ascii_lowercase(), content: dec(c.trim()) });
                }
                Ok(())
            }))
            .append_element_content_handler(element!("link[href]", |el| {
                out.borrow_mut().links.push(Link {
                    rel: el.get_attribute("rel").unwrap_or_default(),
                    r#type: el.get_attribute("type").unwrap_or_default(),
                    href: dec(el.get_attribute("href").unwrap_or_default().trim()),
                });
                Ok(())
            }))
            .append_element_content_handler(element!("iframe[src], frame[src], embed[src]", |el| {
                out.borrow_mut().iframes.push(dec(el.get_attribute("src").unwrap_or_default().trim()));
                Ok(())
            }))
            .append_element_content_handler(element!("base[href]", |el| {
                let mut o = out.borrow_mut();
                if o.base_href.is_none() {
                    o.base_href = el.get_attribute("href").map(|h| dec(h.trim()));
                }
                Ok(())
            }))
            .append_element_content_handler(element!("a[href]", |el| {
                anchor_text.borrow_mut().clear();
                out.borrow_mut()
                    .anchors
                    .push(Anchor { href: dec(el.get_attribute("href").unwrap_or_default().trim()), text: String::new() });
                if let Some(handlers) = el.end_tag_handlers() {
                    let (out, anchor_text) = (Rc::clone(&out), Rc::clone(&anchor_text));
                    let h: lol_html::EndTagHandler<'static> = Box::new(move |_end| {
                        let t = collapse_ws(&dec(&anchor_text.borrow()));
                        if let Some(a) = out.borrow_mut().anchors.last_mut() {
                            a.text = t;
                        }
                        Ok(())
                    });
                    handlers.push(h);
                }
                Ok(())
            }))
            .append_element_content_handler(text!("a[href]", |t| {
                anchor_text.borrow_mut().push_str(t.as_str());
                Ok(())
            }));
        let mut rewriter = HtmlRewriter::new(settings, |_: &[u8]| {});
        if rewriter.write(html.as_bytes()).is_err() || rewriter.end().is_err() {
            // keep whatever was collected
        }
    }
    let mut o = std::mem::take(&mut *out.borrow_mut());
    // anchors never closed (EOF): text stays as collected so far
    if let Some(a) = o.anchors.last_mut()
        && a.text.is_empty()
    {
        a.text = collapse_ws(&dec(&anchor_text.borrow()));
    }
    o
}

fn dec(s: &str) -> String {
    #[cfg(feature = "hand")]
    {
        super::hand::decode_entities(s)
    }
    #[cfg(not(feature = "hand"))]
    {
        s.replace("&amp;", "&")
    }
}
