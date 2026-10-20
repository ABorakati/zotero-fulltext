//! Thin wrapper over the html5gum WHATWG tokenizer that collects everything the classifier and the PDF-link
//! extractor look at. Owned by core-page (spec: `docs/wp/core-page.md`).
//!
//! The tokenizer is not tree-aware. `naively_switch_states(true)` keeps `<script>` / `<style>` / `<title>` /
//! `<textarea>` bodies from being tokenized as tags and ALSO treats `<noscript>`, `<iframe>`, `<noembed>` and `<xmp>`
//! bodies as raw text, exactly like a browser with scripting enabled. That is the behaviour we want: a `<meta refresh>`
//! or link inside `<noscript>` (a false positive on LWW, ScienceDirect and Europe PMC pages) never reaches the
//! collected lists. Attribute values arrive entity-decoded (`&amp;` becomes `&`); attribute and tag names arrive
//! lower-case; for a duplicated attribute the FIRST occurrence wins (WHATWG).
//!
//! Because the tokenizer has no tree, a small stack of open elements is kept on the side (see [`Element::in_refs`]):
//! it is only used to learn whether an `<a>` / `<link>` / frame sat inside a reference-list region, and to capture
//! the text of a few "access" marker elements ([`Mark`]). End tags pop to the nearest open element of the same name
//! (implied end tags are therefore handled the lenient way browsers do), and the stack depth is capped so a
//! document of 2 MiB of unclosed `<div>` costs bounded time and memory.
//!
//! Totality: any `&str` produces a [`Doc`]. Every collected list and every captured text has a cap, so a hostile or
//! simply huge page cannot make the result unbounded: see the `MAX_*` constants.
use html5gum::{DefaultEmitter, StartTag, Token, Tokenizer};

/// Longest `<script>` text kept (the ScienceDirect preloaded state can be large). Also the upper bound for any
/// other captured text.
pub const MAX_SCRIPT_TEXT: usize = 1 << 20;
/// Longest `<title>` text kept.
pub const MAX_TITLE_TEXT: usize = 4096;
/// Longest `<a>` descendant text kept.
pub const MAX_ANCHOR_TEXT: usize = 8192;
/// Longest text kept for a [`Mark`].
pub const MAX_MARK_TEXT: usize = 256;
/// Open-element stack depth beyond which new elements are no longer tracked.
const MAX_DEPTH: usize = 256;
const MAX_ANCHORS: usize = 8192;
const MAX_METAS: usize = 4096;
const MAX_LINKS: usize = 2048;
const MAX_FRAMES: usize = 512;
const MAX_FORMS: usize = 256;
const MAX_INPUTS: usize = 2048;
const MAX_SCRIPTS: usize = 1024;
const MAX_REFRESHES: usize = 64;
const MAX_MARKS: usize = 256;

/// Elements that never have content and therefore never get an end tag.
const VOID: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "meta", "param", "source", "track", "wbr",
];

/// Class tokens and ids (lower-case) that make an element a [`Mark`]: the "does the reader have access" hints.
const MARKED: &[&str] = &[
    "accesscontent",
    "access-options-link-text",
    "check-access-popover",
    "doi-access",
    "access-icon",
    "free-access",
];

/// A generic element: lower-case tag name, attributes in name order, and text for `a` (descendant text,
/// whitespace-collapsed).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Element {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    /// True when the element sat inside a reference-list region (an ancestor with class `references`, `ref-list`,
    /// `reflist`, `bibliography` or an id such as `references`, `ref-list`, `ref12`). Links there point at cited
    /// works, never at the article itself.
    pub in_refs: bool,
}

impl Element {
    /// Attribute value by (lower-case) name.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// True when the `class` attribute contains `class` as a whole token.
    pub fn has_class(&self, class: &str) -> bool {
        self.attr("class")
            .is_some_and(|c| c.split_whitespace().any(|t| t == class))
    }

    /// Like [`Element::has_class`] but ASCII case-insensitive.
    pub fn has_class_ci(&self, class: &str) -> bool {
        self.attr("class")
            .is_some_and(|c| c.split_whitespace().any(|t| t.eq_ignore_ascii_case(class)))
    }

