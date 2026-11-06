//! rec (CRNN/SVTR CTC) side of the OCR pair — crop rectification, line
//! preprocessing, dictionary handling and CTC greedy decoding (M3-T10).
//!
//! All parameters are the PINNED artifacts' facts, not policy:
//! * the dictionary lives INSIDE the rec repo's `inference.yml`
//!   (`PostProcess.character_dict`, 18,383 entries, verified 2026-11-07) —
//!   the file is prefetched as `dict.yml` and parsed by a hand-rolled
//!   minimal reader (no YAML dependency: the file is a flat `name` +
//!   list-of-scalars block; see [`parse_dict`]);
//! * line preprocessing is `RecResizeImg image_shape [3, 48, 320]`:
//!   height 48, natural aspect width, zero-padded right up to at least 320,
//!   capped at 3,200 (the graph's declared dynamic-width envelope, same
//!   `inference.yml`), `(v/255 − 0.5)/0.5` in **BGR** order (the
//!   oar-ocr/Apache-2.0 constant preprocessing for CRNN-family rec);
//! * decoding is PaddleOCR `CTCLabelDecode`: blank = class 0, dict entries
//!   follow, `use_space_char` appends one ASCII space (confirmed at load
//!   against the probed class count).

use image::RgbImage;

use super::db::{Pt, Quad};
use crate::pipeline::PipelineError;

/// rec input height (`image_shape[1]`).
pub(super) const REC_HEIGHT: u32 = 48;
/// rec line width floor — `image_shape[2]` (320): lines pad up to this,
/// matching the classic PaddleOCR rec batching width.
pub(super) const REC_MIN_WIDTH: u32 = 320;
/// rec line width ceiling — the graph's declared max dynamic width (3,200);
/// wider crops resize down to it (aspect distortion on absurd lines only).
pub(super) const REC_MAX_WIDTH: u32 = 3200;
/// rec score gate: lines below this mean confidence are dropped
/// (PaddleOCR `drop_score = 0.5`).
pub(super) const DROP_SCORE: f32 = 0.5;

// ---------------------------------------------------------------------------
// dictionary
// ---------------------------------------------------------------------------

/// Parse the `character_dict` list out of the rec artifact's
/// `inference.yml`. The file is machine-generated YAML of the shape
/// `PostProcess: / name: CTCLabelDecode / character_dict: / - entry …`
/// where every entry is one grapheme line; YAML single-quotes exactly the
/// entries that would otherwise parse as flow syntax (`'"'`, `'-'`,
/// `''''`… — 31 of them in the pinned file, verified 2026-11-07). This
/// reader handles that subset and refuses anything else loudly — a
/// format change upstream fails at load, never silently mis-decodes.
pub(super) fn parse_dict(yml: &str) -> Result<Vec<String>, PipelineError> {
    let mut entries = Vec::new();
    let mut in_dict = false;
    for line in yml.lines() {
        if !in_dict {
            if line.trim_end() == "  character_dict:" {
                in_dict = true;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("  - ") {
            entries.push(unquote_yaml_scalar(rest));
        } else if line.starts_with(' ') || line.trim().is_empty() {
            // indented continuation / blank — the pinned file has none
            // after the list starts, but tolerate blanks
            continue;
        } else {
            break; // next top-level key: the list is over
        }
    }
    if entries.is_empty() {
        return Err(PipelineError::extract(
            "dict.yml carries no character_dict entries — the artifact format changed upstream; \
             re-pin after review",
        ));
    }
    Ok(entries)
}

/// Strip one layer of YAML single quotes (`'a'` → `a`, `''` → `'` inside).
fn unquote_yaml_scalar(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].replace("''", "'")
    } else {
        s.to_string()
    }
}

