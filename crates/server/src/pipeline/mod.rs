//! File-layer pipeline plugin system (design.md §5.10; skeleton in M2-T2,
//! extractor/chunker kinds in T4, embedder kinds + model prefetch in T5).
//!
//! Four strategy axes live behind traits ([`Extractor`], [`Chunker`],
//! [`Embedder`], [`Reranker`] — the fourth, query-side, M3-T11) plus the
//! query-side merge option ([`merge_hitlists`]);
//! the ORCHESTRATION is fixed mechanism in code, the POLICY (which kinds,
//! which parameters) is TOML (§5.10.0 axiom 1).
//!
//! # Content routing (M2.1 step 2: probe → affinity → route)
//!
//! Extract chains run in three MECHANISM stages, zero config surface:
//! [`probe::probe`] classifies the bytes into a [`ContentClass`] (pdf family
//! only today; other families read as Unknown), kinds declare their [`Fit`]
//! per class ([`Extractor::affinity`]), and the ingest worker's pure router
//! turns member affinities into a try order — the config-ordered chain stays
//! the correctness backstop (a routed member that still yields falls back to
//! the remaining members), while mis-ORDERED chains can no longer put a
//! quality-inferior member first for content a later member serves exactly.
//!
//! # Container unpacking (M2.1 step 3)
//!
//! A kind is either a LEAF or a CONTAINER ([`Extractor::unpack`], default
//! `Ok(None)` = leaf). A container's children ([`ChildDoc`]) re-enter the
//! USER-CONFIGURED routing — each child's mime resolves to a chain in the
//! same ingest config — so a container kind never digests its own children
//! (the operator's per-mime chains apply inside archives automatically).
//! The production kinds are all leaves in M2.1 (epub/docx/eml land in M3
//! with the zip dependency); the ingest worker owns the mechanism and its
//! bomb guards.
//!
//! # Normalize preprocessing (M2.1 step 3, [`normalize`])
//!
//! BEFORE routing, every document and every container child runs through
//! one fixed, ordered table of built-in byte-level transforms
//! ([`normalize::normalize`]; M2: gunzip). The placement criterion and the
//! not-an-axis ruling live in that module's docs: mechanically-always-
//! correct ⇒ normalizer; format knowledge or a user choice ⇒ kind (the
//! container role above). Probe and routing always see the POST-normalize
//! state, and the stage contributes nothing to the pipeline fingerprint.
//!
//! # Registration model (§5.10.2, Rust form, v2.1)
//!
//! * **Config layer, two-stage**: first read the `kind` STRING from an
//!   instance section, then deserialize the section into the per-kind config
//!   struct (each `deny_unknown_fields`). This is deliberate: serde's
//!   internally-tagged enums do not support `deny_unknown_fields`, so a
//!   single-enum one-shot parse cannot give the fail-fast unknown-key error
//!   the design demands.
//! * **Runtime layer, closed enum dispatch**: each axis has a `*Kind` enum
//!   (`extract::ExtractorKind`, `chunk::ChunkerKind`,
//!   `embed::EmbedderKind`). Adding a kind = adding a variant; every match
//!   is exhaustive, there are ZERO vtables on the hot path. `dyn` is
//!   reserved for the v2 wasm slot (§5.10.8: not in v1) and the contract
//!   tests, which must accept ANY implementation.
//!
//! # DIP boundary (machine-enforced by review, stated here)
//!
//! Engine-native types — ort sessions, pdfium handles, HTTP clients — may
//! only appear INSIDE a kind module (`extract/txt.rs`,
//! `chunk/token_window.rs`, `embed/ort.rs`, `embed/openai.rs` + the shared
//! `embed/http.rs` leaf).
//! This module tree's core (traits, errors, merge, config) sees only the
//! traits and [`PipelineError`]; swapping pdfium for another PDF engine must
//! not touch anything outside `extract/pdf_*.rs`.
//!
//! # Modality is a closed enum (§5.10.1)
//!
//! [`Modality`] carries `Text` (M2) and `Image` (M3-T1). Adding values
//! (`PageImage`, `Audio`) is a DESIGNED-FOR event: the compiler walks every
//! exhaustive match and the ingest router fans Parts out by modality with
//! zero routing-config changes. Video is NOT a modality (video extractors
//! demux into the three existing ones).

pub mod chunk;
pub mod config;
pub mod contract;
pub mod embed;
pub mod engines;
pub mod extract;
pub mod gate;
pub mod models;
pub mod normalize;
pub mod probe;
pub mod rerank;

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// data model
// ---------------------------------------------------------------------------

/// Content modality of a [`Part`] / capability declaration of an
/// [`Embedder`]. Closed enum by design (see module docs); `M3+ adds values`
/// is in-design behavior, not a breaking change. `Image` landed in M3-T1
/// (mechanism ground); the siglip2 embedder (M3-T2) is its first taker —
/// the pdf-pages/image extractor kinds are T3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Modality {
    /// Plain UTF-8 text (the only M2 modality).
    Text,
    /// A raster image (jpeg/png today): carried by [`Part::image`] /
    /// [`Query::Image`], chunked atomically (§5.10.2: the chunker axis
    /// serves text only), fanned out to embedders declaring Image.
    Image,
}

impl std::fmt::Display for Modality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Modality::Text => f.write_str("text"),
            Modality::Image => f.write_str("image"),
        }
    }
}

impl Modality {
    /// The remedy line the ingest fan-out gate quotes when NO listed
    /// embedder accepts this modality on its doc side — what capability is
    /// missing, phrased BY the modality (the [`ContentClass::remedy`]
    /// precedent: the vocabulary that is missing names the capability to
    /// add, so a future non-image modality cannot inherit an image-specific
    /// hint).
    pub fn remedy(self) -> &'static str {
        match self {
            Modality::Image => {
                "add an embedder that accepts image documents to this pipeline's embedders \
                 list (the siglip2 kind does), or keep image sources out of it"
            }
            // every built-in embedder kind accepts text documents, so an
            // all-text-kind pipeline with text chunks never reaches this
            // aggregate; only a custom kind accepting nothing could
            Modality::Text => {
                "every built-in embedder kind accepts text documents — this aggregate is \
                 reachable only through custom kinds that accept no modalities at all"
            }
        }
    }
}

/// STRUCTURAL kind of a text region (T12, retrieval-quality solution D-5):
/// what a labeled span of a [`Part`]'s text IS, as opposed to what it
/// carries ([`Modality`]). The vocabulary is closed exactly like
/// `Modality`/`ContentClass` — four values, kebab-case `Display`, adding one
/// is a designed-for compiler-walked event.
///
/// NOT a registration axis: unlike `ExtractorKind`/`ChunkerKind` there is no
/// config string to parse and no availability list to keep in parity — the
/// kind_REGISTRY_views-agree discipline does not apply. The string form is
/// still load-bearing downstream (`x-vf-chunk-kind` metadata, filterable,
/// default-`"body"` — later T12 slices), which is why `Display` is pinned by
/// a unit test instead of a registry table.
///
/// `Body` is the DEFAULT in both directions: unlabeled text is body, and a
/// window the chunker cannot confidently attribute is body (the
/// recall-protection arbitration — 宁可漏排不可误排, D-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChunkKind {
    /// Ordinary prose — the default and the ambiguity fallback.
    Body,
    /// Table-of-contents / index-like listing (line-level TOC spans, D-1).
    Toc,
    /// Figure/table caption text.
    Caption,
    /// Legend/key block (figure legends, symbol keys).
    Legend,
}

