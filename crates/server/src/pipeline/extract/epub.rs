//! `epub` CONTAINER kind (M3-T7): an epub file `unpack`s into spine-ordered
//! XHTML chapter children — the first production container kind (everything
//! before it was a leaf; the role was proven in M2.1 step 3 with the cfg(test)
//! TestContainer).
//!
//! # The format intelligence IS the OPF spine ordering (the §0 criterion)
//!
//! The §0 normalize/container ruling (design.md §0, 2026-11-05) puts
//! 「机械恒对→normalizer（机制不可配）；格式智力/用户可选→kind」 — and for an
//! epub the format intelligence is unambiguous: the READING ORDER lives in
//! the OPF `<spine>`, not in the zip entry order (a book's chapters are
//! authored, renamed and repackaged freely; `ch2, ch3, ch1` on disk with a
//! `ch1, ch2, ch3` spine is a perfectly ordinary epub). Unzipping is the
//! mechanical half — the zip crate already in the tree does that; deciding
//! WHICH entries are the book, and in WHICH order, is a judgement about the
//! format. That judgement lives HERE, in the kind, per the criterion.
//!
//! # Children re-enter USER routing (never self-digested)
//!
//! §0: 「容器永不自消化子文档——子文档重回用户配置的路由；用户配的 html
//! 链自动适用于 epub 章节」. This kind produces [`ChildDoc`]s with
//! `mime = text/html` and lets the ingest worker route each one through the
//! user's configured `html` chain — the T6 html extractor is the receiving
//! end by construction, with zero special-casing on either side (the docx
//! module's contrast note, decided before either kind existed). The kind
//! knows nothing about html extraction; the html kind knows nothing about
//! epub.
//!
//! # What is produced, and what deliberately is not
//!
//! * **Produced**: one ChildDoc per spine itemref whose manifest
//!   media-type is an XHTML form (`application/xhtml+xml` or `text/html`),
//!   in SPINE order; `name` is the zip entry path (provenance/logs only),
//!   `mime` is `text/html` (the routing identity), and — since T18, under
//!   the `chapter-titles` knob — `title` is the chapter's title by the
//!   two-level chain: the nav/NCX label for its entry, else the FILE NAME
//!   STEM (an epub with no readable TOC degrades to filename chapter
//!   titles — 有界退化, not an error; D-9.2).
//! * **NOT produced**: everything else. Images, stylesheets, fonts and the
//!   EPUB3 nav document are RESOURCES of the book, not independently
//!   routable user documents — an operator's html chain expresses no
//!   opinion about a PNG. A spine itemref of a non-XHTML media-type (an
//!   SVG cover, say) is skipped the same way: it rides the spine but is
//!   not a chapter.
//! * `linear="no"` spine items ARE produced: non-linear is a hint for
//!   progressive rendering, not a statement that the content is not part
//!   of the book.
//!
//! # The OCF walk (container.xml → OPF → spine)
//!
//! 1. the zip's FIRST entry must be `mimetype` holding
//!    `application/epub+zip` up to ASCII-whitespace padding (strict OCF says
//!    unpadded, real-world producers ship CRLF/LF and the reader ecosystem
//!    tolerates it — see the check site for the deliberate-deviation note;
//!    the OCF container contract otherwise — the one cheap,
//!    mechanical integrity check the format itself prescribes; anything
//!    else is not an epub and fails loudly here, not as a weird parse
//!    error three steps later);
//! 2. `META-INF/container.xml` names the OPF (`<rootfile full-path=…>`;
//!    the FIRST rootfile is the primary one, per OCF);
//! 3. the OPF's `<manifest>` maps id → (href, media-type); the `<spine>`
//!    lists itemrefs — the reading order. hrefs are resolved relative to
//!    the OPF's own directory (`.`/`..` segments normalized,
//!    percent-decoding applied — both are ordinary href spellings).
//!
//! # Guards (the docx discipline, same constants — two legs since the
//! # 2026-11-11 裁决 2)
//!
//! * [`super::CONTAINER_MAX_ENTRIES`] = 4096 checked before ANY entry is
//!   read (the entry-count leg; widened from 256 by 裁决 2 — a 1560-chapter
//!   novel is legal, and the count is only the bomb's weak proxy); the
//!   worker's MAX_CHILDREN guards the produced-children side at the SAME
//!   number (the two constants move together or children get dropped).
//! * [`super::ExpansionBudget`] over
//!   [`super::CONTAINER_EXPANSION_RATIO`] = 256: the decompressed bytes
//!   the unpack READS may total at most `container bytes × 256`, charged
//!   chunk-by-chunk while reading (the ratio leg — the bomb's ESSENCE:
//!   few entries exploding to gigabytes is the same attack as millions of
//!   tiny ones, and the count alone never sees it).
//! * [`super::ENTRY_DECOMPRESSION_CEILING`] = 256 MiB per entry, enforced
//!   while READING (zip headers lie about sizes; the cap bounds what a
//!   bomb can make this process materialize before the worker's
//!   max_file_size_mb gate sees the child). Unchanged by 裁决 2: one leg
//!   bounds a single child, the other the aggregate.
//! * both guard legs refuse with the shared two-branch vocabulary — "too
//!   many entries OR an expansion ratio past 256:1 — this one: …" — so a
//!   rejected book's reason names which leg tripped and both numbers.
//! * missing/duplicate-broken structure (no mimetype entry, mimetype not
//!   first, no container.xml, no rootfile, no OPF at the declared path,
//!   spine referencing an absent manifest id, empty spine, XML that does
//!   not parse) ⇒ loud Err naming the failing step — the member yields
//!   and the chain may recover; an epub with zero chapters is an Err, not
//!   a silent nothing.
//!
//! # Affinity: deliberately undeclared
//!
//! The ContentClass vocabulary is PDF-shaped (probe covers the pdf family
//! only) — an epub source probes as Unknown and the router never consults
//! affinity for it (the image-kind precedent, M3-T4; the docx/html twins,
//! M3-T6).
//!
//! # Role separation
//!
//! unpack is the kind's ONE product path: [`Extractor::extract`] refuses
//! loudly (a container never extracts — the worker short-circuits extract
//! for kinds whose unpack yields children; reaching extract is a routing
//! bug, the TestContainer-established semantics).
//!
//! # Contract behavior
//!
//! Empty input → Err; wrong mime → Err (exact gate on
//! `application/epub+zip`); empty mime tolerated; truncated zip → Err;
//! unpack output is `Ok(Some(children))` — never `Ok(None)` (that is the
//! leaf verdict).

use super::super::gate;
use super::{ChildDoc, Extractor, PipelineError};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::io::Read;

/// The one OCF path this kind reads after the mimetype check.
const CONTAINER_XML: &str = "META-INF/container.xml";

/// The epub container mime (also the required mimetype-entry content).
pub(crate) const EPUB_MIME: &str = "application/epub+zip";

/// Chapter forms a spine item may carry (case-insensitive; anything else
/// rides the spine but is a resource — module docs).
const XHTML_MEDIA_TYPES: &[&str] = &["application/xhtml+xml", "text/html"];

/// Runtime instance: the reading order stays a format fact (not a user
/// decision), but T18's `chapter-titles` knob rides along as the one
/// config bit — titles are chunk-identity-relevant, hence the knob, hence
/// the runtime carries it (the `PdfTextExtractor::with_structure` shape).
#[derive(Debug, Clone, Copy)]
pub struct EpubExtractor {
    chapter_titles: bool,
}

