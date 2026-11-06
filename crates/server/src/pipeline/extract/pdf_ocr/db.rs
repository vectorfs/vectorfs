//! DB (Differentiable Binarization) det post-processing — pure geometry, no
//! deps beyond `image` (M3-T10).
//!
//! The pipeline: probability map → binary mask (`thresh`) → connected
//! components → per component min-area rect (`min_size` gate) → mean
//! probability inside the rect (`box_thresh` gate) → polygon offset by
//! `area · unclip_ratio / perimeter` → min-area rect of the offset →
//! scaled back to page coordinates → reading order.
//!
//! # Reference provenance
//!
//! The algorithm is PaddleOCR's `DBPostProcess` (Apache-2.0,
//! PaddlePaddle/PaddleOCR `ppocr/postprocess/db_postprocess.py`), with the
//! Rust-level mechanics sourced from the Apache-2.0 **oar-ocr** crate
//! (github.com/greatv/oar-ocr, `oar-ocr-core/src/processors/db_*.rs`,
//! studied 2026-11-07) — the §0 2026-11-07 ruling demoted oar-ocr to a
//! REFERENCE because its `ort =2.0.0-rc.13` pin would double our runtime;
//! this file reimplements the same semantics against in-tree types only.
//! Two deliberate substitutions (both documented at their sites):
//! * connected-component labeling + boundary points instead of cv2
//!   `findContours` (the min-area rect over a component's boundary points
//!   equals the one over its contour chain — both are the hull's);
//! * exact miter offset of the (always convex) quad instead of pyclipper's
//!   round-join inflate (`unclip`'s input is a min-area RECT, and the
//!   second min-area rect over either offset shape is the same rectangle
//!   — corner arcs vs mitered corners only differ below the tangent lines).

use image::RgbImage;

/// DB postprocess constants — copied from the PINNED artifact's
/// `inference.yml` (PP-OCRv5_mobile_det_onnx, verified 2026-11-07): they are
/// properties of the trained weights, not policy (the siglip2 preprocessing
/// ruling).
pub(super) const THRESH: f32 = 0.3;
pub(super) const BOX_THRESH: f32 = 0.6;
pub(super) const MAX_CANDIDATES: usize = 1000;
pub(super) const UNCLIP_RATIO: f32 = 1.5;
/// PaddleOCR's min box side (their `min_size = 3`); +2 after unclip.
pub(super) const MIN_SIZE: f32 = 3.0;
/// det preprocessing: `DetResizeForTest: resize_long: 960` (the same yml).
pub(super) const RESIZE_LONG: u32 = 960;

/// A point in page-pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Pt {
    pub x: f32,
    pub y: f32,
}

impl Pt {
    pub(super) fn new(x: f32, y: f32) -> Self {
        Pt { x, y }
    }
}

/// A detection quad in reading orientation: top-left, top-right,
/// bottom-right, bottom-left.
pub(super) type Quad = [Pt; 4];

// ---------------------------------------------------------------------------
// det preprocessing (resize_long 960 + BGR + ImageNet normalize + CHW)
// ---------------------------------------------------------------------------