impl std::fmt::Display for ChunkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            ChunkKind::Body => f.write_str("body"),
            ChunkKind::Toc => f.write_str("toc"),
            ChunkKind::Caption => f.write_str("caption"),
            ChunkKind::Legend => f.write_str("legend"),
        }
    }
}

impl ChunkKind {
    /// The inverse of `Display` (the `x-vf-chunk-kind` value grammar): the
    /// closed four-word vocabulary, or `None` for anything else. The
    /// `/x-vf/Search` face validates its `includeKinds` superset parameter
    /// through here (T12: an unknown kind must fail loud with a
    /// field-addressable error, never silently match nothing — the m5
    /// path discipline).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "body" => Some(ChunkKind::Body),
            "toc" => Some(ChunkKind::Toc),
            "caption" => Some(ChunkKind::Caption),
            "legend" => Some(ChunkKind::Legend),
            _ => None,
        }
    }
}

/// Content CLASS of one source's bytes (M2.1 step 2: probe → affinity →
/// route): what the bytes ARE, family-agnostic — the same vocabulary serves
/// the pdf family today and the image family in M3.
///
/// Lives beside [`Modality`] with the same "increment adds values"哲学:
/// modality says what a Part CARRIES, class says what the SOURCE bytes
/// contain. The ingest worker's classification PROBE ([`probe::probe`])
/// derives it; extractor kinds declare their [`Fit`] for it via
/// [`Extractor::affinity`]; the router ([`crate::ingest`]'s `route`) turns
/// the two into a try order. Closed enum, kebab-case Display, zero config
/// surface (§5.10.8 stands: this is mechanism, not a routing table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContentClass {
    /// A real text layer: extraction is a decode, quality is exact.
    TextNative,
    /// Raster scans dominate; a text extractor has nothing honest to read —
    /// OCR territory.
    ScanDominant,
    /// Text pages and scan/image pages coexist: a text extractor is CAPABLE
    /// (partial output — today's pdfium behavior on such documents) but not
    /// the best possible answer (OCR on the scan pages would add more).
    MixedTextScan,
    /// No text and no scan payload at all (blank pages): nothing to extract
    /// and nothing to OCR.
    NoText,
}

impl std::fmt::Display for ContentClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            ContentClass::TextNative => f.write_str("text-native"),
            ContentClass::ScanDominant => f.write_str("scan-dominant"),
            ContentClass::MixedTextScan => f.write_str("mixed-text-scan"),
            ContentClass::NoText => f.write_str("no-text"),
        }
    }
}

impl ContentClass {
    /// The remedy line the router quotes when EVERY member of a chain
    /// refuses this class — what capability is missing, phrased by the
    /// CLASS (the class knows what it lacks; a PDF-specific hint here would
    /// misdirect every future non-PDF family). This is why the all-refuse
    /// remedy is a method and not a route()-local string: scan classes
    /// genuinely need an OCR member, but `no-text` needs nothing any
    /// extractor can provide — pointing its operator at OCR would be wrong
    /// advice, and was (hardening, 2026-11-05: the pre-method message
    /// hardcoded the OCR hint for every class).
    pub fn remedy(self) -> &'static str {
        match self {
            ContentClass::ScanDominant | ContentClass::MixedTextScan => {
                "add an OCR-capable extractor to this chain (the planned pdf-ocr kind, M3)"
            }
            ContentClass::NoText => {
                "no text layer and no raster content — no extractor, OCR included, can index \
                 this document; verify the file is the one you meant to ingest"
            }
            // a text-native refusing member is possible (a text-family kind
            // under a mis-probed class), but an ALL-refuse text-native chain
            // cannot: every text extractor declares text-native Preferred
            ContentClass::TextNative => {
                "no member of this chain extracts native text — every built-in text kind does; \
                 this aggregate is reachable only through custom undeclared kinds"
            }
        }
    }
}

/// How well an extractor kind serves a [`ContentClass`] — the three-level
/// closed vocabulary kinds declare (see [`Extractor::affinity`]) and the
/// ingest router consumes: first `Preferred` wins, else first `Capable`,
/// else the chain is `Refused` outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// The right tool: exact, preferred output for this class.
    Preferred,
    /// Can serve it honestly (partial/degraded-but-real output); chosen only
    /// when no member is `Preferred`.
    Capable,
    /// Will not serve this class at all — running it would Err or, worse,
    /// produce quality-inferior output the chain would then ACCEPT.
    Refuses,
}

/// One extracted payload: a modality plus its content channels. `text` is
/// the M2 shape and stays exactly that for `Modality::Text` parts; image
/// parts (M3-T1) carry their bytes in [`Part::image`] with an EMPTY text
/// channel — audio (M4) grows its own field the same additive way.
///
/// T12 (retrieval-quality solution §2-T12.2) adds two additive annotation
/// channels plus one audit counter, all anchored to `text` and all empty/0
/// for every pre-T12 producer/consumer: [`Part::labels`] (structural
/// spans), [`Part::pages`] (the page-offset table T13 shares), and
/// [`Part::furniture_removed`]. They are PLAIN Vecs/ints, not Options, for
/// the same reason `image` is additive-but-explicit: an absent annotation
/// and an empty annotation mean the same thing here, and one-line
/// `Vec::new()`/`0` literals keep every existing construction site honest
/// about the new channels.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub modality: Modality,
    pub text: String,
    /// Image payload, present iff the part is `Modality::Image` (the
    /// constructors keep the pair consistent; the chunker bypass enforces
    /// it loudly at the mechanism seam). `Option` + a never-touched `text`
    /// field is deliberately additive: every existing text-part literal and
    /// assertion keeps compiling and passing unchanged.
    pub image: Option<ImagePayload>,
    /// Structural labels over `text` (T12): BYTE ranges carrying a
    /// [`ChunkKind`]. Anchored in bytes because the EXTRACTOR produces them
    /// by slicing `text` as UTF-8; the CHUNKER owns the byte→char
    /// conversion (D-1: "字节锚定，chunker 统一换算 char 域——windowing 的
    /// CJK 防线") — it must never slice on a raw byte offset. Empty = no
    /// structural annotation (the pre-T12 shape; every region is body).
    pub labels: Vec<LabelSpan>,
    /// Page-offset table (T12/T13 shared surgery): (1-based page number →
    /// BYTE offset into `text` where that page's text begins). Empty = the
    /// extractor has no page information (pageless sources and the pre-T12
    /// producers; the T12 pdf-text structure path emits its table for
    /// EVERY page count, single-page included — the engine always knows
    /// its pages. D-7's fork判据 reads this same presence). The chunker
    /// does NOT consume it in T12's foundation slice — it rides along so
    /// T13's window-coordinate stamping needs no second Part operation.
    pub pages: Vec<(u32, usize)>,
    /// Furniture-removal audit count (T12): how many lines THIS extractor
    /// dropped as cross-page furniture (headers/footers/copyright straps).
    /// Default 0 — the pre-T12 producers and every non-PDF kind remove
    /// nothing; a NON-zero count means the text channel is lossier than
    /// the source by exactly that many lines, an auditable fact instead of
    /// a silent behavior change (静默行为变可审计行为, solution §2-T12.3).
    /// This slice only PRODUCES the count; the ingest-status consumer is
    /// the later T12 slice.
    pub furniture_removed: usize,
    /// Container-section ordinal (T13, the ORCHESTRATION source of the
    /// coordinate triad — ADR 0005): the 1-based position of the container
    /// child this part came from, stamped by the ingest container walk
    /// ([`crate::ingest`]'s `run_member`) — NOT by any extractor. `None`
    /// is the ordinary value: leaf documents have no container, and
    /// extractors never touch the field. Unlike the annotation channels
    /// above it is a genuine `Option` (absence is the norm for every
    /// non-child source, not an "empty annotation"), and the stamping
    /// order rule is documented at the stamping site (an outer container
    /// stamps last and wins over an inner walk's numbering).
    pub section: Option<u32>,
}

