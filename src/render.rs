//! The renderer core: no I/O, draws into caller-owned pixel buffers.
//!
//! Pixel format everywhere: premultiplied alpha, bytes B, G, R, A (the
//! memory layout of `WL_SHM_FORMAT_ARGB8888` and of a 32-bit ARGB X11 visual
//! on little-endian machines). Row stride is `width * 4`.
//!
//! Geometry follows upstream's cu-glow page (Claude Desktop 5.1231): sizes
//! are CSS (logical) pixels and get multiplied by the buffer scale.

use std::cell::RefCell;

use tiny_skia::{
    Color, FillRule, GradientStop, LineJoin, Mask, Paint, PathBuilder, PixmapMut, Point,
    RadialGradient, SpreadMode, Stroke, Transform,
};

use crate::anim::{PressFrame, RING_BORDER, RING_RADIUS};

/// Upstream's clay, `rgb(217, 119, 87)`.
pub const CLAY: (u8, u8, u8) = (217, 119, 87);

/// One inset box-shadow band of the glow: CSS blur radius and alpha.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub blur: f32,
    pub alpha: f32,
}

const fn band(blur: f32, alpha: f32) -> Band {
    Band { blur, alpha }
}

/// The glow's 0 % / 100 % keyframe (and its reduced-motion look).
pub const GLOW_BASE: [Band; 3] = [band(50.0, 0.5), band(100.0, 0.3), band(150.0, 0.1)];
/// The glow's 50 % keyframe.
pub const GLOW_PEAK: [Band; 3] = [band(65.0, 0.7), band(115.0, 0.5), band(165.0, 0.2)];
/// `box-shadow: inset 0 0 0 2px` of the own-cursor look.
pub const RIM_WIDTH: f32 = 2.0;

/// The arrow (upstream's SVG path, viewBox 0 0 18 18, drawn 22 px wide).
pub const ARROW_PATH: &str =
    "M3.17 2.68 L13.50 7.61 L10.47 8.58 Q8.83 9.11 8.21 10.72 L7.06 13.72 Z";

/// Cursor surface: logical size and where its hotspot sits inside it. Room
/// for the arrow at full lift, its shadow and a small tilt.
pub const CURSOR_SIZE: i32 = 192;
pub const CURSOR_HOT: i32 = 64;
/// Ring surface: logical size, centred on the press point (the ring ends at
/// 34 * 1.45 = 49.3 px).
pub const RING_SIZE: i32 = 112;
pub const RING_HALF: i32 = RING_SIZE / 2;

fn bgra(r: u8, g: u8, b: u8, a: f32) -> Color {
    // tiny-skia writes R,G,B,A; the buffer is B,G,R,A: swap red and blue.
    Color::from_rgba8(b, g, r, (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Complementary error function (Abramowitz and Stegun 7.1.26, |err| < 1.5e-7).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * z);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let r = poly * (-z * z).exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

/// Standard normal CDF.
fn phi(x: f64) -> f64 {
    0.5 * erfc(-x / std::f64::consts::SQRT_2)
}

/// Fraction of a Gaussian (sigma `s`) centred on each pixel of a `n` pixel
/// long box that falls inside the box: 1 deep inside, 0.5 on the edge.
fn inside_profile(n: usize, s: f64) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let c = i as f64 + 0.5;
            (phi(c / s) - phi((c - n as f64) / s)) as f32
        })
        .collect()
}

/// Clear a buffer to transparent.
pub fn clear(buf: &mut [u8]) {
    buf.fill(0);
}