impl Default for EpubExtractor {
    /// `new()`'s twin — the adjudicated default (titles ON, T18).
    fn default() -> Self {
        EpubExtractor {
            chapter_titles: default_chapter_titles(),
        }
    }
}

impl EpubExtractor {
    /// The default-config runtime (chapter-titles ON, the T18 default).
    pub fn new() -> Self {
        Self::default()
    }

    /// T18 `chapter-titles` knob (config arm of the builder).
    pub fn with_chapter_titles(mut self, chapter_titles: bool) -> Self {
        self.chapter_titles = chapter_titles;
        self
    }
}

impl Extractor for EpubExtractor {
    fn extract(&self, _bytes: &[u8], _mime: &str) -> Result<Vec<super::Part>, PipelineError> {
        // role separation made loud (module docs): the worker re-routes the
        // children; extract() here is a routing bug and must fail visibly
        // instead of guessing
        Err(PipelineError::extract(
            "epub is a container kind: it unpacks (OPF spine order), never extracts — the \
             worker re-routes its children through the user's configured chains; extract() \
             here is a routing bug",
        ))
    }

    fn unpack(&self, bytes: &[u8], mime: &str) -> Result<Option<Vec<ChildDoc>>, PipelineError> {
        // the shared input gates (byte-identical messages across kinds —
        // see pipeline/gate.rs)
        gate::refuse_empty_input("epub", bytes)?;
        gate::refuse_foreign_mime("epub", mime, gate::Accept::Exact(EPUB_MIME))?;
        Ok(Some(spine_children(bytes, self.chapter_titles)?))
    }
}

/// The OCF walk (module docs): mimetype check → container.xml → OPF →
/// spine-ordered chapter children. Every structural failure is a loud Err
/// naming the step; the two bomb-guard legs (count, ratio) refuse with
/// the shared two-branch vocabulary (裁决 2). `chapter_titles` (T18)
/// stamps each child's title by the D-9.2 two-level chain.
fn spine_children(bytes: &[u8], chapter_titles: bool) -> Result<Vec<ChildDoc>, PipelineError> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
        PipelineError::extract(format!(
            "epub: cannot open as a zip archive (corrupt upload, truncated transfer, or not a \
             .epub file): {e}"
        ))
    })?;
    if zip.len() > super::CONTAINER_MAX_ENTRIES {
        return Err(PipelineError::extract(format!(
            "epub: the package lists {} entries (max {}) — refused before reading any entry; \
             the container bomb guard trips on too many entries OR an expansion ratio past \
             {}:1 ({} container bytes × {}), and this package trips the ENTRY COUNT; if this \
             is a legitimate book, it is too large for one source",
            zip.len(),
            super::CONTAINER_MAX_ENTRIES,
            super::CONTAINER_EXPANSION_RATIO,
            bytes.len(),
            super::CONTAINER_EXPANSION_RATIO
        )));
    }
    // 裁决 2's ratio leg: everything this unpack READS charges the budget
    // (non-spine resources are never read, so only what the process
    // actually materializes counts against the container's bytes)
    let mut budget = super::ExpansionBudget::new("epub", bytes.len());
    // (1) the OCF mimetype contract: FIRST entry, exact content
    let mut first = zip.by_index(0).map_err(zip_err("epub", "mimetype"))?;
    if first.name_raw() != b"mimetype" {
        return Err(PipelineError::extract(format!(
            "epub: the first zip entry is '{}' — the OCF container contract requires an \
             uncompressed 'mimetype' entry holding '{EPUB_MIME}' first; this is not a valid \
             .epub file",
            String::from_utf8_lossy(first.name_raw())
        )));
    }
    let mut mt = Vec::new();
    first
        .read_to_end(&mut mt)
        .map_err(|e| PipelineError::extract(format!("epub: reading the mimetype entry: {e}")))?;
    budget.charge("mimetype", mt.len() as u64)?;
    // Strict OCF says the mimetype content is UNPADDED (no leading/trailing
    // whitespace) — but real-world producers (mostly Windows tooling) ship
    // CRLF/LF padding, and the wider reader ecosystem (calibre et al.)
    // tolerates it; the mimetype is advisory metadata (container.xml/OPF
    // drive everything), so refusing a whole book over it is hostile to the
    // files-in-product-out contract. We trim ASCII whitespace before the
    // equality check — a DELIBERATE, documented deviation from strict OCF.
    if mt.trim_ascii() != EPUB_MIME.as_bytes() {
        return Err(PipelineError::extract(format!(
            "epub: the mimetype entry holds '{}', not '{EPUB_MIME}' — the OCF container \
             contract is violated; this is not a valid .epub file",
            String::from_utf8_lossy(&mt)
        )));
    }
    drop(first);
    // (2) container.xml -> the primary OPF path
    let opf_path = {
        let xml = read_entry(&mut zip, CONTAINER_XML, &mut budget)?;
        let xml = utf8(CONTAINER_XML, &xml)?;
        let Some(opf) = parse_container_xml(&xml)? else {
            return Err(PipelineError::extract(format!(
                "epub: {CONTAINER_XML} declares no <rootfile full-path=…> — an epub without a \
                 package document has no reading order to unpack"
            )));
        };
        opf
    };
    // (3) the OPF: manifest map + spine order + (T18) the TOC pointer —
    // EPUB3 `properties="nav"` or EPUB2 `spine@toc`
    let (manifest, spine, toc) = {
        let xml = read_entry(&mut zip, &opf_path, &mut budget)?;
        let xml = utf8(&opf_path, &xml)?;
        parse_opf(&opf_path, &xml)?
    };
    if spine.is_empty() {
        return Err(PipelineError::extract(format!(
            "epub: the OPF '{opf_path}' has an empty spine — no reading order, no chapters"
        )));
    }
    // T18 chapter titles (D-9.2, knob-gated): resolved entry-path -> label
    // from the nav/NCX document, when the package declares one; `None` =
    // no map (and knob off), the per-child filename-stem fallback decides
    let titles = if chapter_titles {
        toc.and_then(|toc| chapter_title_map(&mut zip, &opf_path, &toc, &mut budget))
    } else {
        None
    };
    // children in SPINE order: XHTML manifest items only (module docs)
    let mut children = Vec::with_capacity(spine.len());
    for idref in spine {
        let Some((_id, href, media_type, _props)) =
            manifest.iter().find(|(id, _, _, _)| id == &idref)
        else {
            return Err(PipelineError::extract(format!(
                "epub: the OPF spine references idref '{idref}' but the manifest declares no \
                 such item — a broken package document is refused, not guessed around"
            )));
        };
        if !XHTML_MEDIA_TYPES.contains(&media_type.to_ascii_lowercase().as_str()) {
            continue; // a resource riding the spine (SVG cover, …)
        }
        let entry_path = resolve_href(&opf_path, href);
        let bytes = read_entry(&mut zip, &entry_path, &mut budget)?;
        if !chapter_titles {
            children.push(ChildDoc::new(entry_path, "text/html", bytes));
            continue;
        }
        // the two-level chain: nav/NCX label for this entry, else the
        // child FILE NAME stem (the last path segment minus extension —
        // `OEBPS/ch1.xhtml` degrades to `ch1`; bounded, documented)
        let title = titles
            .as_ref()
            .and_then(|m| m.get(&entry_path))
            .cloned()
            .unwrap_or_else(|| file_stem(&entry_path));
        children.push(ChildDoc::new_titled(entry_path, "text/html", bytes, title));
    }
    Ok(children)
}