/// One structural label over a [`Part`]'s text (T12): a BYTE range plus its
/// [`ChunkKind`]. Producers are the extractors (furniture/TOC/caption
/// heuristics — later T12 slices); the consumer is the chunker axis, which
/// derives forced window endpoints from NON-body spans and judges each
/// window's kind (see `chunk::windowing`). Offsets are byte offsets into
/// `Part::text` and are expected on char boundaries (producers slice text
/// they themselves built); a consumer clamping defensively is documented at
/// the conversion site.
// (no Copy: `Range<usize>` is Clone-only; labels move, they never hot-path)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelSpan {
    /// Half-open BYTE range into `Part::text` (start inclusive, end
    /// exclusive).
    pub range: std::ops::Range<usize>,
    pub kind: ChunkKind,
}

/// The image content channel of a [`Part`] (M3-T1): raw container bytes
/// plus the mime the EXTRACTOR declared for them (`image/jpeg` / `image/png`
/// / …). The mime rides along because embedder preprocessing keys off it
/// (resize/crop/normalize differ per container) — it is validated against
/// the bytes' magic at the chunker bypass ([`ImagePayload::validate`]), so
/// a wrong declaration fails loudly instead of surfacing as wrong
/// preprocessing inside a T2 embedder.
#[derive(Debug, Clone, PartialEq)]
pub struct ImagePayload {
    /// Raw image-container bytes (`bytes::Bytes`: payloads are handed
    /// around by reference inside one CPU segment, never re-read).
    pub bytes: bytes::Bytes,
    /// The declared container mime; must be the container's CANONICAL mime
    /// (see [`ImagePayload::validate`]).
    pub mime: String,
}

/// A child document unpacked from a container (zip/mime-multipart/tar…): the
/// container kind's ONE product (M2.1 step 3). Children do not stay inside
/// the kind — the ingest worker re-routes each one through the USER-CONFIGURED
/// extract chain its mime resolves to, so configuration coherence is
/// automatic (an operator's html-chain config applies to epub chapters
/// without the epub kind knowing anything about it — the Tika
/// CompositeParser/PackageParser shape, deliberately stronger than
/// unstructured's hardcoded delegation).
///
/// * `bytes` — the entry's raw payload (`bytes::Bytes`: children are handed
///   around by reference inside one CPU segment, never re-read).
/// * `mime` — the entry's OWN mime; routing key for the child chain.
/// * `name` — the chapter/entry path (e.g. `OEBPS/ch1.xhtml`): provenance
///   and logs only, never routing input (a child has no filename to guess
///   from — the mime IS its identity).
#[derive(Debug, Clone, PartialEq)]
pub struct ChildDoc {
    pub bytes: bytes::Bytes,
    pub mime: String,
    pub name: String,
}

impl ChildDoc {
    /// Convenience constructor (tests and future M3 kinds).
    pub fn new(
        name: impl Into<String>,
        mime: impl Into<String>,
        bytes: impl Into<bytes::Bytes>,
    ) -> Self {
        ChildDoc {
            bytes: bytes.into(),
            mime: mime.into(),
            name: name.into(),
        }
    }
}

impl Part {
    /// Convenience constructor for the M2 text-only shape (image channel
    /// `None`). Signature unchanged by the T12 surgery: the new
    /// `labels`/`pages`/`furniture_removed` channels open EMPTY (no
    /// structural annotation — callers stay zero-diff; the label-aware
    /// path constructs the struct directly or mutates the fields).
    pub fn text(s: impl Into<String>) -> Self {
        Part {
            modality: Modality::Text,
            text: s.into(),
            image: None,
            labels: Vec::new(),
            pages: Vec::new(),
            furniture_removed: 0,
            section: None,
        }
    }

    /// Convenience constructor for an image part: empty text channel, the
    /// payload in the image channel. Infallible by design (it only moves
    /// values); the bytes-vs-mime honesty check happens at the mechanism
    /// seam ([`ImagePayload::validate`], called by the chunker bypass).
    /// Image parts never carry labels/pages (nothing is anchored to an
    /// empty text channel).
    pub fn image(mime: impl Into<String>, bytes: impl Into<bytes::Bytes>) -> Self {
        Part {
            modality: Modality::Image,
            text: String::new(),
            image: Some(ImagePayload {
                bytes: bytes.into(),
                mime: mime.into(),
            }),
            labels: Vec::new(),
            pages: Vec::new(),
            furniture_removed: 0,
            section: None,
        }
    }
}

impl ImagePayload {
    /// Mechanism guard at the chunker bypass (M3-T1): sniff the container
    /// magic (`image::guess_format`, header bytes only — nothing is decoded
    /// here) and require it to BE the declared mime, canonically. Two loud
    /// refusals, both `PipelineError::chunk` (the stage refusing them):
    ///
    /// * bytes with NO known image magic — garbage must not enter the index
    ///   as an "image chunk" (the no-taker embedder gate would then mislead
    ///   with an "add an image embedder" remedy for what is a corrupt file);
    /// * magic ≠ declared mime (or a non-canonical alias like `image/jpg`)
    ///   — the declared mime drives embedder preprocessing, so a mismatch
    ///   fails HERE, at the seam, naming both values.
    pub fn validate(&self) -> Result<(), PipelineError> {
        let sniffed = image::guess_format(&self.bytes).map_err(|_| {
            PipelineError::chunk(format!(
                "image payload carries no known image-container magic (jpeg/png today) — \
                 refusing to index {} non-image bytes as an image chunk",
                self.bytes.len()
            ))
        })?;
        let canonical = sniffed.to_mime_type();
        if canonical != self.mime {
            return Err(PipelineError::chunk(format!(
                "image payload mime mismatch: the bytes sniff as {canonical} but the part \
                 declares '{}' — the declared mime drives embedder preprocessing; use the \
                 container's canonical mime",
                self.mime
            )));
        }
        Ok(())
    }
}

