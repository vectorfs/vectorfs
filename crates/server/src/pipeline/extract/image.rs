//! `image` extractor kind (M3-T4): a picture FILE as a source — jpeg/png
//! bytes become ONE image [`Part`] carrying the ORIGINAL bytes verbatim.
//! This is the user-facing supply side of the image chain (§5.10.1): the
//! file layer can now claim `photo.jpg` the way it claims `doc.pdf`, and
//! the siglip2 embedder (M3-T2) turns it into a vector — closing
//! "图片文件 → embedding → 以图搜图" end to end. `pdf-pages` (M3-T3) feeds
//! the same embedder from the PDF side; this kind feeds it from image
//! files directly.
//!
//! # Layer ruling: validate-only decode, verbatim payload
//!
//! The kind DECODES the image (image crate, jpeg+png — already in tree for
//! the M3-T1 sniff) but ships the ORIGINAL bytes as the Part payload: no
//! resize, no re-encode, no pixel conversion. Preprocessing is the
//! EMBEDDER's layer (siglip2 owns resize/normalize; §5.10.2's axis split) —
//! an extractor that reshaped pixels would (a) steal the embedder's
//! preprocessing decision and (b) make Part bytes differ from source bytes,
//! breaking the "same bytes in, same parts out" determinism contract's
//! intuitive reading (a re-encode is a NEW image). The decode itself is
//! still performed — as VALIDATION: a truncated or corrupt container Errs
//! HERE, at the honest seam, instead of surfacing as a cryptic embedder
//! failure after the chunk was already committed to an index.
//!
//! # Declared mime: sniffed, canonical
//!
//! The Part's mime is the SNIFFED container's canonical mime
//! (`image::guess_format` → `to_mime_type`, the same source
//! [`crate::pipeline::ImagePayload::validate`] trusts), NOT the routing
//! mime parameter: the bytes are the truth, the canonical form is what the
//! chunker-bypass sniff guard demands, and a `.png` file mis-routed under
//! any other label still indexes as what it IS.
//!
//! # Guards (aligned with the T2 siglip2 decode budget)
//!
//! * [`DECODE_LIMIT`]: header-time width/height cap (`image::Limits`)
//!   before any pixel buffer exists — 4096 px per side, the SAME constant
//!   and form as the siglip2 embedder's decode budget (a bomb this
//!   pipeline would eventually feed that embedder must refuse at the FIRST
//!   layer it meets, and both layers refuse at the same line).
//! * No page/child caps: exactly one Part, always (the MAX_RENDER_PAGES /
//!   MAX_CHILDREN bomb shapes have no analog — the input size cap is the
//!   ingest worker's `max_file_size_mb` poison guard, upstream of extract).
//!
//! # Affinity: deliberately undeclared
//!
//! The [`ContentClass`] vocabulary is PDF-shaped (text layer vs scan) and
//! the classification PROBE covers the pdf family only — an image source
//! probes as Unknown and the router never consults affinity for it. The
//! kind therefore declares nothing (the trait default, `Capable` for every
//! class): reached only through its own `img` route or a `default` chain,
//! where config order is the whole truth.
//!
//! # Contract behavior
//!
//! Empty input → Err (uniform empty-source rule, gate.rs); non-`image/*`
//! mime → Err (family gate — `image/` prefix, the txt kind's `text/`
//! precedent: subtypes without a row of their own fall through here);
//! empty mime = unknown, tolerated; garbage / unsupported container (the
//! sniff knows more formats than jpeg/png — gif, bmp, webp …) / truncated
//! bytes / oversize dimensions → Err, never a panic; the single Part is
//! `Modality::Image` whose payload passes [`ImagePayload::validate`] by
//! construction (the declared mime IS the sniff).

use super::super::gate;
use super::{Extractor, Part, PipelineError};

/// Decode-budget guard: width/height cap before any pixel buffer is
/// materialized (`image` Limits, checked at header time) — byte-for-byte
/// the siglip2 embedder's [`DECODE_LIMIT`] form (embed/siglip2.rs): the
/// same pipeline feeds both layers, so both refuse at the same line.
const DECODE_LIMIT: u32 = 4096;

/// Runtime instance (configless — the kind has no knobs; preprocessing
/// parameters belong to the embedder, §5.10.8's minimal-surface spirit).
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageExtractor;