/// The classic glow: inset box-shadows (blur `b` = a Gaussian with sigma
/// b/2, as the CSS spec defines it), composited source-over in clay.
/// `buf` is cleared first. `scale` is physical pixels per logical pixel.
pub fn draw_glow(buf: &mut [u8], w: usize, h: usize, scale: f32, bands: &[Band]) {
    clear(buf);
    if w == 0 || h == 0 || bands.is_empty() {
        return;
    }
    let mut prof = Vec::with_capacity(bands.len());
    let mut margin = 0usize;
    for b in bands {
        let sigma = (b.blur as f64 / 2.0 * scale as f64).max(0.5);
        prof.push((b.alpha, inside_profile(w, sigma), inside_profile(h, sigma)));
        // beyond 3.5 sigma the band adds less than 1/255
        margin = margin.max((3.5 * sigma).ceil() as usize);
    }
    let (r, g, bl) = (CLAY.0 as f32, CLAY.1 as f32, CLAY.2 as f32);
    let px = |x: usize, y: usize, row: &mut [u8]| {
        let mut t = 1.0f32;
        for (a, pxs, pys) in &prof {
            t *= 1.0 - a * (1.0 - pxs[x] * pys[y]);
        }
        let alpha = 1.0 - t;
        if alpha > 0.5 / 255.0 {
            let o = x * 4;
            row[o] = (bl * alpha + 0.5) as u8;
            row[o + 1] = (g * alpha + 0.5) as u8;
            row[o + 2] = (r * alpha + 0.5) as u8;
            row[o + 3] = (255.0 * alpha + 0.5) as u8;
        }
    };
    for (y, row) in buf.chunks_exact_mut(w * 4).take(h).enumerate() {
        if y < margin || y + margin >= h || 2 * margin >= w {
            for x in 0..w {
                px(x, y, row);
            }
        } else {
            for x in (0..margin).chain(w - margin..w) {
                px(x, y, row);
            }
        }
    }
}

/// The own-cursor rim: a 2 px solid clay border on all four edges.
/// `buf` is cleared first.
pub fn draw_rim(buf: &mut [u8], w: usize, h: usize, scale: f32) {
    clear(buf);
    let t = ((RIM_WIDTH * scale).round() as usize).max(1);
    let px = [CLAY.2, CLAY.1, CLAY.0, 255];
    for (y, row) in buf.chunks_exact_mut(w * 4).take(h).enumerate() {
        let full = y < t || y + t >= h;
        for x in 0..w {
            if full || x < t || x + t >= w {
                row[x * 4..x * 4 + 4].copy_from_slice(&px);
            }
        }
    }
}

/// Multiply a premultiplied buffer by `opacity` (group opacity).
pub fn apply_opacity(buf: &mut [u8], opacity: f32) {
    if opacity >= 1.0 {
        return;
    }
    let k = (opacity.max(0.0) * 256.0) as u32;
    for v in buf.iter_mut() {
        *v = ((*v as u32 * k) >> 8) as u8;
    }
}

pub fn arrow_path() -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    pb.move_to(3.17, 2.68);
    pb.line_to(13.50, 7.61);
    pb.line_to(10.47, 8.58);
    pb.quad_to(8.83, 9.11, 8.21, 10.72);
    pb.line_to(7.06, 13.72);
    pb.close();
    pb.finish().expect("static arrow path")
}

/// The arrow's pose inside a buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArrowParams {
    /// Hotspot position inside the buffer, logical pixels.
    pub hot_x: f32,
    pub hot_y: f32,
    /// Tilt in degrees (clockwise).
    pub deg: f32,
    /// Lift 0..1 (bigger, softer shadow).
    pub lift: f32,
    /// Press bump scale around the hotspot.
    pub press_scale: f32,
}