/// A chunker product: the file-layer view of one future vector row.
/// `index` is assigned sequentially by the mechanism across all parts of one
/// source (text windows AND atomic image chunks share the one sequence,
/// §5.10.2), so chunk keys stay stable and ordered.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub index: u32,
    pub text: String,
    /// Image payload, present iff this is an atomic image chunk (the
    /// chunker-axis bypass materializes image Parts one-to-one). Empty for
    /// text windows — same additive shape and reasoning as [`Part::image`].
    pub image: Option<ImagePayload>,
    /// Structural kind (T12): `Body` for unlabeled/ambiguous windows and
    /// for atomic image chunks (an image carries no text structure to
    /// label); a non-body kind only when the window falls entirely inside
    /// one kind's label span (the chunker's judgment — see
    /// `chunk::windowing`). Default `Body` keeps every pre-T12 producer
    /// byte-compatible in the `Chunk::modality` sense: equality on old
    /// fixtures changes only by the uniformly-added field.
    pub kind: ChunkKind,
    /// Content-addressing PAGE interval (T13, ADR 0005): the INCLUSIVE
    /// 1-based page range the window's chars cover, `(start, end)` — a
    /// window straddling a page boundary reports both pages, and the
    /// mandated overlap makes consecutive windows' intervals overlap
    /// HONESTLY (跨窗区间如实重叠, D-7 — no dedup, no shrinking). `Some` iff
    /// the source part carried a page-offset table ([`Part::pages`], the
    /// D-7 fork: pages present ⇒ page keys and NO line keys); `None` for
    /// pageless sources and atomic image chunks.
    pub page: Option<(u32, u32)>,
    /// Content-addressing LINE interval (T13): the inclusive 1-based line
    /// range — `1 + '\n' count before the window's first char` to
    /// `1 + '\n' count before the window's exclusive end` (a window ending
    /// in a '\n' carries that break, so its end is the line the break
    /// OPENS — the boundary semantics `chunk::windowing::Coords` pins).
    /// The D-7 fork's other arm: `Some` iff the part carried NO page
    /// table. Line numbers count within the part's own text (container
    /// children restart at line 1 — never across parts); md-sourced line
    /// numbers address the EXTRACTED text (the md kind strips heading
    /// markers), a product-domain caveat the CLI notes.
    pub line: Option<(u32, u32)>,
    /// Content-addressing SECTION ordinal (T13): the container-child
    /// position this chunk's part came from (1-based, ingest order),
    /// passed through untouched from [`Part::section`] (the orchestration
    /// source — extractors never set it). `None` for every leaf source.
    pub section: Option<u32>,
}

impl Chunk {
    /// The chunk's modality: `Image` iff it carries an image payload, else
    /// `Text`. Derived, not stored — a chunk IS an image chunk exactly when
    /// it carries the payload, so the two can never disagree; consumers
    /// (ingest routing, `x-vf-modality` metadata, per-modality chunk keys)
    /// all read this one verdict.
    pub fn modality(&self) -> Modality {
        match self.image {
            Some(_) => Modality::Image,
            None => Modality::Text,
        }
    }
}

/// Query-side input. `Image` (M3-T1: raw bytes + mime, the query-side twin
/// of [`ImagePayload`]) is the dispatch point the search face will route by
/// query modality (§5.10.1 symmetric fan-out); `Audio` lands in M4.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Text(String),
    /// Image query: container bytes + declared mime. Text-only embedders
    /// REFUSE it loudly (their `embed_query` errors; the kind set grows an
    /// image-query taker in T2).
    Image(Vec<u8>, String),
}

impl Query {
    /// Noun phrase for error/progress lines ("a text query", "an image
    /// query (mime image/png)") — modality-first, never the bytes.
    pub fn describe(&self) -> String {
        match self {
            Query::Text(_) => "a text query".to_string(),
            Query::Image(_, mime) => format!("an image query (mime {mime})"),
        }
    }
}

// ---------------------------------------------------------------------------
// unified error
// ---------------------------------------------------------------------------

/// The ONE error type crossing the pipeline seams (DIP boundary: kind
/// modules translate their native errors into this, never leak them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    /// Configuration / registration failure. `line` is the 1-based TOML line
    /// the error is attributed to (fail-fast errors must carry it,
    /// design.md §5.10.3), when a span was available.
    Config {
        message: String,
        line: Option<usize>,
    },
    /// Extraction failed (bad bytes, wrong mime, decoder error).
    Extract(String),
    /// Chunking failed.
    Chunk(String),
    /// Embedding failed (model load, inference, provider).
    Embed(String),
    /// Underlying storage/transport I/O failed.
    Io(String),
}

impl PipelineError {
    /// Config error with a known TOML line.
    pub fn config(message: impl Into<String>, line: Option<usize>) -> Self {
        PipelineError::Config {
            message: message.into(),
            line,
        }
    }

    /// Extract error (no line: these are runtime, not config).
    pub fn extract(message: impl Into<String>) -> Self {
        PipelineError::Extract(message.into())
    }

    /// Chunk error.
    pub fn chunk(message: impl Into<String>) -> Self {
        PipelineError::Chunk(message.into())
    }

    /// Embed error.
    pub fn embed(message: impl Into<String>) -> Self {
        PipelineError::Embed(message.into())
    }

    /// Io error.
    pub fn io(message: impl Into<String>) -> Self {
        PipelineError::Io(message.into())
    }
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // One actionable line; `at line N` is the repair hint the
            // fail-fast contract (§5.10.3) requires.
            PipelineError::Config { message, line } => match line {
                Some(n) => write!(f, "{message} at line {n}"),
                None => write!(f, "{message}"),
            },
            PipelineError::Extract(m) => write!(f, "extract failed: {m}"),
            PipelineError::Chunk(m) => write!(f, "chunk failed: {m}"),
            PipelineError::Embed(m) => write!(f, "embed failed: {m}"),
            PipelineError::Io(m) => write!(f, "io failed: {m}"),
        }
    }
}

impl std::error::Error for PipelineError {}

