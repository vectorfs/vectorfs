//! `html` extractor kind (M3-T6): an HTML document file becomes ONE text
//! [`Part`] — the body text with block-level elements ending lines and
//! script/style/head noise stripped.
//!
//! # Leaf kind — one html file IS one document
//!
//! The §0 normalize/container criterion (「子文档重回用户配置的路由，容器
//! 永不自消化」) puts html on the LEAF side without argument: the file's
//! content is one document's worth of text, and the only "inside" (the DOM)
//! is a rendering detail, not an independently routable user document. The
//! interesting §0 note for html is the CONTAINER side of the same ruling:
//! 「用户配的 html 链自动适用于 epub 章节」— when T7's epub container kind
//! `unpack`s its XHTML chapters, those children carry `text/html` semantics
//! and re-enter USER routing, landing on this kind through the ordinary
//! child-route mechanism. Nothing here knows about epub, and nothing there
//! needs to know about this kind — the configuration-coherence property is
//! the mechanism's, not a special case on either side.
//!
//! # Parser selection: html5ever (decided by measurement, not taste)
//!
//! Real-world HTML is tag soup BY SPECIFICATION (HTML5 defines recovery
//! rules for broken markup; XHTML-style well-formedness is the exception).
//! The candidates were measured on 14 adversarial-but-ordinary shapes
//! (2026-11-06 bench, recorded with the selection):
//!
//! * **quick-xml 0.41 lenient** (`check_end_names = false`, the lightweight
//!   streaming option): two disqualifying failures — a `<script>` body
//!   containing `<` (e.g. `if (a < b)`, on a large share of real pages)
//!   made the reader silently swallow the REST OF THE PAGE (content loss
//!   without an error), and a bare `&` (any URL query string, `?a=1&b=2`)
//!   was a hard parse error that also dropped the remainder. A text
//!   extractor that silently loses the page after the first script is
//!   worse than no extractor: embeddings would index a phantom half page.
//! * **html5ever 0.40** (the servo-lineage WHATWG parser): all 14 shapes
//!   correct — script/style RAWTEXT mode, implied end tags (`<li>` without
//!   `</li>`), unquoted attributes, void elements, uppercase tags, entity
//!   decoding (`&nbsp;`, `&#233;`), bare `&` tolerated, CJK intact. The
//!   cost is a heavier dependency (9 new pure-Rust crates, the
//!   markup5ever/tendril/string_cache chain — see Cargo.toml) and the
//!   hand-rolled [`Sink`] below (~150 lines).
//!
//! The upstream `markup5ever_rcdom` sink was NOT taken: it is an
//! unofficial crate pinned to html5ever 0.33 — pulling it would compile a
//! SECOND html5ever copy into the binary. The minimal sink here implements
//! the full [`TreeSink`] protocol (reparent/remove/insert-before included —
//! the tree builder drives those on table-soup and formatting-element
//! recovery) on Rc nodes.
//!
//! # Text formation (block-level line breaking, CJK-friendly)
//!
//! * skipped subtrees: `script`, `style`, `head` (title/meta/link are
//!   metadata), `noscript`, `template`;
//! * every BLOCK-level element ends its line (`<p>`, headings, `li`, `tr`,
//!   `td`, `div`, `<br>`, …) — the walk emits one `'\n'` per block close,
//!   so a table row reads as its cells' lines with a `"\n\n"` row gap
//!   (`html`/`body` are exempt: structure every page carries, not visual
//!   blocks);
//! * inside a text node, runs of ASCII whitespace collapse to one space
//!   (source formatting), and a run whose BOTH neighbors are CJK is
//!   dropped entirely (CJK sets no inter-word spaces; a formatting newline
//!   between 汉字 must not become one) — the "CJK 友好" rule, small and
//!   pinned by test; node EDGES keep their space ("a " + "bold" + " c"
//!   around an inline `<b>` stays "a bold c");
//! * two formatting passes after the walk: line-edge spaces trimmed (the
//!   block break already ended the line), and the whole text trimmed
//!   (leading `<br>`/indentation and trailing block closes are layout);
//! * ONE text Part, the same Part-formation ruling as docx/pdf-text/md:
//!   the chunker's window machinery wants one char sequence whose semantic
//!   cuts land on the `"\n"` line boundaries this kind emits.
//!
//! # Guards
//!
//! html5ever is a streaming parser bounded by input size; the input itself
//! is capped upstream by the ingest worker's `max_file_size_mb` (the image
//! kind's ruling: no entry/page caps for a one-Part leaf). Non-UTF-8 input
//! is re-decoded lossily by the parser's `from_utf8` wrapper (real-world
//! legacy pages are not always UTF-8; a hard refusal would break the most
//! common legacy shape, and lossy replacement characters are visible, not
//! silent). Zero extractable text (a page that is all script/style/head)
//! ⇒ loud Err.
//!
//! # Affinity: deliberately undeclared
//!
//! The [`ContentClass`] vocabulary is PDF-shaped (its probe covers the pdf
//! family only) — an html source probes as Unknown and the router never
//! consults affinity for it (the image-kind precedent, M3-T4; the docx kind
//! its M3-T6 twin).
//!
//! # Contract behavior
//!
//! Empty input → Err; wrong mime → Err (exact gate on `text/html`); empty
//! mime tolerated; garbage input parses (HTML5 recovery) and errs only on
//! the no-text rule; output is exactly ONE `Modality::Text` Part.