/// The class table: index 0 = CTC blank, 1..=n the dict entries, and — when
/// the probed class count says so — one trailing ASCII space
/// (`use_space_char`). Built once at model load; the builder ADAPTS the
/// trailing space to the graph (declared classes = dict + 2 with the space,
/// dict + 1 without) and errors on any other count — a mismatched dict/model
/// pair would silently decode garbage otherwise.
pub(super) fn class_table(
    dict: &[String],
    declared_classes: usize,
) -> Result<Vec<String>, PipelineError> {
    let with_space = dict.len() + 2;
    let without_space = dict.len() + 1;
    let mut table: Vec<String> = std::iter::once(String::new())
        .chain(dict.iter().cloned())
        .collect();
    match declared_classes {
        n if n == with_space => table.push(" ".to_string()),
        n if n == without_space => {}
        n => {
            return Err(PipelineError::extract(format!(
                "the rec graph declares {n} output classes but its dictionary carries {} entries \
                 (+ blank{} = expected {}) — a mismatched det/rec/dict pair; re-download the model",
                dict.len(),
                if n > without_space { " + space" } else { "" },
                with_space
            )));
        }
    }
    Ok(table)
}

// ---------------------------------------------------------------------------
// crop rectification (PaddleOCR get_rotate_crop_image)
// ---------------------------------------------------------------------------

/// Crop one det quad out of the page, perspective-rectified to its
/// axis-aligned rectangle (PaddleOCR's `get_rotate_crop_image`: output
/// width = max of the two horizontal edges, height = max of the vertical
/// ones, sampled through the unit-square→quad homography; a TALL crop
/// (height ≥ 1.5 × width) rotates 270° — vertical text reads top-down).
/// Reference: oar-ocr's `utils/transform.rs` (Apache-2.0), reimplemented
/// against `image` + a hand-solved 8×8 system (no nalgebra).
pub(super) fn crop_quad(img: &RgbImage, q: &Quad) -> Option<RgbImage> {
    let dist = |a: Pt, b: Pt| (a.x - b.x).hypot(a.y - b.y);
    let w = dist(q[0], q[1]).max(dist(q[3], q[2])).round();
    let h = dist(q[0], q[3]).max(dist(q[1], q[2])).round();
    if w < 1.0 || h < 1.0 {
        return None; // degenerate quad (all corners within one pixel)
    }
    let (w, h) = (w as u32, h as u32);
    if w > img.width() * 4 || h > img.height() * 4 {
        return None; // absurd (post-offset blowup) — refuse, never OOM
    }
    // EXACT axis-aligned integer rectangles (the common DB output for
    // upright text) copy verbatim — no homography solve, no resampling
    // blur at the half-pixel centers (the oar-ocr fast-path discipline)
    let integerish = |v: f32| (v - v.round()).abs() <= 0.01;
    if integerish(q[0].x)
        && integerish(q[0].y)
        && integerish(q[1].x)
        && integerish(q[1].y)
        && integerish(q[2].x)
        && integerish(q[2].y)
        && integerish(q[3].x)
        && integerish(q[3].y)
    {
        let x0 = q.iter().map(|p| p.x).fold(f32::INFINITY, f32::min).round() as i64;
        let y0 = q.iter().map(|p| p.y).fold(f32::INFINITY, f32::min).round() as i64;
        let x1 = q
            .iter()
            .map(|p| p.x)
            .fold(f32::NEG_INFINITY, f32::max)
            .round() as i64;
        let y1 = q
            .iter()
            .map(|p| p.y)
            .fold(f32::NEG_INFINITY, f32::max)
            .round() as i64;
        if x1 - x0 == w as i64
            && y1 - y0 == h as i64
            && x0 >= 0
            && y0 >= 0
            && (x1 as u32) <= img.width()
            && (y1 as u32) <= img.height()
        {
            let exact = image::imageops::crop_imm(img, x0 as u32, y0 as u32, w, h).to_image();
            return Some(orient_vertical(exact));
        }
    }
    // homography mapping the output rect (w×h) onto the quad: for each
    // output pixel, src = H·(x, y, 1) — solved directly for the 4
    // corner correspondences (h22 = 1), Gaussian elimination on 8×8.
    let dst = [
        Pt::new(0.0, 0.0),
        Pt::new(w as f32, 0.0),
        Pt::new(w as f32, h as f32),
        Pt::new(0.0, h as f32),
    ];
    let hh = solve_homography(&dst, q)?;
    let mut out = image::RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = hh.apply(x as f32 + 0.5, y as f32 + 0.5);
            out.put_pixel(x, y, sample_bilinear(img, sx, sy));
        }
    }
    Some(orient_vertical(out))
}