impl From<std::io::Error> for PipelineError {
    fn from(e: std::io::Error) -> Self {
        PipelineError::io(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// the three strategy-axis traits (§5.10.2 signatures, v2.1)
// ---------------------------------------------------------------------------

/// `{bytes, mime} -> Vec<Part>`: turn one Source object's raw bytes into
/// extracted payloads.
///
/// Sync on purpose: extraction is a CPU segment; the ingest worker calls it
/// inside `spawn_blocking` (§5.10.5), and an async signature would only add
/// ceremony every kind would immediately wrap away.
pub trait Extractor: Send + Sync {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError>;

    /// Class-level AFFINITY declaration (M2.1 step 2): how well this kind
    /// serves a probed [`ContentClass`]. The ingest router consumes it to
    /// order an extract chain (first `Preferred`, else first `Capable`;
    /// all-`Refuses` fails the file with an aggregate reason instead of
    /// running anyone).
    ///
    /// * **Class-level, not instance-level**: the declaration is a property
    ///   of the KIND's core semantics — config knobs (e.g. pdf-inspector's
    ///   `markdown`) never change routing, which is why the signature takes
    ///   no `&self` state beyond the kind itself and returns a static,
    ///   stateless verdict.
    /// * **Default `Capable`** for every class: an implementation that does
    ///   not declare affinity can never `Refuse`, so under a known class the
    ///   router picks it exactly where config order would have — routing
    ///   degenerates to the pure M2.1 linear chain (backward compatible by
    ///   construction; contract suites and future kinds need no change).
    fn affinity(&self, class: ContentClass) -> Fit {
        // deliberately silence the unused-parameter lint IN the default (an
        // undeclared kind is Capable for every class); overriding kinds match
        // on the class.
        let _ = class;
        Fit::Capable
    }

    /// Container role (§5.10.3): unpack the bytes into child documents that
    /// RE-ENTER the user-configured extractor routing (the mime short key of
    /// each child resolves to a chain in the SAME ingest config), instead of
    /// this kind extracting them itself. A kind is either a LEAF
    /// ([`extract`](Extractor::extract)) or a CONTAINER (`unpack`) — never
    /// both; containers never digest children themselves (configuration
    /// coherence: the user's html-chain config applies to epub chapters
    /// automatically, which a self-digesting container would silently break).
    ///
    /// * **Default `Ok(None)` = leaf**: every existing kind keeps its exact
    ///   semantics with zero changes — the worker treats `None` as "run
    ///   `extract`", the pre-container path byte-for-byte.
    /// * **`Ok(Some(children))` = container**: the worker routes each child
    ///   by ITS mime through the user's chains (full probe → affinity →
    ///   yield semantics per child) and aggregates the winning Parts in
    ///   child order; bomb guards (nesting depth, child count, per-child
    ///   size) are mechanism constants in the worker, not kind policy.
    /// * **mime gate semantics carry over**: a container reached with a mime
    ///   that is not its own ⇒ `Err` — the member yields and the chain falls
    ///   to the next member, exactly like a leaf's extract-time refusal.
    /// * **Determinism**: children order is the kind's return order (e.g.
    ///   spine order for epub); the worker walks it sequentially, no
    ///   concurrency (§5.10.5).
    fn unpack(&self, _bytes: &[u8], _mime: &str) -> Result<Option<Vec<ChildDoc>>, PipelineError> {
        // deliberately silence the unused-parameter lint IN the default (a
        // kind that does not override unpack is a leaf); overriding kinds
        // parse the bytes.
        let _ = (_bytes, _mime);
        Ok(None)
    }
}

/// `&[Part] -> Vec<Chunk>`: split extracted payloads into embeddable chunks.
///
/// The CHUNKER AXIS serves text only (§5.10.2): the axis dispatch
/// ([`crate::pipeline::chunk::ChunkerKind::chunk`]) performs the modality
/// split in the mechanism layer — image parts never reach a kind, they
/// become atomic chunks there — so implementations see text parts.
/// Implementations still skip non-Text parts defensively: a routing bug
/// must not corrupt chunk text.
pub trait Chunker: Send + Sync {
    fn chunk(&self, parts: &[Part]) -> Result<Vec<Chunk>, PipelineError>;
}

/// Content -> vector engine, both ingestion and query sides. Declares its
/// `dim` and the modalities it accepts; one configured instance maps 1:1 to
/// one derived index (§5.10.1).
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    async fn embed_query(&self, q: &Query) -> Result<Vec<f32>, PipelineError>;
    async fn embed_documents(&self, chunks: &[Chunk]) -> Result<Vec<Vec<f32>>, PipelineError>;
    /// Declared output dimensionality (ort: probed at startup; openai:
    /// declared in config, never probed over the network).
    fn dim(&self) -> usize;
    /// Modalities this instance accepts on the DOCUMENT side (ingestion
    /// routing fans a Part out to every doc-modality-matching embedder).
    fn doc_modalities(&self) -> &'static [Modality];
    /// Modalities accepted on the QUERY side.
    fn query_modalities(&self) -> &'static [Modality];
}

/// One rerank input (M3-T11): the candidate's identity (a chunk key) plus
/// the TEXT the cross-encoder reads — `/x-vf/Search` feeds the stored
/// `x-vf-chunk-text`. An empty `text` (image chunks store none; buckets
/// with `store_chunk_text = false`) is LEGAL and scores however the model
/// scores an empty second member — deterministic and total; the hit is
/// never dropped (rerank re-orders, it does not filter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerankCandidate {
    pub key: String,
    pub text: String,
}

impl RerankCandidate {
    pub fn new(key: impl Into<String>, text: impl Into<String>) -> Self {
        RerankCandidate {
            key: key.into(),
            text: text.into(),
        }
    }
}

/// The FOURTH axis (§5.10.2 Reranker row, query side, M3-T11):
/// `query × candidates -> scored`. Implementations return one [`Hit`] per
/// input candidate — same key set, never fewer (the output of rerank is an
/// ORDER, not a filter) — sorted score descending with ties broken by key
/// ascending, such that re-calling with the candidates in any permutation
/// yields the identical output (the contract suite is
/// [`contract::reranker_contract`]; the built-in kind achieves it by
/// key-sorting before batch composition — see `rerank/cross_encoder.rs`).
#[async_trait::async_trait]
pub trait Reranker: Send + Sync {
    async fn rerank(
        &self,
        query: &str,
        candidates: &[RerankCandidate],
    ) -> Result<Vec<Hit>, PipelineError>;
}

// ---------------------------------------------------------------------------
// query-side merge (pure function; a query OPTION, not an axis)
// ---------------------------------------------------------------------------

/// One merged query result row. Minimal in M2 (key + fused score); T3's
/// hybrid search may add source/modality attribution additively.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub key: String,
    pub score: f64,
}

/// Per-embedder-instance result list fed to [`merge_hitlists`]. Callers
/// guarantee keys are unique WITHIN one list (one list = one index's
/// ranking); the same key MAY appear across lists — that is the point.
pub type HitList = Vec<Hit>;

/// Query-result merge strategy. Closed set like everything else in the
/// plugin system: M2 registered exactly `rrf`; M3-T8 adds `weighted` by the
/// planned variant + parse arm (`first` is still M3+ — a config saying
/// `merge = "first"` fails loudly instead of half-working).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// Reciprocal Rank Fusion.
    Rrf,
    /// Weighted rank fusion (M3-T8): [`merge_hitlists_weighted`] — RRF with a
    /// per-embedder-instance weight on each rank contribution. RANK-based by
    /// design: raw distances of different embedders are dimensionally
    /// incomparable (cosine of one model space vs another's), so the only
    /// cross-source currency the fusion mixes is the dimensionless rank.
    Weighted,
}

impl Merge {
    /// Parse the config string form (M3-T8: `rrf` | `weighted`).
    pub fn parse(s: &str) -> Result<Merge, String> {
        match s {
            "rrf" => Ok(Merge::Rrf),
            "weighted" => Ok(Merge::Weighted),
            other => Err(format!(
                "unknown merge strategy '{other}' (allowed: rrf, weighted)"
            )),
        }
    }
}

impl std::fmt::Display for Merge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Merge::Rrf => f.write_str("rrf"),
            Merge::Weighted => f.write_str("weighted"),
        }
    }
}

/// RRF smoothing constant, the literature-default k = 60 (Cormack et al.
/// 2009). Damps rank-1 dominance so one list cannot crown an unconfirmed
/// key; exact value is convention, not tuning.
pub const RRF_K: u64 = 60;

/// Fuse per-list rankings into one total order.
///
/// * **Single list**: passthrough identity — same rows, same order, same
///   scores. With nothing to fuse, RRF would only destroy score semantics
///   (1/(k+r) is comparable ACROSS lists, meaningless as an absolute
///   relevance), so the raw scores stay actionable for thresholds/rerankers.
/// * **Multiple lists**: classic RRF — each key accumulates
///   `1/(RRF_K + rank)` per list (rank 1 = best), output sorted by score
///   descending, ties broken by key ascending. The tie-break makes the
///   order a pure function of the input: deterministic and reproducible.
pub fn merge_hitlists(strategy: Merge, lists: &[HitList]) -> HitList {
    match strategy {
        // Weighted WITHOUT its weights is uniform (w=1) — numerically the
        // rrf fusion (1.0·x is bit-exact x in IEEE 754); the
        // configured-weights entry is [`merge_hitlists_weighted`]. The arm
        // exists to keep the closed-set match total.
        Merge::Rrf | Merge::Weighted => fuse_uniform(lists),
    }
}