    /// True when the whitespace-separated attribute `name` (for example `rel`) contains `token` (ASCII
    /// case-insensitive).
    pub fn has_token(&self, name: &str, token: &str) -> bool {
        self.attr(name)
            .is_some_and(|v| v.split_whitespace().any(|t| t.eq_ignore_ascii_case(token)))
    }

    /// True when the attribute `name` is present, whatever its value.
    pub fn has_attr(&self, name: &str) -> bool {
        self.attrs.iter().any(|(k, _)| k == name)
    }
}

/// A `<meta>` with a name-like attribute (`name`, `property`, `itemprop`). Names are lower-cased, values trimmed.
/// `content` falls back to the `contents` attribute (Embedded Metadata accepts both). `<meta http-equiv=refresh>`
/// goes to [`Doc::refreshes`] instead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub name: String,
    pub http_equiv: Option<String>,
    pub content: String,
    pub scheme: Option<String>,
}

/// A `<form>` with the `<input>` elements seen before the next `</form>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Form {
    pub id: Option<String>,
    pub name: Option<String>,
    pub action: String,
    pub method: String,
    pub inputs: Vec<Element>,
}

/// A `<script>`: `src` or inline `text` (capped at [`MAX_SCRIPT_TEXT`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Script {
    pub src: Option<String>,
    pub kind: Option<String>,
    pub id: Option<String>,
    pub text: String,
}

/// A `<meta http-equiv="refresh">`, content unparsed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refresh {
    pub content: String,
}

impl Refresh {
    /// `(delay seconds, target url)`. Accepts `N`, `N;url=X`, `N; URL='X'`, `N,X`; the URL is unquoted and trimmed.
    /// A refresh without a URL (Cloudflare `content="360"`) yields `(Some(360.0), None)`.
    pub fn parse(&self) -> (Option<f32>, Option<String>) {
        let c = self.content.trim();
        let (delay, rest) = match c.find([';', ',']) {
            Some(i) => (c.get(..i).unwrap_or(""), c.get(i + 1..).unwrap_or("")),
            None => (c, ""),
        };
        let delay = delay.trim().parse::<f32>().ok();
        let rest = rest.trim();
        let rest = match rest.get(..3) {
            Some(p) if p.eq_ignore_ascii_case("url") => rest
                .get(3..)
                .unwrap_or("")
                .trim_start()
                .trim_start_matches('=')
                .trim(),
            _ => rest,
        };
        let url = rest.trim_matches(['\'', '"']).trim();
        (delay, (!url.is_empty()).then(|| url.to_owned()))
    }
}

/// An element carrying one of the "access" marker classes or ids (`.accessContent`, `#check-access-popover`,
/// `div.doi-access`, `.access-icon.oa`, `.free-access`): its lower-cased classes plus the text it contained.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mark {
    pub tag: String,
    /// Lower-cased `id`.
    pub id: Option<String>,
    /// Lower-cased class tokens.
    pub classes: Vec<String>,
    /// Descendant text, whitespace-collapsed, at most [`MAX_MARK_TEXT`] bytes.
    pub text: String,
}

impl Mark {
    /// True when `token` (lower-case) is the id or one of the classes.
    pub fn has(&self, token: &str) -> bool {
        self.id.as_deref() == Some(token) || self.classes.iter().any(|c| c == token)
    }
}

/// Everything collected from one HTML document, in document order within each list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doc {
    pub title: String,
    pub base_href: Option<String>,
    pub metas: Vec<Meta>,
    /// Every `<link>` that has an `href`.
    pub links: Vec<Element>,
    /// Every `<a>` that has an `href` (text collected).
    pub anchors: Vec<Element>,
    /// `iframe`, `frame`, `embed` (with `src`) and `object` (with `data`).
    pub frames: Vec<Element>,
    pub forms: Vec<Form>,
    /// Every `<input>`, also present in its form's `inputs`.
    pub inputs: Vec<Element>,
    pub scripts: Vec<Script>,
    pub refreshes: Vec<Refresh>,
    /// Elements with an "access" marker class or id (see [`Mark`]).
    pub marks: Vec<Mark>,
}

impl Doc {
    /// First `<meta>` whose lower-case name equals `name`.
    pub fn meta(&self, name: &str) -> Option<&str> {
        self.metas
            .iter()
            .find(|m| m.name == name && !m.content.is_empty())
            .map(|m| m.content.as_str())
    }