/// The child file-name STEM (the D-9.2 fallback rung): the last path
/// segment of the entry path minus its extension — a chapter without a
// TOC label still names itself something stable and human-guessable.
fn file_stem(entry_path: &str) -> String {
    let file = entry_path.rsplit('/').next().unwrap_or(entry_path);
    match file.rsplit_once('.') {
        // a dotless name (rare, legal in a zip) keeps itself as the stem
        Some((stem, _)) => stem.to_string(),
        None => file.to_string(),
    }
}

/// The TOC document the OPF points at (T18, D-9.2): the EPUB3 nav (a
/// manifest item whose `properties` list carries the `nav` token) or the
/// EPUB2 NCX (the manifest item `spine@toc` names). `href` is OPF-relative
/// (resolved by the caller); `ncx` picks the label grammar.
#[derive(Debug)]
struct TocDoc {
    href: String,
    ncx: bool,
}

/// Resolve the package's TOC document into an entry-path -> label map
/// (`None` when the named part is missing — the fallback rung's call, an
/// epub whose declared TOC is absent degrades to filename stems, 有界退
/// 化; unparseable XML is the same refusal-not-guess call). The map is
/// keyed by the RESOLVED entry path so the spine loop's `resolve_href`
/// spellings join regardless of `.`/`..`/percent-encoding variance
/// between the OPF and the TOC's own hrefs.
fn chapter_title_map(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    opf_path: &str,
    toc: &TocDoc,
    budget: &mut super::ExpansionBudget,
) -> Option<std::collections::HashMap<String, String>> {
    let toc_path = resolve_href(opf_path, &toc.href);
    let xml = read_entry(zip, &toc_path, budget).ok()?;
    let xml = utf8(&toc_path, &xml).ok()?;
    let anchors = if toc.ncx {
        parse_ncx(&xml)
    } else {
        parse_nav(&xml)
    };
    let map = anchors
        .into_iter()
        .filter_map(|(href, label)| {
            let label = label.trim();
            if label.is_empty() {
                None
            } else {
                Some((resolve_href(&toc_path, &href), label.to_string()))
            }
        })
        .collect();
    Some(map)
}

/// EPUB3 nav document -> `[(href, label)]`: the text of every `<a href>`
/// inside a `<nav epub:type="toc">`. Other navs (landmarks, page-list)
/// carry different hrefs for different purposes — only the TOC nav's
/// anchors are chapter titles, so a nav document without one contributes
/// nothing (the fallback rung's call again).
fn parse_nav(xml: &str) -> Vec<(String, String)> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut in_toc_nav = 0usize; // nesting depth inside the toc <nav>
    let mut in_anchor = false;
    let mut href = String::new();
    let mut label = String::new();
    let mut buf = Vec::new();
    loop {
        let Ok(event) = reader.read_event_into(&mut buf) else {
            return out; // malformed tail: keep the anchors that parsed
        };
        match event {
            Event::Start(e) => {
                let name = e.name().into_inner();
                if is(name, b"nav") {
                    // local-name `type` matches `epub:type` (attr()'s
                    // prefix-stripping rule) — the EPUB3 spelling
                    if attr(&e, b"type").as_deref() == Some("toc") {
                        in_toc_nav += 1;
                    }
                } else if in_toc_nav > 0 && is(name, b"a") {
                    in_anchor = true;
                    href = attr(&e, b"href").unwrap_or_default();
                    label.clear();
                }
            }
            Event::Text(t) => {
                if in_anchor && let Ok(txt) = t.xml_content(quick_xml::XmlVersion::Implicit1_0) {
                    label.push_str(&txt);
                }
            }
            Event::End(e) => {
                let name = e.name().into_inner();
                if is(name, b"a") && in_anchor {
                    in_anchor = false;
                    out.push((std::mem::take(&mut href), std::mem::take(&mut label)));
                } else if is(name, b"nav") && in_toc_nav > 0 {
                    in_toc_nav -= 1;
                }
            }
            Event::Eof => return out,
            _ => {}
        }
        buf.clear();
    }
}

/// EPUB2 NCX -> `[(href, label)]`: each `<navPoint>`'s `<navLabel><text>`
/// applies to its own `<content src>` (label-before-content is the NCX's
/// own element order, so the pending-label state machine is exact for
/// nested navPoints too).
fn parse_ncx(xml: &str) -> Vec<(String, String)> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut in_nav_label = false;
    let mut label = String::new();
    let mut pending: Option<String> = None;
    let mut buf = Vec::new();
    loop {
        let Ok(event) = reader.read_event_into(&mut buf) else {
            return out;
        };
        match event {
            Event::Start(e) => {
                let name = e.name().into_inner();
                if is(name, b"navLabel") {
                    in_nav_label = true;
                    label.clear();
                }
            }
            // `<content src=…/>` is self-closing in every real NCX — the
            // Empty arm is the one that fires, Start kept away since a
            // paired `<content>` spelling carries no text either way
            Event::Empty(e) => {
                if is(e.name().into_inner(), b"content")
                    && let Some(src) = attr(&e, b"src")
                    && let Some(l) = pending.take()
                {
                    out.push((src, l));
                }
            }
            Event::Text(t) => {
                if in_nav_label && let Ok(txt) = t.xml_content(quick_xml::XmlVersion::Implicit1_0) {
                    label.push_str(&txt);
                }
            }
            Event::End(e) => {
                let name = e.name().into_inner();
                if is(name, b"navLabel") {
                    in_nav_label = false;
                    let l = label.trim();
                    if !l.is_empty() {
                        pending = Some(l.to_string());
                    }
                }
            }
            Event::Eof => return out,
            _ => {}
        }
        buf.clear();
    }
}

// ---------------------------------------------------------------------------
// shared zip plumbing (the docx read-loop generalized for container kinds)
// ---------------------------------------------------------------------------

/// Map a zip error to a loud extract Err naming the kind and entry context.
fn zip_err(kind: &'static str, what: &str) -> impl Fn(zip::result::ZipError) -> PipelineError {
    move |e| {
        PipelineError::extract(format!(
            "{kind}: reading '{what}' from the package failed (corrupt archive): {e}"
        ))
    }
}

/// Read one whole entry BY NAME with the decompression ceiling and the
/// cumulative expansion budget enforced while reading (headers lie; the
/// caps bound what a bomb can materialize, per entry and in aggregate).
fn read_entry(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
    budget: &mut super::ExpansionBudget,
) -> Result<Vec<u8>, PipelineError> {
    let mut entry = zip
        .by_name(name)
        .map_err(|_| PipelineError::extract(format!("epub: the package has no '{name}' part")))?;
    super::read_entry_capped(
        &mut entry,
        super::ENTRY_DECOMPRESSION_CEILING,
        budget,
        "epub",
        name,
    )
}

/// Entry bytes that must be XML: OOXML/OPF are UTF-8 by specification.
fn utf8(name: &str, bytes: &[u8]) -> Result<String, PipelineError> {
    String::from_utf8(bytes.to_vec()).map_err(|e| {
        PipelineError::extract(format!(
            "epub: '{name}' is not valid UTF-8 (the XML specs require UTF-8): {e}"
        ))
    })
}

// ---------------------------------------------------------------------------
// XML pulls (quick-xml, the docx reader configuration: end-name checking ON)
// ---------------------------------------------------------------------------

/// Local-name matcher (the docx `is` helper): OPF/OCF elements carry a
/// default namespace or a legacy `opf:`/`ocf:` prefix; the LOCAL name is
/// the identifier.
fn is(qname: &[u8], local: &[u8]) -> bool {
    qname == local || qname.strip_prefix(b"opf:") == Some(local)
}