/// Shared uniform-weight body of [`merge_hitlists`] (see that function's
/// docs; single-list passthrough + rank fusion + key tie-break).
fn fuse_uniform(lists: &[HitList]) -> HitList {
    if lists.len() == 1 {
        return lists[0].clone();
    }
    let mut acc: BTreeMap<String, f64> = BTreeMap::new();
    for list in lists {
        for (rank0, hit) in list.iter().enumerate() {
            *acc.entry(hit.key.clone()).or_insert(0.0) += 1.0 / (RRF_K as f64 + rank0 as f64 + 1.0);
        }
    }
    let mut merged: HitList = acc
        .into_iter()
        .map(|(key, score)| Hit { key, score })
        .collect();
    // total_cmp: a total order over f64 (no NaN by construction —
    // sums of positive rationals), so sort is fully deterministic.
    merged.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
    merged
}

/// Weighted rank fusion (M3-T8): like RRF, but each list's contribution
/// `1/(RRF_K + rank)` is scaled by that list's WEIGHT — `w / (RRF_K + rank)`
/// — with weights keyed by list identity. The CALLER (ops_xvf Search) keys
/// lists by EMBEDDER INSTANCE name: one list is one embedder's derived index.
///
/// # Why ranks, not scores (the normalization ruling)
///
/// Different embedders produce distances in incomparable units — cosine
/// distance in one model's space vs another model's (or metric's) space.
/// Neither alternative offered a honest fusion:
/// * weighting RAW scores (`w · 1/(1+d)`) sums apples and oranges — a scalar
///   weight cannot repair a missing unit;
/// * min-max normalizing within each source manufactures a [0,1] band out of
///   whatever the CURRENT top-k happened to return (one outlier distance or
///   a different topK reshuffles every normalized score) — comparability by
///   accident, not by semantics.
///
/// Rank is the only cross-source quantity that needs no unit: every list
/// orders ITS OWN hits by ITS OWN metric, and `w_i` then expresses exactly
/// what the operator means — "trust embedder i's ranking more". With every
/// weight at 1.0 the fusion is bit-identical to rrf.
///
/// A list whose name has NO entry in `weights` weighs 1.0 (neutral): query
/// modality filtering means not every configured embedder co-occurs in every
/// query, so requiring a weight per embedder would over-configure. Weights
/// are validated positive+finite at config parse (config.rs); this pure
/// function does not second-guess its inputs.
///
/// Single list: passthrough identity — same rows, order, raw scores as rrf
/// (with nothing to weigh, the weights are meaningless and the raw scores
/// stay actionable). Ties break by key ascending; `total_cmp` keeps the
/// order a pure function of the input.
pub fn merge_hitlists_weighted(
    lists: &[(&str, &HitList)],
    weights: &BTreeMap<String, f64>,
) -> HitList {
    if lists.len() == 1 {
        return lists[0].1.clone();
    }
    let mut acc: BTreeMap<String, f64> = BTreeMap::new();
    for (name, list) in lists {
        let w = weights.get(*name).copied().unwrap_or(1.0);
        for (rank0, hit) in list.iter().enumerate() {
            *acc.entry(hit.key.clone()).or_insert(0.0) += w / (RRF_K as f64 + rank0 as f64 + 1.0);
        }
    }
    let mut merged: HitList = acc
        .into_iter()
        .map(|(key, score)| Hit { key, score })
        .collect();
    // no NaN by construction when weights are finite (config-validated);
    // total_cmp still gives a total order regardless
    merged.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
    merged
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(pairs: &[(&str, f64)]) -> HitList {
        pairs
            .iter()
            .map(|(k, s)| Hit {
                key: (*k).to_string(),
                score: *s,
            })
            .collect()
    }

    #[test]
    fn merge_single_list_is_identity() {
        let one = hits(&[("a", 0.9), ("b", 0.5), ("c", 0.1)]);
        let out = merge_hitlists(Merge::Rrf, &[one.clone()]);
        assert_eq!(out, one, "single list must keep order AND raw scores");
        // byte-identical scores, not just order
        assert_eq!(out[0].score.to_bits(), 0.9f64.to_bits());
    }

    #[test]
    fn merge_rrf_fuses_and_orders() {
        let l1 = hits(&[("a", 9.0), ("b", 8.0)]);
        let l2 = hits(&[("c", 7.0), ("a", 6.0)]);
        let out = merge_hitlists(Merge::Rrf, &[l1, l2]);
        // a = 1/61 + 1/62 beats b = 1/62 and c = 1/61
        assert_eq!(out[0].key, "a");
        assert_eq!(
            out.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
        let expected = 1.0 / 61.0 + 1.0 / 62.0;
        assert!((out[0].score - expected).abs() < 1e-15);
        // union preserved, no duplicates
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn merge_tie_breaks_by_key_deterministically() {
        // a is rank-1 of list1, b is rank-1 of list2: identical fused scores.
        let l1 = hits(&[("b", 1.0), ("x", 0.5)]);
        let l2 = hits(&[("a", 1.0), ("y", 0.5)]);
        let out = merge_hitlists(Merge::Rrf, &[l1.clone(), l2.clone()]);
        assert_eq!(out[0].score.to_bits(), out[1].score.to_bits(), "tie setup");
        assert!(out[0].key < out[1].key, "ties break by key ascending");
        // total order: non-increasing scores, equal-score runs key-sorted
        for w in out.windows(2) {
            let ok = w[0].score > w[1].score || (w[0].score == w[1].score && w[0].key < w[1].key);
            assert!(ok, "not totally ordered: {out:?}");
        }
        // determinism: same input, same output
        assert_eq!(merge_hitlists(Merge::Rrf, &[l1, l2]), out);
    }

    #[test]
    fn merge_edges() {
        assert!(merge_hitlists(Merge::Rrf, &[]).is_empty());
        assert!(merge_hitlists(Merge::Rrf, &[HitList::new(), HitList::new()]).is_empty());
        // same key ranked 1st in one list, 2nd in the other: contributions
        // accumulate
        let out = merge_hitlists(
            Merge::Rrf,
            &[hits(&[("k", 1.0)]), hits(&[("j", 0.9), ("k", 0.5)])],
        );
        assert_eq!(out.len(), 2);
        assert!((out[0].score - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-15);
        // single-list passthrough keeps even a duplicated key as-is
        // (keys unique per list is the CALLER contract; merge does not
        // second-guess one index's ranking)
        let dup = hits(&[("k", 1.0), ("k", 0.5)]);
        assert_eq!(merge_hitlists(Merge::Rrf, &[dup.clone()]), dup);
    }

    #[test]
    fn merge_parse_closed_set() {
        assert_eq!(Merge::parse("rrf"), Ok(Merge::Rrf));
        assert_eq!(Merge::parse("weighted"), Ok(Merge::Weighted));
        // 'first' is still M3+: closed set, loud refusal
        let err = Merge::parse("first").unwrap_err();
        assert!(err.contains("unknown merge strategy 'first'"), "{err}");
        assert!(err.contains("allowed: rrf, weighted"), "{err}");
        assert_eq!(Merge::Weighted.to_string(), "weighted");
    }

    // ---- weighted (M3-T8) ----

    fn w(weights: &[(&str, f64)]) -> BTreeMap<String, f64> {
        weights.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn weighted_uniform_weights_are_bit_identical_to_rrf() {
        // w=1 everywhere must reproduce rrf EXACTLY — same keys, same order,
        // byte-identical scores (the additive-redline property: a deployment
        // flipping merge=rrf -> weighted with equal weights sees no change).
        let l1 = hits(&[("a", 0.9), ("b", 0.5)]);
        let l2 = hits(&[("c", 0.8), ("a", 0.4), ("d", 0.1)]);
        let rrf = merge_hitlists(Merge::Rrf, &[l1.clone(), l2.clone()]);
        let wgt =
            merge_hitlists_weighted(&[("e1", &l1), ("e2", &l2)], &w(&[("e1", 1.0), ("e2", 1.0)]));
        assert_eq!(rrf.len(), wgt.len());
        for (r, g) in rrf.iter().zip(&wgt) {
            assert_eq!(r.key, g.key);
            assert_eq!(r.score.to_bits(), g.score.to_bits(), "w=1 must be rrf");
        }
        // unkeyed names default to 1.0 — same bit-identity
        let defaulted = merge_hitlists_weighted(&[("e1", &l1), ("e2", &l2)], &BTreeMap::new());
        for (r, g) in rrf.iter().zip(&defaulted) {
            assert_eq!(r.score.to_bits(), g.score.to_bits());
        }
    }

    #[test]
    fn weighted_scores_are_exact_weighted_rank_sums() {
        let l1 = hits(&[("a", 5.0), ("b", 4.0)]);
        let l2 = hits(&[("b", 3.0), ("a", 2.0)]);
        let out = merge_hitlists_weighted(
            &[("big", &l1), ("small", &l2)],
            &w(&[("big", 3.0), ("small", 1.0)]),
        );
        // a: 3/61 + 1/62;  b: 3/62 + 1/61 — a wins under these weights
        let a = 3.0 / 61.0 + 1.0 / 62.0;
        let b = 3.0 / 62.0 + 1.0 / 61.0;
        assert!((out[0].score - a).abs() < 1e-15);
        assert_eq!(out[0].key, "a");
        assert!((out[1].score - b).abs() < 1e-15);
        assert_eq!(out[1].key, "b");
        // same lists under rrf (uniform) have a = 1/61+1/62 = b -> tie,
        // key ascending puts a first with EQUAL scores: the weight is what
        // breaks the symmetry, not the tie-break
        let rrf = merge_hitlists(
            Merge::Rrf,
            &[
                hits(&[("a", 5.0), ("b", 4.0)]),
                hits(&[("b", 3.0), ("a", 2.0)]),
            ],
        );
        assert_eq!(rrf[0].score.to_bits(), rrf[1].score.to_bits());
    }

    #[test]
    fn weighted_weight_reorders_the_fusion() {
        // list a ranks x first, list b ranks y first (disjoint fillers so
        // only the top keys compete). With b weighted 10x, y wins; with a
        // weighted 10x, x wins — the weight is the only difference.
        let l1 = hits(&[("x", 1.0), ("qa", 0.5)]);
        let l2 = hits(&[("y", 1.0), ("qb", 0.5)]);
        let y_first =
            merge_hitlists_weighted(&[("a", &l1), ("b", &l2)], &w(&[("a", 1.0), ("b", 10.0)]));
        assert_eq!(y_first[0].key, "y");
        let x_first =
            merge_hitlists_weighted(&[("a", &l1), ("b", &l2)], &w(&[("a", 10.0), ("b", 1.0)]));
        assert_eq!(x_first[0].key, "x");
    }

    #[test]
    fn weighted_single_list_is_identity_and_ties_break_by_key() {
        // identity: raw scores survive, weight irrelevant
        let one = hits(&[("a", 0.9), ("b", 0.5)]);
        let out = merge_hitlists_weighted(&[("only", &one)], &w(&[("only", 7.0)]));
        assert_eq!(out, one);
        assert_eq!(out[0].score.to_bits(), 0.9f64.to_bits());
        // tie discipline: same key-shape, symmetric weights -> equal scores
        // -> key ascending
        let l1 = hits(&[("zz", 1.0)]);
        let l2 = hits(&[("aa", 1.0)]);
        let out =
            merge_hitlists_weighted(&[("e1", &l1), ("e2", &l2)], &w(&[("e1", 2.0), ("e2", 2.0)]));
        assert_eq!(out[0].score.to_bits(), out[1].score.to_bits());
        assert_eq!((out[0].key.as_str(), out[1].key.as_str()), ("aa", "zz"));
        // determinism: same input, same output
        let again = merge_hitlists_weighted(
            &[("e1", &hits(&[("zz", 1.0)])), ("e2", &hits(&[("aa", 1.0)]))],
            &w(&[("e1", 2.0), ("e2", 2.0)]),
        );
        assert_eq!(out, again);
    }

    #[test]
    fn weighted_edges() {
        assert!(merge_hitlists_weighted(&[], &BTreeMap::new()).is_empty());
        let empty: HitList = Vec::new();
        assert!(
            merge_hitlists_weighted(&[("a", &empty)], &BTreeMap::new()).is_empty(),
            "single EMPTY list is the single-list identity"
        );
        // union preserved across weighted lists, no duplicates
        let lk = hits(&[("k", 1.0)]);
        let lj = hits(&[("j", 1.0), ("k", 1.0)]);
        let out = merge_hitlists_weighted(&[("a", &lk), ("b", &lj)], &w(&[("a", 2.0), ("b", 0.5)]));
        let mut keys: Vec<&str> = out.iter().map(|h| h.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["j", "k"]);
    }

    #[test]
    fn content_class_and_fit_vocabulary() {
        // kebab-case Display (progress lines + statusReason quote these)
        let all = [
            (ContentClass::TextNative, "text-native"),
            (ContentClass::ScanDominant, "scan-dominant"),
            (ContentClass::MixedTextScan, "mixed-text-scan"),
            (ContentClass::NoText, "no-text"),
        ];
        for (c, s) in all {
            assert_eq!(c.to_string(), s);
        }
        // the fit vocabulary is three-level and closed (router relies on
        // exactly these three verdicts)
        assert_ne!(Fit::Preferred, Fit::Capable);
        assert_ne!(Fit::Capable, Fit::Refuses);
        assert_ne!(Fit::Preferred, Fit::Refuses);
        // the trait DEFAULT is Capable for every class — the backward-compat
        // degeneration (an undeclared kind never refuses, routing collapses
        // to config order); pinned again with a local implementation in the
        // ingest router tests.
        struct Undeclared;
        impl Extractor for Undeclared {
            fn extract(&self, _: &[u8], _: &str) -> Result<Vec<Part>, PipelineError> {
                unreachable!("affinity-only test double")
            }
        }
        for (c, _) in all {
            assert_eq!(Undeclared.affinity(c), Fit::Capable, "{c}");
        }
        // the unpack DEFAULT is the LEAF verdict (M2.1 step 3): a kind that
        // does not override unpack never produces children, so every
        // pre-container implementation keeps its exact semantics. The four
        // production kinds are pinned again at the ExtractorKind dispatch
        // (extract/mod.rs tests).
        assert_eq!(
            Undeclared.unpack(b"anything", "application/octet-stream"),
            Ok(None)
        );
    }

    #[test]
    fn pipeline_error_display_carries_line() {
        let e = PipelineError::config("[ingest.d]: bad thing", Some(14));
        assert_eq!(e.to_string(), "[ingest.d]: bad thing at line 14");
        let e = PipelineError::config("[ingest.d]: bad thing", None);
        assert_eq!(e.to_string(), "[ingest.d]: bad thing");
        assert!(
            PipelineError::extract("boom")
                .to_string()
                .starts_with("extract failed")
        );
    }

    // ---- M3-T1 image modality mechanism ground ----

    #[test]
    fn modality_display_and_remedy_vocabulary() {
        // the closed vocabulary's two views (Display feeds progress lines,
        // chunk keys and x-vf-modality metadata; remedy feeds the no-taker
        // gate) — same pinning style as content_class_and_fit_vocabulary
        let all = [(Modality::Text, "text"), (Modality::Image, "image")];
        for (m, s) in all {
            assert_eq!(m.to_string(), s);
        }
        for (m, _) in all {
            let r = m.remedy();
            assert!(!r.is_empty(), "{m}");
        }
        // the image remedy names the missing capability (an image-accepting
        // embedder), the text remedy explains the aggregate is built-in
        // unreachable — phrased BY the modality, never by a specific kind
        assert!(Modality::Image.remedy().contains("image documents"));
        assert!(Modality::Image.remedy().contains("embedder"));
        assert!(Modality::Text.remedy().contains("built-in embedder"));
    }

    #[test]
    fn part_constructors_pin_the_channel_pairing() {
        // text parts: the M2 shape byte-for-byte, image channel None
        let t = Part::text("hello");
        assert_eq!(t.modality, Modality::Text);
        assert_eq!(t.text, "hello");
        assert_eq!(t.image, None);
        // image parts: payload in the image channel, EMPTY text channel,
        // modality Image (the constructors keep the pair consistent)
        let i = Part::image("image/png", &b"\x89PNG\r\n\x1a\n"[..]);
        assert_eq!(i.modality, Modality::Image);
        assert_eq!(i.text, "");
        let payload = i.image.as_ref().expect("image part carries payload");
        assert_eq!(payload.mime, "image/png");
        assert_eq!(&payload.bytes[..], b"\x89PNG\r\n\x1a\n");
        // the pairing invariant the chunker bypass enforces loudly
        assert!(Part::text("x").image.is_none());
        assert!(Part::image("image/jpeg", Vec::<u8>::new()).image.is_some());
        // T12: both convenience constructors open the annotation channels
        // EMPTY (no structural annotation is the pre-T12 meaning; the
        // label-aware producers build the struct directly)
        assert!(Part::text("x").labels.is_empty());
        assert!(Part::text("x").pages.is_empty());
        assert_eq!(Part::text("x").furniture_removed, 0);
        assert!(Part::image("image/png", Vec::<u8>::new()).labels.is_empty());
        assert!(Part::image("image/png", Vec::<u8>::new()).pages.is_empty());
        assert_eq!(
            Part::image("image/png", Vec::<u8>::new()).furniture_removed,
            0
        );
        // T13: the orchestration channel opens None (a leaf source has no
        // container section; only the ingest walk stamps it)
        assert_eq!(Part::text("x").section, None);
        assert_eq!(Part::image("image/png", Vec::<u8>::new()).section, None);
    }

    #[test]
    fn chunk_modality_is_derived_from_the_payload_channel() {
        let text_window = Chunk {
            index: 0,
            text: "window".to_string(),
            image: None,
            kind: ChunkKind::Body,
            page: None,
            line: None,
            section: None,
        };
        let atomic = Chunk {
            index: 1,
            text: String::new(),
            image: Some(ImagePayload {
                bytes: bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
                mime: "image/png".to_string(),
            }),
            kind: ChunkKind::Body,
            page: None,
            line: None,
            section: None,
        };
        assert_eq!(text_window.modality(), Modality::Text);
        assert_eq!(atomic.modality(), Modality::Image);
        // the Display form is what chunk keys and metadata carry
        assert_eq!(atomic.modality().to_string(), "image");
    }

    #[test]
    fn chunk_kind_display_vocabulary_is_pinned() {
        // T12: ChunkKind is NOT a registry axis (no KIND_LABELS parity table
        // applies), but its Display is load-bearing downstream (the
        // `x-vf-chunk-kind` metadata value and the D-5 filter vocabulary
        // `body|toc|caption|legend`) — pinned here so a future variant must
        // land its string consciously. Same pinning style as
        // modality_display_and_remedy_vocabulary.
        let all = [
            (ChunkKind::Body, "body"),
            (ChunkKind::Toc, "toc"),
            (ChunkKind::Caption, "caption"),
            (ChunkKind::Legend, "legend"),
        ];
        for (k, s) in all {
            assert_eq!(k.to_string(), s);
        }
        // closed vocabulary: the four values are pairwise distinct (a match
        // on kind elsewhere may rely on exactly these discriminants)
        for (i, (a, _)) in all.iter().enumerate() {
            for (j, (b, _)) in all.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
        }
        // Display/parse round-trip: the x-vf-chunk-kind value grammar is
        // closed in BOTH directions (includeKinds validates through parse)
        for (k, s) in all {
            assert_eq!(ChunkKind::parse(s), Some(k), "{s}");
        }
        assert_eq!(ChunkKind::parse("header"), None);
        assert_eq!(ChunkKind::parse(""), None);
        // LabelSpan carries its kind by value (Copy) alongside the byte range
        let span = LabelSpan {
            range: 0..3,
            kind: ChunkKind::Toc,
        };
        assert_eq!(span.kind.to_string(), "toc");
        assert_eq!(span.range, 0..3);
    }

    #[test]
    fn query_image_variant_and_describe() {
        assert_eq!(
            Query::Text("q".into()).describe(),
            "a text query",
            "describe never leaks the text"
        );
        let q = Query::Image(vec![0x89, 0x50], "image/png".to_string());
        assert_eq!(q.describe(), "an image query (mime image/png)");
        // structural equality carries bytes + mime (the dispatch payload)
        assert_eq!(q, Query::Image(vec![0x89, 0x50], "image/png".into()));
        assert_ne!(q, Query::Image(vec![0x89, 0x50], "image/jpeg".into()));
    }

    #[test]
    fn image_payload_validate_sniffs_magic_and_mime() {
        // PNG magic + canonical png mime: passes
        ImagePayload {
            bytes: bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\nrest-not-decoded"),
            mime: "image/png".to_string(),
        }
        .validate()
        .expect("png magic + png mime");
        // JPEG magic (FF D8 FF) + canonical jpeg mime: passes
        ImagePayload {
            bytes: bytes::Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0xE0]),
            mime: "image/jpeg".to_string(),
        }
        .validate()
        .expect("jpeg magic + jpeg mime");
        // no image magic at all: loud, says "no known image-container magic"
        let e = ImagePayload {
            bytes: bytes::Bytes::from_static(b"not an image at all"),
            mime: "image/png".to_string(),
        }
        .validate()
        .unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("no known image-container magic"), "{msg}");
        assert!(msg.starts_with("chunk failed"), "{msg}");
        // magic present but the DECLARED mime disagrees (or is a
        // non-canonical alias): loud, names both values
        for wrong in ["image/jpeg", "image/jpg", "application/octet-stream"] {
            let e = ImagePayload {
                bytes: bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
                mime: wrong.to_string(),
            }
            .validate()
            .unwrap_err();
            let msg = e.to_string();
            assert!(msg.contains("sniff as image/png"), "{msg}");
            assert!(msg.contains(wrong), "{msg}");
        }
    }
}