/// The transform from arrow viewBox units to buffer pixels, reproducing
/// upstream's element chain: `#cursor-tilt` rotates around (2.2, 1.6) in
/// viewBox units, the 22 px SVG maps 18 units to 22 px and sits at (-3, -2)
/// so its point (3, 2) px is the hotspot, `#cursor-arrow` scales around the
/// hotspot (press bump) and `#cursor-body` scales by 2 * (1 + 0.06 lift).
pub fn arrow_transform(p: &ArrowParams, scale: f32) -> Transform {
    let (sin, cos) = p.deg.to_radians().sin_cos();
    let k1 = 22.0 / 18.0;
    let k2 = 2.0 * (1.0 + 0.06 * p.lift) * p.press_scale;
    let m = scale * k2 * k1;
    // v' = R (v - c) + c ; u = k1 v' - (3,2) ; px = scale (hot + k2 u)
    let (cx, cy) = (2.2f32, 1.6f32);
    let rcx = cos * cx - sin * cy;
    let rcy = sin * cx + cos * cy;
    let tx = scale * (p.hot_x + k2 * (k1 * (cx - rcx) - 3.0));
    let ty = scale * (p.hot_y + k2 * (k1 * (cy - rcy) - 2.0));
    Transform::from_row(m * cos, m * sin, -m * sin, m * cos, tx, ty)
}

/// Box sizes for three box blurs approximating a Gaussian of `sigma`.
fn boxes_for_gauss(sigma: f32) -> [usize; 3] {
    let n = 3.0f32;
    let w_ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = w_ideal.floor() as i32;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wl = wl.max(1);
    let wu = wl + 2;
    let m_ideal = (12.0 * sigma * sigma - n * (wl * wl) as f32 - 4.0 * n * wl as f32 - 3.0 * n)
        / (-4.0 * wl as f32 - 4.0);
    let m = m_ideal.round() as i32;
    let mut out = [0usize; 3];
    for (i, o) in out.iter_mut().enumerate() {
        *o = if (i as i32) < m { wl } else { wu } as usize;
    }
    out
}

/// One box blur pass of radius `r` along rows (`horizontal`) or columns,
/// limited to the rectangle `x0..x1, y0..y1`.
fn box_pass(
    src: &[u8],
    dst: &mut [u8],
    w: usize,
    r: usize,
    horizontal: bool,
    (x0, x1, y0, y1): (usize, usize, usize, usize),
) {
    let div = (2 * r + 1) as u32;
    let (outer, inner) = if horizontal {
        (y0..y1, x0..x1)
    } else {
        (x0..x1, y0..y1)
    };
    let idx = |o: usize, i: usize| if horizontal { o * w + i } else { i * w + o };
    for o in outer {
        let (lo, hi) = (inner.start as isize, inner.end as isize);
        let at = |i: isize| -> u32 {
            if i < lo || i >= hi {
                0
            } else {
                src[idx(o, i as usize)] as u32
            }
        };
        let mut acc: u32 = 0;
        for i in -(r as isize)..=(r as isize) {
            acc += at(lo + i);
        }
        for i in lo..hi {
            dst[idx(o, i as usize)] = ((acc + div / 2) / div) as u8;
            acc += at(i + r as isize + 1);
            acc -= at(i - r as isize);
        }
    }
}

/// Reusable per-thread memory for [`draw_arrow`]: the silhouette mask and
/// two blur planes, so a glide does not allocate per frame.
struct Scratch {
    sil: Option<Mask>,
    a: Vec<u8>,
    b: Vec<u8>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = const {
        RefCell::new(Scratch {
            sil: None,
            a: Vec::new(),
            b: Vec::new(),
        })
    };
}

impl Scratch {
    fn ensure(&mut self, w: usize, h: usize) -> bool {
        let fits = self
            .sil
            .as_ref()
            .is_some_and(|m| m.width() as usize == w && m.height() as usize == h);
        if !fits {
            self.sil = Mask::new(w as u32, h as u32);
            self.a = vec![0; w * h];
            self.b = vec![0; w * h];
        }
        self.sil.is_some()
    }
}

/// `x0, x1, y0, y1` (end exclusive).
type Region = (usize, usize, usize, usize);