/// Vertical text rotates 270° so it reads top-down (PaddleOCR's
/// `height ≥ 1.5 × width` rule).
fn orient_vertical(img: RgbImage) -> RgbImage {
    if img.height() >= img.width() * 3 / 2 {
        image::imageops::rotate270(&img)
    } else {
        img
    }
}

/// 3×3 homography (h22 fixed at 1) mapping `from` → `to` point sets.
struct Homography([f32; 8]);

impl Homography {
    fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let m = &self.0;
        let d = m[6] * x + m[7] * y + 1.0;
        if d.abs() < 1e-12 {
            return (f32::NAN, f32::NAN);
        }
        (
            (m[0] * x + m[1] * y + m[2]) / d,
            (m[3] * x + m[4] * y + m[5]) / d,
        )
    }
}

/// Solve H with H·from[i] ~ to[i] for 4 correspondences (8 unknowns,
/// h22 = 1): rows [fx, fy, 1, 0, 0, 0, −fx·tx, −fy·tx | tx] and the y twin,
/// then Gaussian elimination with partial pivoting.
fn solve_homography(from: &[Pt; 4], to: &[Pt; 4]) -> Option<Homography> {
    let mut a = [[0f32; 8]; 8];
    let mut b = [0f32; 8];
    for i in 0..4 {
        let (fx, fy) = (from[i].x, from[i].y);
        let (tx, ty) = (to[i].x, to[i].y);
        a[i * 2] = [fx, fy, 1.0, 0.0, 0.0, 0.0, -fx * tx, -fy * tx];
        b[i * 2] = tx;
        a[i * 2 + 1] = [0.0, 0.0, 0.0, fx, fy, 1.0, -fx * ty, -fy * ty];
        b[i * 2 + 1] = ty;
    }
    // elimination
    for col in 0..8 {
        let piv = (col..8).max_by(|&r1, &r2| a[r1][col].abs().total_cmp(&a[r2][col].abs()))?;
        if a[piv][col].abs() < 1e-10 {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);
        for row in (col + 1)..8 {
            let f = a[row][col] / a[col][col];
            // copy the pivot row out first: row != col, but the borrow
            // checker cannot see that through two index expressions
            let pivot = a[col];
            for (arc, acc) in a[row][col..].iter_mut().zip(pivot[col..].iter()) {
                *arc -= f * acc;
            }
            b[row] -= f * b[col];
        }
    }
    // back-substitution
    let mut x = [0f32; 8];
    for row in (0..8).rev() {
        let mut acc = b[row];
        for c in (row + 1)..8 {
            acc -= a[row][c] * x[c];
        }
        x[row] = acc / a[row][row];
        if !x[row].is_finite() {
            return None;
        }
    }
    Some(Homography(x))
}

/// Bilinear sample with nearest-edge clamping (off-image corners of the
/// unclipped quad clamp to the border instead of ringing).
fn sample_bilinear(img: &RgbImage, x: f32, y: f32) -> image::Rgb<u8> {
    if !x.is_finite() || !y.is_finite() {
        return image::Rgb([0, 0, 0]);
    }
    let (w, h) = (img.width() as f32, img.height() as f32);
    let x = x.clamp(0.0, w - 1.0);
    let y = y.clamp(0.0, h - 1.0);
    let (x0, y0) = (x.floor() as u32, y.floor() as u32);
    let (x1, y1) = (
        (x0 + 1).min(img.width() - 1),
        (y0 + 1).min(img.height() - 1),
    );
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let tl = img.get_pixel(x0, y0).0;
    let tr = img.get_pixel(x1, y0).0;
    let bl = img.get_pixel(x0, y1).0;
    let br = img.get_pixel(x1, y1).0;
    let mut out = [0u8; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let top = tl[c] as f32 * (1.0 - fx) + tr[c] as f32 * fx;
        let bot = bl[c] as f32 * (1.0 - fx) + br[c] as f32 * fx;
        *o = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
    }
    image::Rgb(out)
}