impl Extractor for ImageExtractor {
    fn extract(&self, bytes: &[u8], mime: &str) -> Result<Vec<Part>, PipelineError> {
        // the shared input gates (byte-identical messages across kinds —
        // see pipeline/gate.rs)
        gate::refuse_empty_input("image", bytes)?;
        // FAMILY gate, the txt precedent: `image/*` subtypes without a row
        // of their own (image/webp, …) route here and die honestly at the
        // container check below, on the bytes
        gate::refuse_foreign_mime("image", mime, gate::Accept::Family("image/"))?;
        // sniff FIRST (the ImagePayload::validate source of truth): the
        // declared mime of the Part is the bytes' canonical container mime
        let format = image::guess_format(bytes).map_err(|_| {
            PipelineError::extract(format!(
                "image kind: these {} bytes carry no known image-container magic (jpeg/png \
                 supported) — not an image file",
                bytes.len()
            ))
        })?;
        if !matches!(format, image::ImageFormat::Jpeg | image::ImageFormat::Png) {
            return Err(PipelineError::extract(format!(
                "image kind: the bytes sniff as {format:?}, which this pipeline does not index \
                 (jpeg/png supported); the image crate recognizing a format and the file layer \
                 carrying it are separate decisions"
            )));
        }
        // validate-only decode under the bomb budget: truncated/corrupt
        // containers and oversize dimensions Err HERE (module docs), the
        // decoded pixels themselves are dropped — the payload stays verbatim
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(DECODE_LIMIT);
        limits.max_image_height = Some(DECODE_LIMIT);
        let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| PipelineError::extract(format!("image kind: reading bytes: {e}")))?;
        reader.limits(limits);
        reader.decode().map_err(|e| {
            PipelineError::extract(format!(
                "image kind: decoding the image failed (corrupt/truncated container, or a \
                     dimension over the {DECODE_LIMIT}px decode budget): {e}"
            ))
        })?;
        Ok(vec![Part::image(
            format.to_mime_type(),
            bytes::Bytes::copy_from_slice(bytes),
        )])
    }
}

// ---------------------------------------------------------------------------
// config (no knobs — the tag is the whole section)
// ---------------------------------------------------------------------------

/// Per-kind config struct (`deny_unknown_fields`): the typed `kind` tag
/// doubles as the allowed-field list in unknown-key errors and as the
/// fingerprint payload (canonical form `kind = "image"`, knob-free).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageConfig {
    pub kind: ImageTag,
}

/// Literal tag: only deserializes from `"image"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ImageTag {
    #[serde(rename = "image")]
    Image,
}