use super::super::gate;
use super::{Extractor, Modality, Part, PipelineError};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, ExpandedName, QualName, parse_document};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

/// Runtime instance (configless — the kind has no knobs; which elements are
/// noise is a format fact, not a user decision).
#[derive(Debug, Clone, Copy, Default)]
pub struct HtmlExtractor;

impl Extractor for HtmlExtractor {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        // the shared input gates (byte-identical messages across kinds —
        // see pipeline/gate.rs)
        gate::refuse_empty_input("html", bytes)?;
        gate::refuse_foreign_mime("html", mime, gate::Accept::Exact("text/html"))?;
        let text = html_to_text(bytes)?;
        if text.trim().is_empty() {
            return Err(PipelineError::extract(
                "html: the page contains no extractable text (all content is script/style/head \
                 noise) — nothing to index",
            ));
        }
        Ok(vec![Part {
            modality: Modality::Text,
            text,
            image: None,
            labels: Vec::new(),
            pages: Vec::new(),
            real_lines: false,
            furniture_removed: 0,
            section: None,
            sections: Vec::new(),
            title: None,
        }])
    }
}

/// html5ever driver: parse the bytes (UTF-8, lossy on invalid bytes —
/// module docs) through the minimal sink, walk the tree into text, then two
/// formatting passes: line-edge spaces trimmed (a block break already ends
/// the line — leading indentation after `'\n'` is source formatting), and
/// the WHOLE text trimmed (a page's leading `<br>`/indentation and trailing
/// block closes are layout, not content).
fn html_to_text(bytes: &[u8]) -> Result<String, PipelineError> {
    let sink = Sink::default();
    let document = parse_document(sink, Default::default())
        .from_utf8()
        .one(bytes);
    let mut out = String::new();
    walk(&document, &mut out);
    let lines: Vec<&str> = out.split('\n').map(|line| line.trim_matches(' ')).collect();
    Ok(lines.join("\n").trim().to_string())
}

// ---------------------------------------------------------------------------
// the minimal TreeSink (module docs: why hand-rolled, why Rc nodes)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum NodeData {
    Document,
    Element {
        name: QualName,
    },
    /// Text content. html5ever appends to adjacent text nodes; String (not
    /// StrTendril) keeps the sink decoupled from tendril's buffer pooling.
    Text {
        contents: RefCell<String>,
    },
    /// Comments, doctypes and processing instructions: not text.
    Opaque,
}

struct Node {
    parent: Cell<Option<Weak<Node>>>,
    children: RefCell<Vec<Handle>>,
    data: NodeData,
}

type Handle = Rc<Node>;

fn new_node(data: NodeData) -> Handle {
    Rc::new(Node {
        parent: Cell::new(None),
        children: RefCell::new(Vec::new()),
        data,
    })
}