/// One attribute value by local name (namespaced spellings tolerated),
/// XML-1.0-normalized (entity-resolved; tab/newline to space — the spec's
/// attribute normalization); `None` when absent.
fn attr(e: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    for a in e.attributes() {
        let Ok(a) = a else { continue };
        let key = a.key.as_ref();
        // local name: the part after a namespace colon (or the whole key)
        let local = key.split(|&b| b == b':').next_back().unwrap_or(key);
        if local == name
            && let Ok(v) = a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
        {
            return Some(v.into_owned());
        }
    }
    None
}

/// container.xml → the FIRST `<rootfile full-path=…>` (the primary one per
/// OCF). `Ok(None)` when no rootfile element exists (the caller's loud Err).
fn parse_container_xml(xml: &str) -> Result<Option<String>, PipelineError> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| xml_err("META-INF/container.xml", e))?;
        match event {
            Event::Start(e) | Event::Empty(e) => {
                if is(e.name().into_inner(), b"rootfile")
                    && let Some(path) = attr(&e, b"full-path")
                {
                    return Ok(Some(path));
                }
            }
            Event::Eof => return Ok(None),
            _ => {}
        }
        buf.clear();
    }
}

/// One `<item id=… href=… media-type=… [properties=…]>` row of the OPF
/// manifest (the T18 properties column rides along for the EPUB3 nav
/// lookup).
type OpfItem = (String, String, String, Option<String>); // (id, href, media-type, properties)

/// [`parse_opf`]'s product: (manifest items, spine idrefs, TOC document).
type Opf = (Vec<OpfItem>, Vec<String>, Option<TocDoc>);

/// OPF → `(manifest items, spine idrefs, TOC document)`. The manifest is
/// collected in document order; the spine is collected in document order —
/// WHICH IS THE READING ORDER and therefore the children order (module
/// docs). The TOC pointer (T18, D-9.2) is the EPUB3 nav — the manifest
/// item whose `properties` token list carries `nav` — or, failing that,
/// the EPUB2 `spine@toc` idref resolved through the manifest (an NCX);
/// `None` when the package declares neither (the filename-stem rung).
fn parse_opf(opf_path: &str, xml: &str) -> Result<Opf, PipelineError> {
    let mut reader = Reader::from_str(xml);
    let mut manifest: Vec<OpfItem> = Vec::new(); // (id, href, media-type, properties)
    let mut spine: Vec<String> = Vec::new();
    let mut spine_toc_id: Option<String> = None;
    let mut in_manifest = false;
    let mut in_spine = false;
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| xml_err(opf_path, e))?;
        match event {
            Event::Start(e) => {
                let name = e.name().into_inner();
                if is(name, b"manifest") {
                    in_manifest = true;
                } else if is(name, b"spine") {
                    in_spine = true;
                    // EPUB2's TOC pointer: the manifest id of the NCX
                    spine_toc_id = attr(&e, b"toc");
                } else if in_manifest && is(name, b"item") {
                    manifest_item(&e, &mut manifest);
                } else if in_spine
                    && is(name, b"itemref")
                    && let Some(idref) = attr(&e, b"idref")
                {
                    spine.push(idref);
                }
            }
            Event::Empty(e) => {
                // self-closing spellings of the same shapes
                let name = e.name().into_inner();
                if in_manifest && is(name, b"item") {
                    manifest_item(&e, &mut manifest);
                } else if in_spine
                    && is(name, b"itemref")
                    && let Some(idref) = attr(&e, b"idref")
                {
                    spine.push(idref);
                }
            }
            Event::End(e) => {
                let name = e.name().into_inner();
                if is(name, b"manifest") {
                    in_manifest = false;
                } else if is(name, b"spine") {
                    in_spine = false;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    // EPUB3 first (a 3.0 package may carry both a nav and a legacy NCX —
    // the spec's own reading order for TOC precedence), EPUB2 second
    let toc = manifest
        .iter()
        .find(|(_, _, _, props)| {
            props
                .as_deref()
                .is_some_and(|p| p.split_whitespace().any(|t| t == "nav"))
        })
        .map(|(_, href, _, _)| TocDoc {
            href: href.clone(),
            ncx: false,
        })
        .or_else(|| {
            spine_toc_id.as_deref().and_then(|id| {
                manifest
                    .iter()
                    .find(|(mid, _, _, _)| mid == id)
                    .map(|(_, href, _, _)| TocDoc {
                        href: href.clone(),
                        ncx: true,
                    })
            })
        });
    Ok((manifest, spine, toc))
}

/// One `<item id=… href=… media-type=…>` into the manifest (items without
/// the first three attributes cannot serve a spine reference and are
/// skipped — they may legitimately be metadata-only; `properties` is
/// optional and absent for every pre-EPUB3 package).
fn manifest_item(
    e: &quick_xml::events::BytesStart<'_>,
    manifest: &mut Vec<(String, String, String, Option<String>)>,
) {
    if let (Some(id), Some(href), Some(mt)) =
        (attr(e, b"id"), attr(e, b"href"), attr(e, b"media-type"))
    {
        manifest.push((id, href, mt, attr(e, b"properties")));
    }
}

fn xml_err(name: &str, e: quick_xml::Error) -> PipelineError {
    PipelineError::extract(format!(
        "epub: '{name}' does not parse as XML (corrupt package document): {e}"
    ))
}

/// Resolve an OPF-relative href against the OPF's own directory:
/// percent-decode, then walk `.` (skip) / `..` (pop) segments. `..` past
/// the root saturates at the root — a hostile path stays inside the zip's
/// flat namespace either way (zip lookup is by exact name; there is no
/// file system to escape).
fn resolve_href(opf_path: &str, href: &str) -> String {
    let href = percent_encoding::percent_decode_str(href)
        .decode_utf8_lossy()
        .to_string();
    let mut segments: Vec<&str> = Vec::new();
    if let Some((dir, _)) = opf_path.rsplit_once('/') {
        segments.extend(dir.split('/'));
    }
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    segments.join("/")
}

// ---------------------------------------------------------------------------
// config (one knob since T18 — chapter-titles, the migration carrier)
// ---------------------------------------------------------------------------

/// The T18 `chapter-titles` knob's default: ON. A NAMED function (not
/// `#[serde(default)]`, which for `bool` can only mean `false`): the
/// builtin-path construction and the deserialization path share this one
/// definition (the `default_structure` anti-drift note — a default that
/// lives in two places eventually disagrees).
fn default_chapter_titles() -> bool {
    true
}

/// Per-kind config struct (`deny_unknown_fields`): the typed `kind` tag
/// doubles as the allowed-field list in unknown-key errors and as the
/// fingerprint payload (canonical form `kind = "epub"` + the knob below).
///
/// The T18 `chapter-titles` knob (default ON) gates the chapter-title
/// chain (design D-9.2): nav/NCX label > child file-name stem. Titles
/// change the stamped `x-vf-section-title` metadata and the CCH embedding
/// input, so they are chunk-identity-relevant — the knob serializes into
/// the fingerprint ALWAYS (value-as-canonical, the T12 `structure`
/// precedent and the pdf-inspector `markdown` one: no elide at the
/// default, an elided default would punch a hole in "the fingerprint
/// owns chunk-set identity"). Upgrading to T18 flips every epub-route
/// bucket's fingerprint — the gate refuses startup and demands the
/// explicit reindex wave (D-9.1; the reading order itself stays a format
/// fact, not a knob).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpubConfig {
    pub kind: EpubTag,
    /// T18 chapter-title propagation (R8 structure axis): stamp each spine
    /// child's chapter title (the two-level chain above). `false` = the
    /// pre-T18 byte-exact shape (no titles, no `x-vf-section-title`).
    /// Config form is kebab-case like every knob (`chapter-titles`, the
    /// `context-header` spelling discipline).
    #[serde(rename = "chapter-titles", default = "default_chapter_titles")]
    pub chapter_titles: bool,
}

/// Literal tag: only deserializes from `"epub"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EpubTag {
    #[serde(rename = "epub")]
    Epub,
}