// ---------------------------------------------------------------------------
// line preprocessing + CTC decoding
// ---------------------------------------------------------------------------

/// Resize one crop to height 48 at natural aspect, zero-pad the right up to
/// [`REC_MIN_WIDTH`], cap at [`REC_MAX_WIDTH`], normalize
/// `(v / 255 − 0.5) / 0.5` in BGR, CHW. Returns (tensor, tensor_width).
/// Pure: the unit-test target.
pub(super) fn rec_preprocess(crop: &RgbImage) -> (Vec<f32>, u32) {
    let (w, h) = crop.dimensions();
    let natural = ((REC_HEIGHT as f32 * w as f32 / h.max(1) as f32).ceil() as u32).max(1);
    let out_w = natural.clamp(REC_MIN_WIDTH, REC_MAX_WIDTH);
    let resized_w = natural.min(out_w);
    let resized = image::imageops::resize(
        crop,
        resized_w,
        REC_HEIGHT,
        image::imageops::FilterType::Triangle,
    );
    let plane = (out_w * REC_HEIGHT) as usize;
    let mut out = vec![-1f32; 3 * plane]; // zero pixels normalize to −1
    for (i, px) in resized.pixels().enumerate() {
        for c in 0..3 {
            // plane 0 = B (source index 2), 1 = G, 2 = R
            out[c * plane + i] = (px.0[2 - c] as f32 / 255.0 - 0.5) / 0.5;
        }
    }
    (out, out_w)
}

/// CTC greedy decode (PaddleOCR `CTCLabelDecode`): per-timestep argmax,
/// collapse consecutive duplicates, drop blanks (class 0), map through the
/// class table; confidence = mean argmax value over the KEPT timesteps.
/// `logits` is `[timesteps][classes]` row-major (the rec session output).
/// Pure: the unit-test target.
pub(super) fn ctc_greedy_decode(
    logits: &[f32],
    timesteps: usize,
    classes: usize,
    table: &[String],
) -> (String, f32) {
    let argmax = |t: usize| -> (usize, f32) {
        let row = &logits[t * classes..(t + 1) * classes];
        let (mut bi, mut bv) = (0usize, f32::NEG_INFINITY);
        for (i, &v) in row.iter().enumerate() {
            if v > bv {
                (bi, bv) = (i, v);
            }
        }
        (bi, bv)
    };
    let mut text = String::new();
    let mut conf_sum = 0f32;
    let mut conf_n = 0usize;
    let mut prev = usize::MAX;
    for t in 0..timesteps {
        let (idx, val) = argmax(t);
        if idx != 0 && idx != prev && idx < table.len() {
            text.push_str(&table[idx]);
            conf_sum += val;
            conf_n += 1;
        }
        prev = idx;
    }
    let conf = if conf_n == 0 {
        0.0
    } else {
        conf_sum / conf_n as f32
    };
    (text, conf)
}