    /// First `<meta>` whose name ENDS WITH `suffix` (`citation_pdf_url` also matches `bepress_citation_pdf_url`,
    /// like Zotero's Embedded Metadata translator).
    pub fn meta_suffix(&self, suffix: &str) -> Option<&str> {
        self.metas
            .iter()
            .find(|m| m.name.ends_with(suffix) && !m.content.is_empty())
            .map(|m| m.content.as_str())
    }

    /// First `<meta>` whose name CONTAINS `part` (Cambridge Core pads the attribute with whitespace).
    pub fn meta_contains(&self, part: &str) -> Option<&str> {
        self.metas
            .iter()
            .find(|m| m.name.contains(part) && !m.content.is_empty())
            .map(|m| m.content.as_str())
    }

    /// All non-empty values of metas named exactly `name`.
    pub fn meta_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.metas
            .iter()
            .filter(move |m| m.name == name && !m.content.is_empty())
            .map(|m| m.content.as_str())
    }

    /// All `<meta>` elements whose name equals `name` (also those with an empty content).
    pub fn metas_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Meta> {
        self.metas.iter().filter(move |m| m.name == name)
    }

    /// First `<input>` with this `id` (exact, case-sensitive like the DOM).
    pub fn input_by_id(&self, id: &str) -> Option<&Element> {
        self.inputs.iter().find(|i| i.attr("id") == Some(id))
    }

    /// First `<a href>` with this `id`.
    pub fn anchor_by_id(&self, id: &str) -> Option<&Element> {
        self.anchors.iter().find(|a| a.attr("id") == Some(id))
    }

    /// `<link>` elements whose `rel` has the token `rel` and whose `type` contains `ty` (both case-insensitive).
    pub fn links_with<'a>(
        &'a self,
        rel: &'a str,
        ty: &'a str,
    ) -> impl Iterator<Item = &'a Element> {
        self.links.iter().filter(move |l| {
            l.has_token("rel", rel)
                && l.attr("type")
                    .is_some_and(|t| t.to_ascii_lowercase().contains(&ty.to_ascii_lowercase()))
        })
    }

    /// All marks carrying the id or class `token` (lower-case).
    pub fn marks_with<'a>(&'a self, token: &'a str) -> impl Iterator<Item = &'a Mark> {
        self.marks.iter().filter(move |m| m.has(token))
    }

    /// True when any script has a `src` containing `part`.
    pub fn has_script_src(&self, part: &str) -> bool {
        self.scripts
            .iter()
            .any(|s| s.src.as_deref().is_some_and(|src| src.contains(part)))
    }
}

/// Collapse whitespace runs (including NBSP) to single spaces and trim.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Which element's text is currently being collected.
enum Open {
    Nothing,
    Title,
    Anchor(usize),
    Script(usize),
}

impl Open {
    fn cap(&self) -> usize {
        match self {
            Open::Nothing => 0,
            Open::Title => MAX_TITLE_TEXT,
            Open::Anchor(_) => MAX_ANCHOR_TEXT,
            Open::Script(_) => MAX_SCRIPT_TEXT,
        }
    }
}

/// One open element on the side stack.
struct Frame {
    tag: String,
    region: bool,
}

struct Builder {
    doc: Doc,
    open: Open,
    text: String,
    form_open: bool,
    stack: Vec<Frame>,
    /// Number of frames on `stack` that are reference-list regions.
    region_depth: usize,
    /// `(index into doc.marks, index of its frame on stack)` for marks whose element is still open.
    open_marks: Vec<(usize, usize)>,
}