fn detach(node: &Handle) {
    if let Some(parent) = node.parent.replace(None).and_then(|w| w.upgrade()) {
        parent
            .children
            .borrow_mut()
            .retain(|c| !Rc::ptr_eq(c, node));
    }
}

fn attach(parent: &Handle, child: Handle) {
    detach(&child);
    child.parent.set(Some(Rc::downgrade(parent)));
    parent.children.borrow_mut().push(child);
}

/// The minimal html5ever [`TreeSink`]: builds exactly the tree the walker
/// needs (element names + text), implementing every mutation the WHATWG
/// tree-construction algorithm can request.
#[derive(Default)]
struct Sink {
    document: RefCell<Option<Handle>>,
}

impl TreeSink for Sink {
    type Handle = Handle;
    type Output = Handle;
    type ElemName<'a> = ExpandedName<'a>;

    fn finish(self) -> Self::Output {
        self.document.borrow().clone().expect("get_document ran")
    }
    fn parse_error(&self, _msg: std::borrow::Cow<'static, str>) {
        // recovery is the HTML5 contract (module docs); errors are not
        // output, only input shape
    }
    fn get_document(&self) -> Handle {
        if let Some(doc) = self.document.borrow().clone() {
            return doc;
        }
        let doc = new_node(NodeData::Document);
        *self.document.borrow_mut() = Some(doc.clone());
        doc
    }
    fn elem_name<'a>(&'a self, target: &'a Handle) -> ExpandedName<'a> {
        match target.data {
            NodeData::Element { ref name, .. } => name.expanded(),
            _ => panic!("elem_name on a non-element node"),
        }
    }
    fn create_element(
        &self,
        name: QualName,
        _attrs: Vec<Attribute>,
        _flags: ElementFlags,
    ) -> Handle {
        // attributes are dropped: the text walk never reads them
        new_node(NodeData::Element { name })
    }
    fn create_comment(&self, _text: StrTendril) -> Handle {
        new_node(NodeData::Opaque)
    }
    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> Handle {
        new_node(NodeData::Opaque)
    }
    fn append(&self, parent: &Handle, child: NodeOrText<Handle>) {
        match child {
            NodeOrText::AppendText(t) => {
                // merge into an adjacent text node (the RcDom discipline —
                // the walker relies on text being contiguous)
                if let Some(NodeData::Text { contents }) =
                    parent.children.borrow().last().map(|h| &h.data)
                {
                    contents.borrow_mut().push_str(&t);
                    return;
                }
                attach(
                    parent,
                    new_node(NodeData::Text {
                        contents: RefCell::new(t.to_string()),
                    }),
                );
            }
            NodeOrText::AppendNode(n) => attach(parent, n),
        }
    }
    fn append_based_on_parent_node(
        &self,
        element: &Handle,
        prev_element: &Handle,
        child: NodeOrText<Handle>,
    ) {
        // the RcDom dance (Weak is not Copy, so peek via replace + restore)
        let p = element.parent.replace(None);
        let has_parent = p.is_some();
        element.parent.set(p);
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }
    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {}
    fn get_template_contents(&self, target: &Handle) -> Handle {
        // templates are a skipped subtree anyway (module docs); contents
        // folded into the element keeps the protocol satisfied
        target.clone()
    }
    fn same_node(&self, x: &Handle, y: &Handle) -> bool {
        Rc::ptr_eq(x, y)
    }
    fn set_quirks_mode(&self, _mode: QuirksMode) {}
    fn append_before_sibling(&self, sibling: &Handle, child: NodeOrText<Handle>) {
        // peek the parent (Weak is not Copy: replace + restore)
        let wp = sibling.parent.replace(None);
        sibling.parent.set(wp.clone());
        let Some(parent) = wp.and_then(|w| w.upgrade()) else {
            return;
        };
        let i = parent
            .children
            .borrow()
            .iter()
            .position(|c| Rc::ptr_eq(c, sibling));
        let Some(i) = i else {
            return;
        };
        match child {
            NodeOrText::AppendText(t) => {
                // merge with the PREVIOUS text sibling when adjacent (the
                // RcDom discipline)
                if i > 0 {
                    let prev = parent.children.borrow()[i - 1].clone();
                    if let NodeData::Text { contents } = &prev.data {
                        contents.borrow_mut().push_str(&t);
                        return;
                    }
                }
                let n = new_node(NodeData::Text {
                    contents: RefCell::new(t.to_string()),
                });
                n.parent.set(Some(Rc::downgrade(&parent)));
                parent.children.borrow_mut().insert(i, n);
            }
            NodeOrText::AppendNode(n) => {
                detach(&n);
                n.parent.set(Some(Rc::downgrade(&parent)));
                parent.children.borrow_mut().insert(i, n);
            }
        }
    }
    fn add_attrs_if_missing(&self, _target: &Handle, _attrs: Vec<Attribute>) {}
    fn remove_from_parent(&self, target: &Handle) {
        detach(target);
    }
    fn reparent_children(&self, node: &Handle, new_parent: &Handle) {
        let kids: Vec<_> = std::mem::take(&mut *node.children.borrow_mut());
        let mut children = new_parent.children.borrow_mut();
        for k in kids {
            k.parent.set(Some(Rc::downgrade(new_parent)));
            children.push(k);
        }
    }
}