/// det preprocessing per the artifact's yml: downscale so the long side is
/// at most [`RESIZE_LONG`] (never upscale — PaddleOCR's `resize_long`), snap
/// both sides UP to a multiple of 32 (the DB head's upsample stride — its
/// Resize ops fail on other shapes, discovered against the real model; the
/// yml's 32×32 dynamic-shape floor is the same constraint, and the ≤32 px
/// stretch is the ONNX deployment convention, RapidOCR's behavior), then
/// normalize in **BGR** channel order with the ImageNet mean/std the yml
/// lists (PaddleOCR reads images as BGR; the stats apply in that order —
/// the oar-ocr note), laid out CHW float32. Returns (tensor, resized_w,
/// resized_h). Pure: the unit-test target.
pub(super) fn det_preprocess(rgb: &RgbImage) -> (Vec<f32>, u32, u32) {
    let round32 = |v: u32| -> u32 { v.div_ceil(32) * 32 };
    let (w, h) = rgb.dimensions();
    let long = w.max(h);
    let scale = if long > RESIZE_LONG {
        RESIZE_LONG as f32 / long as f32
    } else {
        1.0
    };
    let (rw, rh) = (
        round32(((w as f32 * scale).round() as u32).max(1)),
        round32(((h as f32 * scale).round() as u32).max(1)),
    );
    let resized = if (rw, rh) != (w, h) {
        image::imageops::resize(rgb, rw, rh, image::imageops::FilterType::Triangle)
    } else {
        rgb.clone()
    };
    // (v / 255 − mean) / std per channel, BGR planes (yml `img_mode: BGR`)
    const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const STD: [f32; 3] = [0.229, 0.224, 0.225];
    let plane = (rw * rh) as usize;
    let mut out = vec![0f32; 3 * plane];
    for (i, px) in resized.pixels().enumerate() {
        for c in 0..3 {
            // plane 0 = B (source index 2), 1 = G, 2 = R
            let v = px.0[2 - c] as f32 / 255.0;
            out[c * plane + i] = (v - MEAN[c]) / STD[c];
        }
    }
    (out, rw, rh)
}

// ---------------------------------------------------------------------------
// geometry: hull, min-area rect, ordering, offset
// ---------------------------------------------------------------------------