// ---------------------------------------------------------------------------
// tests — synthetic yml / logits / images with hand-computed expectations
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The pinned dict's quoting shapes, miniature.
    const DICT_YML: &str = "Global:\n  model_name: fixture\nPreProcess:\n  transform_ops:\n  - \
                            RecResizeImg:\n      image_shape:\n      - 3\n      - 48\n      - 320\n\
                            PostProcess:\n  name: CTCLabelDecode\n  character_dict:\n  - 'A'\n  - \
                            'B'\n  - '-'\n  - '\"'\n  - ''''\n  - 中\n  - 🇨🇳\n";

    #[test]
    fn dict_parses_with_yaml_quoting_handled() {
        let dict = parse_dict(DICT_YML).unwrap();
        assert_eq!(dict, ["A", "B", "-", "\"", "'", "中", "\u{1f1e8}\u{1f1f3}"]);
        // class table with the space (dict + blank + space = 9 classes)
        let table = class_table(&dict, 9).unwrap();
        assert_eq!(table.len(), 9);
        assert_eq!(table[0], "", "blank at class 0");
        assert_eq!(table[1], "A");
        assert_eq!(table[8], " ", "appended ASCII space");
        // without the space (dict + blank = 8)
        let table = class_table(&dict, 8).unwrap();
        assert_eq!(table.len(), 8);
        assert_eq!(table.last().unwrap(), "\u{1f1e8}\u{1f1f3}");
        // any other class count is a load-time error naming both numbers
        let err = class_table(&dict, 12).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("12 output classes"), "{msg}");
        assert!(msg.contains("7 entries"), "{msg}");
        // empty dict refused
        assert!(parse_dict("Global: {}").is_err());
    }

    #[test]
    fn ctc_decode_collapses_repeats_and_blanks() {
        // classes: 0 blank, 1 'A', 2 'B', 3 'C'
        let table = vec![
            "".to_string(),
            "A".to_string(),
            "B".to_string(),
            "C".to_string(),
        ];
        let mk = |idx: &[usize]| -> (Vec<f32>, usize) {
            let mut v = vec![0f32; idx.len() * 4];
            for (t, &c) in idx.iter().enumerate() {
                v[t * 4 + c] = 1.0;
            }
            (v, idx.len())
        };
        // A A blank B B → "AB" (repeats collapse, blank separates nothing here)
        let (logits, t) = mk(&[1, 1, 0, 2, 2]);
        assert_eq!(ctc_greedy_decode(&logits, t, 4, &table).0, "AB");
        // blank-separated identical classes survive: A blank A → "AA"
        let (logits, t) = mk(&[1, 0, 1]);
        assert_eq!(ctc_greedy_decode(&logits, t, 4, &table).0, "AA");
        // all blank → empty, confidence 0
        let (logits, t) = mk(&[0, 0, 0]);
        let (text, conf) = ctc_greedy_decode(&logits, t, 4, &table);
        assert_eq!(text, "");
        assert_eq!(conf, 0.0);
        // confidence = mean over KEPT timesteps only
        let (logits, t) = mk(&[1, 0, 2]);
        let (_, conf) = ctc_greedy_decode(&logits, t, 4, &table);
        assert!((conf - 1.0).abs() < 1e-6);
        // out-of-table classes (a 6-class graph against this 4-entry
        // table) decode as nothing: class 5 is dropped, class 2 reads "B"
        let mut wide = vec![0f32; 2 * 6];
        wide[5] = 1.0;
        wide[6 + 2] = 1.0;
        assert_eq!(ctc_greedy_decode(&wide, 2, 6, &table).0, "B");
    }

    #[test]
    fn rec_preprocess_resizes_pads_and_normalizes_bgr() {
        // a 80×16 solid-color crop: 48/16 = 3× → 240×48 natural width < 320
        // → padded to 320 with zero columns (normalized −1)
        let crop = image::RgbImage::from_pixel(80, 16, image::Rgb([255, 128, 0]));
        let (t, w) = rec_preprocess(&crop);
        assert_eq!(w, 320);
        assert_eq!(t.len(), 3 * 48 * 320);
        let plane = (48 * 320) as usize;
        // B plane (source idx 2) = 0 → (0−0.5)/0.5 = −1
        assert!((t[0] - (-1.0)).abs() < 1e-6, "B=0 channel value: {}", t[0]);
        // G = 128/255 → (0.50196−0.5)/0.5 ≈ 0.0039
        assert!((t[plane] - ((128.0 / 255.0 - 0.5) / 0.5)).abs() < 1e-4);
        // R = 255 → +1; the plane index math proven by the LAST pixel of
        // the resized area (row 47, col 239)
        let last_resized = 2 * plane + (48 * 240 - 1);
        assert!((t[last_resized] - 1.0).abs() < 1e-6, "R=255 → +1");
        // the pad region (col ≥ 240) is −1 in every plane
        assert_eq!(t[2 * plane + 48 * 320 - 1], -1.0);
        assert_eq!(t[plane + 48 * 320 - 1], -1.0, "G plane pad");
        // a WIDE crop: 2000×40 → 2400×48 natural, no pad, no cap
        let wide = image::RgbImage::from_pixel(2000, 40, image::Rgb([0, 0, 0]));
        let (_, w2) = rec_preprocess(&wide);
        assert_eq!(w2, 2400);
        // an ABSURD crop: 20000×20 → natural 48000 → capped at 3200
        let absurd = image::RgbImage::from_pixel(20000, 20, image::Rgb([0, 0, 0]));
        let (_, w3) = rec_preprocess(&absurd);
        assert_eq!(w3, REC_MAX_WIDTH);
    }

    #[test]
    fn crop_quad_rectifies_axis_aligned_and_rotated() {
        // axis-aligned: crop is the exact rectangle
        let img = image::RgbImage::from_fn(100, 100, |x, y| image::Rgb([x as u8, y as u8, 60]));
        let q: Quad = [
            Pt::new(10.0, 20.0),
            Pt::new(60.0, 20.0),
            Pt::new(60.0, 50.0),
            Pt::new(10.0, 50.0),
        ];
        let crop = crop_quad(&img, &q).unwrap();
        assert_eq!(crop.dimensions(), (50, 30));
        assert_eq!(*crop.get_pixel(0, 0), image::Rgb([10, 20, 60]));
        // exact copy (the fast path): last pixel = source (59, 49)
        assert_eq!(*crop.get_pixel(49, 29), image::Rgb([59, 49, 60]));
        // rotated 90°: a tall quad comes out rotated to horizontal reading
        let q: Quad = [
            Pt::new(10.0, 20.0),
            Pt::new(30.0, 20.0),
            Pt::new(30.0, 80.0),
            Pt::new(10.0, 80.0),
        ];
        let crop = crop_quad(&img, &q).unwrap();
        assert_eq!(crop.dimensions(), (60, 20), "tall crop rotated 270°");
        // a 45°-rotated 40×40 square (diamond): rectifies to 40×40
        let q: Quad = [
            Pt::new(34.1, 15.9),
            Pt::new(62.4, 44.1),
            Pt::new(34.1, 72.4),
            Pt::new(5.9, 44.1),
        ];
        let crop = crop_quad(&img, &q).unwrap();
        assert_eq!(crop.dimensions(), (40, 40));
        // degenerate quad → None
        let bad: Quad = [Pt::new(0.0, 0.0); 4];
        assert!(crop_quad(&img, &bad).is_none());
    }

    #[test]
    fn homography_maps_unit_corners_exactly() {
        let q: Quad = [
            Pt::new(3.0, 5.0),
            Pt::new(53.0, 7.0),
            Pt::new(51.0, 37.0),
            Pt::new(1.0, 35.0),
        ];
        let dst = [
            Pt::new(0.0, 0.0),
            Pt::new(50.0, 0.0),
            Pt::new(50.0, 30.0),
            Pt::new(0.0, 30.0),
        ];
        let h = solve_homography(&dst, &q).unwrap();
        for (d, s) in dst.iter().zip(q.iter()) {
            let (x, y) = h.apply(d.x, d.y);
            assert!(
                (x - s.x).abs() < 1e-3 && (y - s.y).abs() < 1e-3,
                "{x},{y} vs {s:?}"
            );
        }
    }
}