// ---------------------------------------------------------------------------
// the text walk (module docs: skips, block-level breaks, whitespace rules)
// ---------------------------------------------------------------------------

/// Subtrees that are not content (module docs).
fn is_noise(local: &str) -> bool {
    matches!(
        local,
        "script" | "style" | "head" | "title" | "meta" | "link" | "base" | "noscript" | "template"
    )
}

/// Block-level elements end a text line (module docs: the walk emits one
/// line per block, `'\n'`-joined) — `<br>` included, a void element with no
/// children whose whole meaning is the line break. `html`/`body` are
/// deliberately ABSENT: they are document structure every page carries, not
/// visual blocks — counting them would end every extraction in a stack of
/// trailing blank lines.
fn is_block(local: &str) -> bool {
    matches!(
        local,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "br"
            | "canvas"
            | "caption"
            | "dd"
            | "details"
            | "dialog"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "option"
            | "p"
            | "pre"
            | "section"
            | "select"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "textarea"
            | "th"
            | "thead"
            | "tr"
            | "ul"
            | "video"
            | "audio"
            | "picture"
    )
}

/// CJK test for the whitespace rule (module docs): Hiragana, Katakana,
/// CJK Unified Ideographs + extensions, fullwidth forms — the ranges that
/// never take inter-word spaces.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // Hiragana + Katakana
        | 0x3400..=0x4DBF // CJK ext A
        | 0x4E00..=0x9FFF // CJK Unified
        | 0xF900..=0xFAFF // CJK compat
        | 0x20000..=0x2FA1F // CJK ext B.. + compat sup
        | 0xFF00..=0xFFEF // fullwidth forms
    )
}

/// Collapse one text node's formatting whitespace (module docs): ASCII
/// whitespace runs → one space; a run whose BOTH neighbors are CJK is
/// dropped entirely (CJK sets no inter-word spaces — a source-formatting
/// newline between 汉字 must not become one). Node EDGES keep their space:
/// "a " and " c" around an inline `<b>` are real content spaces, and
/// line-edge cleanup happens once, at block boundaries, in [`html_to_text`].
fn normalize_whitespace(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_whitespace() {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_whitespace() {
                j += 1;
            }
            let between_cjk = i > 0 && is_cjk(chars[i - 1]) && j < chars.len() && is_cjk(chars[j]);
            if !between_cjk {
                out.push(' ');
            }
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn walk(node: &Handle, out: &mut String) {
    match &node.data {
        NodeData::Text { contents } => {
            let normalized = normalize_whitespace(&contents.borrow());
            out.push_str(&normalized);
        }
        NodeData::Opaque => {}
        NodeData::Document => {
            for c in node.children.borrow().iter() {
                walk(c, out);
            }
        }
        NodeData::Element { name } => {
            let local = name.local.as_ref();
            if is_noise(local) {
                return;
            }
            for c in node.children.borrow().iter() {
                walk(c, out);
            }
            if is_block(local) {
                out.push('\n');
            }
        }
    }
}

// ---------------------------------------------------------------------------
// config (no knobs — the tag is the whole section)
// ---------------------------------------------------------------------------

/// Per-kind config struct (`deny_unknown_fields`): the typed `kind` tag
/// doubles as the allowed-field list in unknown-key errors and as the
/// fingerprint payload (canonical form `kind = "html"`, knob-free).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HtmlConfig {
    pub kind: HtmlTag,
}

/// Literal tag: only deserializes from `"html"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HtmlTag {
    #[serde(rename = "html")]
    Html,
}