/// Composite a drop shadow of the silhouette `sil` (moved down by `dy` px,
/// blurred by `sigma` px) in black at `alpha` onto `dst`
/// (premultiplied source-over). Only the region around `bbox` is touched.
#[allow(clippy::too_many_arguments)]
fn drop_shadow(
    dst: &mut [u8],
    (w, h): (usize, usize),
    sil: &[u8],
    bbox: Region,
    (sigma, dy, alpha): (f32, f32, f32),
    a: &mut [u8],
    b: &mut [u8],
) {
    let shift = dy.round().max(0.0) as usize;
    let blur = sigma >= 0.3;
    let g = if blur {
        (3.0 * sigma).ceil() as usize + 1
    } else {
        0
    };
    let rect = (
        bbox.0.saturating_sub(g),
        (bbox.1 + g).min(w),
        (bbox.2 + shift).saturating_sub(g).min(h),
        (bbox.3 + shift + g).min(h),
    );
    if rect.0 >= rect.1 || rect.2 >= rect.3 {
        return;
    }
    for y in rect.2..rect.3 {
        let row = &mut a[y * w + rect.0..y * w + rect.1];
        if y >= shift {
            let src = (y - shift) * w;
            row.copy_from_slice(&sil[src + rect.0..src + rect.1]);
        } else {
            row.fill(0);
        }
    }
    if blur {
        for bx in boxes_for_gauss(sigma) {
            let r = (bx - 1) / 2;
            box_pass(a, b, w, r, true, rect);
            box_pass(b, a, w, r, false, rect);
        }
    }
    let k = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    for y in rect.2..rect.3 {
        for x in rect.0..rect.1 {
            let m = (a[y * w + x] as u32 * k + 127) / 255;
            if m == 0 {
                continue;
            }
            let inv = 255 - m;
            let o = (y * w + x) * 4;
            for c in &mut dst[o..o + 3] {
                *c = ((*c as u32 * inv + 127) / 255) as u8;
            }
            dst[o + 3] = (m + (dst[o + 3] as u32 * inv + 127) / 255) as u8;
        }
    }
}

/// The drawn arrow: white outline, clay body, upstream's two drop shadows
/// (`0 .5px .6px rgba(0,0,0,.2)` and `0 (1.5 + 4.5 lift)px (2.5 + 6 lift)px
/// rgba(0,0,0,.25 - .08 lift)` in body units). `buf` is cleared first.
pub fn draw_arrow(buf: &mut [u8], w: usize, h: usize, scale: f32, p: &ArrowParams, opacity: f32) {
    clear(buf);
    let path = arrow_path();
    let tf = arrow_transform(p, scale);
    let outline = Stroke {
        width: 4.4,
        line_join: LineJoin::Round,
        ..Default::default()
    };
    let body = Stroke {
        width: 2.3,
        line_join: LineJoin::Round,
        ..Default::default()
    };
    let Some(white) = path.stroke(&outline, 1.0) else {
        return;
    };

    // shadows of the silhouette (the white layer: fill + 4.4 stroke)
    if let Some(bounds) = white.bounds().transform(tf) {
        let bbox = (
            (bounds.left().floor() as isize - 2).clamp(0, w as isize) as usize,
            (bounds.right().ceil() as isize + 2).clamp(0, w as isize) as usize,
            (bounds.top().floor() as isize - 2).clamp(0, h as isize) as usize,
            (bounds.bottom().ceil() as isize + 2).clamp(0, h as isize) as usize,
        );
        SCRATCH.with_borrow_mut(|sc| {
            if !sc.ensure(w, h) {
                return;
            }
            let Scratch { sil, a, b } = sc;
            let sil = sil.as_mut().expect("ensured");
            sil.clear();
            sil.fill_path(&path, FillRule::Winding, true, tf);
            sil.fill_path(&white, FillRule::Winding, true, tf);
            let body_k = 2.0 * (1.0 + 0.06 * p.lift) * scale;
            let lift = p.lift.clamp(0.0, 1.0);
            // drop-shadow's blur length is the Gaussian's standard deviation
            // (SVG feGaussianBlur stdDeviation), in body units
            let shadows = [
                (
                    (2.5 + 6.0 * lift) * body_k,
                    (1.5 + 4.5 * lift) * body_k,
                    0.25 - 0.08 * lift,
                ),
                (0.6 * body_k, 0.5 * body_k, 0.2),
            ];
            for sh in shadows {
                drop_shadow(buf, (w, h), sil.data(), bbox, sh, a, b);
            }
        });
    }

    let Some(mut pm) = PixmapMut::from_bytes(buf, w as u32, h as u32) else {
        return;
    };
    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    paint.set_color(bgra(255, 255, 255, 1.0));
    pm.fill_path(&white, &paint, FillRule::Winding, tf, None);
    pm.fill_path(&path, &paint, FillRule::Winding, tf, None);
    paint.set_color(bgra(CLAY.0, CLAY.1, CLAY.2, 1.0));
    pm.stroke_path(&path, &paint, &body, tf, None);
    pm.fill_path(&path, &paint, FillRule::Winding, tf, None);
    apply_opacity(buf, opacity);
}