/// Tokenize `html` and collect a [`Doc`]. Total: any input produces a value.
pub fn parse(html: &str) -> Doc {
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut b = Builder {
        doc: Doc::default(),
        open: Open::Nothing,
        text: String::new(),
        form_open: false,
        stack: Vec::new(),
        region_depth: 0,
        open_marks: Vec::new(),
    };
    for token in Tokenizer::new_with_emitter(html, emitter) {
        let Ok(token) = token; // &str input: the error type is Infallible
        match token {
            Token::StartTag(tag) => b.start(&tag),
            Token::EndTag(tag) => b.end(&String::from_utf8_lossy(&tag.name)),
            Token::String(s) => b.chars(&String::from_utf8_lossy(&s.value)),
            _ => {}
        }
    }
    b.close_open();
    for m in &mut b.doc.marks {
        m.text = collapse_ws(&m.text);
    }
    b.doc
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
fn prefix(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("")
}

/// True for ids / classes that mark a reference-list region.
fn is_ref_region(el: &Element) -> bool {
    fn id_is_ref(id: &str) -> bool {
        if matches!(
            id,
            "references" | "bibliography" | "ref-list" | "reflist" | "refs"
        ) {
            return true;
        }
        id.strip_prefix("ref").is_some_and(|rest| {
            rest.starts_with("erence")
                || rest.starts_with(['s', '-', '_', '.', ':'])
                || rest.starts_with(|c: char| c.is_ascii_digit())
        })
    }
    fn class_is_ref(c: &str) -> bool {
        matches!(
            c,
            "references"
                | "reference-list"
                | "ref-list"
                | "reflist"
                | "bibliography"
                | "references-list"
                | "citedby-list"
        )
    }
    el.attr("id")
        .is_some_and(|id| id_is_ref(&id.to_ascii_lowercase()))
        || el.attr("class").is_some_and(|c| {
            c.split_whitespace()
                .any(|t| class_is_ref(&t.to_ascii_lowercase()))
        })
}

/// The marker `Mark` for an element, if its id or one of its classes is in [`MARKED`].
fn as_mark(el: &Element) -> Option<Mark> {
    let id = el.attr("id").map(str::to_ascii_lowercase);
    let classes: Vec<String> = el
        .attr("class")
        .map(|c| c.split_whitespace().map(str::to_ascii_lowercase).collect())
        .unwrap_or_default();
    let hit = id.as_deref().is_some_and(|i| MARKED.contains(&i))
        || classes.iter().any(|c| MARKED.contains(&c.as_str()));
    hit.then(|| Mark {
        tag: el.tag.clone(),
        id,
        classes,
        text: String::new(),
    })
}

impl Builder {
    fn start(&mut self, tag: &StartTag<()>) {
        let name = String::from_utf8_lossy(&tag.name).into_owned();
        let attrs: Vec<(String, String)> = tag
            .attributes
            .iter()
            .map(|(k, v)| {
                (
                    String::from_utf8_lossy(k).into_owned(),
                    String::from_utf8_lossy(&v.value).into_owned(),
                )
            })
            .collect();
        let el = Element {
            tag: name.clone(),
            attrs,
            text: String::new(),
            in_refs: self.region_depth > 0,
        };
        let region = is_ref_region(&el);
        let mark = as_mark(&el);
        match name.as_str() {
            "title" => {
                self.close_open();
                if self.doc.title.is_empty() {
                    self.open = Open::Title;
                }
            }
            "meta" => self.meta(&el),
            "base" => {
                if self.doc.base_href.is_none() {
                    self.doc.base_href = el.attr("href").map(|h| h.trim().to_owned());
                }
            }
            "link" => {
                if el.attr("href").is_some() && self.doc.links.len() < MAX_LINKS {
                    self.doc.links.push(el);
                }
            }
            "a" => {
                self.close_open();
                if el.attr("href").is_some() && self.doc.anchors.len() < MAX_ANCHORS {
                    self.doc.anchors.push(el);
                    self.open = Open::Anchor(self.doc.anchors.len() - 1);
                }
            }
            "iframe" | "frame" | "embed" if el.attr("src").is_some() => {
                if self.doc.frames.len() < MAX_FRAMES {
                    self.doc.frames.push(el);
                }
            }
            "object" if el.attr("data").is_some() => {
                if self.doc.frames.len() < MAX_FRAMES {
                    self.doc.frames.push(el);
                }
            }
            "form" => {
                self.form_open = true;
                if self.doc.forms.len() < MAX_FORMS {
                    self.doc.forms.push(Form {
                        id: el.attr("id").map(str::to_owned),
                        name: el.attr("name").map(str::to_owned),
                        action: el.attr("action").unwrap_or("").to_owned(),
                        method: el.attr("method").unwrap_or("get").to_ascii_lowercase(),
                        inputs: Vec::new(),
                    });
                }
            }
            "input" => {
                if self.doc.inputs.len() < MAX_INPUTS {
                    if self.form_open
                        && let Some(f) = self.doc.forms.last_mut()
                    {
                        f.inputs.push(el.clone());
                    }
                    self.doc.inputs.push(el);
                }
            }
            "script" => {
                self.close_open();
                if self.doc.scripts.len() < MAX_SCRIPTS {
                    self.doc.scripts.push(Script {
                        src: el.attr("src").map(str::to_owned),
                        kind: el.attr("type").map(str::to_owned),
                        id: el.attr("id").map(str::to_owned),
                        text: String::new(),
                    });
                    self.open = Open::Script(self.doc.scripts.len() - 1);
                }
            }
            _ => {}
        }
        if !tag.self_closing && !VOID.contains(&name.as_str()) {
            self.push_frame(name, region, mark);
        }
    }

    /// Track an open element (bounded) and, when it is a marker element, start capturing its text.
    fn push_frame(&mut self, tag: String, region: bool, mark: Option<Mark>) {
        if self.stack.len() >= MAX_DEPTH {
            return;
        }
        if region {
            self.region_depth += 1;
        }
        self.stack.push(Frame { tag, region });
        if let Some(m) = mark
            && self.doc.marks.len() < MAX_MARKS
        {
            self.doc.marks.push(m);
            self.open_marks
                .push((self.doc.marks.len() - 1, self.stack.len() - 1));
        }
    }

    fn meta(&mut self, el: &Element) {
        let content = el
            .attr("content")
            .or_else(|| el.attr("contents"))
            .unwrap_or("")
            .trim()
            .to_owned();
        let http_equiv = el.attr("http-equiv").map(|h| h.trim().to_ascii_lowercase());
        if http_equiv.as_deref() == Some("refresh") {
            if self.doc.refreshes.len() < MAX_REFRESHES {
                self.doc.refreshes.push(Refresh { content });
            }
            return;
        }
        let name = el
            .attr("name")
            .or_else(|| el.attr("property"))
            .or_else(|| el.attr("itemprop"));
        if let Some(name) = name
            && self.doc.metas.len() < MAX_METAS
        {
            self.doc.metas.push(Meta {
                name: name.trim().to_ascii_lowercase(),
                http_equiv,
                content,
                scheme: el.attr("scheme").map(|s| s.trim().to_ascii_lowercase()),
            });
        }
    }

    fn end(&mut self, name: &str) {
        match name {
            "form" => self.form_open = false,
            "title" | "a" | "script" => self.close_open(),
            _ => {}
        }
        // Pop to the nearest open element of this name; an end tag without a matching start tag is ignored.
        if let Some(pos) = self.stack.iter().rposition(|f| f.tag == name) {
            let removed = self.stack.drain(pos..).filter(|f| f.region).count();
            self.region_depth = self.region_depth.saturating_sub(removed);
            self.open_marks.retain(|&(_, frame)| frame < pos);
        }
    }

    fn chars(&mut self, s: &str) {
        let cap = self.open.cap();
        if cap > 0 && self.text.len() < cap {
            let room = cap - self.text.len();
            self.text.push_str(prefix(s, room));
        }
        if !matches!(self.open, Open::Script(_)) {
            for &(mi, _) in &self.open_marks {
                if let Some(m) = self.doc.marks.get_mut(mi)
                    && m.text.len() < MAX_MARK_TEXT
                {
                    let room = MAX_MARK_TEXT - m.text.len();
                    m.text.push_str(prefix(s, room));
                    m.text.push(' ');
                }
            }
        }
    }

    /// Store the collected text into the element that was open and reset.
    fn close_open(&mut self) {
        let text = std::mem::take(&mut self.text);
        match std::mem::replace(&mut self.open, Open::Nothing) {
            Open::Title => self.doc.title = collapse_ws(&text),
            Open::Anchor(i) => {
                if let Some(a) = self.doc.anchors.get_mut(i) {
                    a.text = collapse_ws(&text);
                }
            }
            Open::Script(i) => {
                if let Some(s) = self.doc.scripts.get_mut(i) {
                    s.text = text;
                }
            }
            Open::Nothing => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_head_and_body_structures() {
        let d = parse(
            r#"<html><head><title> A  b </title><meta name="Citation_PDF_URL" content="/x.pdf">
            <meta http-equiv="refresh" content="3;url=/err.html"><link rel="alternate" type="application/pdf" href="/y.pdf">
            <script>var a = "<a href='no'>";</script></head>
            <body><noscript><meta http-equiv="refresh" content="0; url='/js'"></noscript>
            <a class="btn pdf" href="/z.pdf">Download <b>PDF</b></a>
            <form id="f" method="POST" action="/go"><input type="hidden" name="SAMLResponse" value="x"></form></body></html>"#,
        );
        assert_eq!(d.title, "A b");
        assert_eq!(d.meta("citation_pdf_url"), Some("/x.pdf"));
        assert_eq!(
            d.refreshes.len(),
            1,
            "a refresh inside <noscript> must not be collected"
        );
        assert_eq!(d.refreshes[0].parse().1.as_deref(), Some("/err.html"));
        assert_eq!(d.links[0].attr("href"), Some("/y.pdf"));
        assert_eq!(
            d.anchors.len(),
            1,
            "markup inside <script> must not become tags"
        );
        assert_eq!(d.anchors[0].text, "Download PDF");
        assert!(d.anchors[0].has_class("pdf"));
        assert_eq!(d.forms[0].method, "post");
        assert_eq!(d.forms[0].inputs[0].attr("name"), Some("SAMLResponse"));
    }

    #[test]
    fn refresh_without_url_has_no_target() {
        assert_eq!(
            Refresh {
                content: "360".into()
            }
            .parse(),
            (Some(360.0), None)
        );
        assert_eq!(
            Refresh {
                content: "2; URL='http://x.org/a?b=1'".into()
            }
            .parse(),
            (Some(2.0), Some("http://x.org/a?b=1".into()))
        );
    }

    #[test]
    fn refresh_forms() {
        assert_eq!(
            Refresh {
                content: "0;url=/a".into()
            }
            .parse(),
            (Some(0.0), Some("/a".into()))
        );
        assert_eq!(
            Refresh {
                content: "5, /b".into()
            }
            .parse(),
            (Some(5.0), Some("/b".into()))
        );
        assert_eq!(
            Refresh {
                content: "  3 ; URL = \"/c\" ".into()
            }
            .parse(),
            (Some(3.0), Some("/c".into()))
        );
        assert_eq!(Refresh { content: "".into() }.parse(), (None, None));
        assert_eq!(
            Refresh {
                content: ";url=/d".into()
            }
            .parse(),
            (None, Some("/d".into()))
        );
    }

    #[test]
    fn total_on_garbage() {
        let _ = parse("<<<>>><a href=<meta name=\u{0}\u{ffff}");
        let _ = parse("");
        let _ = parse("<a href=\"unterminated");
        let _ = parse("<meta name=x content=\"never closed");
        let _ = parse("<!-- never closed comment <a href=x>");
        let _ = parse("<script>never closed <a href=x>");
        let _ = parse("\u{0}\u{0}<a\u{0}href=\u{0}x\u{0}>\u{0}");
    }

    #[test]
    fn attribute_syntax_variants() {
        let d = parse(
            r#"<A HREF=/unquoted.pdf CLASS='single quoted' data-x=1>t</A>
               <a href="/first.pdf" href="/second.pdf">dup</a>
               <a href='/a?x=1&amp;y=2&#38;z=3'>ent</a>
               <a name="no-href">ignored</a>
               <META NAME="Citation_Title" CONTENT="Upper">"#,
        );
        assert_eq!(d.anchors.len(), 3, "an <a> without href is ignored");
        assert_eq!(d.anchors[0].attr("href"), Some("/unquoted.pdf"));
        assert!(d.anchors[0].has_class("quoted"));
        assert!(d.anchors[0].has_class_ci("SINGLE"));
        assert_eq!(d.anchors[0].attr("data-x"), Some("1"));
        assert_eq!(
            d.anchors[1].attr("href"),
            Some("/first.pdf"),
            "first duplicate attribute wins"
        );
        assert_eq!(
            d.anchors[2].attr("href"),
            Some("/a?x=1&y=2&z=3"),
            "entities in attribute values are decoded"
        );
        assert_eq!(d.meta("citation_title"), Some("Upper"));
    }

    #[test]
    fn title_entities_and_base_href() {
        let d = parse(
            "<html><head><title>Fish &amp; Chips &lt;1&gt;</title><base href=\"https://cdn.example.org/a/\"><base href=\"/ignored/\"></head></html>",
        );
        assert_eq!(d.title, "Fish & Chips <1>");
        assert_eq!(
            d.base_href.as_deref(),
            Some("https://cdn.example.org/a/"),
            "first <base href> wins"
        );
    }

    #[test]
    fn meta_name_property_itemprop_and_contents_fallback() {
        let d = parse(
            r#"<meta property="og:type" content="Article"><meta itemprop="identifier" content="10.1/x">
               <meta name="citation_pdf_url" contents="/via-contents.pdf"><meta name="x" content="  padded  ">
               <meta name="citation_doi" content="">"#,
        );
        assert_eq!(d.meta("og:type"), Some("Article"));
        assert_eq!(d.meta("identifier"), Some("10.1/x"));
        assert_eq!(d.meta("citation_pdf_url"), Some("/via-contents.pdf"));
        assert_eq!(d.meta("x"), Some("padded"));
        assert_eq!(
            d.meta("citation_doi"),
            None,
            "an empty content is not a value"
        );
        assert_eq!(d.metas_named("citation_doi").count(), 1);
        assert_eq!(d.meta_suffix("pdf_url"), Some("/via-contents.pdf"));
        assert_eq!(d.meta_contains("pdf_url"), Some("/via-contents.pdf"));
    }

    #[test]
    fn nested_markup_in_anchor_text() {
        let d = parse(
            "<a href=/x><span class=i></span>Full <em>Text</em>\n  <b>PDF</b> &amp; more</a>",
        );
        assert_eq!(d.anchors[0].text, "Full Text PDF & more");
    }

    #[test]
    fn noscript_iframe_noembed_xmp_bodies_are_raw_text() {
        let d = parse(
            r#"<noscript><meta http-equiv="Refresh" content="3;url=/_layouts/Error/JavaScript.html"><a href="/ns.pdf">x</a></noscript>
               <iframe src="/frame.pdf"><a href="/inside-iframe.pdf">y</a></iframe>
               <noembed><a href="/noembed.pdf">z</a></noembed><xmp><a href="/xmp.pdf">w</a></xmp>
               <a href="/real.pdf">real</a>"#,
        );
        assert!(
            d.refreshes.is_empty(),
            "LWW-style <noscript> refresh must not be collected"
        );
        let hrefs: Vec<_> = d.anchors.iter().filter_map(|a| a.attr("href")).collect();
        assert_eq!(hrefs, vec!["/real.pdf"]);
        assert_eq!(
            d.frames.len(),
            1,
            "the <iframe> element itself is still recorded"
        );
    }

    #[test]
    fn region_tracking_marks_reference_anchors() {
        let d = parse(
            r#"<a href="/top.pdf">top</a>
               <div id="references"><ol class="x"><li><a href="/ref1.pdf">r1</a></li></ol></div>
               <a href="/after.pdf">after</a>
               <ol class="references"><li><a href="/ref2.pdf">r2</a></li></ol>
               <ul><li id="ref7"><a href="/ref3.pdf">r3</a></li></ul>
               <div id="refresh-banner"><a href="/not-a-ref.pdf">n</a></div>"#,
        );
        let in_refs: Vec<(&str, bool)> = d
            .anchors
            .iter()
            .map(|a| (a.attr("href").unwrap_or(""), a.in_refs))
            .collect();
        assert_eq!(
            in_refs,
            vec![
                ("/top.pdf", false),
                ("/ref1.pdf", true),
                ("/after.pdf", false),
                ("/ref2.pdf", true),
                ("/ref3.pdf", true),
                ("/not-a-ref.pdf", false),
            ]
        );
    }

    #[test]
    fn marks_capture_text_and_classes() {
        let d = parse(
            r#"<div class="doi-access"><span>Free Access</span></div><div class="x">other</div>
               <span class="access-icon oa"></span><div id="check-access-popover">buy</div>"#,
        );
        assert_eq!(
            d.marks_with("doi-access").next().map(|m| m.text.as_str()),
            Some("Free Access")
        );
        assert!(d.marks.iter().any(|m| m.has("access-icon") && m.has("oa")));
        assert!(d.marks_with("check-access-popover").next().is_some());
        assert_eq!(d.marks.len(), 3, "unmarked elements are not recorded");
    }

    #[test]
    fn links_and_inputs_helpers() {
        let d = parse(
            r#"<link rel="alternate stylesheet" type="application/PDF" href="/a.pdf"><link rel="canonical" href="/c">
               <input id="redirectURL" value="x"><input id="other" value="y">
               <a id="pdf-link" href="javascript:;" data-article-url="/p.pdf">PDF</a>"#,
        );
        assert_eq!(d.links_with("alternate", "application/pdf").count(), 1);
        assert_eq!(
            d.input_by_id("redirectURL").and_then(|i| i.attr("value")),
            Some("x")
        );
        assert_eq!(
            d.anchor_by_id("pdf-link")
                .and_then(|a| a.attr("data-article-url")),
            Some("/p.pdf")
        );
        assert!(d.anchors[0].has_attr("data-article-url"));
        assert!(d.links[0].has_token("rel", "ALTERNATE"));
    }

    #[test]
    fn forms_collect_their_inputs() {
        let d = parse(
            r#"<form method=POST action=/a><input name=SAMLRequest value=1><input name=RelayState value=2></form><input name=late>"#,
        );
        assert_eq!(d.forms.len(), 1);
        assert_eq!(d.forms[0].method, "post");
        assert_eq!(d.forms[0].inputs.len(), 2);
        assert_eq!(d.inputs.len(), 3);
    }

    #[test]
    fn two_mebibytes_of_div_is_linear_and_bounded() {
        let html = "<div>".repeat(2 * 1024 * 1024 / 5);
        let d = parse(&html);
        assert!(d.anchors.is_empty());
        // And with anchors: the list is capped.
        let anchors = "<a href=/x.pdf>pdf</a>".repeat(2 * 1024 * 1024 / 22);
        let d = parse(&anchors);
        assert_eq!(d.anchors.len(), MAX_ANCHORS);
    }

    #[test]
    fn pathological_nesting_and_unmatched_end_tags_stay_cheap() {
        // A full 256-deep stack followed by 300k end tags that match nothing: each scans at most MAX_DEPTH frames.
        let deep = format!("{}{}", "<div>".repeat(300), "</span>".repeat(300_000));
        let d = parse(&deep);
        assert!(d.anchors.is_empty());
        // 200k unclosed anchors: each start closes the previous one; the list is capped.
        let d = parse(&"<a href=x>t".repeat(200_000));
        assert_eq!(d.anchors.len(), MAX_ANCHORS);
        // Reference regions that never close do not leak into a later, unrelated parse.
        let d = parse(r#"<div class="references"><a href=/r>r</a>"#);
        assert!(d.anchors[0].in_refs);
        assert!(!parse(r#"<a href=/x>x</a>"#).anchors[0].in_refs);
    }

    #[test]
    fn captured_text_is_capped() {
        let big = format!(
            "<script>{}</script><a href=/x>{}</a><title>{}</title>",
            "s".repeat(MAX_SCRIPT_TEXT + 5000),
            "a".repeat(100_000),
            "t".repeat(100_000)
        );
        let d = parse(&big);
        assert_eq!(d.scripts[0].text.len(), MAX_SCRIPT_TEXT);
        assert!(d.anchors[0].text.len() <= MAX_ANCHOR_TEXT);
        assert!(d.title.len() <= MAX_TITLE_TEXT);
    }

    #[test]
    fn multibyte_text_is_never_split() {
        let s = "é".repeat(MAX_TITLE_TEXT);
        let d = parse(&format!("<title>{s}</title>"));
        assert!(d.title.len() <= MAX_TITLE_TEXT);
        assert!(d.title.chars().all(|c| c == 'é'));
    }

    #[test]
    fn unterminated_tags_at_eof_do_not_leak_partial_elements() {
        let d = parse(r#"<a href="/ok.pdf">ok</a><a href="/cut.pdf"#);
        assert_eq!(d.anchors.len(), 1);
        assert_eq!(d.anchors[0].attr("href"), Some("/ok.pdf"));
    }

    #[test]
    fn json_ld_script_text_and_kind_are_kept() {
        let d = parse(r#"<script type="application/ld+json">{"doi":"10.1/abc"}</script>"#);
        assert_eq!(d.scripts[0].kind.as_deref(), Some("application/ld+json"));
        assert_eq!(d.scripts[0].text, r#"{"doi":"10.1/abc"}"#);
        assert!(!d.has_script_src("nothing"));
    }
}