// ---------------------------------------------------------------------------
// tests — in-test encoders (the pdf-pages suite's discipline: no committed
// image bytes; solid colors keep every assertion exact)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pipeline::Modality;

    /// A solid-color PNG at `size` (the siglip2 suite's builder, shared
    /// shape — PNG because the committed fixtures and pdf-pages both speak
    /// it natively).
    pub(crate) fn solid_png(size: (u32, u32), rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(size.0, size.1, image::Rgb(rgb));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    /// The jpeg twin of [`solid_png`] — exercises the second container
    /// through the sniff AND the decoder.
    pub(crate) fn solid_jpeg(size: (u32, u32), rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(size.0, size.1, image::Rgb(rgb));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        buf.into_inner()
    }

    /// A leaked `&'static` PNG for case tables that demand 'static bytes
    /// (the mod.rs registry sweep; the models.rs `Box::leak` precedent).
    pub(crate) fn static_png() -> &'static [u8] {
        Box::leak(solid_png((8, 8), [10, 20, 30]).into_boxed_slice())
    }

    #[test]
    fn png_and_jpeg_become_one_verbatim_image_part() {
        for (bytes, mime) in [
            (solid_png((16, 12), [200, 30, 90]), "image/png"),
            (solid_jpeg((16, 12), [200, 30, 90]), "image/jpeg"),
        ] {
            let parts = ImageExtractor.extract(&bytes, mime).unwrap();
            assert_eq!(parts.len(), 1, "one Part, the whole file");
            assert_eq!(parts[0].modality, Modality::Image);
            assert_eq!(parts[0].text, "", "image parts carry no text");
            let payload = parts[0].image.as_ref().expect("payload present");
            assert_eq!(payload.mime, mime, "declared mime is the sniffed canonical");
            assert_eq!(&payload.bytes[..], &bytes[..], "payload is VERBATIM");
            payload.validate().expect("sniff-honest by construction");
        }
        // determinism: same bytes in, same parts out (trivial for a
        // passthrough, pinned anyway — the contract's first assertion)
        let png = solid_png((8, 8), [1, 2, 3]);
        assert_eq!(
            ImageExtractor.extract(&png, "image/png").unwrap(),
            ImageExtractor.extract(&png, "image/png").unwrap()
        );
    }

    #[test]
    fn declared_mime_comes_from_the_sniff_not_the_route() {
        // a .png routed under a foreign-but-family label (the mislabeled
        // upload shape): the Part still declares what the bytes ARE
        let png = solid_png((8, 8), [9, 9, 9]);
        let parts = ImageExtractor.extract(&png, "image/webp").unwrap();
        assert_eq!(parts[0].image.as_ref().unwrap().mime, "image/png");
    }

    #[test]
    fn empty_wrong_mime_garbage_and_foreign_container_are_errors() {
        // empty input
        let err = ImageExtractor.extract(b"", "image/png").unwrap_err();
        assert!(err.to_string().contains("empty input"), "{err}");
        // wrong mime (non-family)
        let png = solid_png((4, 4), [0, 0, 0]);
        let err = ImageExtractor.extract(&png, "application/pdf").unwrap_err();
        assert!(err.to_string().contains("cannot handle mime"), "{err}");
        // empty mime = unknown, tolerated
        assert!(ImageExtractor.extract(&png, "").is_ok());
        // garbage with no image magic
        let err = ImageExtractor
            .extract(b"definitely not an image", "image/png")
            .unwrap_err();
        assert!(
            err.to_string().contains("no known image-container magic"),
            "{err}"
        );
        // a container the sniff KNOWS but the pipeline does not index
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
        let err = ImageExtractor.extract(&gif, "image/gif").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Gif"), "{msg}");
        assert!(msg.contains("jpeg/png supported"), "{msg}");
        // truncated png (byte-half): the decoder must Err, never panic
        let big = solid_png((64, 64), [5, 5, 5]);
        let err = ImageExtractor
            .extract(&big[..big.len() / 2], "image/png")
            .unwrap_err();
        assert!(
            err.to_string().contains("decoding the image failed"),
            "{err}"
        );
    }

    #[test]
    fn decode_budget_refuses_oversize_dimensions() {
        // 5000×1 is over the 4096 decode limit: refused at HEADER time
        // (the limits guard), before any pixel buffer is materialized —
        // the siglip2 suite's twin assertion
        let wide = solid_png((5000, 1), [1, 2, 3]);
        let err = ImageExtractor.extract(&wide, "image/png").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("decoding the image failed"), "{msg}");
        assert!(
            msg.contains("exceeds limit"),
            "the LIMITS refusal, not truncation: {msg}"
        );
        assert!(msg.contains("4096"), "{msg}");
    }

    #[test]
    fn affinity_is_the_undeclared_default() {
        // no declaration (module docs): the dispatch spells the trait
        // default out — Capable for EVERY class, so the kind can never be
        // all-refused out of a chain it was explicitly configured into
        use crate::pipeline::{ContentClass, Fit};
        for class in [
            ContentClass::TextNative,
            ContentClass::ScanDominant,
            ContentClass::MixedTextScan,
            ContentClass::NoText,
        ] {
            assert_eq!(Extractor::affinity(&ImageExtractor, class), Fit::Capable);
        }
    }

    #[test]
    fn config_roundtrip() {
        let cfg: ImageConfig = toml::from_str("kind = \"image\"").unwrap();
        assert_eq!(cfg.kind, ImageTag::Image);
        assert_eq!(toml::to_string(&cfg).unwrap().trim(), "kind = \"image\"");
        // wrong literal rejected (stage-2 tag guards stage-1 dispatch);
        // unknown field rejected (no knobs)
        assert!(toml::from_str::<ImageConfig>("kind = \"txt\"").is_err());
        assert!(toml::from_str::<ImageConfig>("kind = \"image\"\ndpi = 72").is_err());
    }
}