#[allow(clippy::too_many_arguments)]
/// The press ring and bloom centred on (`cx`, `cy`) logical inside the
/// buffer. `buf` is cleared first.
pub fn draw_ring(
    buf: &mut [u8],
    w: usize,
    h: usize,
    scale: f32,
    cx: f32,
    cy: f32,
    f: &PressFrame,
    opacity: f32,
) {
    clear(buf);
    let Some(mut pm) = PixmapMut::from_bytes(buf, w as u32, h as u32) else {
        return;
    };
    let (x, y) = (cx * scale, cy * scale);
    let (r, g, b) = CLAY;
    if f.bloom_alpha > 0.0 {
        // radial-gradient(closest-side, clay 60 %, transparent), 68 px box
        let rad = (RING_RADIUS * f.bloom_scale) as f32 * scale;
        let a = f.bloom_alpha as f32;
        if let Some(shader) = RadialGradient::new(
            Point::from_xy(x, y),
            Point::from_xy(x, y),
            rad,
            vec![
                GradientStop::new(0.0, bgra(r, g, b, 0.6 * a)),
                GradientStop::new(1.0, bgra(r, g, b, 0.0)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            let paint = Paint {
                shader,
                anti_alias: true,
                ..Default::default()
            };
            if let Some(c) = PathBuilder::from_circle(x, y, rad) {
                pm.fill_path(&c, &paint, FillRule::Winding, Transform::identity(), None);
            }
        }
    }
    if f.ring_alpha > 0.0 {
        // a 2.5 px border inside a 68 px circle, both scaled
        let k = f.ring_scale as f32 * scale;
        let border = RING_BORDER as f32 * k;
        let rad = RING_RADIUS as f32 * k - border / 2.0;
        let mut paint = Paint {
            anti_alias: true,
            ..Default::default()
        };
        paint.set_color(bgra(r, g, b, f.ring_alpha as f32));
        if let Some(c) = PathBuilder::from_circle(x, y, rad.max(0.1)) {
            let stroke = Stroke {
                width: border,
                ..Default::default()
            };
            pm.stroke_path(&c, &paint, &stroke, Transform::identity(), None);
        }
    }
    apply_opacity(buf, opacity);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(buf: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let o = (y * w + x) * 4;
        [buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]
    }

    #[test]
    fn erf_is_accurate() {
        assert!((phi(0.0) - 0.5).abs() < 1e-7);
        assert!((phi(1.0) - 0.841_344_7).abs() < 1e-6);
        assert!((phi(-2.0) - 0.022_750_1).abs() < 1e-6);
    }

    #[test]
    fn rim_on_all_edges_only() {
        let (w, h) = (40, 30);
        let mut b = vec![7u8; w * h * 4];
        draw_rim(&mut b, w, h, 1.5);
        // 2 px * 1.5 = 3 px
        for (x, y) in [
            (0, 15),
            (2, 15),
            (39, 15),
            (37, 15),
            (20, 0),
            (20, 2),
            (20, 29),
            (20, 27),
        ] {
            assert_eq!(px(&b, w, x, y), [87, 119, 217, 255], "({x},{y})");
        }
        for (x, y) in [(3, 15), (36, 15), (20, 3), (20, 26), (20, 15)] {
            assert_eq!(px(&b, w, x, y), [0, 0, 0, 0], "({x},{y})");
        }
    }

    #[test]
    fn glow_matches_measured_upstream_profile() {
        // upstream on X11, peak keyframe, measured edge alpha: about 0.52 at
        // the edge, 0.20 at 50 px, 0.044 at 100 px, 0 in the middle.
        let (w, h) = (800, 600);
        let mut b = vec![0u8; w * h * 4];
        draw_glow(&mut b, w, h, 1.0, &GLOW_PEAK);
        let a = |x: usize| px(&b, w, x, 300)[3] as f32 / 255.0;
        assert!((a(0) - 0.55).abs() < 0.05, "edge {}", a(0));
        assert!((a(50) - 0.19).abs() < 0.03, "50px {}", a(50));
        assert!((a(100) - 0.045).abs() < 0.02, "100px {}", a(100));
        assert_eq!(a(400), 0.0);
        // premultiplied clay: B < G < R, all <= A
        let p = px(&b, w, 0, 300);
        assert!(p[0] < p[1] && p[1] < p[2] && p[2] <= p[3]);
        // symmetric edges, corners strongest
        assert_eq!(px(&b, w, 0, 300), px(&b, w, w - 1, 300));
        assert_eq!(px(&b, w, 400, 0), px(&b, w, 400, h - 1));
        assert!(px(&b, w, 0, 0)[3] > px(&b, w, 0, 300)[3]);
        // the base keyframe is dimmer
        let mut base = vec![0u8; w * h * 4];
        draw_glow(&mut base, w, h, 1.0, &GLOW_BASE);
        assert!(px(&base, w, 0, 300)[3] < px(&b, w, 0, 300)[3]);
    }

    #[test]
    fn glow_scales_with_buffer_scale() {
        let (w, h) = (600, 400);
        let mut b1 = vec![0u8; w * h * 4];
        let mut b2 = vec![0u8; w * 2 * h * 2 * 4];
        draw_glow(&mut b1, w, h, 1.0, &GLOW_BASE);
        draw_glow(&mut b2, w * 2, h * 2, 2.0, &GLOW_BASE);
        let a1 = px(&b1, w, 30, 200)[3] as i32;
        let a2 = px(&b2, w * 2, 60, 400)[3] as i32;
        assert!((a1 - a2).abs() <= 2, "{a1} vs {a2}");
    }

    #[test]
    fn arrow_tip_at_the_hotspot() {
        let s = 1.0;
        let (w, h) = (CURSOR_SIZE as usize, CURSOR_SIZE as usize);
        let mut b = vec![0u8; w * h * 4];
        let p = ArrowParams {
            hot_x: CURSOR_HOT as f32,
            hot_y: CURSOR_HOT as f32,
            deg: 0.0,
            lift: 0.0,
            press_scale: 1.0,
        };
        draw_arrow(&mut b, w, h, s, &p, 1.0);
        let hot = CURSOR_HOT as usize;
        // the tip (3.17, 2.68) vb lands about (+1.7, +2.6) px from the
        // hotspot and is clay; its white outline reaches up-left past the
        // hotspot
        let tip = px(&b, w, hot + 2, hot + 3);
        assert!(
            tip[3] == 255 && tip[2] > 200 && tip[0] < 110,
            "clay at the tip {tip:?}"
        );
        let rim = px(&b, w, hot - 1, hot - 1);
        assert!(
            rim[3] > 200 && rim[0] > 200 && rim[1] > 200,
            "white outline {rim:?}"
        );
        // body: clay, about 10 px right and 10 px down of the tip
        let body = px(&b, w, hot + 12, hot + 12);
        assert_eq!(body[3], 255);
        assert!(body[2] > 200 && body[0] < 110, "clay body {body:?}");
        // nothing up-left of the hotspot except faint shadow
        assert!(px(&b, w, hot - 10, hot - 10)[3] < 10);
        // shadow below the arrow is dark and translucent
        let mut found = false;
        for y in hot + 30..hot + 45 {
            let p = px(&b, w, hot + 8, y);
            if p[3] > 5 && p[3] < 120 && p[2] < 20 {
                found = true;
            }
        }
        assert!(found, "drop shadow under the arrow");
    }

    #[test]
    fn arrow_transform_maps_hotspot() {
        let p = ArrowParams {
            hot_x: 10.0,
            hot_y: 20.0,
            deg: 30.0,
            lift: 0.5,
            press_scale: 0.9,
        };
        // the hotspot is viewBox point (3, 2) * 18/22 when untilted; with a
        // tilt it moves around (2.2, 1.6), so check the untilted case
        let p0 = ArrowParams { deg: 0.0, ..p };
        let mut pt = [Point::from_xy(3.0 * 18.0 / 22.0, 2.0 * 18.0 / 22.0)];
        arrow_transform(&p0, 1.5).map_points(&mut pt);
        assert!((pt[0].x - 15.0).abs() < 1e-3 && (pt[0].y - 30.0).abs() < 1e-3);
        // the tilt pivot (2.2, 1.6) does not move under rotation
        let mut a = [Point::from_xy(2.2, 1.6)];
        let mut b = [Point::from_xy(2.2, 1.6)];
        arrow_transform(&p, 1.0).map_points(&mut a);
        arrow_transform(&p0, 1.0).map_points(&mut b);
        assert!((a[0].x - b[0].x).abs() < 1e-3 && (a[0].y - b[0].y).abs() < 1e-3);
    }

    #[test]
    fn ring_draws_an_annulus() {
        let s = 1.0;
        let (w, h) = (RING_SIZE as usize, RING_SIZE as usize);
        let mut b = vec![0u8; w * h * 4];
        let f = PressFrame {
            ring_scale: 1.0,
            ring_alpha: 0.7,
            bloom_scale: 1.0,
            bloom_alpha: 0.0,
            arrow_scale: 1.0,
        };
        let c = RING_HALF as f32;
        draw_ring(&mut b, w, h, s, c, c, &f, 1.0);
        let c = RING_HALF as usize;
        // on the ring (radius 32.75), alpha about 0.7
        let on = px(&b, w, c + 33, c);
        assert!((on[3] as i32 - 178).abs() < 40, "{on:?}");
        assert_eq!(px(&b, w, c, c)[3], 0, "hollow centre");
        assert_eq!(px(&b, w, c + 40, c)[3], 0, "nothing outside");
        // bloom fills the centre
        let f = PressFrame {
            bloom_alpha: 0.5,
            ..f
        };
        draw_ring(&mut b, w, h, s, c as f32, c as f32, &f, 1.0);
        assert!(px(&b, w, c, c)[3] > 50);
        // group opacity 0 clears everything
        draw_ring(&mut b, w, h, s, c as f32, c as f32, &f, 0.0);
        assert!(b.iter().all(|&v| v == 0));
    }

    #[test]
    fn box_sizes() {
        let b = boxes_for_gauss(5.0);
        assert!(b.iter().all(|&x| x % 2 == 1));
        let var: f32 = b.iter().map(|&x| ((x * x - 1) as f32) / 12.0).sum();
        assert!((var.sqrt() - 5.0).abs() < 0.6, "{b:?}");
    }
}