// ---------------------------------------------------------------------------
// tests — the 14-shape selection bench (every failure mode that ruled out
// the lenient-XML alternative stays pinned here), the CJK rule, the gates
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A leaked `&'static` html page for case tables that demand 'static
    /// bytes (the mod.rs registry sweep).
    pub(crate) fn static_html() -> &'static [u8] {
        Box::leak(
            b"<html><body><h1>reg</h1><p>istry</p></body></html>"
                .to_vec()
                .into_boxed_slice(),
        )
    }

    fn extract_text(html: &[u8]) -> Result<String, PipelineError> {
        Ok(HtmlExtractor.extract(html, "text/html")?.remove(0).text)
    }

    #[test]
    fn body_text_with_block_breaks_and_noise_stripped() {
        let text = extract_text(
            b"<html><head><title>Ignored title</title><meta charset=\"utf-8\">\
              <link href=\"style.css\"></head>\
              <body><h1>Heading</h1><p>First paragraph.</p><p>Second paragraph.</p></body></html>",
        )
        .unwrap();
        assert_eq!(text, "Heading\nFirst paragraph.\nSecond paragraph.");
        // inline elements do NOT break lines
        let text = extract_text(b"<p>a <b>bold</b> c</p>").unwrap();
        assert_eq!(text, "a bold c");
    }

    /// THE selection bench (module docs): the shapes a lenient XML reader
    /// failed on real-world pages, pinned so the parser choice stays
    /// justified by executable evidence.
    #[test]
    fn tag_soup_parses_per_html5_spec() {
        // script body with `<` (the quick-xml silent-page-loss shape): the
        // REST of the page must survive, the script content must not
        let text = extract_text(
            b"<html><body><p>before</p><script>if (a < b) { alert('x') }</script><p>after</p></body></html>",
        )
        .unwrap();
        assert_eq!(text, "before\nafter");
        // script containing a fake close tag in a string
        let text = extract_text(b"<body><script>var s = \"</div>\";</script><p>real</p>").unwrap();
        assert_eq!(text, "real");
        // bare ampersand (URL query strings): tolerated, not an error
        let text = extract_text(b"<p>see <a href=\"?a=1&amp;b=2\">link</a> and AT&T</p>").unwrap();
        assert_eq!(text, "see link and AT&T");
        // implied end tags: <li> without </li>, <p> without </p>
        let text = extract_text(b"<ul><li>one<li>two<li>three</ul><p>para1<p>para2").unwrap();
        assert_eq!(text, "one\ntwo\nthree\n\npara1\npara2");
        // unquoted attributes, void elements without slashes, uppercase
        let text = extract_text(b"<DIV ID=main><BR><IMG SRC=x.png>text<HR></DIV>").unwrap();
        assert_eq!(text, "text");
        // style with '>' in selectors (RAWTEXT mode)
        let text = extract_text(b"<style>div > p { color: red }</style><p>visible</p>").unwrap();
        assert_eq!(text, "visible");
        // entities: named (incl. &nbsp;), numeric decimal and hex
        let text =
            extract_text("<p>Tom &amp; Jerry&nbsp;&lt;tag&gt; caf&#233;</p>".as_bytes()).unwrap();
        assert_eq!(text, "Tom & Jerry\u{a0}<tag> café");
        // comments and doctype are not text
        let text = extract_text(b"<!DOCTYPE html><!-- a comment --><p>text</p>").unwrap();
        assert_eq!(text, "text");
        // textarea content is raw text (escapable raw text element)
        let text = extract_text(b"<textarea>a < b</textarea><p>ok</p>").unwrap();
        assert_eq!(text, "a < b\nok");
        // table soup: implied tbody, cells closed by siblings
        let text = extract_text(b"<table><tr><td>a<td>b<tr><td>c</table><p>done</p>").unwrap();
        assert_eq!(text, "a\nb\n\nc\n\n\n\ndone");
    }

    #[test]
    fn cjk_whitespace_rule() {
        // a formatting newline BETWEEN CJK chars collapses to nothing (CJK
        // sets no spaces); between ASCII words it is one space
        let text = extract_text("<div>汉字\n汉字\n汉字</div><p>word\nword</p>".as_bytes()).unwrap();
        assert_eq!(text, "汉字汉字汉字\nword word");
        // mixed edges keep the boundary space
        let text = extract_text("<p>abc\n汉字</p>".as_bytes()).unwrap();
        assert_eq!(text, "abc 汉字");
    }

    #[test]
    fn empty_wrong_mime_garbage_and_noise_only_are_errors() {
        // empty input
        let err = HtmlExtractor.extract(b"", "text/html").unwrap_err();
        assert!(err.to_string().contains("empty input"), "{err}");
        // wrong mime
        let err = HtmlExtractor
            .extract(b"<p>x</p>", "application/pdf")
            .unwrap_err();
        assert!(err.to_string().contains("cannot handle mime"), "{err}");
        // empty mime tolerated
        assert!(HtmlExtractor.extract(b"<p>x</p>", "").is_ok());
        // garbage bytes: HTML5 recovery parses them into a text node of
        // replacement characters (from_utf8 is lossy BY DESIGN — module
        // docs: legacy non-UTF-8 pages are the common case and the damage
        // is visible, not silent) — Ok with that text, never a panic
        let parts = HtmlExtractor.extract(b"\xff\xfe\x01", "text/html").unwrap();
        assert_eq!(parts.len(), 1);
        // a whitespace-only page has NO extractable text
        let err = HtmlExtractor
            .extract(b"<html><body>  \n\t </body></html>", "text/html")
            .unwrap_err();
        assert!(err.to_string().contains("no extractable text"), "{err}");
        // all-noise page: scripts and styles only, no text
        let err = HtmlExtractor
            .extract(
                b"<html><head><title>t</title></head><body><script>var x = 1;</script>\
                  <style>p{}</style></body></html>",
                "text/html",
            )
            .unwrap_err();
        assert!(err.to_string().contains("no extractable text"), "{err}");
    }

    #[test]
    fn deterministic_and_truncated_never_panics() {
        let page = b"<html><body><p>content here</p><p>more content</p></body></html>";
        let a = HtmlExtractor.extract(page, "text/html").unwrap();
        let b = HtmlExtractor.extract(page, "text/html").unwrap();
        assert_eq!(a, b);
        // truncated halves: never a panic, Ok-with-text is legal (the half
        // still carries text content)
        let half = &page[..page.len() / 2];
        let _ = HtmlExtractor.extract(half, "text/html");
    }

    #[test]
    fn affinity_is_the_undeclared_default() {
        use crate::pipeline::{ContentClass, Fit};
        for class in [
            ContentClass::TextNative,
            ContentClass::ScanDominant,
            ContentClass::MixedTextScan,
            ContentClass::NoText,
        ] {
            assert_eq!(Extractor::affinity(&HtmlExtractor, class), Fit::Capable);
        }
    }

    #[test]
    fn config_roundtrip() {
        let cfg: HtmlConfig = toml::from_str("kind = \"html\"").unwrap();
        assert_eq!(cfg.kind, HtmlTag::Html);
        assert_eq!(toml::to_string(&cfg).unwrap().trim(), "kind = \"html\"");
        // wrong literal rejected; unknown field rejected (no knobs)
        assert!(toml::from_str::<HtmlConfig>("kind = \"txt\"").is_err());
        assert!(toml::from_str::<HtmlConfig>("kind = \"html\"\ntags = \"b\"").is_err());
    }
}