impl Default for EpubConfig {
    /// The one construction path for the adjudicated default (the
    /// `PdfTextConfig::default` discipline): serde's
    /// `default = "default_chapter_titles"` and the ingest builtin's
    /// literal read the SAME definition.
    fn default() -> Self {
        EpubConfig {
            kind: EpubTag::Epub,
            chapter_titles: default_chapter_titles(),
        }
    }
}

// ---------------------------------------------------------------------------
// tests — in-test epub builders (the docx discipline: no committed binary
// blobs in UNIT tests; the zip structure is built with the same zip crate
// the kind reads). The COMMITTED e2e fixture lives under
// tests/fixtures/containers/ (a real .epub written once, read by the
// worker-level tests).
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// THE e2e-fixture content in builder form (the committed book.epub is
    /// this exact shape — the unit tests prove the unpack, the worker test
    /// proves the chain).
    pub(crate) const CHAPTERS: &[(&str, &str)] = &[
        ("ch1", "第一章　起航<br/>正文第一段：初始的内容。"),
        ("ch2", "第二章　转折<br/>正文第二段：发展的事件。"),
        ("ch3", "第三章　归宿<br/>正文第三段：结束的旅程。"),
    ];

    fn chapter_xhtml(title: &str, body: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <html xmlns=\"http://www.w3.org/1999/xhtml\">\
             <head><title>{title}</title><link rel=\"stylesheet\" href=\"style.css\"/></head>\
             <body><h1>{title}</h1><p>{body}</p></body></html>"
        )
    }

    /// Build a real minimal .epub: mimetype FIRST and stored (the OCF
    /// contract), then META-INF/container.xml, the OPF (its `spine` lists
    /// `spine_order` — scrambled orders are the point), the chapters in
    /// NATURAL order (so zip order ≠ spine order whenever the spine is
    /// scrambled), plus optional extra entries (non-spine resources).
    pub(crate) fn build_epub(spine_order: &[&str], extra: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let manifest = CHAPTERS
            .iter()
            .map(|(id, _)| {
                format!(
                    "<item id=\"{id}\" href=\"{id}.xhtml\" media-type=\"application/xhtml+xml\"/>"
                )
            })
            .collect::<String>()
            + "<item id=\"css\" href=\"style.css\" media-type=\"text/css\"/>\
                <item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\"/>\
                <item id=\"cover\" href=\"cover.png\" media-type=\"image/png\"/>";
        let spine = spine_order
            .iter()
            .map(|id| format!("<itemref idref=\"{id}\"/>"))
            .collect::<String>();
        let opf = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" \
             unique-identifier=\"id\"><metadata/><manifest>{manifest}</manifest>\
             <spine>{spine}</spine></package>"
        );
        let container_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <container version=\"1.0\" \
             xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\
             <rootfiles><rootfile full-path=\"OEBPS/content.opf\" \
             media-type=\"application/oebps-package+xml\"/></rootfiles></container>";
        let nav = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <html xmlns=\"http://www.w3.org/1999/xhtml\" \
             xmlns:epub=\"http://www.idpf.org/2007/ops\">\
             <body><nav epub:type=\"toc\"><ol><li><a href=\"ch1.xhtml\">目录</a></li></ol></nav>\
             </body></html>";

        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        // mimetype: FIRST and STORED (uncompressed) — the OCF contract the
        // unpack checks
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        for (name, bytes) in [
            ("META-INF/container.xml", container_xml.as_bytes()),
            ("OEBPS/content.opf", opf.as_bytes()),
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(bytes).unwrap();
        }
        // chapters in NATURAL order — the spine decides the reading order
        for (id, (title, body)) in CHAPTERS
            .iter()
            .enumerate()
            .map(|(i, (id, body))| (*id, (format!("第{}章", ["一", "二", "三"][i]), body)))
        {
            zip.start_file(format!("OEBPS/{id}.xhtml"), opts).unwrap();
            zip.write_all(chapter_xhtml(&title, body).as_bytes())
                .unwrap();
        }
        zip.start_file("OEBPS/style.css", opts).unwrap();
        zip.write_all(b"p { margin: 0 } /* STYLE-RESOURCE-NOT-A-CHAPTER */")
            .unwrap();
        zip.start_file("OEBPS/nav.xhtml", opts).unwrap();
        zip.write_all(nav.as_bytes()).unwrap();
        zip.start_file("OEBPS/cover.png", opts).unwrap();
        zip.write_all(&[0x89, 0x50, 0x4e, 0x47]).unwrap(); // magic-only stub
        for (name, bytes) in extra {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// Unpack helper (the mime gate is part of the contract — always the
    /// canonical mime).
    fn unpack(epub: &[u8]) -> Result<Option<Vec<ChildDoc>>, PipelineError> {
        EpubExtractor::new().unpack(epub, EPUB_MIME)
    }

    /// THE format-intelligence test: the spine — not the zip entry order —
    /// is the children order. The builder writes chapters ch1,ch2,ch3 on
    /// disk; a scrambled spine must scramble the children identically.
    /// Non-spine resources (css/nav/cover — nav.xhtml is even an XHTML
    /// file, just not on the spine) produce NOTHING.
    #[test]
    fn children_follow_spine_order_not_zip_order() {
        let epub = build_epub(&["ch2", "ch3", "ch1"], &[]);
        let children = unpack(&epub).unwrap().expect("container verdict");
        assert_eq!(
            children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["OEBPS/ch2.xhtml", "OEBPS/ch3.xhtml", "OEBPS/ch1.xhtml"],
            "reading order = spine order, not zip entry order"
        );
        for c in &children {
            assert_eq!(c.mime, "text/html", "chapters carry the html routing mime");
            assert!(!c.bytes.is_empty(), "{} carries its entry bytes", c.name);
        }
        // the scrambled spine's FIRST chapter is ch2's content
        assert!(String::from_utf8_lossy(&children[0].bytes).contains("第二章"));
        // nav.xhtml is XHTML but NOT on the spine: no child for it (nor for
        // the css/png resources)
        assert!(
            !children.iter().any(|c| c.name.contains("nav")
                || c.name.contains("css")
                || c.name.contains("png")),
            "non-spine resources are not routable documents: {:?}",
            children.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        // determinism
        assert_eq!(unpack(&epub).unwrap(), Some(children.clone()));
    }

    #[test]
    fn spine_item_of_non_xhtml_media_type_is_skipped() {
        // a manifest item with an SVG media-type rides the spine (a cover
        // page) but is a resource, not a chapter: spine [cover, ch1] yields
        // ONE child
        use std::io::Write;
        let opf = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\">\
            <manifest><item id=\"cover\" href=\"cover.svg\" media-type=\"image/svg+xml\"/>\
            <item id=\"ch1\" href=\"ch1.xhtml\" media-type=\"application/xhtml+xml\"/></manifest>\
            <spine><itemref idref=\"cover\"/><itemref idref=\"ch1\"/></spine></package>";
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"content.opf\"/></rootfiles></container>",
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(opf.as_bytes()).unwrap();
        zip.start_file("ch1.xhtml", opts).unwrap();
        zip.write_all(b"<html><body><p>x</p></body></html>")
            .unwrap();
        zip.start_file("cover.svg", opts).unwrap();
        zip.write_all(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>")
            .unwrap();
        let epub = zip.finish().unwrap().into_inner();
        let children = unpack(&epub).unwrap().expect("container verdict");
        assert_eq!(
            children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["ch1.xhtml"],
            "the SVG spine item is a resource, not a chapter"
        );
    }

    #[test]
    fn corrupt_structures_fail_loudly_at_the_failing_step() {
        use std::io::Write;
        // (a) not a zip at all
        let err = unpack(b"plainly not a zip").unwrap_err();
        assert!(
            err.to_string().contains("cannot open as a zip archive"),
            "{err}"
        );
        // (b) valid zip, but the FIRST entry is not mimetype
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file("ch1.xhtml", opts).unwrap();
        zip.write_all(b"<html/>").unwrap();
        let not_epub = zip.finish().unwrap().into_inner();
        let err = unpack(&not_epub).unwrap_err();
        assert!(
            err.to_string().contains("first zip entry is 'ch1.xhtml'"),
            "{err}"
        );
        // (c) mimetype entry with WRONG content
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(b"application/zip").unwrap();
        let wrong = zip.finish().unwrap().into_inner();
        let err = unpack(&wrong).unwrap_err();
        assert!(err.to_string().contains("holds 'application/zip'"), "{err}");
        // (c′) mimetype with CRLF padding — real-world producers ship this
        // (the deliberate OCF tolerance, see the check site): PASSES, and
        // yields the same verdict as the clean form
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(b"application/epub+zip\r\n").unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"content.opf\"/></rootfiles></container>",
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(
            b"<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\">\
             <metadata/><manifest><item id=\"ch1\" href=\"ch1.xhtml\" \
             media-type=\"application/xhtml+xml\"/></manifest>\
             <spine><itemref idref=\"ch1\"/></spine></package>",
        )
        .unwrap();
        zip.start_file("ch1.xhtml", opts).unwrap();
        zip.write_all(b"<html><body><p>tolerated</p></body></html>")
            .unwrap();
        let padded = zip.finish().unwrap().into_inner();
        let children = unpack(&padded)
            .unwrap()
            .expect("CRLF-padded mimetype is tolerated");
        assert_eq!(
            children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["ch1.xhtml"],
            "the padded-mimetype container still walks to its chapters"
        );
        // (d) mimetype ok, no META-INF/container.xml
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("OEBPS/content.opf", opts).unwrap();
        zip.write_all(b"<package/>").unwrap();
        let no_container = zip.finish().unwrap().into_inner();
        let err = unpack(&no_container).unwrap_err();
        assert!(
            err.to_string()
                .contains("has no 'META-INF/container.xml' part"),
            "{err}"
        );
        // (e) container.xml with NO rootfile
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(b"<container/>").unwrap();
        let no_rootfile = zip.finish().unwrap().into_inner();
        let err = unpack(&no_rootfile).unwrap_err();
        assert!(err.to_string().contains("declares no <rootfile"), "{err}");
        // (f) rootfile pointing at a MISSING opf
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"no-such.opf\"/></rootfiles></container>",
        )
        .unwrap();
        let missing_opf = zip.finish().unwrap().into_inner();
        let err = unpack(&missing_opf).unwrap_err();
        assert!(
            err.to_string().contains("has no 'no-such.opf' part"),
            "{err}"
        );
        // (g) spine idref absent from the manifest
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"content.opf\"/></rootfiles></container>",
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(
            b"<package><manifest><item id=\"a\" href=\"a.xhtml\" \
               media-type=\"application/xhtml+xml\"/></manifest>\
               <spine><itemref idref=\"ghost\"/></spine></package>",
        )
        .unwrap();
        let ghost = zip.finish().unwrap().into_inner();
        let err = unpack(&ghost).unwrap_err();
        assert!(
            err.to_string().contains("idref 'ghost'"),
            "a broken spine reference errs, never guesses: {err}"
        );
        // (h) empty spine
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"content.opf\"/></rootfiles></container>",
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(b"<package><manifest/><spine/></package>")
            .unwrap();
        let empty_spine = zip.finish().unwrap().into_inner();
        let err = unpack(&empty_spine).unwrap_err();
        assert!(err.to_string().contains("empty spine"), "{err}");
        // (i) OPF XML that does not parse (mismatched tags)
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"content.opf\"/></rootfiles></container>",
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(b"<package><manifest></spine></package>")
            .unwrap();
        let bad_xml = zip.finish().unwrap().into_inner();
        let err = unpack(&bad_xml).unwrap_err();
        assert!(err.to_string().contains("does not parse as XML"), "{err}");
        // (j) truncated REAL epub: byte-half cuts the central directory
        let epub = build_epub(&["ch1", "ch2"], &[]);
        let err = unpack(&epub[..epub.len() / 2]).unwrap_err();
        assert!(
            err.to_string().contains("cannot open as a zip archive"),
            "truncated zip framing must be detected: {err}"
        );
        // the uniform gates
        let err = unpack(b"").unwrap_err();
        assert!(err.to_string().contains("empty input"), "{err}");
        let epub = build_epub(&["ch1"], &[]);
        let err = EpubExtractor::new()
            .unpack(&epub, "application/zip")
            .unwrap_err();
        assert!(err.to_string().contains("cannot handle mime"), "{err}");
        assert!(unpack(&epub).is_ok(), "canonical mime passes");
        assert!(
            EpubExtractor::new().unpack(&epub, "").is_ok(),
            "empty mime tolerated"
        );
    }

    #[test]
    fn href_resolution_normalizes_relative_spellings() {
        // OPF at OEBPS/content.opf: plain href, ./ and ../ spellings, and a
        // percent-encoded space all resolve to the one entry path
        assert_eq!(
            resolve_href("OEBPS/content.opf", "ch1.xhtml"),
            "OEBPS/ch1.xhtml"
        );
        assert_eq!(
            resolve_href("OEBPS/content.opf", "./ch1.xhtml"),
            "OEBPS/ch1.xhtml"
        );
        assert_eq!(
            resolve_href("OEBPS/text/content.opf", "../ch1.xhtml"),
            "OEBPS/ch1.xhtml"
        );
        assert_eq!(
            resolve_href("content.opf", "my%20chapter.xhtml"),
            "my chapter.xhtml"
        );
        // a root-level OPF has no dir; `..` saturates at the root
        assert_eq!(resolve_href("content.opf", "ch1.xhtml"), "ch1.xhtml");
        assert_eq!(resolve_href("content.opf", "../ch1.xhtml"), "ch1.xhtml");
    }

    #[test]
    fn entry_count_bomb_refused_before_any_entry_read() {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        for i in 0..super::super::CONTAINER_MAX_ENTRIES {
            zip.start_file(format!("junk/{i}"), opts).unwrap();
            zip.write_all(b"x").unwrap();
        }
        let bomb = zip.finish().unwrap().into_inner();
        let err = unpack(&bomb).unwrap_err();
        let msg = err.to_string();
        // the count leg's numbers + the 裁决 2 two-branch vocabulary naming
        // WHICH leg tripped (the count, not the ratio)
        assert!(
            msg.contains(&format!(
                "lists {} entries (max {})",
                super::super::CONTAINER_MAX_ENTRIES + 1,
                super::super::CONTAINER_MAX_ENTRIES
            )),
            "{msg}"
        );
        assert!(
            msg.contains("too many entries OR an expansion ratio past"),
            "{msg}"
        );
        assert!(msg.contains("trips the ENTRY COUNT"), "{msg}");
        assert!(msg.contains("refused before reading any entry"), "{msg}");
    }

    /// Build a REAL N-chapter book (the 2026-11-11 裁决 2 corpus shape: a
    /// 1560-chapter novel must pass — `build_epub` above is fixed at the
    /// three-chapter fixture, so the long-book tests roll their own with
    /// the same zip crate the kind reads).
    fn build_long_epub(chapters: usize) -> Vec<u8> {
        use std::io::Write;
        let manifest = (0..chapters)
            .map(|i| {
                format!(
                    "<item id=\"c{i}\" href=\"c{i}.xhtml\" media-type=\"application/xhtml+xml\"/>"
                )
            })
            .collect::<String>();
        let spine = (0..chapters)
            .map(|i| format!("<itemref idref=\"c{i}\"/>"))
            .collect::<String>();
        let opf = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\">\
             <manifest>{manifest}</manifest><spine>{spine}</spine></package>"
        );
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"content.opf\"/></rootfiles></container>",
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(opf.as_bytes()).unwrap();
        for i in 0..chapters {
            zip.start_file(format!("c{i}.xhtml"), opts).unwrap();
            zip.write_all(
                format!(
                    "<html><body><h1>第{}章</h1><p>第{}章的正文内容，一段足够真实的小节文字。</p></body></html>",
                    i + 1,
                    i + 1
                )
                .as_bytes(),
            )
            .unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// THE 裁决 2 positive: a 1500-chapter book (the corpus that tripped
    /// the old 256-entry guard is 1560 — same shape, comfortably under
    /// the widened 4096) unpacks whole, spine-ordered, every chapter a
    /// text/html child carrying bytes.
    #[test]
    fn thousand_plus_chapter_book_passes_the_widened_guard() {
        let epub = build_long_epub(1500);
        let children = unpack(&epub).unwrap().expect("container verdict");
        assert_eq!(children.len(), 1500, "every chapter survives the unpack");
        // spine order = natural order here: first child c0, last c1499
        assert_eq!(children[0].name, "c0.xhtml");
        assert_eq!(children[1499].name, "c1499.xhtml");
        assert_eq!(children[0].mime, "text/html");
        assert!(
            String::from_utf8_lossy(&children[1499].bytes).contains("第1500章"),
            "the last chapter's bytes arrived intact"
        );
    }

    /// THE 裁决 2 ratio leg: FEW entries (nowhere near 4096), each well
    /// under the 256 MiB per-entry ceiling — but the decompressed total
    /// blows past `container bytes × 256`, and the unpack refuses WHILE
    /// READING with the two-branch vocabulary naming the RATIO branch and
    /// both numbers. (All-zero chapters deflate ~1000:1 — exactly the
    /// compressible-payload shape the count-only guard could never see.)
    #[test]
    fn expansion_ratio_bomb_refused_while_reading() {
        use std::io::Write;
        let zeros = vec![0u8; 2 * 1024 * 1024]; // 2 MiB, ~2 KB deflated
        let opf = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\">\
            <manifest><item id=\"c0\" href=\"c0.xhtml\" media-type=\"application/xhtml+xml\"/>\
            <item id=\"c1\" href=\"c1.xhtml\" media-type=\"application/xhtml+xml\"/>\
            <item id=\"c2\" href=\"c2.xhtml\" media-type=\"application/xhtml+xml\"/></manifest>\
            <spine><itemref idref=\"c0\"/><itemref idref=\"c1\"/><itemref idref=\"c2\"/>\
            </spine></package>";
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"content.opf\"/></rootfiles></container>",
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(opf.as_bytes()).unwrap();
        for i in 0..3 {
            zip.start_file(format!("c{i}.xhtml"), opts).unwrap();
            zip.write_all(&zeros).unwrap();
        }
        let bomb = zip.finish().unwrap().into_inner();
        // ~6 MiB decompressed against a ~6 KB container: ~1000×, far past 256×
        let err = unpack(&bomb).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("expansion-ratio bomb guard refuses while reading"),
            "{msg}"
        );
        assert!(
            msg.contains("too many entries OR an expansion ratio past"),
            "{msg}"
        );
        assert!(msg.contains("this one: the ratio"), "{msg}");
        // the two numbers: the running total and the limit it crossed
        assert!(msg.contains("past the container limit"), "{msg}");
        assert!(
            msg.contains(&format!("× {}", super::super::CONTAINER_EXPANSION_RATIO)),
            "{msg}"
        );
        // and it is NOT the count leg (3 chapters is nowhere near 4096)
        assert!(!msg.contains("ENTRY COUNT"), "{msg}");
    }

    /// extract() is the loud routing-bug refusal (role separation) — pinned
    /// here so the dispatch arm cannot silently change meaning.
    #[test]
    fn extract_refuses_loudly_never_guesses() {
        let err = EpubExtractor::new()
            .extract(b"whatever", EPUB_MIME)
            .unwrap_err();
        assert!(
            err.to_string().contains("container kind: it unpacks"),
            "{err}"
        );
    }

    #[test]
    fn affinity_is_the_undeclared_default() {
        // no declaration (module docs): Capable for EVERY class — the
        // ContentClass vocabulary is PDF-shaped, an epub source probes
        // Unknown and the router never consults this
        use crate::pipeline::{ContentClass, Fit};
        for class in [
            ContentClass::TextNative,
            ContentClass::ScanDominant,
            ContentClass::MixedTextScan,
            ContentClass::NoText,
        ] {
            assert_eq!(
                Extractor::affinity(&EpubExtractor::new(), class),
                Fit::Capable
            );
        }
    }

    #[test]
    fn config_roundtrip() {
        let cfg: EpubConfig = toml::from_str("kind = \"epub\"").unwrap();
        assert_eq!(cfg.kind, EpubTag::Epub);
        // 恒序列化 (the T12 `structure` discipline): the knob serializes at
        // the DEFAULT too — an elided default would punch a hole in "the
        // fingerprint owns chunk-set identity"
        assert!(cfg.chapter_titles);
        assert_eq!(
            toml::to_string(&cfg).unwrap().trim(),
            "kind = \"epub\"\nchapter-titles = true"
        );
        // the spelled-off form roundtrips; Default agrees with the
        // deserialization default (one construction path)
        let off: EpubConfig = toml::from_str("kind = \"epub\"\nchapter-titles = false").unwrap();
        assert!(!off.chapter_titles);
        assert_eq!(
            toml::to_string(&off).unwrap().trim(),
            "kind = \"epub\"\nchapter-titles = false"
        );
        assert_eq!(EpubConfig::default(), cfg);
        // wrong literal rejected (stage-2 tag guards stage-1 dispatch);
        // unknown field still rejected (one knob, not an open struct)
        assert!(toml::from_str::<EpubConfig>("kind = \"zip\"").is_err());
        assert!(toml::from_str::<EpubConfig>("kind = \"epub\"\ndpi = 300").is_err());
    }

    // ---- T18 chapter titles (D-9.2): the two-level chain ----

    /// EPUB3 nav labels win: the properties="nav" document names each
    /// spine chapter, and the children carry its toc-nav anchor texts
    /// verbatim (spine order, not nav order — the scramble must hold; and
    /// the landmarks nav's anchors must NOT leak — only epub:type="toc").
    #[test]
    fn chapter_titles_nav_labels_take_priority() {
        use std::io::Write;
        let manifest = CHAPTERS
            .iter()
            .map(|(id, _)| {
                format!(
                    "<item id=\"{id}\" href=\"{id}.xhtml\" media-type=\"application/xhtml+xml\"/>"
                )
            })
            .collect::<String>()
            + "<item id=\"nv\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" \
                properties=\"nav\"/>";
        let opf = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\">\
             <metadata/><manifest>{manifest}</manifest>\
             <spine><itemref idref=\"ch2\"/><itemref idref=\"ch3\"/><itemref idref=\"ch1\"/>\
             </spine></package>"
        );
        let nav = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <html xmlns=\"http://www.w3.org/1999/xhtml\" \
             xmlns:epub=\"http://www.idpf.org/2007/ops\">\
             <body><nav epub:type=\"toc\"><ol>\
             <li><a href=\"ch1.xhtml\">第一章　起航</a></li>\
             <li><a href=\"ch2.xhtml\">第二章　转折</a></li>\
             <li><a href=\"ch3.xhtml\">第三章　归宿</a></li>\
             </ol></nav>\
             <nav epub:type=\"landmarks\"><ol><li><a href=\"ch1.xhtml\">正文开头</a></li></ol>\
             </nav></body></html>";
        let container_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <container version=\"1.0\" \
             xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\
             <rootfiles><rootfile full-path=\"OEBPS/content.opf\" \
             media-type=\"application/oebps-package+xml\"/></rootfiles></container>";
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        for (name, bytes) in [
            ("META-INF/container.xml", container_xml.as_bytes()),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/nav.xhtml", nav.as_bytes()),
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(bytes).unwrap();
        }
        for (id, _) in CHAPTERS {
            zip.start_file(format!("OEBPS/{id}.xhtml"), opts).unwrap();
            zip.write_all(b"<html><body><p>chapter body</p></body></html>")
                .unwrap();
        }
        let epub = zip.finish().unwrap().into_inner();
        let children = EpubExtractor::new()
            .unpack(&epub, EPUB_MIME)
            .unwrap()
            .expect("container verdict");
        let got: Vec<(&str, &str)> = children
            .iter()
            .map(|c| (c.name.as_str(), c.title.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            got,
            vec![
                ("OEBPS/ch2.xhtml", "第二章　转折"),
                ("OEBPS/ch3.xhtml", "第三章　归宿"),
                ("OEBPS/ch1.xhtml", "第一章　起航"),
            ],
            "nav labels follow the spine order; landmarks never leak"
        );
    }

    /// EPUB2 NCX labels: spine@toc names the NCX manifest item, its
    /// navMap labels apply (nested navPoints keep their own), and the
    /// NCX's own directory anchors the src resolution.
    #[test]
    fn chapter_titles_ncx_labels_via_spine_toc() {
        use std::io::Write;
        let opf = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"2.0\">\
            <metadata/><manifest>\
            <item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\
            <item id=\"c1\" href=\"text/c1.xhtml\" media-type=\"application/xhtml+xml\"/>\
            <item id=\"c2\" href=\"text/c2.xhtml\" media-type=\"application/xhtml+xml\"/>\
            </manifest><spine toc=\"ncx\">\
            <itemref idref=\"c1\"/><itemref idref=\"c2\"/></spine></package>";
        let ncx = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <ncx xmlns=\"http://www.daisy.org/z3986/2005/ncx/\" version=\"2005-1\">\
            <navMap>\
            <navPoint><navLabel><text>上篇　灯起</text></navLabel>\
            <content src=\"text/c1.xhtml\"/>\
            <navPoint><navLabel><text>第一节</text></navLabel>\
            <content src=\"text/c2.xhtml\"/></navPoint></navPoint>\
            </navMap></ncx>";
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"content.opf\"/></rootfiles></container>",
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(opf.as_bytes()).unwrap();
        zip.start_file("toc.ncx", opts).unwrap();
        zip.write_all(ncx.as_bytes()).unwrap();
        for name in ["text/c1.xhtml", "text/c2.xhtml"] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(b"<html><body><p>body</p></body></html>")
                .unwrap();
        }
        let epub = zip.finish().unwrap().into_inner();
        let children = EpubExtractor::new()
            .unpack(&epub, EPUB_MIME)
            .unwrap()
            .expect("container verdict");
        let got: Vec<(&str, &str)> = children
            .iter()
            .map(|c| (c.name.as_str(), c.title.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            got,
            vec![("text/c1.xhtml", "上篇　灯起"), ("text/c2.xhtml", "第一节")],
            "NCX labels apply, nested navPoints keep their own"
        );
    }

    /// The fallback rung: no nav property and no spine@toc (the shared
    /// builder's shape) ⇒ every child names itself by FILE NAME STEM; and
    /// the knob OFF ⇒ the pre-T18 shape (title None everywhere).
    #[test]
    fn chapter_titles_fall_back_to_file_stems_and_knob_off_is_none() {
        let epub = build_epub(&["ch2", "ch1"], &[]);
        let children = EpubExtractor::new()
            .unpack(&epub, EPUB_MIME)
            .unwrap()
            .expect("container verdict");
        assert_eq!(
            children
                .iter()
                .map(|c| c.title.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("ch2"), Some("ch1")],
            "no nav/NCX ⇒ file-name stems (bounded degradation)"
        );
        let off = EpubExtractor::new()
            .with_chapter_titles(false)
            .unpack(&epub, EPUB_MIME)
            .unwrap()
            .expect("container verdict");
        assert!(
            off.iter().all(|c| c.title.is_none()),
            "knob off ⇒ the pre-T18 untitled shape"
        );
    }

    /// A declared nav the package does not actually carry is a DEGRADED
    /// book, not an error: the stem rung answers (有界退化 — design §5.6).
    #[test]
    fn chapter_titles_missing_nav_part_degrades_to_stems() {
        use std::io::Write;
        let opf = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
            <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\">\
            <metadata/><manifest>\
            <item id=\"nv\" href=\"ghost.xhtml\" media-type=\"application/xhtml+xml\" \
            properties=\"nav\"/>\
            <item id=\"c1\" href=\"c1.xhtml\" media-type=\"application/xhtml+xml\"/>\
            </manifest><spine><itemref idref=\"c1\"/></spine></package>";
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.start_file(
            "mimetype",
            opts.compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(EPUB_MIME.as_bytes()).unwrap();
        zip.start_file("META-INF/container.xml", opts).unwrap();
        zip.write_all(
            b"<container><rootfiles><rootfile full-path=\"content.opf\"/></rootfiles></container>",
        )
        .unwrap();
        zip.start_file("content.opf", opts).unwrap();
        zip.write_all(opf.as_bytes()).unwrap();
        zip.start_file("c1.xhtml", opts).unwrap();
        zip.write_all(b"<html><body><p>body</p></body></html>")
            .unwrap();
        let epub = zip.finish().unwrap().into_inner();
        let children = EpubExtractor::new()
            .unpack(&epub, EPUB_MIME)
            .unwrap()
            .expect("container verdict");
        assert_eq!(
            children[0].title.as_deref(),
            Some("c1"),
            "a declared-but-missing nav degrades to the stem rung"
        );
    }
}