/// Andrew monotone chain over the component's boundary points (duplicates
/// and collinear midpoints are fine; output is strictly convex, CCW in
/// y-down screen coordinates).
fn convex_hull(points: &[Pt]) -> Vec<Pt> {
    let mut pts = points.to_vec();
    if pts.len() < 3 {
        pts.dedup();
        return pts;
    }
    pts.sort_by(|a, b| {
        a.x.partial_cmp(&b.x)
            .unwrap()
            .then(a.y.partial_cmp(&b.y).unwrap())
    });
    pts.dedup();
    let cross = |o: &Pt, a: &Pt, b: &Pt| (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x);
    let mut lower: Vec<Pt> = Vec::with_capacity(pts.len());
    for p in &pts {
        while lower.len() >= 2 && cross(&lower[lower.len() - 2], &lower[lower.len() - 1], p) <= 0.0
        {
            lower.pop();
        }
        lower.push(*p);
    }
    let mut upper: Vec<Pt> = Vec::with_capacity(pts.len());
    for p in pts.iter().rev() {
        while upper.len() >= 2 && cross(&upper[upper.len() - 2], &upper[upper.len() - 1], p) <= 0.0
        {
            upper.pop();
        }
        upper.push(*p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// Min-area enclosing rectangle of a point set: convex hull + rotating
/// calipers (the min-area rect has one side collinear with a hull edge).
/// Returns the rect's corner quad (reading-ordered, see [`order_quad`]) and
/// its (long_side, short_side). `None` for degenerate (< 3 hull points)
/// input.
fn min_area_quad(points: &[Pt]) -> Option<(Quad, f32, f32)> {
    let hull = convex_hull(points);
    if hull.len() < 3 {
        return None;
    }
    let mut best: Option<(f32, Pt, Pt, f32, f32)> = None; // (area, u, v, w, h)
    let n = hull.len();
    for i in 0..n {
        let a = hull[i];
        let b = hull[(i + 1) % n];
        let len = (b.x - a.x).hypot(b.y - a.y);
        if len <= f32::EPSILON {
            continue;
        }
        let u = Pt::new((b.x - a.x) / len, (b.y - a.y) / len);
        let v = Pt::new(-u.y, u.x);
        let (mut min_u, mut max_u, mut min_v, mut max_v) = (
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
        );
        for p in &hull {
            let pu = p.x * u.x + p.y * u.y;
            let pv = p.x * v.x + p.y * v.y;
            min_u = min_u.min(pu);
            max_u = max_u.max(pu);
            min_v = min_v.min(pv);
            max_v = max_v.max(pv);
        }
        let (wu, hv) = (max_u - min_u, max_v - min_v);
        let area = wu * hv;
        if best.map(|(ba, ..)| area < ba).unwrap_or(true) {
            best = Some((area, u, v, wu, hv));
        }
    }
    let (_, u, v, wu, hv) = best?;
    let cu = (min_proj(&hull, u) + max_proj(&hull, u)) / 2.0;
    let cv = (min_proj(&hull, v) + max_proj(&hull, v)) / 2.0;
    let center = Pt::new(cu * u.x + cv * v.x, cu * u.y + cv * v.y);
    // long side = the reading direction (x axis of the crop)
    let (ux, vx, w, h) = if wu >= hv {
        (u, v, wu, hv)
    } else {
        (v, u, hv, wu)
    };
    let corner = |su: f32, sv: f32| {
        Pt::new(
            center.x + ux.x * su * w / 2.0 + vx.x * sv * h / 2.0,
            center.y + ux.y * su * w / 2.0 + vx.y * sv * h / 2.0,
        )
    };
    Some((
        order_quad(&[
            corner(-1.0, -1.0),
            corner(1.0, -1.0),
            corner(1.0, 1.0),
            corner(-1.0, 1.0),
        ]),
        w,
        h,
    ))
}

fn min_proj(points: &[Pt], axis: Pt) -> f32 {
    points
        .iter()
        .map(|p| p.x * axis.x + p.y * axis.y)
        .fold(f32::INFINITY, f32::min)
}

fn max_proj(points: &[Pt], axis: Pt) -> f32 {
    points
        .iter()
        .map(|p| p.x * axis.x + p.y * axis.y)
        .fold(f32::NEG_INFINITY, f32::max)
}

/// Order any 4 corners into reading orientation (TL, TR, BR, BL) — the
/// PaddleX `get_mini_boxes` ordering: sort by x, the two left-most keep
/// top-before-bottom, likewise the two right-most.
fn order_quad(points: &[Pt]) -> Quad {
    let mut p: Vec<Pt> = points.to_vec();
    p.sort_by(|a, b| {
        a.x.partial_cmp(&b.x)
            .unwrap()
            .then(a.y.partial_cmp(&b.y).unwrap())
    });
    let (i_tl, i_bl) = if p[1].y > p[0].y {
        (0usize, 1usize)
    } else {
        (1, 0)
    };
    let (i_tr, i_br) = if p[3].y > p[2].y {
        (2usize, 3usize)
    } else {
        (3, 2)
    };
    [p[i_tl], p[i_tr], p[i_br], p[i_bl]]
}

fn quad_edges(q: &Quad) -> [(Pt, Pt); 4] {
    [(q[0], q[1]), (q[1], q[2]), (q[2], q[3]), (q[3], q[0])]
}

fn quad_perimeter(q: &Quad) -> f32 {
    quad_edges(q)
        .iter()
        .map(|(a, b)| (b.x - a.x).hypot(b.y - a.y))
        .sum()
}

fn quad_area(q: &Quad) -> f32 {
    // shoelace over the 4 corners (reading order traces the quad once)
    let mut s = 0.0;
    for i in 0..4 {
        let a = q[i];
        let b = q[(i + 1) % 4];
        s += a.x * b.y - b.x * a.y;
    }
    (s / 2.0).abs()
}

fn quad_centroid(q: &Quad) -> Pt {
    Pt::new(
        q.iter().map(|p| p.x).sum::<f32>() / 4.0,
        q.iter().map(|p| p.y).sum::<f32>() / 4.0,
    )
}

/// Expand a convex quad outward by `area · unclip_ratio / perimeter` on
/// every side (PaddleOCR's `unclip`, pyclipper offset): each edge's LINE is
/// shifted outward by that distance and adjacent shifted lines intersect at
/// the new corners (miter join — see module docs for why this matches a
/// round-join inflate through the second min-area rect). `None` on
/// degenerate geometry.
fn unclip_quad(q: &Quad, ratio: f32) -> Option<Quad> {
    let area = quad_area(q);
    let perimeter = quad_perimeter(q);
    if area <= f32::EPSILON || perimeter <= f32::EPSILON {
        return None;
    }
    let delta = area * ratio / perimeter;
    if !delta.is_finite() || delta <= 0.0 {
        return None;
    }
    let centroid = quad_centroid(q);
    // each edge becomes the shifted LINE {m + delta·n + t·d}, with n the
    // outward normal and d = n rotated a quarter turn (the edge direction)
    let mut lines: [(Pt, Pt); 4] = [(Pt::new(0.0, 0.0), Pt::new(0.0, 0.0)); 4]; // (base, dir)
    for (i, (a, b)) in quad_edges(q).iter().enumerate() {
        let len = (b.x - a.x).hypot(b.y - a.y);
        if len <= f32::EPSILON {
            return None;
        }
        let mut n = Pt::new((b.y - a.y) / len, -(b.x - a.x) / len);
        let mid = Pt::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        // orient the normal AWAY from the centroid (robust to either
        // traversal order of the quad)
        let to_centroid = Pt::new(centroid.x - mid.x, centroid.y - mid.y);
        if n.x * to_centroid.x + n.y * to_centroid.y > 0.0 {
            n = Pt::new(-n.x, -n.y);
        }
        lines[i] = (
            Pt::new(mid.x + delta * n.x, mid.y + delta * n.y),
            Pt::new(-n.y, n.x),
        );
    }
    // corner i = intersection of shifted lines i-1 and i (they meet at vertex i)
    let mut out: Quad = [Pt::new(0.0, 0.0); 4];
    for i in 0..4 {
        let (m1, d1) = lines[(i + 3) % 4];
        let (m2, d2) = lines[i];
        // solve m1 + t*d1 = m2 + s*d2 (Cramer) → the corner point
        let den = d1.x * d2.y - d1.y * d2.x;
        if den.abs() < 1e-9 {
            return None; // adjacent edges parallel after offset — degenerate
        }
        let dx = m2.x - m1.x;
        let dy = m2.y - m1.y;
        let t = (dx * d2.y - dy * d2.x) / den;
        let p = Pt::new(m1.x + t * d1.x, m1.y + t * d1.y);
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        out[i] = p;
    }
    Some(out)
}

/// Mean probability inside the quad — PaddleOCR's `box_score_fast`
/// (`cv2.fillPoly` over the box + mean of pred under the mask), as a
/// scanline fill: for each pixel-row center in the quad's y range,
/// intersect the row with the quad edges and average `pred` between the
/// crossing pairs. Deterministic and view-free.
fn box_score_fast(pred: &[f32], pw: usize, ph: usize, q: &Quad) -> f32 {
    let y0 = q.iter().map(|p| p.y).fold(f32::INFINITY, f32::min).floor() as i64;
    let y1 = q
        .iter()
        .map(|p| p.y)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as i64;
    let mut total = 0f32;
    let mut count = 0usize;
    for row in y0.max(0)..=y1.min(ph as i64 - 1) {
        let sy = row as f32 + 0.5;
        let mut xs: Vec<f32> = Vec::with_capacity(4);
        for (a, b) in quad_edges(q) {
            let (ya, yb) = (a.y, b.y);
            if (sy >= ya && sy < yb) || (sy >= yb && sy < ya) {
                let t = (sy - ya) / (yb - ya);
                xs.push(a.x + t * (b.x - a.x));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for pair in xs.as_chunks::<2>().0 {
            let (xa, xb) = (
                (pair[0].ceil() as i64).max(0),
                (pair[1].floor() as i64).min(pw as i64 - 1),
            );
            for x in xa.max(0)..=xb {
                total += pred[row as usize * pw + x as usize];
                count += 1;
            }
        }
    }
    if count == 0 {
        0.0
    } else {
        total / count as f32
    }
}

// ---------------------------------------------------------------------------
// connected components (8-connectivity, BFS; boundary points kept)
// ---------------------------------------------------------------------------

/// One detected region: its boundary points (the contour-point cloud a
/// cv2 `findContours` chain would trace — the min-area rect over both is
/// the hull's).
struct Component {
    boundary: Vec<Pt>,
}

/// Label the binary mask into components; keep each component's boundary
/// points (any pixel with an off-mask 4-neighbor). Iterative BFS (no
/// recursion), one pass, `u32` labels.
fn components(mask: &[bool], w: usize, h: usize) -> Vec<Component> {
    let mut labels = vec![u32::MAX; w * h];
    let mut out = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut next_label = 0u32;
    for start in 0..mask.len() {
        if !mask[start] || labels[start] != u32::MAX {
            continue;
        }
        let label = next_label;
        next_label += 1;
        labels[start] = label;
        stack.clear();
        stack.push(start);
        let mut boundary = Vec::new();
        while let Some(idx) = stack.pop() {
            let (x, y) = (idx % w, idx / w);
            // 8-connectivity for the FLOOD; boundary = a 4-neighbor is off
            // mask (image edge counts as boundary)
            let mut is_boundary = false;
            for (dx, dy, cardinal) in [
                (-1i64, 0i64, true),
                (1, 0, true),
                (0, -1, true),
                (0, 1, true),
                (-1, -1, false),
                (1, -1, false),
                (-1, 1, false),
                (1, 1, false),
            ] {
                let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 {
                    if cardinal {
                        is_boundary = true;
                    }
                    continue;
                }
                let n = ny as usize * w + nx as usize;
                if !mask[n] {
                    if cardinal {
                        is_boundary = true;
                    }
                } else if labels[n] == u32::MAX {
                    labels[n] = label;
                    stack.push(n);
                }
            }
            if is_boundary {
                boundary.push(Pt::new(x as f32, y as f32));
            }
        }
        out.push(Component { boundary });
    }
    out
}

// ---------------------------------------------------------------------------
// reading order
// ---------------------------------------------------------------------------

/// Sort quads into reading order (top-to-bottom, left-to-right within a
/// text row): rows are banded at half the median quad height (so ascenders
/// do not split a line), rows sort by y, members of a row by x. The
/// successor of PaddleOCR's `sorted_boxes`.
pub(super) fn reading_order(mut quads: Vec<Quad>) -> Vec<Quad> {
    if quads.len() <= 1 {
        return quads;
    }
    let mut heights: Vec<f32> = quads
        .iter()
        .map(|q| {
            let top = (q[0].y + q[1].y) / 2.0;
            let bot = (q[3].y + q[2].y) / 2.0;
            (bot - top).abs()
        })
        .collect();
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median_h = heights[heights.len() / 2].max(1.0);
    let band = (median_h * 0.5).max(1.0);
    let key = |q: &Quad| -> (i64, f32) {
        let cy = q.iter().map(|p| p.y).sum::<f32>() / 4.0;
        let cx = q.iter().map(|p| p.x).sum::<f32>() / 4.0;
        ((cy / band) as i64, cx)
    };
    quads.sort_by(|a, b| {
        let (ka, kb) = (key(a), key(b));
        ka.0.cmp(&kb.0)
            .then_with(|| ka.1.partial_cmp(&kb.1).unwrap())
    });
    quads
}

// ---------------------------------------------------------------------------
// the postprocess entry point
// ---------------------------------------------------------------------------

/// DB postprocess (the module's pipeline): `prob` is the det probability
/// map (`[ph][pw]` row-major), `(src_w, src_h)` the ORIGINAL page pixel
/// size the map's coordinates scale back to. Returns reading-ordered
/// quads in page coordinates (each `[TL, TR, BR, BL]`).
pub(super) fn db_postprocess(
    prob: &[f32],
    pw: usize,
    ph: usize,
    src_w: u32,
    src_h: u32,
) -> Vec<Quad> {
    let mask: Vec<bool> = prob.iter().map(|&p| p > THRESH).collect();
    let mut quads: Vec<Quad> = Vec::new();
    for comp in components(&mask, pw, ph).into_iter().take(MAX_CANDIDATES) {
        // PaddleOCR skips contours with < 3 points
        if comp.boundary.len() < 3 {
            continue;
        }
        // first min-area rect: the text region as detected
        let Some((quad, _w, h)) = min_area_quad(&comp.boundary) else {
            continue;
        };
        let min_side = h;
        if min_side < MIN_SIZE {
            continue;
        }
        let score = box_score_fast(prob, pw, ph, &quad);
        if score < BOX_THRESH {
            continue;
        }
        // expand, then take the min-area rect of the expanded region
        let Some(expanded) = unclip_quad(&quad, UNCLIP_RATIO) else {
            continue;
        };
        let Some((final_quad, _w2, h2)) = min_area_quad(&expanded) else {
            continue;
        };
        if h2 < MIN_SIZE + 2.0 {
            continue;
        }
        // scale back to page coordinates (PaddleOCR: x / prob_w * dest_w, clamped)
        let (sx, sy) = (src_w as f32 / pw as f32, src_h as f32 / ph as f32);
        let scaled: Quad = final_quad.map(|p| {
            Pt::new(
                (p.x * sx).round().clamp(0.0, src_w as f32),
                (p.y * sy).round().clamp(0.0, src_h as f32),
            )
        });
        quads.push(scaled);
    }
    reading_order(quads)
}

// ---------------------------------------------------------------------------
// tests — synthetic probability maps with hand-computed expected boxes
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A probability map of `w × h` with a 0.9-valued axis-aligned region
    /// `[x0, x1) × [y0, y1)` and 0.02 background — the canonical "one text
    /// line" specimen.
    fn map_with_region(w: usize, h: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> Vec<f32> {
        let mut v = vec![0.02f32; w * h];
        for y in y0..y1 {
            for x in x0..x1 {
                v[y * w + x] = 0.9;
            }
        }
        v
    }

    fn quad_bounds(q: &Quad) -> (f32, f32, f32, f32) {
        (
            q.iter().map(|p| p.x).fold(f32::INFINITY, f32::min),
            q.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max),
            q.iter().map(|p| p.y).fold(f32::INFINITY, f32::min),
            q.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max),
        )
    }

    #[test]
    fn one_region_yields_one_expanded_axis_aligned_box() {
        // 200×60 map, region [40,160)×[20,40): a 120×20 text line. Expected
        // box: the region grown by the unclip offset
        // delta = area·1.5/perimeter = 2400·1.5/280 ≈ 12.86px on each side.
        let (w, h) = (200usize, 60usize);
        let prob = map_with_region(w, h, 40, 20, 160, 40);
        let quads = db_postprocess(&prob, w, h, 400, 120); // 2× page scale
        assert_eq!(quads.len(), 1, "one region → one box");
        let (x0, x1, y0, y1) = quad_bounds(&quads[0]);
        // the unclip offset: grown by ~12.86 map px (±1 rounding), then ×2
        let d = 2400.0 * 1.5 / 280.0;
        let ex0 = (40.0 - d) * 2.0;
        let ex1 = (160.0 + d) * 2.0;
        let ey0 = (20.0 - d) * 2.0;
        let ey1 = (40.0 + d) * 2.0;
        for (got, want) in [(x0, ex0), (x1, ex1), (y0, ey0), (y1, ey1)] {
            assert!(
                (got - want).abs() <= 4.0,
                "got {got}, expected ~{want} (unclip ±1px + 2× scale rounding)"
            );
        }
        // reading orientation: TL top-left, TR top-right, BR, BL
        let q = quads[0];
        assert!(q[0].y <= q[2].y && q[0].x <= q[1].x, "{q:?}");
    }

    #[test]
    fn two_side_by_side_regions_sort_left_to_right() {
        let (w, h) = (300, 100);
        let mut prob = map_with_region(w, h, 10, 40, 100, 60);
        for y in 40..60 {
            for x in 180..280 {
                prob[y * w + x] = 0.9;
            }
        }
        let quads = db_postprocess(&prob, w, h, 300, 100);
        assert_eq!(quads.len(), 2);
        // both on one text row: x order decides (banded at median height)
        let cx = |q: &Quad| q.iter().map(|p| p.x).sum::<f32>() / 4.0;
        assert!(cx(&quads[0]) < 100.0, "first is the LEFT region");
        assert!(cx(&quads[1]) > 180.0, "second is the RIGHT region");
    }

    #[test]
    fn two_stacked_regions_sort_top_to_bottom() {
        let (w, h) = (100, 200);
        let mut prob = map_with_region(w, h, 20, 20, 80, 40);
        for y in 120..140 {
            for x in 20..80 {
                prob[y * w + x] = 0.9;
            }
        }
        let quads = db_postprocess(&prob, w, h, 100, 200);
        assert_eq!(quads.len(), 2);
        let cy = |q: &Quad| q.iter().map(|p| p.y).sum::<f32>() / 4.0;
        assert!(cy(&quads[0]) < 50.0 && cy(&quads[1]) > 100.0);
    }

    #[test]
    fn low_score_and_tiny_regions_are_filtered() {
        // region at 0.4 mean (above the 0.3 binarize thresh, below the 0.6
        // box_thresh): detected as a component, then dropped by score
        let (w, h) = (100, 100);
        let mut prob = vec![0.02f32; w * h];
        for y in 10..50 {
            for x in 10..90 {
                prob[y * w + x] = 0.4;
            }
        }
        // a 2px-tall region: below min_size 3
        for y in 60..62 {
            for x in 10..90 {
                prob[y * w + x] = 0.9;
            }
        }
        let quads = db_postprocess(&prob, w, h, 100, 100);
        assert!(
            quads.is_empty(),
            "box_thresh + min_size both fire: {quads:?}"
        );
        // sanity: raising the band to 0.9 passes
        for y in 10..50 {
            for x in 10..90 {
                prob[y * w + x] = 0.95;
            }
        }
        assert_eq!(db_postprocess(&prob, w, h, 100, 100).len(), 1);
    }

    #[test]
    fn rotated_region_gets_a_rotated_quad() {
        // a 45° band: the min-area rect must follow the band, not its AABB.
        // Band: points satisfying |x - y| <= 3, x,y in [20, 80) — a diagonal
        // line of slope 1.
        let (w, h) = (100, 100);
        let mut prob = vec![0.02f32; w * h];
        for y in 20..80 {
            for x in 20..80 {
                if (x as i64 - y as i64).abs() <= 3 {
                    prob[y * w + x] = 0.9;
                }
            }
        }
        let quads = db_postprocess(&prob, w, h, 100, 100);
        assert_eq!(quads.len(), 1);
        // the quad's long side direction ≈ (1,1)/√2
        let q = quads[0];
        let dx = q[1].x - q[0].x;
        let dy = q[1].y - q[0].y;
        let len = dx.hypot(dy);
        let cos45 = 1.0 / std::f32::consts::SQRT_2;
        assert!(
            (dx / len - cos45).abs() < 0.05 && (dy / len - cos45).abs() < 0.05,
            "long side follows the 45° band: {q:?}"
        );
    }

    #[test]
    fn det_preprocess_downscales_snaps32_bgr_and_normalizes() {
        // a 32×32 solid image is already at the head's stride: NO resize,
        // so per-plane values are exact (the BGR + ImageNet pin)
        let img = image::RgbImage::from_pixel(32, 32, image::Rgb([200, 100, 10]));
        let (t, w, h) = det_preprocess(&img);
        assert_eq!((w, h), (32, 32));
        assert_eq!(t.len(), 3 * 32 * 32);
        let norm = |v: u8, m: f32, s: f32| (v as f32 / 255.0 - m) / s;
        // plane 0 = B (source idx 2), 1 = G, 2 = R — every plane uniform
        for i in 0..(32 * 32) {
            assert_eq!(t[i], norm(10, 0.485, 0.229));
            assert_eq!(t[32 * 32 + i], norm(100, 0.456, 0.224));
            assert_eq!(t[2 * 32 * 32 + i], norm(200, 0.406, 0.225));
        }
        // a long image downscales AND snaps to /32: 1920×10 → 960×32
        let big = image::RgbImage::from_pixel(1920, 10, image::Rgb([0, 0, 0]));
        let (t2, w2, h2) = det_preprocess(&big);
        assert_eq!((w2, h2), (960, 32), "both sides are multiples of 32");
        assert_eq!(t2.len(), 3 * 960 * 32);
        assert_eq!(t2[0], norm(0, 0.485, 0.229));
        // a sub-stride short side snaps UP: 100×64 → 128×64
        let odd = image::RgbImage::from_pixel(100, 64, image::Rgb([0, 0, 0]));
        let (_, w3, h3) = det_preprocess(&odd);
        assert_eq!((w3, h3), (128, 64));
    }
}
